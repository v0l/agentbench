use crate::agent::Api;
use crate::runner::Runner;
use crate::store::Store;
use agentbench_core::{
    Entry, Id, IssuePatch, IssueState, NewComment, NewIssue, NewSession, Place, SessionId,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Most sessions of a project that may be busy at once before an agent is refused another.
const BUSY_MAX: usize = 100;

/// The runner the tools start and message sessions through. The runner owns the tools, so it
/// hands itself over once it is built.
#[derive(Clone, Default)]
pub struct Handle(Arc<Mutex<Option<Runner>>>);

impl Handle {
    pub fn set(&self, runner: Runner) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(runner);
    }

    fn get(&self) -> Result<Runner, String> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "sessions cannot be started from here".to_string())
    }
}

pub struct StoreApi {
    store: Store,
    changed: Option<std::sync::Arc<tokio::sync::Notify>>,
    runner: Handle,
    /// Sessions the user allowed to start one session in a project, the once.
    once: Mutex<std::collections::HashSet<(SessionId, Id)>>,
    /// (parent, child): sessions a session started, which it may follow in any project.
    children: Mutex<std::collections::HashSet<(SessionId, SessionId)>>,
}

/// Setting: ids of the projects any session may start sessions in.
const ALLOWED_PROJECTS: &str = "start_session_projects";
/// Setting: "1" when any session may start sessions in any project.
const ALLOWED_ANY: &str = "start_session_any_project";

impl StoreApi {
    #[cfg(test)]
    pub fn new(store: Store) -> StoreApi {
        StoreApi {
            store,
            changed: None,
            runner: Handle::default(),
            once: Default::default(),
            children: Default::default(),
        }
    }

    pub fn notifying(
        store: Store,
        changed: std::sync::Arc<tokio::sync::Notify>,
        runner: Handle,
    ) -> StoreApi {
        StoreApi {
            store,
            changed: Some(changed),
            runner,
            once: Default::default(),
            children: Default::default(),
        }
    }

    fn touched(&self) {
        if let Some(c) = &self.changed {
            c.notify_waiters();
        }
    }
}

fn id(args: &Value) -> Result<Id, String> {
    args.get("id").and_then(Value::as_i64).ok_or_else(|| "missing `id`".to_string())
}

fn session_id(args: &Value) -> Result<SessionId, String> {
    let raw = args.get("id").and_then(Value::as_str).ok_or("missing `id`")?;
    raw.parse().map_err(|_| format!("{raw} is not a session id; list_sessions gives them"))
}

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn labels(args: &Value) -> Result<Option<Vec<String>>, String> {
    match args.get("labels") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => Ok(Some(
            a.iter()
                .filter_map(Value::as_str)
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect(),
        )),
        Some(_) => Err("`labels` is a list of strings".into()),
    }
}

fn issue_json(i: &agentbench_core::Issue) -> Value {
    json!({
        "id": i.id, "title": i.title, "state": i.state, "body": i.body,
        "labels": i.labels, "priority": i.priority,
    })
}

fn state(args: &Value) -> Result<Option<IssueState>, String> {
    match text(args, "state") {
        None => Ok(None),
        Some(s) => IssueState::parse(s.trim())
            .map(Some)
            .ok_or_else(|| format!("{s} is not a state: backlog, todo, doing, review or done")),
    }
}

impl StoreApi {
    /// Whether `me` may look at and message `other`: same project, or one it started.
    fn reaches(
        &self,
        me: &agentbench_core::Session,
        other: &agentbench_core::Session,
    ) -> Result<(), String> {
        let child =
            self.children.lock().unwrap_or_else(|e| e.into_inner()).contains(&(me.id, other.id));
        if other.project_id == me.project_id || child {
            Ok(())
        } else {
            Err("that session belongs to another project".into())
        }
    }

    fn allowed_projects(&self) -> Vec<Id> {
        self.store
            .setting(ALLOWED_PROJECTS)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Whether the user has said sessions may start in `target` without asking again.
    fn standing_permission(&self, target: Id) -> bool {
        self.store.setting(ALLOWED_ANY).ok().flatten().as_deref() == Some("1")
            || self.allowed_projects().contains(&target)
    }

    /// The project a tool call names by id or by name.
    fn find_project(&self, v: &Value) -> Result<agentbench_core::Project, String> {
        let projects = self.store.projects().map_err(|e| e.0)?;
        let by_name =
            |n: &str| projects.iter().find(|p| p.name.eq_ignore_ascii_case(n.trim())).cloned();
        let found = match v {
            Value::Number(n) => projects.iter().find(|p| Some(p.id) == n.as_i64()).cloned(),
            Value::String(s) => s
                .trim()
                .parse::<Id>()
                .ok()
                .and_then(|i| projects.iter().find(|p| p.id == i).cloned())
                .or_else(|| by_name(s)),
            _ => None,
        };
        found.ok_or_else(|| format!("no project {v}; list_projects gives them"))
    }
}

impl Api for StoreApi {
    fn call(&self, tool: &str, args: &Value, session: SessionId) -> Result<Value, String> {
        let s = &self.store;
        let me = s.session(session).map_err(|e| e.0)?;
        let project = me.project_id;
        let own = |issue: Id| -> Result<agentbench_core::Issue, String> {
            let i = s.issue(issue).map_err(|e| e.0)?;
            if i.project_id != project {
                return Err(format!("issue #{issue} belongs to another project"));
            }
            Ok(i)
        };
        let out = match tool {
            "list_issues" => {
                let filter = state(args)?;
                let issues: Vec<Value> = s
                    .issues()
                    .map_err(|e| e.0)?
                    .into_iter()
                    .filter(|i| i.project_id == project)
                    .filter(|i| filter.is_none_or(|f| i.state == f))
                    .map(|i| issue_json(&i))
                    .collect();
                json!(issues)
            }
            "file_issue" => {
                let title = text(args, "title").ok_or("missing `title`")?;
                let i = s
                    .add_issue(&NewIssue {
                        project_id: project,
                        title,
                        body: text(args, "body").unwrap_or_default(),
                        state: state(args)?,
                        priority: args.get("priority").and_then(Value::as_i64),
                        labels: labels(args)?.unwrap_or_default(),
                    })
                    .map_err(|e| e.0)?;
                json!({ "filed": i.id, "title": i.title })
            }
            "update_issue" => {
                let issue = id(args)?;
                own(issue)?;
                let patch = IssuePatch {
                    title: text(args, "title"),
                    body: text(args, "body"),
                    state: state(args)?,
                    priority: args.get("priority").and_then(Value::as_i64),
                    labels: labels(args)?,
                };
                let i = s.update_issue(issue, &patch).map_err(|e| e.0)?;
                json!({ "id": i.id, "title": i.title, "state": i.state, "labels": i.labels })
            }
            "get_issue" => {
                let issue = id(args)?;
                let i = own(issue)?;
                let comments: Vec<Value> = s
                    .comments(issue)
                    .map_err(|e| e.0)?
                    .into_iter()
                    .map(|c| json!({ "author": c.author, "body": c.body, "at": c.created_at }))
                    .collect();
                let mut out = issue_json(&i);
                out["comments"] = json!(comments);
                out
            }
            "comment_on_issue" => {
                let issue = id(args)?;
                own(issue)?;
                let body = text(args, "body").ok_or("missing `body`")?;
                let author = format!("{} (session {session})", me.agent);
                s.add_comment(issue, &NewComment { author: Some(author), body })
                    .map_err(|e| e.0)?;
                json!({ "commented": issue })
            }
            "list_sessions" => {
                let trees = s.worktrees(Some(project)).map_err(|e| e.0)?;
                let sessions: Vec<Value> = s
                    .sessions()
                    .map_err(|e| e.0)?
                    .into_iter()
                    .filter(|x| x.project_id == project)
                    .map(|x| {
                        let branch =
                            trees.iter().find(|w| w.id == x.worktree_id).map(|w| w.label());
                        json!({
                            "id": x.id,
                            "title": x.title,
                            "agent": x.agent,
                            "status": x.status,
                            "worktree": branch,
                            "issue": x.issue_id,
                            "you": x.id == session,
                        })
                    })
                    .collect();
                json!(sessions)
            }
            "read_session" => {
                let other = s.session(session_id(args)?).map_err(|e| e.0)?;
                self.reaches(&me, &other)?;
                let last =
                    args.get("last").and_then(Value::as_u64).unwrap_or(20).clamp(1, 200) as usize;
                let msgs = s.messages(other.id, 0).map_err(|e| e.0)?;
                let start = msgs.len().saturating_sub(last);
                let lines: Vec<Value> = msgs[start..]
                    .iter()
                    .map(|m| match &m.entry {
                        Entry::Tool { name, input, error, .. } => {
                            json!({ "tool": name, "input": input, "error": error })
                        }
                        e => json!({ "kind": kind(e), "text": clip(e.text(), 2000) }),
                    })
                    .collect();
                json!({ "session": other.id, "title": other.title, "status": other.status, "messages": lines })
            }
            "session_status" => {
                let other = s.session(session_id(args)?).map_err(|e| e.0)?;
                self.reaches(&me, &other)?;
                let last = s
                    .messages(other.id, 0)
                    .map_err(|e| e.0)?
                    .into_iter()
                    .rev()
                    .find_map(|m| match m.entry {
                        Entry::Assistant { text } => Some(text),
                        _ => None,
                    })
                    .unwrap_or_default();
                json!({
                    "session": other.id,
                    "title": other.title,
                    "status": other.status,
                    "busy": other.status.busy(),
                    "last_reply": clip(&last, 4000),
                })
            }
            "list_projects" => {
                let all: Vec<Value> = s
                    .projects()
                    .map_err(|e| e.0)?
                    .into_iter()
                    .map(|p| json!({ "id": p.id, "name": p.name, "path": p.path, "you": p.id == project }))
                    .collect();
                json!(all)
            }
            // Not offered to the model: the agent asks the user, then records the answer here.
            "check_project" => {
                let p = self.find_project(args.get("project").unwrap_or(&Value::Null))?;
                json!({
                    "id": p.id,
                    "name": p.name,
                    "path": p.path,
                    "allowed": p.id == project || self.standing_permission(p.id),
                })
            }
            "grant_project" => {
                let p = self.find_project(args.get("project").unwrap_or(&Value::Null))?;
                match text(args, "scope").as_deref() {
                    Some("once") => {
                        self.once.lock().unwrap_or_else(|e| e.into_inner()).insert((session, p.id));
                    }
                    Some("project") => {
                        let mut ids = self.allowed_projects();
                        if !ids.contains(&p.id) {
                            ids.push(p.id);
                        }
                        let raw = serde_json::to_string(&ids).unwrap_or_default();
                        s.set_setting(ALLOWED_PROJECTS, &raw).map_err(|e| e.0)?;
                    }
                    Some("all") => s.set_setting(ALLOWED_ANY, "1").map_err(|e| e.0)?,
                    other => return Err(format!("{other:?} is not a scope")),
                }
                json!({ "granted": p.id })
            }
            "start_session" => {
                let runner = self.runner.get()?;
                let (project, other_project) = match args.get("project") {
                    None | Some(Value::Null) => (project, None),
                    Some(v) => {
                        let target = self.find_project(v)?;
                        if target.id == project {
                            (project, None)
                        } else {
                            let once = self
                                .once
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .remove(&(session, target.id));
                            if !once && !self.standing_permission(target.id) {
                                return Err(format!(
                                    "starting a session in {} needs the user's permission, which has not been given",
                                    target.name
                                ));
                            }
                            (target.id, Some(target.name))
                        }
                    }
                };
                let own = |issue: Id| -> Result<agentbench_core::Issue, String> {
                    let i = s.issue(issue).map_err(|e| e.0)?;
                    if i.project_id != project {
                        return Err(format!("issue #{issue} belongs to another project"));
                    }
                    Ok(i)
                };
                let busy = s
                    .sessions()
                    .map_err(|e| e.0)?
                    .iter()
                    .filter(|x| x.project_id == project && x.status.busy())
                    .count();
                if busy >= BUSY_MAX {
                    return Err(format!(
                        "{busy} sessions are already busy in this project; wait for some to finish"
                    ));
                }
                let issue = args.get("issue").and_then(Value::as_i64);
                if let Some(i) = issue {
                    own(i)?;
                }
                let place = if let Some(b) = text(args, "branch").filter(|b| !b.trim().is_empty()) {
                    Place::New { branch: b }
                } else if let Some(w) = args.get("worktree").and_then(Value::as_i64) {
                    Place::Existing { worktree_id: w }
                } else if issue.is_some() {
                    Place::New { branch: String::new() }
                } else {
                    Place::Main
                };
                let agent = text(args, "agent").unwrap_or_else(|| me.agent.clone());
                let prompt = text(args, "prompt").filter(|p| !p.trim().is_empty());
                if prompt.is_none() && issue.is_none() {
                    return Err("give a `prompt`, or an `issue` to work from".into());
                }
                let new = NewSession {
                    project_id: project,
                    agent,
                    place,
                    title: text(args, "title"),
                    model: text(args, "model"),
                    issue_id: issue,
                    prompt,
                    attachments: Vec::new(),
                };
                let started = runner.start_session(new).map_err(|e| e.0)?;
                if let Some(i) = issue
                    && own(i)?.state != IssueState::Doing
                {
                    s.update_issue(
                        i,
                        &IssuePatch { state: Some(IssueState::Doing), ..Default::default() },
                    )
                    .map_err(|e| e.0)?;
                }
                self.children
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert((session, started.id));
                json!({
                "session": started.id,
                "title": started.title,
                "agent": started.agent,
                "worktree": s.worktree(started.worktree_id).ok().map(|w| w.label()),
                "cwd": started.cwd,
                "project": other_project,
                                })
            }
            "send_message" => {
                let runner = self.runner.get()?;
                let other = s.session(session_id(args)?).map_err(|e| e.0)?;
                self.reaches(&me, &other)?;
                if other.id == session {
                    return Err("that is this session; the message would go nowhere".into());
                }
                let body = text(args, "text").filter(|t| !t.trim().is_empty());
                let body = body.ok_or("missing `text`")?;
                if !std::path::Path::new(&other.cwd).is_dir() {
                    return Err(format!("{} no longer exists", other.cwd));
                }
                // The prefix also keeps a message that starts with a slash from running as a command.
                let line =
                    format!("Message from session {session} ({}): {}", me.agent, body.trim());
                runner.send(&other, &line).map_err(|e| e.0)?;
                json!({ "sent": other.id, "status": other.status })
            }
            "list_worktrees" => {
                let trees: Vec<Value> = s
                    .worktrees(Some(project))
                    .map_err(|e| e.0)?
                    .into_iter()
                    .map(|w| {
                        json!({
                            "id": w.id, "branch": w.label(), "path": w.path,
                            "main": w.main, "missing": w.missing,
                        })
                    })
                    .collect();
                json!(trees)
            }
            "create_worktree" => {
                let branch = text(args, "branch").ok_or("missing `branch`")?;
                let w = crate::projects::add_worktree(
                    s,
                    project,
                    &branch,
                    text(args, "base").as_deref(),
                )
                .map_err(|e| e.0)?;
                json!({ "id": w.id, "branch": w.label(), "path": w.path })
            }
            "remember" => {
                let topic = text(args, "topic").map(|t| t.trim().to_string()).unwrap_or_default();
                if topic.is_empty() {
                    return Err("missing `topic`".into());
                }
                let note = text(args, "note").unwrap_or_default();
                s.set_note(project, &topic, &note).map_err(|e| e.0)?;
                if note.trim().is_empty() {
                    json!({ "forgot": topic })
                } else {
                    json!({ "remembered": topic })
                }
            }
            "recall" => {
                let notes = s.notes(project).map_err(|e| e.0)?;
                let want = text(args, "topic").map(|t| t.trim().to_lowercase());
                let notes: Vec<Value> = notes
                    .into_iter()
                    .filter(|(t, b)| {
                        want.as_ref().is_none_or(|w| {
                            t.to_lowercase().contains(w) || b.to_lowercase().contains(w)
                        })
                    })
                    .map(|(topic, note)| json!({ "topic": topic, "note": note }))
                    .collect();
                json!(notes)
            }
            "notify_user" => {
                let message = text(args, "message").filter(|m| !m.trim().is_empty());
                let message = message.ok_or("missing `message`")?;
                s.add_message(
                    session,
                    &Entry::Notice { text: format!("notice: {}", message.trim()) },
                )
                .map_err(|e| e.0)?;
                desktop_notice(&me.title, message.trim());
                json!({ "notified": true })
            }
            "set_model" => {
                let model = text(args, "model").filter(|m| !m.trim().is_empty());
                s.set_model(session, model.as_deref().map(str::trim)).map_err(|e| e.0)?;
                self.touched();
                json!({ "model": model })
            }
            other => return Err(format!("no agentbench tool called {other}")),
        };
        if matches!(
            tool,
            "file_issue"
                | "update_issue"
                | "comment_on_issue"
                | "create_worktree"
                | "start_session"
                | "send_message"
        ) {
            self.touched();
        }
        Ok(out)
    }
}

/// Tell whoever sits at the machine, if there is a way to. A headless server has none, and then
/// the notice in the chat is all there is.
fn desktop_notice(title: &str, message: &str) {
    #[cfg(not(test))]
    {
        use std::process::{Command, Stdio};
        let quiet = |c: &mut Command| {
            let _ = c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        };
        if cfg!(target_os = "macos") {
            let script = format!(
                "display notification {:?} with title {:?}",
                message.replace('"', "'"),
                format!("agentbench: {title}").replace('"', "'")
            );
            quiet(Command::new("osascript").args(["-e", &script]));
        } else if cfg!(target_os = "linux") {
            quiet(Command::new("notify-send").args([
                "--app-name=agentbench",
                &format!("agentbench: {title}"),
                message,
            ]));
        }
    }
    #[cfg(test)]
    let _ = (title, message);
}

fn kind(e: &Entry) -> &'static str {
    match e {
        Entry::User { .. } => "user",
        Entry::Assistant { .. } => "assistant",
        Entry::Thinking { .. } => "thinking",
        Entry::Tool { .. } => "tool",
        Entry::Question { .. } | Entry::Questions { .. } => "question",
        Entry::Error { .. } => "error",
        Entry::Notice { .. } => "notice",
        Entry::Diagnostics { .. } => "diagnostics",
        Entry::Unknown => "unknown",
        Entry::Summary { .. } => "summary",
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    format!("{}...", &text[..text.floor_char_boundary(max)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tracker_tools_stay_inside_the_session_project() {
        let s = Store::memory().unwrap();
        let (p, main) = crate::store::testing::project(&s, "/a");
        let (q, other_main) = crate::store::testing::project(&s, "/b");
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let elsewhere = s.add_session(other_main.id, "pi", "them", None, None).unwrap();
        let theirs = s
            .add_issue(&NewIssue {
                project_id: q.id,
                title: "not mine".into(),
                ..Default::default()
            })
            .unwrap();
        let api = StoreApi::new(s.clone());

        let filed =
            api.call("file_issue", &json!({"title": "Found a bug", "body": "x"}), me.id).unwrap();
        let fid = filed["filed"].as_i64().unwrap();
        assert_eq!(s.issue(fid).unwrap().project_id, p.id);

        let listed = api.call("list_issues", &json!({}), me.id).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        assert!(api.call("list_issues", &json!({"state": "nope"}), me.id).is_err());

        api.call("update_issue", &json!({"id": fid, "state": "review"}), me.id).unwrap();
        assert_eq!(s.issue(fid).unwrap().state, IssueState::Review);
        assert!(
            api.call("update_issue", &json!({"id": theirs.id, "state": "done"}), me.id).is_err()
        );

        api.call("comment_on_issue", &json!({"id": fid, "body": "ready"}), me.id).unwrap();
        assert_eq!(s.comments(fid).unwrap()[0].author, format!("native (session {})", me.id));

        let sessions = api.call("list_sessions", &json!({}), me.id).unwrap();
        assert_eq!(sessions.as_array().unwrap().len(), 1);
        assert_eq!(sessions[0]["you"], true);
        assert!(api.call("read_session", &json!({"id": elsewhere.id}), me.id).is_err());
    }

    #[test]
    fn issues_carry_comments_labels_and_priority() {
        let s = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, "/a");
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        let made = api
            .call(
                "file_issue",
                &json!({"title": "T", "labels": ["bug", " ui "], "state": "todo", "priority": 3}),
                me.id,
            )
            .unwrap();
        let fid = made["filed"].as_i64().unwrap();
        api.call("comment_on_issue", &json!({"id": fid, "body": "see the log"}), me.id).unwrap();
        let got = api.call("get_issue", &json!({"id": fid}), me.id).unwrap();
        assert_eq!(got["labels"], json!(["bug", "ui"]));
        assert_eq!(got["state"], "todo");
        assert_eq!(got["priority"], 3);
        assert_eq!(got["comments"][0]["body"], "see the log");
        api.call("update_issue", &json!({"id": fid, "labels": ["done-ish"]}), me.id).unwrap();
        assert_eq!(s.issue(fid).unwrap().labels, ["done-ish"]);
        assert!(api.call("file_issue", &json!({"title": "x", "labels": "bug"}), me.id).is_err());
    }

    #[test]
    fn notes_are_kept_per_project_and_replaced_by_topic() {
        let s = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, "/a");
        let (_, other) = crate::store::testing::project(&s, "/b");
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let them = s.add_session(other.id, "native", "them", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        api.call("remember", &json!({"topic": "tests", "note": "cargo test"}), me.id).unwrap();
        api.call("remember", &json!({"topic": "tests", "note": "cargo test --all"}), me.id)
            .unwrap();
        api.call("remember", &json!({"topic": "style", "note": "rustfmt"}), me.id).unwrap();
        let all = api.call("recall", &json!({}), me.id).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 2);
        let some = api.call("recall", &json!({"topic": "ALL"}), me.id).unwrap();
        assert_eq!(some[0]["note"], "cargo test --all");
        assert_eq!(api.call("recall", &json!({}), them.id).unwrap(), json!([]));
        api.call("remember", &json!({"topic": "style", "note": ""}), me.id).unwrap();
        assert_eq!(s.notes(main.project_id).unwrap().len(), 1);
        assert!(api.call("remember", &json!({"note": "x"}), me.id).is_err());
    }

    #[test]
    fn session_status_and_the_session_tools_stay_inside_the_project() {
        let s = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, "/a");
        let (_, other) = crate::store::testing::project(&s, "/b");
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let mate = s.add_session(main.id, "pi", "mate", None, None).unwrap();
        let away = s.add_session(other.id, "pi", "away", None, None).unwrap();
        s.add_message(mate.id, &Entry::Assistant { text: "finished".into() }).unwrap();
        let api = StoreApi::new(s.clone());
        let st = api.call("session_status", &json!({"id": mate.id.to_string()}), me.id).unwrap();
        assert_eq!(st["busy"], false);
        assert_eq!(st["last_reply"], "finished");
        assert!(api.call("session_status", &json!({"id": away.id.to_string()}), me.id).is_err());
        assert!(
            api.call("send_message", &json!({"id": away.id.to_string(), "text": "hi"}), me.id)
                .is_err()
        );
        // Without a runner behind it, starting a session says so rather than failing oddly.
        let e = api.call("start_session", &json!({"prompt": "go"}), me.id).unwrap_err();
        assert!(e.contains("cannot be started"), "{e}");
    }

    #[test]
    fn notify_user_leaves_a_notice_in_the_chat() {
        let s = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, "/a");
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        api.call("notify_user", &json!({"message": "blocked on a key"}), me.id).unwrap();
        let last = s.messages(me.id, 0).unwrap().pop().unwrap().entry;
        assert_eq!(last, Entry::Notice { text: "notice: blocked on a key".into() });
        assert!(api.call("notify_user", &json!({}), me.id).is_err());
    }

    #[test]
    fn worktrees_are_listed_and_made() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let s = Store::memory().unwrap();
        let p = crate::projects::add(&s, &main, None, false).unwrap();
        let me =
            s.add_session(s.main_worktree(p.id).unwrap().id, "native", "me", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        let made = api.call("create_worktree", &json!({"branch": "feature-x"}), me.id).unwrap();
        assert_eq!(made["branch"], "feature-x");
        let listed = api.call("list_worktrees", &json!({}), me.id).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 2);
        assert!(api.call("create_worktree", &json!({"branch": "bad name"}), me.id).is_err());
    }
}
