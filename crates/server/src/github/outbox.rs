use crate::store::{NewOutbox, OutboxItem, Store};
use agentbench_core::{Id, PostMode};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const PAUSED: &str = "github_paused";
pub const MAX_ATTEMPTS: i64 = 5;
pub const BACKOFF_MS: i64 = 30_000;

pub const KINDS: [&str; 7] = ["comment", "label_add", "label_remove", "assignee", "issue_state", "review", "merge_request"];

static HOST_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s`(\[])(?:/home/|/Users/|/root/|/mnt/|/tmp/|~/)[^\s`)\]]+").unwrap());

pub fn is_paused(store: &Store) -> bool {
    store.setting(PAUSED).ok().flatten().is_some_and(|v| v == "1")
}

pub fn set_paused(store: &Store, paused: bool) -> Result<(), String> {
    store.set_setting(PAUSED, if paused { "1" } else { "0" }).map_err(|e| e.0)
}

pub fn refuse_unsafe(text: &str) -> Result<(), String> {
    if crate::compact::scrub(text).1 > 0 {
        return Err("refused: the text looks like it contains a secret".into());
    }
    if HOST_PATH.is_match(text) {
        return Err("refused: the text contains a file path from this machine".into());
    }
    Ok(())
}

pub struct Ask {
    pub project: Id,
    pub kind: String,
    pub number: i64,
    pub text: String,
    pub params: Value,
    pub asked_by: String,
    pub trusted: bool,
}

fn validate(ask: &Ask) -> Result<(), String> {
    if !KINDS.contains(&ask.kind.as_str()) {
        return Err(format!("unknown kind {}; the kinds are {}", ask.kind, KINDS.join(", ")));
    }
    if ask.number <= 0 {
        return Err("a post needs the issue or pull request `number`".into());
    }
    let need = |key: &str| {
        ask.params.get(key).and_then(Value::as_str).filter(|v| !v.trim().is_empty()).map(|_| ()).ok_or(format!("`{key}` is needed for {}", ask.kind))
    };
    match ask.kind.as_str() {
        "comment" if ask.text.trim().is_empty() => Err("a comment needs text".into()),
        "label_add" | "label_remove" => need("label"),
        "assignee" => need("login"),
        "issue_state" => {
            let state = ask.params.get("state").and_then(Value::as_str).unwrap_or_default();
            if matches!(state, "closed" | "open") { Ok(()) } else { Err("`state` is closed or open".into()) }
        }
        "review" => {
            let event = ask.params.get("event").and_then(Value::as_str).unwrap_or_default();
            if !matches!(event, "comment" | "approve" | "request_changes") {
                return Err("`event` is comment, approve or request_changes".into());
            }
            if event != "approve" && ask.text.trim().is_empty() && ask.params.get("comments").and_then(Value::as_array).is_none_or(|c| c.is_empty()) {
                return Err("a review needs text or line comments".into());
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn texts(ask: &Ask) -> Vec<String> {
    let mut all = vec![ask.text.clone()];
    if let Some(list) = ask.params.get("comments").and_then(Value::as_array) {
        all.extend(list.iter().filter_map(|c| c.get("body").and_then(Value::as_str)).map(str::to_string));
    }
    all.extend(ask.params.values_of(&["label", "login", "reason"]));
    all
}

trait Values {
    fn values_of(&self, keys: &[&str]) -> Vec<String>;
}

impl Values for Value {
    fn values_of(&self, keys: &[&str]) -> Vec<String> {
        keys.iter().filter_map(|k| self.get(*k).and_then(Value::as_str)).map(str::to_string).collect()
    }
}

pub fn propose(store: &Store, ask: Ask) -> Result<(Id, String), String> {
    let project = store.project(ask.project).map_err(|e| e.0)?;
    if !project.tracker.is_github() {
        return Err("this project does not use GitHub".into());
    }
    validate(&ask)?;
    for t in texts(&ask) {
        refuse_unsafe(&t)?;
    }
    let status = match project.post_mode {
        PostMode::Off => "local",
        PostMode::Auto if ask.trusted && ask.kind != "merge_request" => "approved",
        _ => "proposed",
    };
    let payload = ask.params.to_string();
    let id = store
        .outbox_insert(&NewOutbox {
            project_id: ask.project,
            kind: &ask.kind,
            payload: &payload,
            status,
            asked_by: &ask.asked_by,
            trusted: ask.trusted,
            number: ask.number,
            text: &ask.text,
        })
        .map_err(|e| e.0)?;
    Ok((id, status.to_string()))
}

pub fn approve(store: &Store, id: Id, edited: Option<&str>) -> Result<(), String> {
    let item = store.outbox_item(id).map_err(|e| e.0)?;
    if let Some(t) = edited {
        refuse_unsafe(t)?;
    }
    let ok = store.outbox_transition(id, &["proposed", "failed"], "approved", edited).map_err(|e| e.0)?;
    if !ok {
        return Err(format!("row {id} is {}, only proposed or failed rows are approved", item.status));
    }
    store.outbox_result(id, "approved", None, None, 0, 0).map_err(|e| e.0)
}

pub fn approve_all(store: &Store, project: Id) -> Result<Vec<Id>, String> {
    let mut done = Vec::new();
    for item in store.outbox(Some(project)).map_err(|e| e.0)?.into_iter().filter(|i| i.status == "proposed") {
        approve(store, item.id, None)?;
        done.push(item.id);
    }
    Ok(done)
}

pub fn reject(store: &Store, id: Id) -> Result<(), String> {
    let item = store.outbox_item(id).map_err(|e| e.0)?;
    if store.outbox_transition(id, &["proposed", "approved", "failed", "local"], "rejected", None).map_err(|e| e.0)? {
        Ok(())
    } else {
        Err(format!("row {id} is {} and cannot be rejected", item.status))
    }
}

fn footer(repo: &str) -> String {
    let owner = repo.split('/').next().unwrap_or(repo);
    format!("\n\n---\nposted by agentbench for {owner}")
}

fn temp_file(id: Id, body: &Value) -> Result<std::path::PathBuf, String> {
    let path = std::env::temp_dir().join(format!("agentbench-post-{}-{id}.json", std::process::id()));
    std::fs::write(&path, body.to_string()).map_err(|e| e.to_string())?;
    Ok(path)
}

enum Sent {
    Done(Option<String>),
    Retry(String),
    Fail(String),
}

fn classify(stderr: &str) -> Sent {
    let msg = stderr.trim().to_string();
    let code = Regex::new(r"HTTP (\d{3})").unwrap().captures(&msg).and_then(|c| c[1].parse::<u16>().ok());
    match code {
        Some(c) if c >= 500 => Sent::Retry(msg),
        Some(_) => Sent::Fail(msg),
        None if msg.contains("rate limit") => Sent::Retry(msg),
        None => Sent::Retry(msg),
    }
}

fn run(gh: &str, args: &[&str]) -> Sent {
    let r = super::sync::call(gh, args);
    if !r.ok {
        return classify(&r.stderr);
    }
    let id = serde_json::from_str::<Value>(&r.stdout).ok().and_then(|v| v.get("id").map(|i| i.to_string().trim_matches('"').to_string()));
    Sent::Done(id)
}

fn send(gh: &str, repo: &str, item: &OutboxItem) -> Sent {
    let n = item.number.to_string();
    let params: Value = serde_json::from_str(&item.payload).unwrap_or(Value::Null);
    let text_of = |k: &str| params.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    match item.kind.as_str() {
        "comment" => {
            let file = match temp_file(item.id, &json!({"body": format!("{}{}", item.text, footer(repo))})) {
                Ok(f) => f,
                Err(e) => return Sent::Fail(e),
            };
            let path = file.to_string_lossy().into_owned();
            let url = format!("repos/{repo}/issues/{n}/comments");
            let out = run(gh, &["api", "--method", "POST", &url, "--input", &path]);
            let _ = std::fs::remove_file(file);
            out
        }
        "label_add" => run(gh, &["issue", "edit", &n, "-R", repo, "--add-label", &text_of("label")]),
        "label_remove" => run(gh, &["issue", "edit", &n, "-R", repo, "--remove-label", &text_of("label")]),
        "assignee" => {
            let flag = if params.get("remove").and_then(Value::as_bool).unwrap_or(false) { "--remove-assignee" } else { "--add-assignee" };
            run(gh, &["issue", "edit", &n, "-R", repo, flag, &text_of("login")])
        }
        "issue_state" => {
            if text_of("state") == "open" {
                run(gh, &["issue", "reopen", &n, "-R", repo])
            } else {
                let reason = if text_of("reason") == "not_planned" { "not planned" } else { "completed" };
                run(gh, &["issue", "close", &n, "-R", repo, "--reason", reason])
            }
        }
        "review" => {
            let event = match text_of("event").as_str() {
                "approve" => "APPROVE",
                "request_changes" => "REQUEST_CHANGES",
                _ => "COMMENT",
            };
            let mut body = json!({"event": event});
            if !item.text.trim().is_empty() {
                body["body"] = json!(format!("{}{}", item.text, footer(repo)));
            }
            if let Some(list) = params.get("comments").and_then(Value::as_array).filter(|l| !l.is_empty()) {
                body["comments"] = json!(list
                    .iter()
                    .map(|c| json!({"path": c["path"], "line": c["line"], "side": c.get("side").cloned().unwrap_or(json!("RIGHT")), "body": c["body"]}))
                    .collect::<Vec<_>>());
            }
            let file = match temp_file(item.id, &body) {
                Ok(f) => f,
                Err(e) => return Sent::Fail(e),
            };
            let path = file.to_string_lossy().into_owned();
            let url = format!("repos/{repo}/pulls/{n}/reviews");
            let out = run(gh, &["api", "--method", "POST", &url, "--input", &path]);
            let _ = std::fs::remove_file(file);
            out
        }
        other => Sent::Fail(format!("{other} is not posted by the drain")),
    }
}

pub fn drain(store: &Store, gh: &str, now: i64) -> usize {
    if is_paused(store) {
        return 0;
    }
    let mut posted = 0;
    for item in store.outbox_ready(now).unwrap_or_default() {
        if item.kind == "merge_request" {
            continue;
        }
        let Ok(project) = store.project(item.project_id) else { continue };
        if project.post_mode == PostMode::Off {
            continue;
        }
        let Some(repo) = super::project_repo(&project) else {
            let _ = store.outbox_result(item.id, "failed", None, Some("no GitHub repo for this project"), item.attempts, 0);
            continue;
        };
        let unsafe_text = std::iter::once(item.text.clone()).chain(serde_json::from_str::<Value>(&item.payload).ok().iter().flat_map(|p| {
            p.get("comments").and_then(Value::as_array).into_iter().flatten().filter_map(|c| c.get("body").and_then(Value::as_str)).map(str::to_string).collect::<Vec<_>>()
        })).find_map(|t| refuse_unsafe(&t).err());
        if let Some(why) = unsafe_text {
            let _ = store.outbox_result(item.id, "failed", None, Some(&why), item.attempts, 0);
            continue;
        }
        match store.outbox_transition(item.id, &["approved"], "posting", None) {
            Ok(true) => {}
            _ => continue,
        }
        let attempts = item.attempts + 1;
        match send(gh, &repo, &item) {
            Sent::Done(external) => {
                let _ = store.outbox_result(item.id, "done", external.as_deref(), None, attempts, 0);
                posted += 1;
            }
            Sent::Retry(e) if attempts < MAX_ATTEMPTS => {
                let wait = BACKOFF_MS * (1 << (attempts - 1));
                let _ = store.outbox_result(item.id, "approved", None, Some(&e), attempts, now + wait);
            }
            Sent::Retry(e) | Sent::Fail(e) => {
                let _ = store.outbox_result(item.id, "failed", None, Some(&e), attempts, 0);
            }
        }
    }
    posted
}
