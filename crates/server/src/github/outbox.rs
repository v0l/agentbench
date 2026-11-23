use crate::store::{NewOutbox, OutboxItem, Store};
use agentbench_core::{Id, PostMode};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;

pub const PAUSED: &str = "github_paused";
pub const MAX_ATTEMPTS: i64 = 5;
pub const BACKOFF_MS: i64 = 30_000;

pub const KINDS: [&str; 8] = ["comment", "label_add", "label_remove", "assignee", "issue_state", "review", "merge_request", "issue_create"];

static UUID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b").unwrap());

pub fn sanitize(text: &str) -> String {
    let text = crate::compact::scrub(text).0;
    let text = HOST_PATH.replace_all(&text, " (path)");
    UUID.replace_all(&text, "(id)").into_owned()
}

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
    if ask.number <= 0 && ask.kind != "issue_create" {
        return Err("a post needs the issue or pull request `number`".into());
    }
    let need = |key: &str| {
        ask.params.get(key).and_then(Value::as_str).filter(|v| !v.trim().is_empty()).map(|_| ()).ok_or(format!("`{key}` is needed for {}", ask.kind))
    };
    match ask.kind.as_str() {
        "comment" if ask.text.trim().is_empty() => Err("a comment needs text".into()),
        "label_add" | "label_remove" => need("label"),
        "assignee" => need("login"),
        "issue_create" => need("title"),
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
    all.extend(ask.params.values_of(&["label", "login", "reason", "title"]));
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

pub fn review_event(params: &Value) -> &str {
    params.get("event").and_then(Value::as_str).unwrap_or("comment")
}

fn needs_owner(kind: &str, params: &Value) -> bool {
    kind == "review" && review_event(params) != "comment"
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
        PostMode::Auto if ask.trusted && ask.kind != "merge_request" && !needs_owner(&ask.kind, &ask.params) => "approved",
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

pub fn approve(store: &Store, id: Id, edited: Option<&str>, by: &str) -> Result<(), String> {
    let item = store.outbox_item(id).map_err(|e| e.0)?;
    if let Some(t) = edited {
        refuse_unsafe(t)?;
    }
    let ok = store.outbox_transition(id, &["proposed", "failed"], "approved", edited).map_err(|e| e.0)?;
    if !ok {
        return Err(format!("row {id} is {}, only proposed or failed rows are approved", item.status));
    }
    store.outbox_result(id, "approved", None, None, 0, 0).map_err(|e| e.0)?;
    store.outbox_set_approver(id, by).map_err(|e| e.0)
}

pub fn approve_all(store: &Store, project: Id, by: &str) -> Result<Vec<Id>, String> {
    let mut done = Vec::new();
    for item in store.outbox(Some(project)).map_err(|e| e.0)?.into_iter().filter(|i| i.status == "proposed") {
        approve(store, item.id, None, by)?;
        done.push(item.id);
    }
    Ok(done)
}

pub fn linked(store: &Store, project: Option<Id>) -> crate::store::Result<Vec<OutboxItem>> {
    let mut repos = std::collections::HashMap::new();
    let mut items = store.outbox(project)?;
    for item in &mut items {
        let repo = repos
            .entry(item.project_id)
            .or_insert_with(|| store.project(item.project_id).ok().and_then(|p| super::project_repo(&p)).unwrap_or_default());
        item.url = item.link(repo);
    }
    Ok(items)
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
    OutboxItem::footer(repo)
}

fn temp_file(id: Id, body: &Value) -> Result<std::path::PathBuf, String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("agentbench-post-{}-{id}-{seq}.json", std::process::id()));
    std::fs::write(&path, body.to_string()).map_err(|e| e.to_string())?;
    Ok(path)
}

enum Sent {
    Done(Option<String>),
    Retry(String),
    Fail(String),
}

fn api_detail(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    let mut parts = Vec::new();
    if let Some(m) = v.get("message").and_then(Value::as_str) {
        parts.push(m.to_string());
    }
    for e in v.get("errors").and_then(Value::as_array).into_iter().flatten() {
        match e {
            Value::String(s) => parts.push(s.clone()),
            other => parts.push(other.to_string()),
        }
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

fn failure(r: &super::sync::Reply) -> Sent {
    match api_detail(&r.stdout) {
        Some(d) => classify(&format!("{}: {d}", r.stderr.trim())),
        None => classify(&r.stderr),
    }
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
        return failure(&r);
    }
    let id = serde_json::from_str::<Value>(&r.stdout).ok().and_then(|v| v.get("id").map(|i| i.to_string().trim_matches('"').to_string()));
    Sent::Done(id)
}

fn login_of(stdout: &str, pointer: &str) -> Option<String> {
    serde_json::from_str::<Value>(stdout).ok()?.pointer(pointer)?.as_str().map(str::to_string)
}

fn is_own_pull_request(gh: &str, repo: &str, number: &str) -> bool {
    let me = super::sync::call(gh, &["api", "user"]);
    let pr = super::sync::call(gh, &["api", &format!("repos/{repo}/pulls/{number}")]);
    match (login_of(&me.stdout, "/login"), login_of(&pr.stdout, "/user/login")) {
        (Some(a), Some(b)) => me.ok && pr.ok && a.eq_ignore_ascii_case(&b),
        _ => false,
    }
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
            let own = event != "COMMENT" && is_own_pull_request(gh, repo, &n);
            let event = if own { "COMMENT" } else { event };
            let mut body = json!({"event": event});
            if own {
                let verdict = if text_of("event") == "approve" { "approves" } else { "asks for changes on" };
                body["body"] = json!(format!("agentbench reviewed this and {verdict} it. GitHub does not allow that verdict on your own pull request, so it is posted as a comment.\n\n{}{}", item.text, footer(repo)));
            } else if !item.text.trim().is_empty() {
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
        "issue_create" => {
            let body = json!({"title": text_of("title"), "body": format!("{}{}", item.text, footer(repo)), "labels": ["agentbench"]});
            let file = match temp_file(item.id, &body) {
                Ok(f) => f,
                Err(e) => return Sent::Fail(e),
            };
            let path = file.to_string_lossy().into_owned();
            let url = format!("repos/{repo}/issues");
            let r = super::sync::call(gh, &["api", "--method", "POST", &url, "--input", &path]);
            let _ = std::fs::remove_file(file);
            if !r.ok {
                return failure(&r);
            }
            let number = serde_json::from_str::<Value>(&r.stdout).ok().and_then(|v| v.get("number").and_then(Value::as_i64));
            match number {
                Some(n) => Sent::Done(Some(n.to_string())),
                None => Sent::Fail("gh created the issue but returned no number".into()),
            }
        }
        other => Sent::Fail(format!("{other} is not posted by the drain")),
    }
}

pub const HOURLY_CAP: i64 = 30;

fn allowed_to_send(store: &Store, item: &OutboxItem, now: i64) -> Result<(), String> {
    if item.approved_by.is_empty() {
        let target = store.target_trust(item.project_id, item.number).map_err(|e| e.0)?;
        if !item.trusted || target.is_untrusted() {
            return Err("held: not approved by the owner and the input is untrusted".into());
        }
        if item.kind == "merge_request" {
            return Err("held: a merge is never posted without approval".into());
        }
        if needs_owner(&item.kind, &serde_json::from_str(&item.payload).unwrap_or(Value::Null)) {
            return Err("held: only the owner approves or requests changes on a pull request".into());
        }
    }
    let recent = store
        .outbox(Some(item.project_id))
        .map_err(|e| e.0)?
        .iter()
        .filter(|o| o.status == "done" && now - o.updated_at < 3_600_000)
        .count() as i64;
    if recent >= HOURLY_CAP {
        return Err("held: the hourly post cap is reached".into());
    }
    Ok(())
}

fn link_created(store: &Store, repo: &str, item: &OutboxItem, external: Option<&str>) {
    let params: Value = serde_json::from_str(&item.payload).unwrap_or(Value::Null);
    let (Some(issue), Some(number)) = (params.get("issue_id").and_then(Value::as_i64), external.and_then(|e| e.parse::<i64>().ok())) else {
        return;
    };
    let url = format!("https://github.com/{repo}/issues/{number}");
    let _ = store.link_external(issue, number, &url, "agentbench");
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
        if let Err(why) = allowed_to_send(store, &item, now) {
            let status = if why.contains("cap") { "approved" } else { "proposed" };
            let next = if status == "approved" { now + 600_000 } else { 0 };
            let _ = store.outbox_result(item.id, status, None, Some(&why), item.attempts, next);
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
                if item.kind == "issue_create" {
                    link_created(store, &repo, &item, external.as_deref());
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use agentbench_core::{ProjectPatch, Tracker};

    struct Stub {
        dir: tempfile::TempDir,
        gh: String,
    }

    impl Stub {
        fn new() -> Stub {
            let dir = tempfile::tempdir().unwrap();
            let gh = dir.path().join("gh");
            let script = r#"#!/bin/sh
D="$(dirname "$0")"
echo "$@" >> "$D/calls"
prev=""
for a in "$@"; do
  if [ "$prev" = "--input" ]; then cat "$a" >> "$D/bodies"; echo >> "$D/bodies"; fi
  prev="$a"
done
if [ -f "$D/failures" ] && [ "$(cat "$D/failures")" -gt 0 ]; then
  echo $(( $(cat "$D/failures") - 1 )) > "$D/failures"
  cat "$D/msg" >&2
  exit 1
fi
case "$*" in
  "api user") echo '{"login": "me"}'; exit 0 ;;
  *pulls/7) if [ -f "$D/author" ]; then echo "{\"user\": {\"login\": \"$(cat "$D/author")\"}}"; else echo '{"user": {"login": "other"}}'; fi; exit 0 ;;
  *pulls/9/reviews*) if [ -f "$D/own" ]; then echo '{"message":"Unprocessable Entity","errors":["Can not approve your own pull request"]}'; echo "gh: Unprocessable Entity (HTTP 422)" >&2; exit 1; fi ;;
esac
echo '{"id": 4242, "number": 4242}'
"#;
            crate::testing_stub::write_script(&gh, script);
            Stub { gh: gh.to_string_lossy().into_owned(), dir }
        }

        fn fail(&self, times: u32, msg: &str) {
            std::fs::write(self.dir.path().join("failures"), times.to_string()).unwrap();
            std::fs::write(self.dir.path().join("msg"), msg).unwrap();
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls")).map(|c| c.lines().map(str::to_string).collect()).unwrap_or_default()
        }

        fn bodies(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("bodies")).unwrap_or_default()
        }
    }

    fn setup(mode: PostMode) -> (Store, Id, Stub) {
        let store = Store::memory().unwrap();
        let p = store.add_project("demo", "/tmp/demo-outbox", Some("main"), None).unwrap();
        store
            .update_project(p.id, &ProjectPatch { tracker: Some(Tracker::Github { repo: "o/r".into() }), post_mode: Some(mode), ..Default::default() })
            .unwrap();
        (store, p.id, Stub::new())
    }

    fn comment(project: Id, text: &str, trusted: bool) -> Ask {
        Ask { project, kind: "comment".into(), number: 7, text: text.into(), params: json!({}), asked_by: "session 1".into(), trusted }
    }

    #[test]
    fn an_issue_created_through_the_outbox_carries_the_agentbench_label_and_is_linked() {
        let (store, p, stub) = setup(PostMode::Auto);
        let local = store.add_issue(&agentbench_core::NewIssue { project_id: p, title: "follow up".into(), ..Default::default() }).unwrap();
        let ask = Ask { project: p, kind: "issue_create".into(), number: 0, text: "details".into(), params: json!({"title": "follow up", "issue_id": local.id}), asked_by: "session 1".into(), trusted: true };
        let (_, status) = propose(&store, ask).unwrap();
        assert_eq!(status, "approved");
        assert_eq!(drain(&store, &stub.gh, 1_000), 1);
        assert!(stub.bodies().contains("\"labels\":[\"agentbench\"]"), "{}", stub.bodies());
        let linked = store.issue(local.id).unwrap();
        assert_eq!((linked.source, linked.external_number), (agentbench_core::IssueSource::Github, Some(4242)));
    }

    #[test]
    fn draft_mode_posts_nothing_until_approved() {
        let (store, p, stub) = setup(PostMode::Draft);
        let (id, status) = propose(&store, comment(p, "hello", true)).unwrap();
        assert_eq!(status, "proposed");
        assert_eq!(drain(&store, &stub.gh, 1_000), 0);
        assert!(stub.calls().is_empty());
        approve(&store, id, None, "user").unwrap();
        assert_eq!(drain(&store, &stub.gh, 2_000), 1);
        assert_eq!(stub.calls().len(), 1);
    }

    #[test]
    fn approval_posts_exactly_the_edited_text_once() {
        let (store, p, stub) = setup(PostMode::Draft);
        let (id, _) = propose(&store, comment(p, "first draft", true)).unwrap();
        approve(&store, id, Some("final words"), "user").unwrap();
        drain(&store, &stub.gh, 1_000);
        drain(&store, &stub.gh, 2_000);
        assert_eq!(stub.calls().len(), 1, "posted once");
        let bodies = stub.bodies();
        assert!(bodies.contains("final words") && !bodies.contains("first draft"), "{bodies}");
        assert!(bodies.contains("posted by agentbench for o"), "{bodies}");
        let row = store.outbox_item(id).unwrap();
        assert_eq!((row.status.as_str(), row.external_id.as_deref(), row.text.as_str()), ("done", Some("4242"), "final words"));
        assert!(approve(&store, id, None, "user").is_err(), "a done row is not approved again");
    }

    #[test]
    fn auto_mode_posts_a_trusted_comment_and_holds_an_untrusted_one() {
        let (store, p, stub) = setup(PostMode::Auto);
        let (trusted, s1) = propose(&store, comment(p, "from trusted work", true)).unwrap();
        let (untrusted, s2) = propose(&store, comment(p, "from a stranger's issue", false)).unwrap();
        assert_eq!((s1.as_str(), s2.as_str()), ("approved", "proposed"));
        let merge = Ask { kind: "merge_request".into(), ..comment(p, "", true) };
        assert_eq!(propose(&store, merge).unwrap().1, "proposed", "never a merge");
        drain(&store, &stub.gh, 1_000);
        assert_eq!(store.outbox_item(trusted).unwrap().status, "done");
        assert_eq!(store.outbox_item(untrusted).unwrap().status, "proposed");
        assert_eq!(stub.calls().len(), 1);
    }

    #[test]
    fn a_row_approved_by_nobody_for_untrusted_input_is_held_at_send() {
        let (store, p, stub) = setup(PostMode::Auto);
        let (id, _) = propose(&store, comment(p, "x", false)).unwrap();
        store.outbox_transition(id, &["proposed"], "approved", None).unwrap();
        drain(&store, &stub.gh, 1_000);
        assert!(stub.calls().is_empty());
        assert_eq!(store.outbox_item(id).unwrap().status, "proposed");
    }

    #[test]
    fn off_mode_keeps_rows_as_local_notes() {
        let (store, p, stub) = setup(PostMode::Off);
        let (id, status) = propose(&store, comment(p, "note", true)).unwrap();
        assert_eq!(status, "local");
        assert!(approve(&store, id, None, "user").is_err());
        drain(&store, &stub.gh, 1_000);
        assert!(stub.calls().is_empty());
    }

    #[test]
    fn pause_stops_the_drain_and_resume_lets_it_go() {
        let (store, p, stub) = setup(PostMode::Draft);
        let (id, _) = propose(&store, comment(p, "hello", true)).unwrap();
        approve(&store, id, None, "user").unwrap();
        set_paused(&store, true).unwrap();
        assert_eq!(drain(&store, &stub.gh, 1_000), 0);
        assert!(stub.calls().is_empty());
        set_paused(&store, false).unwrap();
        assert_eq!(drain(&store, &stub.gh, 2_000), 1);
    }

    #[test]
    fn a_502_retries_with_backoff_and_then_posts() {
        let (store, p, stub) = setup(PostMode::Draft);
        stub.fail(1, "gh: Bad Gateway (HTTP 502)");
        let (id, _) = propose(&store, comment(p, "hello", true)).unwrap();
        approve(&store, id, None, "user").unwrap();
        assert_eq!(drain(&store, &stub.gh, 1_000), 0);
        let row = store.outbox_item(id).unwrap();
        assert_eq!((row.status.as_str(), row.attempts), ("approved", 1));
        assert!(row.error.unwrap().contains("502"));
        assert_eq!(drain(&store, &stub.gh, 1_001), 0, "not before the backoff");
        assert_eq!(stub.calls().len(), 1);
        assert_eq!(drain(&store, &stub.gh, 1_000 + BACKOFF_MS + 1), 1);
        assert_eq!(store.outbox_item(id).unwrap().status, "done");
    }

    #[test]
    fn a_422_fails_visibly_and_is_not_retried() {
        let (store, p, stub) = setup(PostMode::Draft);
        stub.fail(5, "gh: Validation Failed (HTTP 422)");
        let (id, _) = propose(&store, comment(p, "hello", true)).unwrap();
        approve(&store, id, None, "user").unwrap();
        drain(&store, &stub.gh, 1_000);
        drain(&store, &stub.gh, 10_000_000);
        let row = store.outbox_item(id).unwrap();
        assert_eq!(row.status, "failed");
        assert!(row.error.unwrap().contains("422"));
        assert_eq!(stub.calls().len(), 1);
    }

    #[test]
    fn the_secret_filter_refuses_a_token_in_the_body() {
        let (store, p, stub) = setup(PostMode::Auto);
        let err = propose(&store, comment(p, "use ghp_abcdefghijklmnopqrstuvwxyz0123 to log in", true)).unwrap_err();
        assert!(err.contains("secret"), "{err}");
        let err = propose(&store, comment(p, "see /home/kieran/.ssh/config", true)).unwrap_err();
        assert!(err.contains("path"), "{err}");
        let (id, _) = propose(&store, comment(p, "fine", true)).unwrap();
        assert!(approve(&store, id, Some("password: hunter2hunter2"), "user").is_err());
        assert!(store.outbox(Some(p)).unwrap().iter().all(|o| !o.text.contains("ghp_")));
        drain(&store, &stub.gh, 1_000);
        assert_eq!(stub.calls().len(), 1);
    }

    #[test]
    fn labels_state_assignee_and_reviews_use_gh() {
        let (store, p, stub) = setup(PostMode::Auto);
        let ask = |kind: &str, params: Value, text: &str| Ask { kind: kind.into(), params, ..comment(p, text, true) };
        propose(&store, ask("label_add", json!({"label": "bug"}), "")).unwrap();
        propose(&store, ask("issue_state", json!({"state": "closed", "reason": "not_planned"}), "")).unwrap();
        propose(&store, ask("assignee", json!({"login": "alice"}), "")).unwrap();
        let (review, status) = propose(&store, ask("review", json!({"event": "request_changes", "comments": [{"path": "a.rs", "line": 3, "body": "fix"}]}), "needs work")).unwrap();
        assert_eq!(status, "proposed", "request changes waits for the owner even in auto mode");
        assert!(propose(&store, ask("label_add", json!({}), "")).is_err());
        assert_eq!(drain(&store, &stub.gh, 1_000), 3);
        approve(&store, review, None, "user").unwrap();
        assert_eq!(drain(&store, &stub.gh, 2_000), 1);
        let calls = stub.calls().join("\n");
        assert!(calls.contains("issue edit 7 -R o/r --add-label bug"), "{calls}");
        assert!(calls.contains("issue close 7 -R o/r --reason not planned"), "{calls}");
        assert!(calls.contains("--add-assignee alice"), "{calls}");
        assert!(calls.contains("pulls/7/reviews"), "{calls}");
        let bodies = stub.bodies();
        assert!(bodies.contains("REQUEST_CHANGES") && bodies.contains("a.rs"), "{bodies}");
    }

    #[test]
    fn a_review_on_the_owners_own_pull_request_is_posted_as_a_comment() {
        let (store, p, stub) = setup(PostMode::Auto);
        std::fs::write(stub.dir.path().join("author"), "me").unwrap();
        let ask = Ask { kind: "review".into(), params: json!({"event": "request_changes", "comments": [{"path": "a.rs", "line": 3, "body": "fix"}]}), ..comment(p, "needs work", true) };
        let (review, _) = propose(&store, ask).unwrap();
        approve(&store, review, None, "user").unwrap();
        assert_eq!(drain(&store, &stub.gh, 1_000), 1);
        let bodies = stub.bodies();
        assert!(bodies.contains("\"event\":\"COMMENT\"") && !bodies.contains("REQUEST_CHANGES"), "{bodies}");
        assert!(bodies.contains("agentbench reviewed this"), "{bodies}");
    }

    #[test]
    fn a_422_row_error_carries_the_github_message() {
        let (store, p, stub) = setup(PostMode::Auto);
        std::fs::write(stub.dir.path().join("own"), "1").unwrap();
        let ask = Ask { kind: "review".into(), number: 9, params: json!({"event": "comment"}), ..comment(p, "hello", true) };
        let (review, _) = propose(&store, ask).unwrap();
        drain(&store, &stub.gh, 1_000);
        let row = store.outbox_item(review).unwrap();
        assert_eq!(row.status, "failed");
        let err = row.error.unwrap_or_default();
        assert!(err.contains("HTTP 422") && err.contains("Can not approve your own pull request"), "{err}");
    }
}
