use crate::agent::Api;
use crate::store::Store;
use foreman_core::{Entry, Id, IssuePatch, IssueState, NewComment, NewIssue};
use serde_json::{Value, json};

pub struct StoreApi {
    store: Store,
    changed: Option<std::sync::Arc<tokio::sync::Notify>>,
}

impl StoreApi {
    #[cfg(test)]
    pub fn new(store: Store) -> StoreApi {
        StoreApi { store, changed: None }
    }

    pub fn notifying(store: Store, changed: std::sync::Arc<tokio::sync::Notify>) -> StoreApi {
        StoreApi { store, changed: Some(changed) }
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

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

fn state(args: &Value) -> Result<Option<IssueState>, String> {
    match text(args, "state") {
        None => Ok(None),
        Some(s) => IssueState::parse(s.trim())
            .map(Some)
            .ok_or_else(|| format!("{s} is not a state: backlog, todo, doing, review or done")),
    }
}

impl Api for StoreApi {
    fn call(&self, tool: &str, args: &Value, session: Id) -> Result<Value, String> {
        let s = &self.store;
        let me = s.session(session).map_err(|e| e.0)?;
        let project = me.project_id;
        let own = |issue: Id| -> Result<foreman_core::Issue, String> {
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
                    .map(|i| json!({ "id": i.id, "title": i.title, "state": i.state, "body": i.body, "labels": i.labels }))
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
                        ..Default::default()
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
                    ..Default::default()
                };
                let i = s.update_issue(issue, &patch).map_err(|e| e.0)?;
                json!({ "id": i.id, "title": i.title, "state": i.state })
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
                let other = s.session(id(args)?).map_err(|e| e.0)?;
                if other.project_id != project {
                    return Err("that session belongs to another project".into());
                }
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
            "set_model" => {
                let model = text(args, "model").filter(|m| !m.trim().is_empty());
                s.set_model(session, model.as_deref().map(str::trim)).map_err(|e| e.0)?;
                self.touched();
                json!({ "model": model })
            }
            other => return Err(format!("no foreman tool called {other}")),
        };
        if matches!(tool, "file_issue" | "update_issue" | "comment_on_issue") {
            self.touched();
        }
        Ok(out)
    }
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
        let me = s.add_session(main.id, "foreman", "me", None, None).unwrap();
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
        assert_eq!(s.comments(fid).unwrap()[0].author, format!("foreman (session {})", me.id));

        let sessions = api.call("list_sessions", &json!({}), me.id).unwrap();
        assert_eq!(sessions.as_array().unwrap().len(), 1);
        assert_eq!(sessions[0]["you"], true);
        assert!(api.call("read_session", &json!({"id": elsewhere.id}), me.id).is_err());
    }
}
