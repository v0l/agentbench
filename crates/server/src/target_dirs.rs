use crate::store::{Store, TargetDir};
use agentbench_core::{Id, Issue, IssueSource, IssueState, Project, SessionId, Worktree};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

pub const ROOT_SETTING: &str = "cargo_target_root";
#[cfg(not(test))]
pub const DEFAULT_ROOT: &str = "/mnt/data/cargo-target";
#[cfg(test)]
pub const DEFAULT_ROOT: &str = "/nonexistent/agentbench-test-cargo-target";
pub const STALE_MS: i64 = 6 * 3_600_000;
pub const LOW_FREE_PERCENT: u64 = 10;

static NAMED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"CARGO_TARGET_DIR=["']?([^\s"'`]+)"#).unwrap());
static ISSUE_BRANCH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^issue-(\d+)(?:-|$)").unwrap());

pub struct Ctx {
    pub root: PathBuf,
    pub proc: PathBuf,
    pub now: i64,
}

impl Ctx {
    pub fn system(store: &Store) -> Ctx {
        Ctx { root: root(store), proc: PathBuf::from("/proc"), now: crate::store::now() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Freed {
    pub project_id: Id,
    pub issue: i64,
    pub path: String,
    pub bytes: u64,
}

pub fn root(store: &Store) -> PathBuf {
    store
        .setting(ROOT_SETTING)
        .ok()
        .flatten()
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT))
}

pub fn issue_dir(root: &Path, project: &str, issue: i64) -> PathBuf {
    root.join(format!("{project}-{issue}"))
}

pub fn owned_issue(root: &Path, project: &str, path: &Path) -> Option<i64> {
    if path.parent()? != root {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    let rest = name.strip_prefix(project)?.strip_prefix('-')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let tail = &rest[digits.len()..];
    if !tail.is_empty() && !tail.starts_with('-') {
        return None;
    }
    digits.parse().ok()
}

fn numbers_of(issue: &Issue) -> Vec<i64> {
    let mut out = vec![issue.id];
    if issue.source == IssueSource::Github
        && let Some(n) = issue.external_number
    {
        out.push(n);
    }
    out
}

pub fn named_in(text: &str, issue: &Issue) -> Vec<PathBuf> {
    NAMED
        .captures_iter(text)
        .map(|c| c[1].replace("<issue>", &issue.id.to_string()))
        .map(|p| PathBuf::from(p.trim_end_matches(['.', ',', ';', ':', ')'])))
        .collect()
}

pub fn record(store: &Store, project: &Project, issue: &Issue, tree: &Worktree, session: SessionId, brief: Option<&str>) {
    let root = root(store);
    let mut candidates = vec![issue_dir(&root, &project.name, issue.id)];
    candidates.extend(brief.map(|b| named_in(b, issue)).unwrap_or_default());
    let numbers = numbers_of(issue);
    for path in candidates {
        if owned_issue(&root, &project.name, &path).is_some_and(|n| numbers.contains(&n)) {
            let _ = store.add_target_dir(&TargetDir {
                path: path.to_string_lossy().into_owned(),
                project_id: project.id,
                issue_id: issue.id,
                worktree_id: Some(tree.id),
                session: Some(session),
            });
        }
    }
}

fn resolve(store: &Store, project: Id, number: i64) -> Option<Issue> {
    let issues = store.issues().unwrap_or_default();
    issues
        .iter()
        .find(|i| i.project_id == project && i.id == number)
        .or_else(|| {
            issues.iter().find(|i| i.project_id == project && i.source == IssueSource::Github && i.external_number == Some(number))
        })
        .cloned()
}

fn under(value: &str, path: &str) -> bool {
    value == path || value.strip_prefix(path).is_some_and(|r| r.starts_with('/'))
}

fn link_under(link: &Path, path: &str) -> bool {
    std::fs::read_link(link).is_ok_and(|t| under(&t.to_string_lossy(), path))
}

pub fn used_by(proc: &Path, path: &Path) -> Option<u32> {
    let path = path.to_string_lossy().trim_end_matches('/').to_string();
    for entry in std::fs::read_dir(proc).ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let dir = entry.path();
        if let Ok(env) = std::fs::read(dir.join("environ")) {
            let named = env
                .split(|b| *b == 0)
                .filter_map(|kv| std::str::from_utf8(kv).ok())
                .filter_map(|kv| kv.split_once('=').map(|(_, v)| v))
                .any(|v| v.split(':').any(|part| under(part.trim_end_matches('/'), &path)));
            if named {
                return Some(pid);
            }
        }
        if link_under(&dir.join("cwd"), &path) {
            return Some(pid);
        }
        if let Ok(fds) = std::fs::read_dir(dir.join("fd"))
            && fds.flatten().any(|fd| link_under(&fd.path(), &path))
        {
            return Some(pid);
        }
    }
    None
}

fn modified(path: &Path) -> i64 {
    std::fs::symlink_metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn last_touched(path: &Path) -> i64 {
    let mut latest = modified(path);
    for child in std::fs::read_dir(path).into_iter().flatten().flatten() {
        let p = child.path();
        latest = latest.max(modified(&p));
        if p.is_dir() && !p.is_symlink() {
            for grand in std::fs::read_dir(&p).into_iter().flatten().flatten() {
                latest = latest.max(modified(&grand.path()));
            }
        }
    }
    latest
}

pub fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    let own = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            meta.blocks() * 512
        }
        #[cfg(not(unix))]
        {
            meta.len()
        }
    };
    if !meta.is_dir() {
        return own;
    }
    own + std::fs::read_dir(path).into_iter().flatten().flatten().map(|e| size(&e.path())).sum::<u64>()
}

fn delete(store: &Store, project: Id, issue: i64, path: &Path) -> Option<Freed> {
    let bytes = size(path);
    std::fs::remove_dir_all(path).ok()?;
    let text = path.to_string_lossy().into_owned();
    let _ = store.forget_target_dir(&text);
    Some(Freed { project_id: project, issue, path: text, bytes })
}

pub fn release(store: &Store, tree: &Worktree, ctx: &Ctx) -> Vec<Freed> {
    if tree.main {
        return Vec::new();
    }
    let Ok(project) = store.project(tree.project_id) else { return Vec::new() };
    let mut candidates: Vec<(PathBuf, Option<Id>)> = store
        .target_dirs()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.worktree_id == Some(tree.id))
        .map(|t| (PathBuf::from(t.path), Some(t.issue_id)))
        .collect();
    if let Some(n) = tree.branch.as_deref().and_then(|b| ISSUE_BRANCH.captures(b)).and_then(|c| c[1].parse::<i64>().ok()) {
        let path = issue_dir(&ctx.root, &project.name, n);
        if !candidates.iter().any(|(p, _)| *p == path) {
            candidates.push((path, resolve(store, project.id, n).map(|i| i.id)));
        }
    }
    let sessions = store.sessions().unwrap_or_default();
    let mut out = Vec::new();
    for (path, issue) in candidates {
        let Some(number) = owned_issue(&ctx.root, &project.name, &path) else { continue };
        if !path.is_dir() {
            let _ = store.forget_target_dir(&path.to_string_lossy());
            continue;
        }
        let elsewhere = issue.is_some_and(|i| sessions.iter().any(|s| s.issue_id == Some(i) && s.worktree_id != tree.id));
        if elsewhere || used_by(&ctx.proc, &path).is_some() {
            continue;
        }
        if let Some(f) = delete(store, project.id, number, &path) {
            let _ = store.log_autopilot(
                Some(project.id),
                issue,
                &format!("deleted build dir {} with worktree {}, freed {}", f.path, tree.label(), crate::pressure::gb_text(f.bytes)),
            );
            out.push(f);
        }
    }
    out
}

pub fn sweep(store: &Store, ctx: &Ctx) -> Vec<Freed> {
    let Ok(entries) = std::fs::read_dir(&ctx.root) else { return Vec::new() };
    let entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.is_symlink()).collect();
    let projects: Vec<Project> = store.projects().unwrap_or_default().into_iter().filter(|p| !p.builtin()).collect();
    let sessions = store.sessions().unwrap_or_default();
    let mut out = Vec::new();
    for path in &entries {
        let Some((project, number)) = projects
            .iter()
            .filter_map(|p| owned_issue(&ctx.root, &p.name, path).map(|n| (p, n)))
            .max_by_key(|(p, _)| p.name.len())
        else {
            continue;
        };
        let Some(issue) = resolve(store, project.id, number) else { continue };
        if issue.state != IssueState::Done || sessions.iter().any(|s| s.issue_id == Some(issue.id)) {
            continue;
        }
        if ctx.now - last_touched(path) < STALE_MS || used_by(&ctx.proc, path).is_some() {
            continue;
        }
        if let Some(f) = delete(store, project.id, number, path) {
            out.push(f);
        }
    }
    for t in store.target_dirs().unwrap_or_default() {
        if !Path::new(&t.path).exists() {
            let _ = store.forget_target_dir(&t.path);
        }
    }
    out
}

pub fn free_space(path: &Path) -> Option<(u64, u64)> {
    let path = path.canonicalize().ok()?;
    let disks = sysinfo::Disks::new_with_refreshed_list().ok()?;
    disks
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| (d.available_space(), d.total_space()))
}

pub fn is_low(free: u64, total: u64) -> bool {
    total > 0 && free.saturating_mul(100) < total.saturating_mul(LOW_FREE_PERCENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentbench_core::{IssuePatch, NewIssue};

    struct Scratch {
        _dir: tempfile::TempDir,
        root: PathBuf,
        proc: PathBuf,
        repo: PathBuf,
        store: Store,
        project: Project,
    }

    fn scratch() -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("cargo-target");
        let proc = dir.path().join("proc");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&proc).unwrap();
        let repo = dir.path().join("demo");
        crate::git::testing::repo(&repo);
        let store = Store::memory().unwrap();
        let project = crate::projects::add(&store, &repo, None, false).unwrap();
        store.set_setting(ROOT_SETTING, &root.to_string_lossy()).unwrap();
        Scratch { _dir: dir, root, proc, repo, store, project }
    }

    fn build_dir(root: &Path, name: &str) -> PathBuf {
        let p = root.join(name);
        std::fs::create_dir_all(p.join("debug/deps")).unwrap();
        std::fs::write(p.join("debug/deps/libdemo.rlib"), vec![7u8; 64 * 1024]).unwrap();
        p
    }

    fn issue(s: &Scratch, title: &str, state: IssueState) -> Issue {
        let i = s.store.add_issue(&NewIssue { project_id: s.project.id, title: title.into(), ..Default::default() }).unwrap();
        s.store.update_issue(i.id, &IssuePatch { state: Some(state), ..Default::default() }).unwrap()
    }

    fn ctx(s: &Scratch, later: i64) -> Ctx {
        Ctx { root: s.root.clone(), proc: s.proc.clone(), now: crate::store::now() + later }
    }

    fn fake_process(s: &Scratch, pid: u32, env: &str) {
        let d = s.proc.join(pid.to_string());
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("environ"), env.replace(' ', "\0")).unwrap();
    }

    #[test]
    fn the_naming_rule_owns_issue_dirs_and_never_the_shared_project_dir() {
        let root = Path::new("/t");
        assert_eq!(owned_issue(root, "demo", Path::new("/t/demo-12")), Some(12));
        assert_eq!(owned_issue(root, "demo", Path::new("/t/demo-12-master")), Some(12));
        assert_eq!(owned_issue(root, "demo", Path::new("/t/demo")), None);
        assert_eq!(owned_issue(root, "demo", Path::new("/t/demo-install")), None);
        assert_eq!(owned_issue(root, "demo", Path::new("/t/demo-12m")), None);
        assert_eq!(owned_issue(root, "demo", Path::new("/t/demo-core-12")), None);
        assert_eq!(owned_issue(root, "demo", Path::new("/elsewhere/demo-12")), None);
    }

    #[test]
    fn a_brief_names_the_target_dir_with_the_issue_filled_in() {
        let s = scratch();
        let i = issue(&s, "x", IssueState::Doing);
        let brief = format!(
            "build with `export CARGO_TARGET_DIR={}/demo-<issue>`. Shared is CARGO_TARGET_DIR={}/demo.",
            s.root.display(),
            s.root.display()
        );
        let named = named_in(&brief, &i);
        assert_eq!(named, vec![s.root.join(format!("demo-{}", i.id)), s.root.join("demo")]);
    }

    #[test]
    fn a_merged_issue_dir_goes_with_its_worktree_and_the_shared_dir_stays() {
        let s = scratch();
        let i = issue(&s, "merged", IssueState::Done);
        let tree = crate::projects::add_worktree(&s.store, s.project.id, &format!("issue-{}-merged", i.id), None).unwrap();
        let session = s.store.add_session(tree.id, "echo", "w", None, Some(i.id)).unwrap();
        let brief = format!("export CARGO_TARGET_DIR={}/demo-{}-extra", s.root.display(), i.id);
        record(&s.store, &s.project, &i, &tree, session.id, Some(&brief));
        let mine = build_dir(&s.root, &format!("demo-{}", i.id));
        let extra = build_dir(&s.root, &format!("demo-{}-extra", i.id));
        let shared = build_dir(&s.root, "demo");
        let freed = release(&s.store, &tree, &ctx(&s, 0));
        assert!(!mine.exists() && !extra.exists(), "{freed:?}");
        assert!(shared.exists(), "the shared project dir is never touched");
        assert_eq!(freed.len(), 2);
        assert!(freed.iter().all(|f| f.bytes >= 64 * 1024), "{freed:?}");
        assert!(s.store.target_dirs().unwrap().is_empty());
        let feed: Vec<String> = s.store.autopilot_log(10).unwrap().into_iter().map(|a| a.text).collect();
        assert!(feed.iter().any(|l| l.contains("deleted build dir") && l.contains("freed")), "{feed:?}");
    }

    #[test]
    fn removing_the_worktree_deletes_its_build_dir() {
        let s = scratch();
        let i = issue(&s, "gone", IssueState::Done);
        let tree = crate::projects::add_worktree(&s.store, s.project.id, &format!("issue-{}-gone", i.id), None).unwrap();
        let mine = build_dir(&s.root, &format!("demo-{}", i.id));
        crate::projects::remove_worktree(&s.store, tree.id, false).unwrap();
        assert!(!mine.exists());
        assert!(s.repo.exists());
    }

    #[test]
    fn a_dir_named_in_a_live_process_environment_is_kept() {
        let s = scratch();
        let i = issue(&s, "busy", IssueState::Done);
        let tree = crate::projects::add_worktree(&s.store, s.project.id, &format!("issue-{}-busy", i.id), None).unwrap();
        let mine = build_dir(&s.root, &format!("demo-{}", i.id));
        fake_process(&s, 4242, &format!("HOME=/h CARGO_TARGET_DIR={}", mine.display()));
        assert!(release(&s.store, &tree, &ctx(&s, 0)).is_empty());
        assert!(sweep(&s.store, &ctx(&s, STALE_MS * 2)).is_empty());
        assert!(mine.exists());
    }

    #[test]
    fn a_real_child_process_with_the_dir_in_its_environment_keeps_it() {
        let s = scratch();
        let mine = build_dir(&s.root, "demo-77");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env("CARGO_TARGET_DIR", &mine)
            .spawn()
            .unwrap();
        let used = used_by(Path::new("/proc"), &mine);
        let other = used_by(Path::new("/proc"), &s.root.join("demo-7"));
        child.kill().unwrap();
        let _ = child.wait();
        assert_eq!(used, Some(child.id()));
        assert_eq!(other, None, "a sibling with a shared prefix is not in use");
    }

    #[test]
    fn the_sweep_deletes_done_stale_dirs_and_keeps_a_running_worker_and_the_shared_dir() {
        let s = scratch();
        let done = issue(&s, "done", IssueState::Done);
        let running = issue(&s, "running", IssueState::Doing);
        let finished_but_open = issue(&s, "open session", IssueState::Done);
        let fresh = issue(&s, "fresh", IssueState::Done);
        let main = s.store.main_worktree(s.project.id).unwrap();
        s.store.add_session(main.id, "echo", "w", None, Some(running.id)).unwrap();
        s.store.add_session(main.id, "echo", "w", None, Some(finished_but_open.id)).unwrap();
        let done_dir = build_dir(&s.root, &format!("demo-{}", done.id));
        let running_dir = build_dir(&s.root, &format!("demo-{}", running.id));
        let open_dir = build_dir(&s.root, &format!("demo-{}", finished_but_open.id));
        let shared = build_dir(&s.root, "demo");
        let unknown = build_dir(&s.root, "demo-99999");

        let soon = sweep(&s.store, &ctx(&s, STALE_MS / 2));
        assert!(soon.is_empty(), "nothing is untouched for six hours yet: {soon:?}");
        let fresh_dir = build_dir(&s.root, &format!("demo-{}", fresh.id));

        let freed = sweep(&s.store, &ctx(&s, STALE_MS + 60_000));
        assert_eq!(freed.iter().map(|f| f.issue).collect::<Vec<_>>(), vec![done.id, fresh.id]);
        assert!(!done_dir.exists() && !fresh_dir.exists());
        assert!(running_dir.exists(), "a running worker keeps its dir");
        assert!(open_dir.exists(), "a session still on the issue keeps its dir");
        assert!(shared.exists(), "the shared project dir is never touched");
        assert!(unknown.exists(), "an issue the tracker does not know is left alone");
    }

    #[test]
    fn low_disk_is_under_ten_percent_free() {
        assert!(is_low(9, 100));
        assert!(!is_low(10, 100));
        assert!(!is_low(0, 0));
    }
}
