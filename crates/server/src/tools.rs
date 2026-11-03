use crate::agent::Api;
use crate::runner::Runner;
use crate::store::Store;
use agentbench_core::{
    Automation, Entry, Id, IssuePatch, IssueState, NewComment, NewIssue, NewSession, Place,
    Project, ProjectPatch, Risky, SessionId,
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
    approved: Mutex<std::collections::HashSet<(SessionId, Id, String)>>,
}

/// Setting: ids of the projects any session may start sessions in.
const ALLOWED_PROJECTS: &str = "start_session_projects";
/// Setting: "1" when any session may start sessions in any project.
const ALLOWED_ANY: &str = "start_session_any_project";

pub const PROPOSAL: &str = "proposal:";

const SET_AUTOMATION: &str = "set_automation";

pub fn required(tool: &str) -> Option<Automation> {
    Some(match tool {
        "file_issue" | "update_issue" | "comment_on_issue" | "remember" | "rename_session" => {
            Automation::Assist
        }
        "start_session" | "send_message" | "stop_session" | "close_session"
        | "create_worktree" | "cleanup_worktree" => Automation::Delegate,
        "merge_branch" => Automation::Ship,
        _ => return None,
    })
}

fn risky_key(action: Risky) -> String {
    format!("risky_action:{}", action.as_str())
}

fn proposal(what: &str, tool: &str, project: Option<&Project>, needs: &str, level: Automation) -> String {
    let name = project.map_or("the global default".to_string(), |p| p.name.clone());
    let detail = json!({
        "tool": tool,
        "project": project.map(|p| p.id),
        "project_name": name,
        "needs": needs,
        "level": level.label(),
        "what": what,
    });
    format!(
        "{PROPOSAL} {what} in {name} needs {needs}, and {name} is at {}. It was not done. Propose it to the user in your reply instead, or ask them to raise the level.\n{detail}",
        level.label()
    )
}

impl StoreApi {
    #[cfg(test)]
    pub fn new(store: Store) -> StoreApi {
        StoreApi {
            store,
            changed: None,
            runner: Handle::default(),
            once: Default::default(),
            children: Default::default(),
            approved: Default::default(),
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
            approved: Default::default(),
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
        if other.project_id == me.project_id || child || self.supervises(me) {
            Ok(())
        } else {
            Err("that session belongs to another project".into())
        }
    }

    fn managed(
        &self,
        me: &agentbench_core::Session,
        args: &Value,
    ) -> Result<agentbench_core::Session, String> {
        if !self.supervises(me) {
            return Err("only the supervisor chat can stop or close other sessions".into());
        }
        let other = self.store.session(session_id(args)?).map_err(|e| e.0)?;
        if other.id == me.id {
            return Err("that is this session".into());
        }
        Ok(other)
    }

    fn supervises(&self, me: &agentbench_core::Session) -> bool {
        self.store.project(me.project_id).is_ok_and(|p| p.supervisor)
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

impl StoreApi {
    fn target(&self, tool: &str, args: &Value, project: Id) -> Option<Project> {
        let s = &self.store;
        let pid = match tool {
            "update_issue" | "comment_on_issue" => s.issue(id(args).ok()?).ok()?.project_id,
            "send_message" | "stop_session" | "close_session" | "rename_session"
            | "merge_branch" => s.session(session_id(args).ok()?).ok()?.project_id,
            "start_session" => match args.get("project").filter(|v| !v.is_null()) {
                Some(v) => self.find_project(v).ok()?.id,
                None => project,
            },
            "cleanup_worktree" => {
                let scoped = args.get("project").is_some_and(|v| !v.is_null());
                let trees = s.worktrees(scoped.then_some(project)).ok()?;
                let found = match (args.get("id").and_then(Value::as_i64), text(args, "branch")) {
                    (Some(i), _) => trees.into_iter().find(|w| w.id == i),
                    (None, Some(b)) => trees.into_iter().find(|w| w.label() == b.trim()),
                    _ => None,
                };
                found.map_or(project, |w| w.project_id)
            }
            _ => project,
        };
        s.project(pid).ok()
    }

    fn take_approval(&self, me: SessionId, project: Id, key: &str) -> bool {
        self.approved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(me, project, key.to_string()))
    }

    fn gate(&self, tool: &str, args: &Value, project: Id, me: SessionId) -> Result<(), String> {
        let Some(needs) = required(tool) else { return Ok(()) };
        let Some(target) = self.target(tool, args, project).filter(|p| !p.supervisor) else {
            return Ok(());
        };
        let level = crate::autopilot::level_of(&self.store, &target);
        if level >= needs || self.take_approval(me, target.id, tool) {
            return Ok(());
        }
        Err(proposal(&describe(tool, args), tool, Some(&target), needs.label(), level))
    }

    fn set_automation(&self, args: &Value, me: SessionId) -> Result<Value, String> {
        let s = &self.store;
        let level = match text(args, "level").map(|l| l.trim().to_lowercase()) {
            None => None,
            Some(l) if l == "default" || l == "inherit" || l.is_empty() => Some(None),
            Some(l) => Some(Some(Automation::parse(&l).ok_or_else(|| {
                format!("{l} is not a level: manual, assist, delegate, autowork, ship or full_auto")
            })?)),
        };
        let risky = match args.get("risky") {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) => Some(
                a.iter()
                    .map(|v| {
                        let t = v.as_str().unwrap_or_default();
                        Risky::parse(t).ok_or_else(|| format!("{t} is not a risky action: deploy, prod_data, external_messages, publish or spend_money"))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Some(_) => return Err("`risky` is a list of action names".into()),
        };
        let deploy = text(args, "deploy");
        let default = crate::autopilot::default_level(s);
        let Some(v) = args.get("project").filter(|v| !v.is_null()) else {
            if risky.is_some() || deploy.is_some() {
                return Err("risky actions and the deploy command are set per project; name the `project`".into());
            }
            return match level {
                None => Ok(json!({ "summary": crate::autopilot::summary(s) })),
                Some(None) => Err("the global default needs a level".into()),
                Some(Some(l)) => {
                    if l > default && !self.take_approval(me, 0, SET_AUTOMATION) {
                        return Err(proposal(
                            &format!("raise the global default level to {}", l.label()),
                            SET_AUTOMATION,
                            None,
                            "the user's confirmation",
                            default,
                        ));
                    }
                    let st = crate::autopilot::apply(
                        s,
                        &agentbench_core::AutopilotPatch { default_level: Some(l), ..Default::default() },
                    )
                    .map_err(|e| e.0)?;
                    Ok(json!({ "default_level": st.default_level.label(), "summary": crate::autopilot::summary(s) }))
                }
            };
        };
        let p = self.find_project(v)?;
        if p.builtin() {
            return Err(format!("{} is built in and has no automation level", p.name));
        }
        if level.is_none() && risky.is_none() && deploy.is_none() {
            return Ok(json!({ "summary": crate::autopilot::summary(s) }));
        }
        let before = p.level(default);
        let after = level.map_or(before, |l| l.unwrap_or(default));
        let adds = risky.as_ref().is_some_and(|r| r.iter().any(|x| !p.risky.contains(x)));
        let new_deploy = deploy.as_ref().is_some_and(|d| Some(d.trim()) != p.deploy.as_deref() && !d.trim().is_empty());
        if (after > before || adds || new_deploy) && !self.take_approval(me, p.id, SET_AUTOMATION) {
            let mut what = Vec::new();
            if after > before {
                what.push(format!("raise {} to {}", p.name, after.label()));
            }
            if adds {
                let on: Vec<&str> = risky.iter().flatten().filter(|x| !p.risky.contains(x)).map(|x| x.label()).collect();
                what.push(format!("switch on {}", on.join(", ")));
            }
            if new_deploy {
                what.push(format!("set the deploy command to `{}`", deploy.as_deref().unwrap_or_default().trim()));
            }
            return Err(proposal(&what.join(" and "), SET_AUTOMATION, Some(&p), "the user's confirmation", before));
        }
        let patch = ProjectPatch { automation: level, risky, deploy, ..Default::default() };
        let p = crate::autopilot::set_project(s, p.id, &patch).map_err(|e| e.0)?;
        Ok(json!({
            "project": p.name,
            "level": p.level(default).label(),
            "own_level": p.automation.is_some(),
            "risky": p.risky.iter().map(|r| r.as_str()).collect::<Vec<_>>(),
            "deploy": p.deploy,
        }))
    }

    fn risky_action(&self, args: &Value, me: SessionId) -> Result<Value, String> {
        let v = args.get("project").filter(|v| !v.is_null()).ok_or("name the `project`")?;
        let p = self.find_project(v)?;
        let raw = text(args, "action").ok_or("missing `action`")?;
        let action = Risky::parse(&raw).ok_or_else(|| {
            format!("{raw} is not a risky action: deploy, prod_data, external_messages, publish or spend_money")
        })?;
        let default = crate::autopilot::default_level(&self.store);
        let key = risky_key(action);
        if !p.allows(default, action) && !self.take_approval(me, p.id, &key) {
            let why = text(args, "reason").map(|r| format!(": {}", clip(r.trim(), 300))).unwrap_or_default();
            let needs = format!("full auto with {} switched on", action.label());
            return Err(proposal(&format!("{}{why}", action.label()), &key, Some(&p), &needs, p.level(default)));
        }
        let run = (action == Risky::Deploy).then(|| p.deploy.clone()).flatten();
        Ok(json!({ "allowed": true, "action": action.as_str(), "project": p.name, "cwd": p.path, "run": run }))
    }

    fn approve(&self, args: &Value, me: SessionId) -> Result<Value, String> {
        let tool = text(args, "tool").ok_or("missing `tool`")?;
        let project = match args.get("project").filter(|v| !v.is_null()) {
            Some(v) => Some(self.find_project(v)?),
            None => None,
        };
        let pid = project.as_ref().map_or(0, |p| p.id);
        match text(args, "scope").as_deref() {
            Some("once") => {
                self.approved.lock().unwrap_or_else(|e| e.into_inner()).insert((me, pid, tool.clone()));
                Ok(json!({ "approved": tool, "once": true }))
            }
            Some("raise") => {
                let needs = required(&tool).ok_or("only a tool with a level can raise the project to it")?;
                let p = project.ok_or("name the `project` to raise")?;
                let default = crate::autopilot::default_level(&self.store);
                if p.level(default) < needs {
                    let patch = ProjectPatch { automation: Some(Some(needs)), ..Default::default() };
                    crate::autopilot::set_project(&self.store, p.id, &patch).map_err(|e| e.0)?;
                }
                Ok(json!({ "approved": tool, "level": needs.label() }))
            }
            other => Err(format!("{other:?} is not a scope")),
        }
    }
}

fn describe(tool: &str, args: &Value) -> String {
    let verb = match tool {
        "file_issue" => "file an issue",
        "update_issue" => "update an issue",
        "comment_on_issue" => "comment on an issue",
        "remember" => "keep a note",
        "rename_session" => "rename a session",
        "start_session" => "start a session",
        "send_message" => "send a message to a session",
        "stop_session" => "stop a session",
        "close_session" => "close a session",
        "create_worktree" => "make a worktree",
        "cleanup_worktree" => "remove a worktree",
        "merge_branch" => "merge a branch and push it",
        other => other,
    };
    let mut shown = args.clone();
    if let Some(o) = shown.as_object_mut() {
        o.remove("project");
    }
    let args = clip(&shown.to_string(), 300);
    if args == "{}" { verb.to_string() } else { format!("{verb} {args}") }
}

impl StoreApi {
    fn cleanup_worktree(
        &self,
        args: &Value,
        me: &agentbench_core::Session,
        scope: Option<Id>,
    ) -> Result<Value, String> {
        let s = &self.store;
        let trees = s.worktrees(scope).map_err(|e| e.0)?;
        let tree = match (args.get("id").and_then(Value::as_i64), text(args, "branch")) {
            (Some(id), _) => trees.into_iter().find(|w| w.id == id),
            (None, Some(b)) => trees.into_iter().find(|w| w.label() == b.trim()),
            (None, None) => return Err("give the worktree's `id` or `branch`".into()),
        };
        let tree = tree.ok_or("no such worktree in this project; list_worktrees gives them")?;
        if tree.main {
            return Err(
                "that is the main worktree, the project itself; it cannot be removed".into()
            );
        }
        if tree.id == me.worktree_id {
            return Err("this session works in it; clean it up from another worktree".into());
        }
        let sessions: Vec<_> = s
            .sessions()
            .map_err(|e| e.0)?
            .into_iter()
            .filter(|x| x.worktree_id == tree.id)
            .collect();
        let runner = self.runner.get().ok();
        if let Some(r) = &runner
            && let Some(busy) = sessions.iter().find(|x| r.is_running(x.id))
        {
            return Err(format!("session {} is still working in it; stop it first", busy.id));
        }
        let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
        let branch = tree.branch.clone();
        let project = s.project(tree.project_id).map_err(|e| e.0)?;
        crate::projects::remove_worktree(s, tree.id, force).map_err(|e| e.0)?;
        if let Some(r) = &runner {
            for x in &sessions {
                r.end(x.id);
            }
        }
        let mut out = json!({
            "removed": tree.path,
            "sessions_deleted": sessions.iter().map(|x| x.id.to_string()).collect::<Vec<_>>(),
        });
        if let Some(b) = branch {
            let keep = args.get("keep_branch").and_then(Value::as_bool).unwrap_or(false);
            match keep {
                true => out["branch_deleted"] = json!(false),
                false => match crate::git::delete_branch(std::path::Path::new(&project.path), &b) {
                    Ok(()) => out["branch_deleted"] = json!(true),
                    Err(e) => {
                        out["branch_deleted"] = json!(false);
                        out["branch_kept"] = json!(format!("{b}: {e}"));
                    }
                },
            }
        }
        Ok(out)
    }
}

impl Api for StoreApi {
    fn call(&self, tool: &str, args: &Value, session: SessionId) -> Result<Value, String> {
        let s = &self.store;
        let me = s.session(session).map_err(|e| e.0)?;
        let sup = self.supervises(&me);
        let project = match (sup, args.get("project")) {
            (true, Some(v))
                if !v.is_null()
                && !matches!(tool, "start_session" | "check_project" | "grant_project" | "set_automation" | "risky_action" | "approve_action") =>
            {
                self.find_project(v)?.id
            }
            _ => me.project_id,
        };
        let everywhere = sup && args.get("project").is_none_or(Value::is_null);
        let own = |issue: Id| -> Result<agentbench_core::Issue, String> {
            let i = s.issue(issue).map_err(|e| e.0)?;
            if i.project_id != project && !sup {
                return Err(format!("issue #{issue} belongs to another project"));
            }
            Ok(i)
        };
        if sup {
            self.gate(tool, args, project, session)?;
        }
        let out = match tool {
            "list_issues" => {
                let filter = state(args)?;
                let issues: Vec<Value> = s
                    .issues()
                    .map_err(|e| e.0)?
                    .into_iter()
                    .filter(|i| everywhere || i.project_id == project)
                    .filter(|i| filter.is_none_or(|f| i.state == f))
                    .map(|i| {
                        let mut out = issue_json(&i);
                        if sup {
                            out["project"] = json!(i.project_id);
                        }
                        out
                    })
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
                let trees =
                    s.worktrees(if everywhere { None } else { Some(project) }).map_err(|e| e.0)?;
                let sessions: Vec<Value> = s
                    .sessions()
                    .map_err(|e| e.0)?
                    .into_iter()
                    .filter(|x| everywhere || x.project_id == project)
                    .map(|x| {
                        let branch =
                            trees.iter().find(|w| w.id == x.worktree_id).map(|w| w.label());
                        json!({
                            "id": x.id,
                            "project": x.project_id,
                            "updated_at": x.updated_at,
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
                let msgs = agentbench_core::shown(&s.messages(other.id, 0).map_err(|e| e.0)?);
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
            "watch_session" => {
                let other = s.session(session_id(args)?).map_err(|e| e.0)?;
                self.reaches(&me, &other)?;
                if other.id == session {
                    return Err("that is this session".into());
                }
                let running = self.runner.get().ok().is_some_and(|r| r.is_running(other.id));
                if !running {
                    return Ok(json!({
                        "session": other.id,
                        "watching": false,
                        "status": other.status,
                        "note": "it is not running, so nothing will fire; read it with read_session",
                    }));
                }
                s.add_watch(other.id, session).map_err(|e| e.0)?;
                json!({ "session": other.id, "watching": true, "status": other.status })
            }
            "user_asks" => {
                let per = args.get("per_session").and_then(Value::as_u64).unwrap_or(5).clamp(1, 50)
                    as usize;
                let limit = args.get("sessions").and_then(Value::as_u64).unwrap_or(30).clamp(1, 200)
                    as usize;
                let mut out = Vec::new();
                for x in s
                    .sessions()
                    .map_err(|e| e.0)?
                    .into_iter()
                    .filter(|x| everywhere || x.project_id == project)
                {
                    if out.len() >= limit {
                        break;
                    }
                    let asks: Vec<Value> = agentbench_core::shown(
                        &s.messages(x.id, 0).map_err(|e| e.0)?,
                    )
                    .into_iter()
                    .filter(|m| matches!(m.entry, Entry::User { .. }))
                    .map(|m| json!({ "at": m.created_at, "text": clip(m.entry.text(), 1000) }))
                    .collect();
                    if asks.is_empty() {
                        continue;
                    }
                    let skip = asks.len().saturating_sub(per);
                    out.push(json!({
                        "session": x.id,
                        "project": x.project_id,
                        "title": x.title,
                        "status": x.status,
                        "issue": x.issue_id,
                        "updated_at": x.updated_at,
                        "asks": asks[skip..],
                    }));
                }
                json!(out)
            }
            "session_status" => {
                let other = s.session(session_id(args)?).map_err(|e| e.0)?;
                self.reaches(&me, &other)?;
                let last = agentbench_core::shown(&s.messages(other.id, 0).map_err(|e| e.0)?)
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
                    .filter(|p| !p.supervisor)
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
                    "allowed": sup || p.id == project || self.standing_permission(p.id),
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
                            if !once && !sup && !self.standing_permission(target.id) {
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
                if sup {
                    s.add_watch(started.id, session).map_err(|e| e.0)?;
                }
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
                    .worktrees((!everywhere).then_some(project))
                    .map_err(|e| e.0)?
                    .into_iter()
                    .map(|w| {
                        json!({
                            "id": w.id, "project": w.project_id, "branch": w.label(), "path": w.path,
                            "main": w.main, "missing": w.missing,
                        })
                    })
                    .collect();
                json!(trees)
            }
            "create_worktree" => {
                if everywhere {
                    return Err("name the `project` to make the worktree in".into());
                }
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
            "cleanup_worktree" => {
                self.cleanup_worktree(args, &me, (!everywhere).then_some(project))?
            }
            "stop_session" => {
                let other = self.managed(&me, args)?;
                let runner = self.runner.get()?;
                let was_running = runner.interrupt(other.id);
                json!({ "stopped": other.id, "was_running": was_running })
            }
            "close_session" => {
                let other = self.managed(&me, args)?;
                let runner = self.runner.get()?;
                runner.interrupt(other.id);
                runner.end(other.id);
                s.delete_session(other.id).map_err(|e| e.0)?;
                json!({ "closed": other.id, "title": other.title })
            }
            "rename_session" => {
                let other = s.session(session_id(args)?).map_err(|e| e.0)?;
                self.reaches(&me, &other)?;
                let title = text(args, "title").ok_or("missing `title`")?;
                s.set_title(other.id, &title, true).map_err(|e| e.0)?;
                json!({ "renamed": other.id, "title": title.trim() })
            }
            "remember" => {
                let topic = text(args, "topic").map(|t| t.trim().to_string()).unwrap_or_default();
                if topic.is_empty() {
                    return Err("missing `topic`".into());
                }
                let note = text(args, "note").unwrap_or_default();
                let based_on = text(args, "based_on");
                if text(args, "scope").is_some_and(|s| s.trim().eq_ignore_ascii_case("global")) {
                    return Err("global notes are read-only to agents: only the user, or a triage proposal the user accepted, can change them. Propose the note to the user in your reply instead.".into());
                }
                s.set_note(project, &topic, &note, "agent", Some(me.id), based_on.as_deref())
                    .map_err(|e| e.0)?;
                if note.trim().is_empty() {
                    json!({ "forgot": topic })
                } else {
                    json!({ "remembered": topic })
                }
            }
            "recall" => {
                let global = text(args, "scope").is_some_and(|s| s.trim().eq_ignore_ascii_case("global"));
                let notes = if global { s.global_notes() } else { s.notes(project) }.map_err(|e| e.0)?;
                let want = text(args, "topic").map(|t| t.trim().to_lowercase());
                let notes: Vec<Value> = notes
                    .into_iter()
                    .filter(|(t, b)| {
                        want.as_ref().is_none_or(|w| {
                            t.to_lowercase().contains(w) || b.to_lowercase().contains(w)
                        })
                    })
                    .map(|(topic, note)| {
                        json!({ "topic": topic, "hash": crate::store::hash_note(&note), "note": note })
                    })
                    .collect();
                json!(notes)
            }
            "send_file" => {
                let path = text(args, "path").filter(|p| !p.trim().is_empty());
                let path = path.ok_or("missing `path`")?;
                let full = std::path::Path::new(&me.cwd).join(path.trim());
                let meta = std::fs::metadata(&full).map_err(|e| format!("{path}: {e}"))?;
                if !meta.is_file() {
                    return Err(format!("{path} is not a file"));
                }
                if meta.len() > agentbench_core::ATTACHMENT_MAX as u64 {
                    return Err(format!(
                        "{path} is over {} MB",
                        agentbench_core::ATTACHMENT_MAX >> 20
                    ));
                }
                let data = std::fs::read(&full).map_err(|e| format!("{path}: {e}"))?;
                let name = text(args, "name")
                    .filter(|n| !n.trim().is_empty())
                    .or_else(|| {
                        full.file_name().map(|n| n.to_string_lossy().into_owned())
                    })
                    .unwrap_or_else(|| "file".into());
                let name = name.rsplit(['/', '\\']).next().unwrap_or("file").trim().to_string();
                let mime = agentbench_core::mime_for(&name).unwrap_or("application/octet-stream");
                let file = s.add_attachment(session, &name, mime, &data).map_err(|e| e.0)?;
                let caption = text(args, "caption").unwrap_or_default().trim().to_string();
                s.add_message(session, &Entry::File { text: caption.clone(), file: file.clone() })
                    .map_err(|e| e.0)?;
                desktop_notice(&me.title, &format!("sent you {}", file.name));
                json!({ "sent": file.name, "size": file.size, "mime": file.mime })
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
            "autopilot" => {
                if !sup {
                    return Err("only the supervisor chat can run the autopilot".into());
                }
                let num = |k: &str| args.get(k).and_then(Value::as_u64).map(|n| n as u32);
                let flag = |k: &str| args.get(k).and_then(Value::as_bool);
                let patch = agentbench_core::AutopilotPatch {
                    enabled: flag("enabled"),
                    cap: num("cap"),
                    project_cap: num("project_cap"),
                    stuck_minutes: args.get("stuck_minutes").and_then(Value::as_u64),
                    max_nudges: num("max_nudges"),
                    default_level: None,
                };
                let state = crate::autopilot::apply(s, &patch).map_err(|e| e.0)?;
                json!(state)
            }
            "merge_branch" | "set_automation" | "risky_action" | "approve_action" if !sup => {
                return Err(format!("only the supervisor chat can use {tool}"));
            }
            "merge_branch" => {
                let other = s.session(session_id(args)?).map_err(|e| e.0)?;
                match crate::autopilot::ship(s, &other) {
                    Ok(done) => json!({
                        "branch": done.branch,
                        "pushed": done.pushed.as_ref().map(|r| r.is_ok()),
                        "deployed": done.deployed.as_ref().map(|r| r.is_ok()),
                        "summary": done.describe(),
                    }),
                    Err(crate::autopilot::Unshipped::NoBranch) => {
                        return Err("that session works in the main checkout; there is no branch to merge".into());
                    }
                    Err(crate::autopilot::Unshipped::Failed(e)) => return Err(format!("merge failed: {e}")),
                }
            }
            "set_automation" => self.set_automation(args, session)?,
            "risky_action" => self.risky_action(args, session)?,
            "approve_action" => self.approve(args, session)?,
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
                | "autopilot"
                | "merge_branch"
                | "set_automation"
                | "approve_action"
                | "update_issue"
                | "comment_on_issue"
                | "create_worktree"
                | "cleanup_worktree"
                | "start_session"
                | "send_message"
                | "stop_session"
                | "close_session"
                | "rename_session"
        ) {
            self.touched();
        }
        Ok(out)
    }
}

/// Tell whoever sits at the machine, if there is a way to. A headless server has none, and then
/// the notice in the chat is all there is.
pub(crate) fn desktop_notice(title: &str, message: &str) {
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
        Entry::File { .. } => "file",
        Entry::Diagnostics { .. } => "diagnostics",
        Entry::Wire { .. } => "sent",
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
    use agentbench_core::{Automation, ProjectPatch, Risky};

    struct Boss {
        s: Store,
        api: StoreApi,
        boss: SessionId,
        project: Id,
        worker: SessionId,
        issue: Id,
        _dir: tempfile::TempDir,
    }

    fn boss() -> Boss {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::memory().unwrap();
        let (p, main) = crate::store::testing::project(&s, "/a");
        let worker = s.add_session(main.id, "native", "w", None, None).unwrap();
        let sup = crate::projects::ensure_supervisor(&s, &dir.path().join("sup")).unwrap();
        let boss = s.add_session(s.main_worktree(sup.id).unwrap().id, "native", "boss", None, None).unwrap();
        let issue = s.add_issue(&NewIssue { project_id: p.id, title: "x".into(), ..Default::default() }).unwrap();
        crate::autopilot::migrate(&s).unwrap();
        Boss { api: StoreApi::new(s.clone()), s, boss: boss.id, project: p.id, worker: worker.id, issue: issue.id, _dir: dir }
    }

    fn at(b: &Boss, level: Automation) {
        b.s.update_project(b.project, &ProjectPatch { automation: Some(Some(level)), ..Default::default() }).unwrap();
    }

    fn calls(b: &Boss) -> Vec<(&'static str, Value, Automation)> {
        let w = b.worker.to_string();
        vec![
            ("list_issues", json!({"project": b.project}), Automation::Manual),
            ("get_issue", json!({"id": b.issue}), Automation::Manual),
            ("read_session", json!({"id": w}), Automation::Manual),
            ("file_issue", json!({"project": b.project, "title": "t"}), Automation::Assist),
            ("update_issue", json!({"id": b.issue, "state": "todo"}), Automation::Assist),
            ("comment_on_issue", json!({"id": b.issue, "body": "c"}), Automation::Assist),
            ("remember", json!({"project": b.project, "topic": "t", "note": "n"}), Automation::Assist),
            ("rename_session", json!({"id": w, "title": "t"}), Automation::Assist),
            ("start_session", json!({"project": b.project, "prompt": "go"}), Automation::Delegate),
            ("send_message", json!({"id": w, "text": "hi"}), Automation::Delegate),
            ("stop_session", json!({"id": w}), Automation::Delegate),
            ("close_session", json!({"id": w}), Automation::Delegate),
            ("create_worktree", json!({"project": b.project, "branch": "x"}), Automation::Delegate),
            ("cleanup_worktree", json!({"project": b.project, "branch": "x"}), Automation::Delegate),
            ("merge_branch", json!({"id": w}), Automation::Ship),
        ]
    }

    fn proposed(r: &Result<Value, String>) -> bool {
        matches!(r, Err(e) if e.starts_with(PROPOSAL))
    }

    #[test]
    fn each_supervisor_tool_below_its_level_becomes_a_proposal() {
        let b = boss();
        for level in Automation::ALL {
            at(&b, level);
            for (tool, args, needs) in calls(&b) {
                let r = b.api.call(tool, &args, b.boss);
                assert_eq!(proposed(&r), level < needs, "{tool} at {level:?}: {r:?}");
                if level < needs {
                    let e = r.unwrap_err();
                    assert!(e.contains(needs.label()) && e.contains(level.label()), "{e}");
                }
            }
        }
    }

    #[test]
    fn the_global_default_gates_a_project_with_no_level() {
        let b = boss();
        let file = json!({"project": b.project, "title": "t"});
        assert!(b.api.call("file_issue", &file, b.boss).is_ok(), "delegate by default");
        crate::autopilot::apply(&b.s, &agentbench_core::AutopilotPatch { default_level: Some(Automation::Manual), ..Default::default() }).unwrap();
        assert!(proposed(&b.api.call("file_issue", &file, b.boss)));
        at(&b, Automation::Assist);
        assert!(b.api.call("file_issue", &file, b.boss).is_ok(), "the project's own level wins");
    }

    #[test]
    fn an_approved_proposal_runs_once() {
        let b = boss();
        at(&b, Automation::Manual);
        let c = json!({"id": b.issue, "body": "c"});
        assert!(proposed(&b.api.call("comment_on_issue", &c, b.boss)));
        assert!(b.api.call("approve_action", &json!({"tool": "comment_on_issue", "project": b.project, "scope": "once"}), b.worker).is_err(), "only the supervisor's own approvals");
        b.api.call("approve_action", &json!({"tool": "comment_on_issue", "project": b.project, "scope": "once"}), b.boss).unwrap();
        assert!(b.api.call("comment_on_issue", &c, b.boss).is_ok());
        assert!(proposed(&b.api.call("comment_on_issue", &c, b.boss)), "used up");
        b.api.call("approve_action", &json!({"tool": "comment_on_issue", "project": b.project, "scope": "raise"}), b.boss).unwrap();
        assert_eq!(b.s.project(b.project).unwrap().automation, Some(Automation::Assist), "raised to what it needs");
        assert!(b.api.call("comment_on_issue", &c, b.boss).is_ok());
    }

    #[test]
    fn workers_are_not_gated_by_the_supervisor_level() {
        let b = boss();
        at(&b, Automation::Manual);
        assert!(b.api.call("file_issue", &json!({"title": "t"}), b.worker).is_ok());
        assert!(b.api.call("update_issue", &json!({"id": b.issue, "state": "review"}), b.worker).is_ok());
    }

    #[test]
    fn risky_actions_are_gated_by_their_toggle_at_every_level() {
        let b = boss();
        for level in Automation::ALL {
            for on in [vec![], vec![Risky::Deploy]] {
                b.s.update_project(b.project, &ProjectPatch { automation: Some(Some(level)), risky: Some(on.clone()), deploy: Some("echo hi".into()), ..Default::default() }).unwrap();
                for r in Risky::ALL {
                    let out = b.api.call("risky_action", &json!({"project": b.project, "action": r.as_str(), "reason": "x"}), b.boss);
                    let want = level == Automation::FullAuto && on.contains(&r);
                    assert_eq!(out.is_ok(), want, "{r:?} at {level:?} with {on:?}: {out:?}");
                    if !want {
                        assert!(proposed(&out), "{out:?}");
                    }
                }
            }
        }
        let ok = b.api.call("risky_action", &json!({"project": b.project, "action": "deploy"}), b.boss).unwrap();
        assert_eq!(ok["run"], "echo hi");
        assert!(b.api.call("risky_action", &json!({"project": b.project, "action": "deploy"}), b.worker).is_err(), "supervisor only");
    }

    #[test]
    fn a_risky_approval_is_one_action_once_and_never_raises_anything() {
        let b = boss();
        at(&b, Automation::Ship);
        let go = json!({"project": b.project, "action": "publish"});
        assert!(proposed(&b.api.call("risky_action", &go, b.boss)));
        assert!(b.api.call("approve_action", &json!({"tool": "risky_action:publish", "project": b.project, "scope": "raise"}), b.boss).is_err());
        b.api.call("approve_action", &json!({"tool": "risky_action:publish", "project": b.project, "scope": "once"}), b.boss).unwrap();
        assert!(proposed(&b.api.call("risky_action", &json!({"project": b.project, "action": "deploy"}), b.boss)), "another action");
        assert!(b.api.call("risky_action", &go, b.boss).is_ok());
        assert!(proposed(&b.api.call("risky_action", &go, b.boss)));
        let p = b.s.project(b.project).unwrap();
        assert_eq!((p.automation, p.risky.len()), (Some(Automation::Ship), 0));
    }

    #[test]
    fn lowering_a_level_applies_and_raising_it_needs_the_user() {
        let b = boss();
        at(&b, Automation::Ship);
        let out = b.api.call("set_automation", &json!({"project": b.project, "level": "assist"}), b.boss).unwrap();
        assert_eq!(out["level"], "assist");
        assert_eq!(b.s.project(b.project).unwrap().automation, Some(Automation::Assist));
        let up = json!({"project": b.project, "level": "ship"});
        assert!(proposed(&b.api.call("set_automation", &up, b.boss)));
        assert_eq!(b.s.project(b.project).unwrap().automation, Some(Automation::Assist));
        let toggle = json!({"project": b.project, "risky": ["deploy"]});
        assert!(proposed(&b.api.call("set_automation", &toggle, b.boss)), "switching a risky action on is a raise");
        b.api.call("approve_action", &json!({"tool": "set_automation", "project": b.project, "scope": "once"}), b.boss).unwrap();
        b.api.call("set_automation", &up, b.boss).unwrap();
        assert_eq!(b.s.project(b.project).unwrap().automation, Some(Automation::Ship));
        let global = json!({"level": "full_auto"});
        assert!(proposed(&b.api.call("set_automation", &global, b.boss)));
        b.api.call("set_automation", &json!({"level": "manual"}), b.boss).unwrap();
        assert_eq!(crate::autopilot::default_level(&b.s), Automation::Manual);
        let read = b.api.call("set_automation", &json!({}), b.boss).unwrap();
        assert!(read["summary"].as_str().unwrap().contains("ship"), "{read}");
        assert!(b.api.call("set_automation", &json!({"level": "manual"}), b.worker).is_err());
        assert!(b.api.call("set_automation", &json!({"level": "turbo"}), b.boss).is_err());
    }

    #[test]
    fn merge_branch_ships_a_worker_branch_at_ship() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let s = Store::memory().unwrap();
        let p = crate::projects::add(&s, &main, None, false).unwrap();
        crate::autopilot::migrate(&s).unwrap();
        let sup = crate::projects::ensure_supervisor(&s, &dir.path().join("sup")).unwrap();
        let boss = s.add_session(s.main_worktree(sup.id).unwrap().id, "native", "boss", None, None).unwrap();
        let tree = crate::projects::add_worktree(&s, p.id, "feat", None).unwrap();
        std::fs::write(std::path::Path::new(&tree.path).join("f.txt"), "x").unwrap();
        for args in [vec!["add", "."], vec!["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "w"]] {
            assert!(std::process::Command::new("git").arg("-C").arg(&tree.path).args(&args).status().unwrap().success());
        }
        let w = s.add_session(tree.id, "native", "w", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        let go = json!({"id": w.id.to_string()});
        assert!(proposed(&api.call("merge_branch", &go, boss.id)));
        assert!(!main.join("f.txt").exists());
        s.update_project(p.id, &ProjectPatch { automation: Some(Some(Automation::Ship)), ..Default::default() }).unwrap();
        let out = api.call("merge_branch", &go, boss.id).unwrap();
        assert!(main.join("f.txt").exists(), "{out}");
        assert_eq!(out["branch"], "feat");
    }

    #[test]
    fn only_the_supervisor_can_toggle_the_autopilot() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, "/a");
        let worker = s.add_session(main.id, "native", "w", None, None).unwrap();
        let sup = crate::projects::ensure_supervisor(&s, &dir.path().join("sup")).unwrap();
        let sup_tree = s.main_worktree(sup.id).unwrap();
        let boss = s.add_session(sup_tree.id, "native", "boss", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        assert!(api.call("autopilot", &json!({"enabled": true}), worker.id).is_err());
        assert!(!crate::autopilot::state(&s).enabled);
        let read = api.call("autopilot", &json!({}), boss.id).unwrap();
        assert_eq!((read["enabled"].clone(), read["cap"].clone()), (json!(false), json!(4)));
        let on = api.call("autopilot", &json!({"enabled": true, "cap": 3}), boss.id).unwrap();
        assert_eq!((on["enabled"].clone(), on["cap"].clone()), (json!(true), json!(3)));
    }

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
    fn the_supervisor_sees_and_files_across_projects() {
        let s = Store::memory().unwrap();
        let (p, main) = crate::store::testing::project(&s, "/a");
        let (q, other_main) = crate::store::testing::project(&s, "/b");
        let dir = tempfile::tempdir().unwrap();
        let sup = crate::projects::ensure_supervisor(&s, dir.path()).unwrap();
        let sup_main = s.main_worktree(sup.id).unwrap();
        let boss = s.add_session(sup_main.id, "native", "boss", None, None).unwrap();
        let a = s.add_session(main.id, "native", "a", None, None).unwrap();
        let b = s.add_session(other_main.id, "native", "b", None, None).unwrap();
        s.add_message(a.id, &Entry::user("fix the login bug")).unwrap();
        s.add_message(b.id, &Entry::user("add dark mode")).unwrap();
        s.add_message(b.id, &Entry::Assistant { text: "on it".into() }).unwrap();
        let api = StoreApi::new(s.clone());

        let all = api.call("list_sessions", &json!({}), boss.id).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 3);
        let asks = api.call("user_asks", &json!({}), boss.id).unwrap();
        let texts: Vec<_> = asks
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|x| x["asks"].as_array().unwrap().iter().map(|a| a["text"].as_str().unwrap()))
            .collect();
        assert!(texts.contains(&"fix the login bug") && texts.contains(&"add dark mode"));
        let only = api.call("user_asks", &json!({"project": q.id}), boss.id).unwrap();
        assert_eq!(only.as_array().unwrap().len(), 1);

        api.call("read_session", &json!({"id": a.id.to_string()}), boss.id).unwrap();
        api.call("send_message", &json!({"id": a.id.to_string(), "text": "hi"}), boss.id)
            .unwrap_err();

        let filed = api
            .call("file_issue", &json!({"title": "login bug", "project": p.id}), boss.id)
            .unwrap();
        assert_eq!(s.issue(filed["filed"].as_i64().unwrap()).unwrap().project_id, p.id);
        let listed = api.call("list_issues", &json!({}), boss.id).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 1);
        api.call("update_issue", &json!({"id": filed["filed"], "state": "doing"}), boss.id)
            .unwrap();

        assert!(api.call("read_session", &json!({"id": b.id.to_string()}), a.id).is_err());
        assert!(api.call("file_issue", &json!({"title": "x", "project": q.id}), a.id).is_ok());
        assert_eq!(
            s.issues().unwrap().iter().filter(|i| i.project_id == q.id).count(),
            0,
            "a normal session ignores `project` and files in its own"
        );
    }

    #[test]
    fn only_the_supervisor_closes_renames_and_sees_worktrees_everywhere() {
        let s = Store::memory().unwrap();
        let (p, main) = crate::store::testing::project(&s, "/a");
        let (_, other_main) = crate::store::testing::project(&s, "/b");
        let dir = tempfile::tempdir().unwrap();
        let sup = crate::projects::ensure_supervisor(&s, dir.path()).unwrap();
        let sup_main = s.main_worktree(sup.id).unwrap();
        let boss = s.add_session(sup_main.id, "native", "boss", None, None).unwrap();
        let a = s.add_session(main.id, "native", "a", None, None).unwrap();
        let b = s.add_session(other_main.id, "native", "b", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        let id = |x: &agentbench_core::Session| json!({"id": x.id.to_string()});

        let trees = api.call("list_worktrees", &json!({}), boss.id).unwrap();
        assert!(trees.as_array().unwrap().len() >= 3, "every project's worktrees: {trees}");
        let one = api.call("list_worktrees", &json!({"project": p.id}), boss.id).unwrap();
        assert_eq!(one.as_array().unwrap().len(), 1);
        assert!(api.call("create_worktree", &json!({"branch": "x"}), boss.id).is_err());

        assert!(api.call("close_session", &id(&b), a.id).is_err(), "not the supervisor");
        assert!(api.call("stop_session", &id(&b), a.id).is_err());
        assert!(api.call("close_session", &id(&boss), boss.id).is_err(), "not itself");

        api.call("rename_session", &json!({"id": b.id.to_string(), "title": "better"}), boss.id)
            .unwrap();
        assert_eq!(s.session(b.id).unwrap().title, "better");
        assert!(
            api.call("rename_session", &json!({"id": b.id.to_string(), "title": "x"}), a.id)
                .is_err(),
            "a normal session cannot rename another project's chat"
        );
        let e = api.call("close_session", &id(&b), boss.id).unwrap_err();
        assert!(e.contains("cannot be started"), "{e}");
    }

    #[test]
    fn watching_needs_a_running_session_and_a_supervisor_can_reach_any() {
        let s = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, "/a");
        let dir = tempfile::tempdir().unwrap();
        let sup = crate::projects::ensure_supervisor(&s, dir.path()).unwrap();
        let sup_main = s.main_worktree(sup.id).unwrap();
        let boss = s.add_session(sup_main.id, "native", "boss", None, None).unwrap();
        let other = s.add_session(main.id, "native", "other", None, None).unwrap();
        let api = StoreApi::new(s.clone());

        let out = api.call("watch_session", &json!({"id": other.id.to_string()}), boss.id).unwrap();
        assert_eq!(out["watching"], false, "nothing is running, so nothing can fire");
        assert!(s.take_watchers(other.id).unwrap().is_empty());
        assert!(api.call("watch_session", &json!({"id": boss.id.to_string()}), boss.id).is_err());
        let mate = s.add_session(main.id, "native", "mate", None, None).unwrap();
        assert!(api.call("watch_session", &json!({"id": other.id.to_string()}), mate.id).is_ok());
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
        assert!(api
            .call("remember", &json!({"topic": "tests", "note": "cargo test --all"}), me.id)
            .is_err());
        let hash = api.call("recall", &json!({"topic": "tests"}), me.id).unwrap()[0]["hash"].clone();
        api.call(
            "remember",
            &json!({"topic": "tests", "note": "cargo test --all", "based_on": hash}),
            me.id,
        )
        .unwrap();
        let stale = api.call(
            "remember",
            &json!({"topic": "tests", "note": "lost", "based_on": hash}),
            me.id,
        );
        assert!(stale.unwrap_err().contains("changed since"));
        assert_eq!(s.note_versions(main.project_id, "tests").unwrap().len(), 2);
        api.call("remember", &json!({"topic": "style", "note": "rustfmt"}), me.id).unwrap();
        let all = api.call("recall", &json!({}), me.id).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 2);
        let some = api.call("recall", &json!({"topic": "ALL"}), me.id).unwrap();
        assert_eq!(some[0]["note"], "cargo test --all");
        assert_eq!(api.call("recall", &json!({}), them.id).unwrap(), json!([]));
        let h = api.call("recall", &json!({"topic": "style"}), me.id).unwrap()[0]["hash"].clone();
        api.call("remember", &json!({"topic": "style", "note": "", "based_on": h}), me.id).unwrap();
        assert_eq!(s.notes(main.project_id).unwrap().len(), 1);
        assert!(api.call("remember", &json!({"note": "x"}), me.id).is_err());
    }

    #[test]
    fn a_prior_note_version_can_be_restored_and_keeps_its_author() {
        let s = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, "/a");
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let p = main.project_id;
        s.set_note(p, "t", "one", "agent", Some(me.id), None).unwrap();
        let h = s.note_hash(p, "t").unwrap();
        s.set_note(p, "t", "two", "user", None, Some(&h)).unwrap();
        let versions = s.note_versions(p, "t").unwrap();
        assert_eq!((versions[0].author.as_str(), versions[1].session), ("user", Some(me.id)));
        s.restore_note(p, versions[1].id, "user", None).unwrap();
        assert_eq!(s.notes(p).unwrap(), [("t".to_string(), "one".to_string())]);
        assert_eq!(s.note_versions(p, "t").unwrap().len(), 3);
    }

    #[test]
    fn global_notes_are_read_only_to_agents() {
        let s = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, "/a");
        let (_, other) = crate::store::testing::project(&s, "/b");
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let them = s.add_session(other.id, "native", "them", None, None).unwrap();
        s.set_global_note("style", "no em dashes").unwrap();
        let api = StoreApi::new(s.clone());
        let refused = api.call("remember", &json!({"scope": "global", "topic": "style", "note": "x"}), me.id);
        assert!(refused.unwrap_err().contains("read-only"));
        assert_eq!(s.global_notes().unwrap(), [("style".to_string(), "no em dashes".to_string())]);
        for id in [me.id, them.id] {
            let seen = api.call("recall", &json!({"scope": "global"}), id).unwrap();
            assert_eq!(seen[0]["note"], "no em dashes");
        }
        assert_eq!(api.call("recall", &json!({}), me.id).unwrap(), json!([]));
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
    fn send_file_puts_the_file_in_the_chat_to_download() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.md"), "# hi").unwrap();
        let s = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, dir.path().to_str().unwrap());
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        let out = api
            .call("send_file", &json!({"path": "report.md", "caption": "the report"}), me.id)
            .unwrap();
        assert_eq!(out["mime"], "text/markdown");
        let Entry::File { text, file } = s.messages(me.id, 0).unwrap().pop().unwrap().entry else {
            panic!("no file entry");
        };
        assert_eq!((text.as_str(), file.name.as_str(), file.size), ("the report", "report.md", 4));
        let (_, bytes) = s.session_attachment(me.id, file.id).unwrap();
        assert_eq!(bytes, b"# hi");
        assert!(api.call("send_file", &json!({"path": "nope.txt"}), me.id).is_err());
        assert!(api.call("send_file", &json!({}), me.id).is_err());
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

    #[test]
    fn cleaning_up_a_worktree_removes_its_checkout_chats_and_merged_branch() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let s = Store::memory().unwrap();
        let p = crate::projects::add(&s, &main, None, false).unwrap();
        let home = s.main_worktree(p.id).unwrap();
        let me = s.add_session(home.id, "native", "me", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        let made = api.call("create_worktree", &json!({"branch": "done-work"}), me.id).unwrap();
        let tree = made["id"].as_i64().unwrap();
        let path = std::path::PathBuf::from(made["path"].as_str().unwrap());
        let chat = s.add_session(tree, "native", "worker", None, None).unwrap();

        let e = api.call("cleanup_worktree", &json!({"id": home.id}), me.id).unwrap_err();
        assert!(e.contains("main worktree"), "{e}");
        let mine = s.add_session(tree, "native", "inside", None, None).unwrap();
        let e = api.call("cleanup_worktree", &json!({"id": tree}), mine.id).unwrap_err();
        assert!(e.contains("this session works in it"), "{e}");
        s.delete_session(mine.id).unwrap();
        std::fs::write(path.join("draft.txt"), "unsaved").unwrap();
        let e = api.call("cleanup_worktree", &json!({"branch": "done-work"}), me.id).unwrap_err();
        assert!(e.contains("uncommitted changes"), "{e}");
        std::fs::remove_file(path.join("draft.txt")).unwrap();

        let out = api.call("cleanup_worktree", &json!({"branch": "done-work"}), me.id).unwrap();
        assert_eq!(out["removed"], made["path"]);
        assert_eq!(out["sessions_deleted"], json!([chat.id.to_string()]));
        assert_eq!(out["branch_deleted"], true);
        assert!(!path.exists());
        assert!(s.session(chat.id).is_err());
        assert!(!crate::git::branch_exists(&main, "done-work"));
        assert_eq!(s.worktrees(Some(p.id)).unwrap().len(), 1);
    }

    #[test]
    fn an_unmerged_branch_is_kept_when_its_worktree_is_cleaned_up() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let s = Store::memory().unwrap();
        let p = crate::projects::add(&s, &main, None, false).unwrap();
        let me =
            s.add_session(s.main_worktree(p.id).unwrap().id, "native", "me", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        let made = api.call("create_worktree", &json!({"branch": "wip"}), me.id).unwrap();
        let path = made["path"].as_str().unwrap();
        let commit = std::process::Command::new("git")
            .args(["-C", path, "-c", "user.name=t", "-c", "user.email=t@t"])
            .args(["commit", "-q", "--allow-empty", "-m", "wip"])
            .status()
            .unwrap();
        assert!(commit.success());
        let out = api.call("cleanup_worktree", &json!({"id": made["id"]}), me.id).unwrap();
        assert_eq!(out["branch_deleted"], false);
        assert!(out["branch_kept"].as_str().unwrap().contains("not fully merged"), "{out}");
        assert!(crate::git::branch_exists(&main, "wip"));
    }
}
