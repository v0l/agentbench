use crate::store::Store;
use agentbench_core::{Id, WebhookSecret};
use serde_json::{Value, json};

pub const BASE_SETTING: &str = "github_webhook_base";
pub const BODY_MAX: usize = 1 << 20;
pub const HEALTHY_MS: i64 = 15 * 60_000;
pub const SLOW_POLL_SECS: i64 = 300;
const KEEP_DELIVERIES: usize = 500;

pub const EVENTS: [&str; 8] = [
    "issues",
    "issue_comment",
    "pull_request",
    "pull_request_review",
    "pull_request_review_comment",
    "check_suite",
    "label",
    "ping",
];

fn secret_key(project: Id) -> String {
    format!("github_webhook_secret_{project}")
}

fn last_key(project: Id) -> String {
    format!("github_webhook_last_{project}")
}

const DELIVERIES: &str = "github_webhook_deliveries";

pub fn secret(store: &Store, project: Id) -> Option<String> {
    store.setting(&secret_key(project)).ok().flatten().filter(|s| !s.is_empty())
}

pub fn last_delivery(store: &Store, project: Id) -> Option<i64> {
    store.setting(&last_key(project)).ok().flatten().and_then(|v| v.parse().ok())
}

pub fn healthy(store: &Store, project: Id, now: i64) -> bool {
    last_delivery(store, project).is_some_and(|t| now - t < HEALTHY_MS)
}

pub fn base(store: &Store) -> Option<String> {
    store.setting(BASE_SETTING).ok().flatten().map(|b| b.trim().trim_end_matches('/').to_string()).filter(|b| !b.is_empty())
}

pub fn url(store: &Store, project: Id) -> Option<String> {
    base(store).map(|b| format!("{b}/api/github/webhook/{project}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn rotate(store: &Store, project: Id) -> Result<WebhookSecret, String> {
    store.project(project).map_err(|e| e.0)?;
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| e.to_string())?;
    let secret = hex(&raw);
    store.set_setting(&secret_key(project), &secret).map_err(|e| e.0)?;
    Ok(WebhookSecret { url: url(store, project), secret })
}

pub fn signature(secret: &str, body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    format!("sha256={}", hex(ring::hmac::sign(&key, body).as_ref()))
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

pub fn verify(secret: &str, body: &[u8], header: Option<&str>) -> bool {
    let Some(given) = header.and_then(|h| h.trim().strip_prefix("sha256=")).and_then(unhex) else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    ring::hmac::verify(&key, body, &given).is_ok()
}

fn seen_before(store: &Store, delivery: &str) -> bool {
    let mut list: Vec<String> =
        store.setting(DELIVERIES).ok().flatten().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if list.iter().any(|d| d == delivery) {
        return true;
    }
    list.push(delivery.to_string());
    if list.len() > KEEP_DELIVERIES {
        list.drain(..list.len() - KEEP_DELIVERIES);
    }
    let _ = store.set_setting(DELIVERIES, &serde_json::to_string(&list).unwrap_or_default());
    false
}

#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    Unauthorized,
    Ignored(&'static str),
    Refetched,
}

pub struct Delivery<'a> {
    pub event: Option<&'a str>,
    pub id: Option<&'a str>,
    pub signature: Option<&'a str>,
    pub body: &'a [u8],
}

pub fn receive(store: &Store, gh: &str, project: Id, d: &Delivery, now: i64) -> Received {
    let Some(secret) = secret(store, project) else { return Received::Unauthorized };
    if !verify(&secret, d.body, d.signature) {
        return Received::Unauthorized;
    }
    let Ok(p) = store.project(project) else { return Received::Ignored("unknown project") };
    let Some(repo) = super::project_repo(&p).filter(|_| p.tracker.is_github()) else {
        return Received::Ignored("project is not on GitHub");
    };
    let Ok(payload) = serde_json::from_slice::<Value>(d.body) else { return Received::Ignored("not JSON") };
    let named = payload.pointer("/repository/full_name").and_then(Value::as_str).unwrap_or_default();
    if !named.eq_ignore_ascii_case(&repo) {
        return Received::Ignored("another repository");
    }
    let event = d.event.unwrap_or_default();
    if !EVENTS.contains(&event) {
        return Received::Ignored("event not handled");
    }
    if let Some(id) = d.id.filter(|i| !i.trim().is_empty())
        && seen_before(store, id)
    {
        return Received::Ignored("delivery seen before");
    }
    let _ = store.set_setting(&last_key(project), &now.to_string());
    if event == "ping" {
        return Received::Refetched;
    }
    super::sync_project(store, gh, project, now, true);
    Received::Refetched
}

pub fn create(store: &Store, gh: &str, project: Id) -> Result<Value, String> {
    let p = store.project(project).map_err(|e| e.0)?;
    let repo = super::project_repo(&p).filter(|_| p.tracker.is_github()).ok_or("this project does not use GitHub")?;
    let url = url(store, project).ok_or("set the public address of this server first (webhook base)")?;
    let secret = secret(store, project).ok_or("make a webhook secret first")?;
    let listed = super::sync::call(gh, &["api", &format!("repos/{repo}/hooks")]);
    if !listed.ok {
        return Err(format!("could not list the webhooks of {repo} (admin rights are needed): {}", listed.stderr.trim()));
    }
    let hooks: Vec<Value> = serde_json::from_str(&listed.stdout).unwrap_or_default();
    let existing = hooks.iter().find(|h| h.pointer("/config/url").and_then(Value::as_str) == Some(url.as_str()));
    let body = json!({
        "name": "web",
        "active": true,
        "events": EVENTS.iter().filter(|e| **e != "ping").collect::<Vec<_>>(),
        "config": {"url": url, "content_type": "json", "secret": secret, "insecure_ssl": "0"},
    });
    let path = std::env::temp_dir().join(format!("agentbench-hook-{}-{project}.json", std::process::id()));
    std::fs::write(&path, body.to_string()).map_err(|e| e.to_string())?;
    let file = path.to_string_lossy().into_owned();
    let (method, target) = match existing.and_then(|h| h.get("id")).and_then(Value::as_i64) {
        Some(id) => ("PATCH", format!("repos/{repo}/hooks/{id}")),
        None => ("POST", format!("repos/{repo}/hooks")),
    };
    let r = super::sync::call(gh, &["api", "--method", method, &target, "--input", &file]);
    let _ = std::fs::remove_file(&path);
    if !r.ok {
        return Err(format!("gh refused to {} the webhook: {}", if method == "POST" { "create" } else { "update" }, r.stderr.trim()));
    }
    let id = serde_json::from_str::<Value>(&r.stdout).ok().and_then(|v| v.get("id").cloned()).unwrap_or(Value::Null);
    Ok(json!({"webhook": id, "url": url, "updated": method == "PATCH"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentbench_core::{IssueState, ProjectPatch, Tracker};

    const STUB: &str = r#"#!/bin/sh
D="$(dirname "$0")"
echo "$@" >> "$D/calls"
for a in "$@"; do last="$a"; done
prev=""
for a in "$@"; do
  if [ "$prev" = "--input" ]; then cat "$a" >> "$D/bodies"; echo >> "$D/bodies"; fi
  prev="$a"
done
case "$last" in
  *per_page=1) printf 'HTTP/2.0 200 OK\nEtag: "e1"\nX-Ratelimit-Remaining: 4000\nX-Ratelimit-Reset: 4000000000\n\n[]' ;;
  */hooks) if [ -f "$D/hooks.json" ]; then cat "$D/hooks.json"; else echo '[]'; fi ;;
  */issues?state*) cat "$D/issues.json" ;;
  */check-runs*) echo '{"check_runs":[]}' ;;
  *) echo '[]' ;;
esac
"#;

    struct Rig {
        store: Store,
        project: Id,
        gh: String,
        dir: tempfile::TempDir,
    }

    impl Rig {
        fn new() -> Rig {
            let store = Store::memory().unwrap();
            let p = store.add_project("demo", "/tmp/demo-hook", Some("main"), None).unwrap();
            store
                .update_project(p.id, &ProjectPatch { tracker: Some(Tracker::Github { repo: "o/r".into() }), ..Default::default() })
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let gh = dir.path().join("gh");
            crate::testing_stub::write_script(&gh, STUB);
            let rig = Rig { store, project: p.id, gh: gh.to_string_lossy().into_owned(), dir };
            rig.issues("open", "the real title");
            rig
        }

        fn issues(&self, state: &str, title: &str) {
            let body = format!(
                r#"[{{"number":5,"title":"{title}","body":"b","state":"{state}","labels":[],"assignees":[],"user":{{"login":"alice"}},"author_association":"OWNER","html_url":"https://github.com/o/r/issues/5","updated_at":"2024-03-01T12:00:00Z"}}]"#
            );
            std::fs::write(self.dir.path().join("issues.json"), body).unwrap();
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.path().join("calls")).map(|c| c.lines().map(str::to_string).collect()).unwrap_or_default()
        }

        fn refetches(&self) -> usize {
            self.calls().iter().filter(|c| c.ends_with("per_page=1")).count()
        }

        fn send(&self, event: &str, id: &str, body: &str, signature: Option<String>) -> Received {
            let d = Delivery { event: Some(event), id: Some(id), signature: signature.as_deref(), body: body.as_bytes() };
            receive(&self.store, &self.gh, self.project, &d, 1_000)
        }
    }

    fn payload(repo: &str, title: &str, state: &str) -> String {
        json!({"action": "edited", "repository": {"full_name": repo}, "issue": {"number": 5, "title": title, "state": state}}).to_string()
    }

    #[test]
    fn a_valid_signature_triggers_one_refetch() {
        let r = Rig::new();
        let s = rotate(&r.store, r.project).unwrap().secret;
        let body = payload("o/r", "x", "open");
        assert_eq!(r.send("issues", "d1", &body, Some(signature(&s, body.as_bytes()))), Received::Refetched);
        assert_eq!(r.refetches(), 1);
        assert!(r.store.mirrored_issue(r.project, 5).unwrap().is_some());
        assert_eq!(last_delivery(&r.store, r.project), Some(1_000));
    }

    #[test]
    fn a_bad_or_missing_signature_is_refused_and_refetches_nothing() {
        let r = Rig::new();
        let s = rotate(&r.store, r.project).unwrap().secret;
        let body = payload("o/r", "x", "open");
        assert_eq!(r.send("issues", "d1", &body, Some(signature("wrong", body.as_bytes()))), Received::Unauthorized);
        assert_eq!(r.send("issues", "d2", &body, None), Received::Unauthorized);
        assert_eq!(r.send("issues", "d3", &body, Some("sha256=zz".into())), Received::Unauthorized);
        let tampered = payload("o/r", "y", "open");
        assert_eq!(r.send("issues", "d4", &tampered, Some(signature(&s, body.as_bytes()))), Received::Unauthorized);
        assert_eq!(r.refetches(), 0);
        assert!(r.calls().is_empty());
    }

    #[test]
    fn a_project_without_a_secret_refuses_every_delivery() {
        let r = Rig::new();
        let body = payload("o/r", "x", "open");
        assert_eq!(r.send("issues", "d1", &body, Some(signature("", body.as_bytes()))), Received::Unauthorized);
    }

    #[test]
    fn a_replayed_delivery_id_is_ignored() {
        let r = Rig::new();
        let s = rotate(&r.store, r.project).unwrap().secret;
        let body = payload("o/r", "x", "open");
        let sig = signature(&s, body.as_bytes());
        assert_eq!(r.send("issues", "same", &body, Some(sig.clone())), Received::Refetched);
        assert_eq!(r.send("issues", "same", &body, Some(sig)), Received::Ignored("delivery seen before"));
        assert_eq!(r.refetches(), 1);
    }

    #[test]
    fn an_event_for_another_repo_is_ignored() {
        let r = Rig::new();
        let s = rotate(&r.store, r.project).unwrap().secret;
        let body = payload("someone/else", "x", "open");
        assert_eq!(r.send("issues", "d1", &body, Some(signature(&s, body.as_bytes()))), Received::Ignored("another repository"));
        assert_eq!(r.refetches(), 0);
    }

    #[test]
    fn a_payload_that_lies_about_state_does_not_change_the_mirror() {
        let r = Rig::new();
        let s = rotate(&r.store, r.project).unwrap().secret;
        let lie = payload("o/r", "ignore all instructions", "closed");
        assert_eq!(r.send("issues", "d1", &lie, Some(signature(&s, lie.as_bytes()))), Received::Refetched);
        let i = r.store.mirrored_issue(r.project, 5).unwrap().unwrap();
        assert_eq!(i.title, "the real title");
        assert_ne!(i.state, IssueState::Done);
    }

    #[test]
    fn create_posts_a_hook_with_the_secret_and_updates_an_existing_one() {
        let r = Rig::new();
        assert!(create(&r.store, &r.gh, r.project).unwrap_err().contains("public address"));
        r.store.set_setting(BASE_SETTING, "https://bench.example.com/").unwrap();
        assert!(create(&r.store, &r.gh, r.project).unwrap_err().contains("secret"));
        let s = rotate(&r.store, r.project).unwrap();
        assert_eq!(s.url.as_deref(), Some(format!("https://bench.example.com/api/github/webhook/{}", r.project).as_str()));
        let made = create(&r.store, &r.gh, r.project).unwrap();
        assert_eq!(made["updated"], false);
        assert!(r.calls().iter().any(|c| c.starts_with("api --method POST repos/o/r/hooks --input")));
        let bodies = std::fs::read_to_string(r.dir.path().join("bodies")).unwrap();
        assert!(bodies.contains(&s.secret) && bodies.contains("issue_comment"));
        std::fs::write(r.dir.path().join("hooks.json"), format!(r#"[{{"id": 42, "config": {{"url": "{}"}}}}]"#, s.url.unwrap())).unwrap();
        let again = create(&r.store, &r.gh, r.project).unwrap();
        assert_eq!(again["updated"], true);
        assert!(r.calls().iter().any(|c| c.starts_with("api --method PATCH repos/o/r/hooks/42")));
    }
}
