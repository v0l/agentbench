use crate::git;
use crate::runner::{Origin, Runner};
use crate::store::Store;
use agentbench_core::{Id, NewComment, Project, Session, SessionStatus};
use serde_json::{Value, json};
use std::path::{Component, Path};

const OWNER_RECENT_MS: i64 = 30 * 60_000;
const STATUS_LINES: usize = 30;
const GENERATED: [&str; 8] = ["png", "jpg", "jpeg", "gif", "webp", "kgs", "lock", "log"];
const GENERATED_DIRS: [&str; 3] = ["target/", "node_modules/", "dist/"];

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Advice {
    AskToCommit,
    Reset,
    Stash,
}

pub struct Report {
    pub advice: Advice,
    pub text: String,
}

pub fn is_generated(file: &str) -> bool {
    let ext = file.rsplit('.').next().filter(|_| file.contains('.')).unwrap_or("");
    GENERATED.contains(&ext.to_lowercase().as_str())
        || GENERATED_DIRS.iter().any(|d| file.starts_with(d) || file.contains(&format!("/{d}")))
}

fn ago(ms: i64) -> String {
    agentbench_core::span(ms.max(0))
}

fn owners(store: &Store, project: Id, except: Option<&Session>, now: i64) -> Vec<Session> {
    let Ok(main) = store.main_worktree(project) else { return Vec::new() };
    store
        .sessions()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.project_id == project && s.worktree_id == main.id && Some(s.id) != except.map(|e| e.id))
        .filter(|s| {
            matches!(s.status, SessionStatus::Running | SessionStatus::Waiting)
                || now - s.updated_at < OWNER_RECENT_MS
        })
        .collect()
}

pub fn report(store: &Store, project: &Project, blocked: Option<(Option<Id>, &Session)>, now: i64) -> Report {
    let main = Path::new(&project.path);
    let entries = git::status_entries(main);
    let files: Vec<String> = entries.iter().map(|(_, f)| f.clone()).collect();
    let short = git::dirty_files(main, STATUS_LINES);
    let own = owners(store, project.id, blocked.map(|(_, s)| s), now);
    let mut touching: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for s in store.sessions().unwrap_or_default().iter().filter(|s| s.project_id == project.id) {
        let Ok(tree) = store.worktree(s.worktree_id) else { continue };
        let Some(branch) = tree.branch.filter(|_| !tree.main) else { continue };
        if !seen.insert(branch.clone()) {
            continue;
        }
        let shared: Vec<String> =
            git::changed_files(main, &branch).into_iter().filter(|f| files.contains(f)).collect();
        if !shared.is_empty() {
            touching.push(format!("- {branch} (session {}): {}", s.id, shared.join(", ")));
        }
    }
    let advice = if !own.is_empty() {
        Advice::AskToCommit
    } else if !files.is_empty() && files.iter().all(|f| is_generated(f)) {
        Advice::Reset
    } else {
        Advice::Stash
    };
    let mut text = String::new();
    match blocked {
        Some((Some(issue), s)) => text.push_str(&format!(
            "Dirty main checkout: the merge of #{issue} (session {}) is waiting because the main checkout has uncommitted changes.\n",
            s.id
        )),
        Some((None, s)) => text.push_str(&format!(
            "Dirty main checkout: the merge of session {} is waiting because the main checkout has uncommitted changes.\n",
            s.id
        )),
        None => text.push_str("Dirty main checkout: a merge is waiting because the main checkout has uncommitted changes.\n"),
    }
    text.push_str(&format!("Project: {} (id {})\nMain checkout: {}\n", project.name, project.id, project.path));
    text.push_str(&format!("git status --short (first {STATUS_LINES}):\n{}\n", short.join("\n")));
    text.push_str(&format!("Stashes already there: {}\n", git::stash_count(main)));
    if own.is_empty() {
        text.push_str("Sessions working in the main checkout: none\n");
    } else {
        text.push_str("Sessions working in the main checkout:\n");
        for s in &own {
            text.push_str(&format!(
                "- {} \"{}\" {}, last activity {} ago\n",
                s.id,
                s.title,
                s.status.as_str(),
                ago(now - s.updated_at)
            ));
        }
    }
    if touching.is_empty() {
        text.push_str("Branches touching the same files: none\n");
    } else {
        text.push_str(&format!("Branches touching the same files:\n{}\n", touching.join("\n")));
    }
    let pass = format!("project \"{}\"", project.name);
    match advice {
        Advice::AskToCommit => {
            let first = &own[0];
            text.push_str(&format!(
                "Recommendation: ask that session to commit. Read session {} with read_session to decide between commit and stash, then call main_checkout with {pass}, action ask_session, id {} and request commit (or stash).",
                first.id, first.id
            ));
        }
        Advice::Reset => {
            let tracked: Vec<String> =
                entries.iter().filter(|(st, _)| st != "??").map(|(_, f)| f.clone()).collect();
            let untracked: Vec<String> =
                entries.iter().filter(|(st, _)| st == "??").map(|(_, f)| f.clone()).collect();
            text.push_str("Recommendation: reset. No session owns these changes and they are generated or untracked files.\n");
            if !tracked.is_empty() {
                text.push_str(&format!("Tracked: git checkout -- {}\n", tracked.join(" ")));
            }
            let dry = git::clean_dry_run(main, &untracked);
            if !untracked.is_empty() {
                text.push_str(&format!("git clean -n -- {} says:\n{}\n", untracked.join(" "), dry));
            }
            text.push_str(&format!(
                "Run main_checkout with {pass}, action reset_paths, dry_run true and paths {}, then again without dry_run{}.",
                json!(files),
                if tracked.is_empty() { "" } else { " and force true" }
            ));
        }
        Advice::Stash => {
            text.push_str(&format!(
                "Recommendation: stash with a named message. No session owns these changes and they look like source edits. Call main_checkout with {pass} and action stash; it stashes with a named message and the merge goes through on the next tick. Use commit with your own message instead if they belong on main."
            ));
        }
    }
    Report { advice, text }
}

pub fn date_of(ms: i64) -> String {
    let z = ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn relative(main: &Path, given: &str) -> Result<String, String> {
    let given = given.trim();
    let path = Path::new(given);
    let rel = if path.is_absolute() {
        path.strip_prefix(main).map_err(|_| format!("{given} is outside the repo {}", main.display()))?
    } else {
        path
    };
    let mut parts: Vec<String> = Vec::new();
    for c in rel.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_string_lossy().to_string()),
            Component::CurDir => {}
            _ => return Err(format!("{given} is outside the repo {}", main.display())),
        }
    }
    if parts.is_empty() {
        return Err(format!("{given} names the whole repo; give the paths to reset"));
    }
    Ok(parts.join("/"))
}

fn strings(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

pub fn act(
    store: &Store,
    runner: Option<&Runner>,
    project: &Project,
    args: &Value,
    now: i64,
) -> Result<Value, String> {
    if project.supervisor {
        return Err("main_checkout needs `project`, a project with a git checkout".into());
    }
    let main = Path::new(&project.path);
    if git::main_worktree(main).is_err() {
        return Err(format!("{} is not a git checkout", project.path));
    }
    if let Some(op) = git::operation_in_progress(main) {
        return Err(format!("{} has {op} in progress; finish or abort it first, main_checkout does nothing meanwhile", project.name));
    }
    let action = text(args, "action").unwrap_or_default().to_lowercase();
    let issue = args
        .get("issue")
        .and_then(Value::as_i64)
        .or_else(|| {
            store
                .workers()
                .unwrap_or_default()
                .into_iter()
                .find(|w| w.project == project.id && w.state == "ready")
                .map(|w| w.issue)
        });
    let (done, out) = match action.as_str() {
        "commit" => commit(project, args)?,
        "stash" => stash(main, issue, now)?,
        "reset_paths" => reset_paths(main, args)?,
        "ask_session" => ask_session(store, runner, project, args)?,
        _ => return Err("`action` is one of commit, stash, reset_paths, ask_session".into()),
    };
    if let Some(done) = done {
        let line = format!("main_checkout {action} on {}: {done}", project.name);
        let _ = store.log_autopilot(Some(project.id), issue, &line);
        if let Some(i) = issue {
            let _ = store.add_comment(
                i,
                &NewComment { author: Some("supervisor".into()), body: format!("autopilot: {line}") },
            );
        }
    }
    Ok(out)
}

fn commit(project: &Project, args: &Value) -> Result<(Option<String>, Value), String> {
    let main = Path::new(&project.path);
    let message = text(args, "message").ok_or("commit needs `message`, the commit message; nothing is committed without one")?;
    let branch = git::current_branch(main).unwrap_or_default();
    let allowed = match &project.branch {
        Some(b) => *b == branch,
        None => matches!(branch.as_str(), "main" | "master"),
    };
    if !allowed {
        return Err(format!("the main checkout is on {branch:?}, not the project's main branch; commit only happens on the main branch"));
    }
    if !git::is_dirty(main)? {
        return Err("the main checkout has nothing to commit".into());
    }
    let paths = strings(args, "paths").iter().map(|p| relative(main, p)).collect::<Result<Vec<_>, _>>()?;
    let head = git::commit_paths(main, message, &paths)?;
    Ok((Some(format!("committed {} as {head}", if paths.is_empty() { "everything".to_string() } else { paths.join(", ") })), json!({ "committed": head, "branch": branch })))
}

fn stash(main: &Path, issue: Option<i64>, now: i64) -> Result<(Option<String>, Value), String> {
    if !git::is_dirty(main)? {
        return Err("the main checkout is clean; nothing to stash".into());
    }
    let what = issue.map(|i| format!("merging #{i}")).unwrap_or_else(|| "merging".into());
    let message = format!("autopilot: dirty main before {what}, {}", date_of(now));
    git::stash_all(main, &message)?;
    Ok((Some(format!("stashed everything as '{message}'")), json!({ "stashed": message, "stashes": git::stash_count(main) })))
}

fn reset_paths(main: &Path, args: &Value) -> Result<(Option<String>, Value), String> {
    let given = strings(args, "paths");
    if given.is_empty() {
        return Err("reset_paths needs `paths`, the files or folders to reset".into());
    }
    let wanted = given.iter().map(|p| relative(main, p)).collect::<Result<Vec<_>, _>>()?;
    let hit = |file: &str| wanted.iter().any(|w| file == w || file.starts_with(&format!("{w}/")));
    let entries: Vec<(String, String)> = git::status_entries(main).into_iter().filter(|(_, f)| hit(f)).collect();
    if entries.is_empty() {
        return Err("none of those paths has uncommitted changes".into());
    }
    let untracked: Vec<String> = entries.iter().filter(|(s, _)| s == "??").map(|(_, f)| f.clone()).collect();
    let added: Vec<String> = entries.iter().filter(|(s, _)| s.starts_with('A')).map(|(_, f)| f.clone()).collect();
    let restored: Vec<String> =
        entries.iter().filter(|(s, _)| s != "??" && !s.starts_with('A')).map(|(_, f)| f.clone()).collect();
    let tracked: Vec<String> = restored.iter().chain(added.iter()).cloned().collect();
    let listing = json!({
        "untracked": untracked,
        "git_clean_dry_run": git::clean_dry_run(main, &untracked),
        "tracked_modified": tracked,
    });
    if flag(args, "dry_run") {
        return Ok((None, json!({ "dry_run": true, "would_reset": listing })));
    }
    if !tracked.is_empty() && !flag(args, "force") {
        return Err(format!(
            "{} is tracked and modified, so resetting it loses work; pass force true to reset it anyway (dry_run true lists what would go)",
            tracked.join(", ")
        ));
    }
    if !restored.is_empty() {
        git::restore_tracked(main, &restored)?;
    }
    if !added.is_empty() {
        git::remove_added(main, &added)?;
    }
    if !untracked.is_empty() {
        git::remove_untracked(main, &untracked)?;
    }
    Ok((Some(format!("reset {}", entries.iter().map(|(_, f)| f.as_str()).collect::<Vec<_>>().join(", "))), json!({ "reset": listing })))
}

fn ask_session(store: &Store, runner: Option<&Runner>, project: &Project, args: &Value) -> Result<(Option<String>, Value), String> {
    let runner = runner.ok_or("the runner is not up")?;
    let id = text(args, "id").ok_or("ask_session needs `id`, the session working in the main checkout")?;
    let session = store.session(id.parse().map_err(|_| format!("{id} is not a session id"))?).map_err(|e| e.0)?;
    let main_tree = store.main_worktree(project.id).map_err(|e| e.0)?;
    if session.project_id != project.id || session.worktree_id != main_tree.id {
        return Err("that session does not work in the main checkout of this project".into());
    }
    let request = text(args, "request").unwrap_or("commit").to_lowercase();
    if !matches!(request.as_str(), "commit" | "stash") {
        return Err("`request` is commit or stash".into());
    }
    let main = Path::new(&project.path);
    let files = git::dirty_files(main, STATUS_LINES);
    if files.is_empty() {
        return Err("the main checkout is clean; there is nothing to ask for".into());
    }
    let ask = match request.as_str() {
        "commit" => "commit them to the main branch with a proper message now (only your own edits), then carry on",
        _ => "stash them with `git stash push -u -m 'main stashed for a merge'`, then carry on",
    };
    let body = format!(
        "The autopilot cannot merge a finished branch because the main checkout you work in has uncommitted changes:\n{}\nPlease {ask}.",
        files.join("\n")
    );
    runner.tell(&session, &body, Origin::new("main_checkout", None)).map_err(|e| e.0)?;
    Ok((Some(format!("asked session {} to {request}", session.id)), json!({ "asked": session.id, "request": request, "files": files })))
}
