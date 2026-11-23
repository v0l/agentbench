use super::map::{self, GhIssue, Mapped, gh_issue, linked_issues, parse_time};
use crate::store::{MirrorPr, Store};
use agentbench_core::{Id, IssueState, Tracker};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::process::{Command, Stdio};

pub const POLL_SECS: i64 = 60;
const FAILURE_BACKOFF_MS: i64 = 60_000;
const MAX_BACKOFF_MS: i64 = 900_000;
const COMMENT_OVERLAP_MS: i64 = 3_600_000;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_sync: Option<i64>,
    #[serde(default)]
    pub last_attempt: Option<i64>,
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub backoff_until: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
}

pub fn state_key(project: Id) -> String {
    format!("github_sync_{project}")
}

pub fn load_state(store: &Store, project: Id) -> SyncState {
    store
        .setting(&state_key(project))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(store: &Store, project: Id, state: &SyncState) {
    if let Ok(json) = serde_json::to_string(state) {
        let _ = store.set_setting(&state_key(project), &json);
    }
    if let Some(at) = state.last_sync {
        let _ = store.set_setting(&format!("github_last_sync_{project}"), &at.to_string());
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Skipped,
    Unchanged,
    Synced,
    RateLimited(i64),
    Failed(String),
}

enum Fail {
    Limited(i64),
    Error(String),
}

pub(super) struct Reply {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

pub(super) fn call(gh: &str, args: &[&str]) -> Reply {
    match Command::new(gh).args(args).stdin(Stdio::null()).output() {
        Ok(o) => Reply {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => Reply { ok: false, stdout: String::new(), stderr: format!("gh: {e}") },
    }
}

struct Probe {
    status: u16,
    etag: Option<String>,
    remaining: Option<i64>,
    reset: Option<i64>,
}

fn parse_probe(out: &str) -> Probe {
    let head = out.split("\r\n\r\n").next().and_then(|h| (h.len() < out.len()).then_some(h)).or_else(|| out.split("\n\n").next()).unwrap_or(out);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut p = Probe { status, etag: None, remaining: None, reset: None };
    for line in lines {
        let Some((k, v)) = line.split_once(':') else { continue };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "etag" => p.etag = Some(v.to_string()),
            "x-ratelimit-remaining" => p.remaining = v.parse().ok(),
            "x-ratelimit-reset" => p.reset = v.parse::<i64>().ok().map(|s| s * 1000),
            _ => {}
        }
    }
    p
}

fn json_values(body: &str) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    for v in serde_json::Deserializer::from_str(body).into_iter::<Value>() {
        match v.map_err(|e| format!("bad JSON from gh: {e}"))? {
            Value::Array(a) => out.extend(a),
            other => out.push(other),
        }
    }
    Ok(out)
}

fn limited(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    t.contains("rate limit") || t.contains("abuse detection")
}

fn list(gh: &str, url: &str) -> Result<Vec<Value>, Fail> {
    let r = call(gh, &["api", "--paginate", url]);
    if !r.ok {
        let text = format!("{} {}", r.stderr.trim(), r.stdout.trim());
        if limited(&text) {
            return Err(Fail::Limited(0));
        }
        return Err(Fail::Error(text.trim().to_string()));
    }
    json_values(&r.stdout).map_err(Fail::Error)
}

fn iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn trailing_number(url: &str) -> Option<i64> {
    url.rsplit('/').next().and_then(|n| n.parse().ok())
}

fn pr_row(v: &Value) -> Option<MirrorPr> {
    let s = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let nested = |a: &str, b: &str| {
        v.get(a).and_then(|x| x.get(b)).and_then(Value::as_str).unwrap_or_default().to_string()
    };
    Some(MirrorPr {
        number: v.get("number")?.as_i64()?,
        title: s("title"),
        body: s("body"),
        author: nested("user", "login"),
        head_ref: nested("head", "ref"),
        base_ref: nested("base", "ref"),
        head_sha: nested("head", "sha"),
        state: "open".into(),
        draft: v.get("draft").and_then(Value::as_bool).unwrap_or(false),
        url: s("html_url"),
        created_at: parse_time(&s("created_at")),
        updated_at: parse_time(&s("updated_at")),
    })
}

struct Fetched {
    issues: Vec<GhIssue>,
    gone: Vec<i64>,
    prs: Vec<MirrorPr>,
    comments: Vec<Value>,
}

fn fetch(gh: &str, repo: &str, store: &Store, project: Id, since: Option<i64>) -> Result<Fetched, Fail> {
    let prs: Vec<MirrorPr> =
        list(gh, &format!("repos/{repo}/pulls?state=open&per_page=100"))?.iter().filter_map(pr_row).collect();
    let issues: Vec<GhIssue> = list(gh, &format!("repos/{repo}/issues?state=open&per_page=100"))?
        .iter()
        .filter(|v| v.get("pull_request").is_none())
        .filter_map(gh_issue)
        .collect();
    let mut comments_url = format!("repos/{repo}/issues/comments?per_page=100");
    if let Some(s) = since {
        comments_url.push_str(&format!("&since={}", iso(s)));
    }
    let comments = list(gh, &comments_url)?;
    let open: HashSet<i64> = issues.iter().map(|i| i.number).collect();
    let mut issues = issues;
    let mut gone = Vec::new();
    for n in store.open_mirrored_numbers(project).map_err(|e| Fail::Error(e.0))? {
        if open.contains(&n) {
            continue;
        }
        let r = call(gh, &["api", &format!("repos/{repo}/issues/{n}")]);
        let text = format!("{} {}", r.stderr, r.stdout);
        if !r.ok {
            if limited(&text) {
                return Err(Fail::Limited(0));
            }
            if text.contains("404") || text.contains("Not Found") || text.contains("410") {
                gone.push(n);
                continue;
            }
            return Err(Fail::Error(text.trim().to_string()));
        }
        let v = json_values(&r.stdout).map_err(Fail::Error)?;
        match v.first().and_then(gh_issue) {
            Some(i) if v[0].get("pull_request").is_none() => issues.push(i),
            _ => gone.push(n),
        }
    }
    Ok(Fetched { issues, gone, prs, comments })
}

fn apply(store: &Store, project: Id, f: &Fetched, gh: &str, repo: &str, now: i64) -> crate::store::Result<()> {
    let trust_of = |user: &str, assoc: &str| super::trust::resolve(store, gh, repo, user, assoc, now);
    let bot = store.setting("github_bot").ok().flatten().filter(|b| !b.trim().is_empty());
    let mut pr_numbers: Vec<i64> = Vec::new();
    let mut linked: HashSet<i64> = HashSet::new();
    let mut links: Vec<(i64, Vec<i64>)> = Vec::new();
    for pr in &f.prs {
        let l = linked_issues(&pr.body, &pr.head_ref);
        linked.extend(l.iter().copied());
        links.push((pr.number, l));
        pr_numbers.push(pr.number);
    }
    for n in &f.gone {
        if let Some(i) = store.mirrored_issue(project, *n)? {
            store.delete_issue(i.id)?;
        }
    }
    let mut wanted_deps: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut fresh: HashSet<Id> = HashSet::new();
    for i in &f.issues {
        match map::map_issue(i, linked.contains(&i.number), bot.as_deref()) {
            Mapped::Remove => {
                if let Some(old) = store.mirrored_issue(project, i.number)? {
                    store.delete_issue(old.id)?;
                }
            }
            Mapped::Keep(mut m) => {
                m.trust = trust_of(&i.author, &i.association);
                let before = store.mirrored_issue(project, i.number)?;
                let id = store.mirror_issue(project, &m)?;
                if before.is_none() {
                    fresh.insert(id);
                }
                if m.state == IssueState::Done && before.is_none_or(|b| b.state != IssueState::Done) {
                    store.unblock_dependents(id)?;
                }
                let deps = wanted_deps.entry(i.number).or_default();
                deps.extend(map::dependencies(&i.body));
                if let Some(parent) = i.parent {
                    wanted_deps.entry(parent).or_default().push(i.number);
                }
            }
        }
    }
    for (number, nums) in wanted_deps {
        let Some(issue) = store.mirrored_issue(project, number)? else { continue };
        let mut ids: Vec<Id> = Vec::new();
        for n in nums.into_iter().filter(|n| *n != number) {
            if let Some(d) = store.mirrored_issue(project, n)?
                && !ids.contains(&d.id)
            {
                ids.push(d.id);
            }
        }
        ids.sort_unstable();
        let mut have = issue.depends_on.clone();
        have.sort_unstable();
        if have != ids {
            let _ = store.set_deps(issue.id, &ids);
        }
    }
    for pr in &f.prs {
        store.mirror_pr(project, pr)?;
    }
    store.close_prs_not_in(project, &pr_numbers)?;
    for (number, nums) in links {
        let mut target = None;
        for n in nums {
            if store.mirrored_issue(project, n)?.is_some() {
                target = Some(n);
                break;
            }
        }
        store.link_pr(project, number, target)?;
    }
    for c in &f.comments {
        let Some(number) = c.get("issue_url").and_then(Value::as_str).and_then(trailing_number) else {
            continue;
        };
        let issue_id = match store.pr_issue(project, number)? {
            Some(id) => Some(id),
            None => store.mirrored_issue(project, number)?.map(|i| i.id),
        };
        let (Some(issue_id), Some(ext)) = (issue_id, c.get("id").and_then(Value::as_i64)) else {
            continue;
        };
        let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or_default();
        let author = c.get("user").and_then(|u| u.get("login")).and_then(Value::as_str).unwrap_or_default();
        let assoc = c.get("author_association").and_then(Value::as_str).unwrap_or_default();
        store.mirror_comment(issue_id, &ext.to_string(), author, s("body"), s("html_url"), parse_time(s("created_at")), trust_of(author, assoc))?;
    }
    super::ready::evaluate(store, project, &fresh, now)?;
    Ok(())
}

fn backoff_after_failure(failures: u32) -> i64 {
    (FAILURE_BACKOFF_MS << failures.saturating_sub(1).min(5)).min(MAX_BACKOFF_MS)
}

pub fn sync_project(store: &Store, gh: &str, project_id: Id, now: i64, force: bool) -> Outcome {
    let mut state = load_state(store, project_id);
    if !force && state.backoff_until.is_some_and(|b| b > now) {
        return Outcome::Skipped;
    }
    state.last_attempt = Some(now);
    let outcome = run_sync(store, gh, project_id, now, &mut state);
    match &outcome {
        Outcome::Synced | Outcome::Unchanged => {
            state.failures = 0;
            state.backoff_until = None;
            state.error = None;
            state.last_sync = Some(now);
        }
        Outcome::RateLimited(until) => {
            state.backoff_until = Some(*until);
            state.error = Some(format!("rate limited until {until}"));
        }
        Outcome::Failed(e) => {
            state.failures += 1;
            state.backoff_until = Some(now + backoff_after_failure(state.failures));
            state.error = Some(e.clone());
        }
        Outcome::Skipped => {}
    }
    save_state(store, project_id, &state);
    outcome
}

fn run_sync(store: &Store, gh: &str, project_id: Id, now: i64, state: &mut SyncState) -> Outcome {
    let project = match store.project(project_id) {
        Ok(p) => p,
        Err(e) => return Outcome::Failed(e.0),
    };
    if !matches!(project.tracker, Tracker::Github { .. }) {
        return Outcome::Failed("project is not in github mode".into());
    }
    let Some(repo) = super::project_repo(&project) else {
        return Outcome::Failed("no GitHub repo for this project".into());
    };
    let probe_url = format!("repos/{repo}/issues?state=all&sort=updated&direction=desc&per_page=1");
    let mut args = vec!["api", "-i"];
    let header;
    if let Some(etag) = &state.etag {
        header = format!("If-None-Match: {etag}");
        args.extend(["-H", header.as_str()]);
    }
    args.push(&probe_url);
    let reply = call(gh, &args);
    let probe = parse_probe(&reply.stdout);
    if probe.status == 0 && !reply.ok {
        return Outcome::Failed(reply.stderr.trim().to_string());
    }
    let exhausted = probe.remaining == Some(0)
        || (matches!(probe.status, 403 | 429) && (probe.remaining == Some(0) || limited(&reply.stdout) || limited(&reply.stderr)));
    if exhausted {
        let until = probe.reset.unwrap_or(now + MAX_BACKOFF_MS);
        return Outcome::RateLimited(until.max(now + 1000));
    }
    if probe.status == 304 {
        return Outcome::Unchanged;
    }
    if !(200..300).contains(&probe.status) {
        return Outcome::Failed(format!("GitHub answered {} {}", probe.status, reply.stderr.trim()).trim().to_string());
    }
    let since = state.last_sync.map(|t| t - COMMENT_OVERLAP_MS);
    let fetched = match fetch(gh, &repo, store, project_id, since) {
        Ok(f) => f,
        Err(Fail::Limited(_)) => return Outcome::RateLimited(now + MAX_BACKOFF_MS),
        Err(Fail::Error(e)) => return Outcome::Failed(e),
    };
    if let Err(e) = apply(store, project_id, &fetched, gh, &repo, now) {
        return Outcome::Failed(e.0);
    }
    state.etag = probe.etag;
    Outcome::Synced
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentbench_core::{IssuePatch, NewComment, ProjectPatch};
    use std::path::{Path, PathBuf};

    struct Stub {
        dir: tempfile::TempDir,
        gh: String,
    }

    impl Stub {
        fn new() -> Stub {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let gh = dir.path().join("gh");
            let script = r#"#!/bin/sh
D="$(dirname "$0")"
echo "$@" >> "$D/calls"
if [ -f "$D/exit" ]; then echo "$(cat "$D/exit_msg" 2>/dev/null)" >&2; exit "$(cat "$D/exit")"; fi
for a in "$@"; do last="$a"; done
case "$last" in
  *per_page=1) cat "$D/probe" ;;
  */pulls?*) cat "$D/pulls.json" ;;
  */issues/comments?*) cat "$D/comments.json" ;;
  */issues/[0-9]*) f="$D/issue_${last##*/}.json"; if [ -f "$f" ]; then cat "$f"; else echo "gh: Not Found (HTTP 404)" >&2; exit 1; fi ;;
  */issues?state*) cat "$D/issues.json" ;;
esac
"#;
            std::fs::write(&gh, script).unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            let s = Stub { gh: gh.to_string_lossy().into_owned(), dir };
            s.put("pulls.json", "[]");
            s.put("issues.json", "[]");
            s.put("comments.json", "[]");
            s.probe("200 OK", "\"e1\"", 4000, 4_000_000_000);
            s
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn put(&self, name: &str, body: &str) {
            std::fs::write(self.path(name), body).unwrap();
        }

        fn probe(&self, status: &str, etag: &str, remaining: i64, reset: i64) {
            self.put(
                "probe",
                &format!("HTTP/2.0 {status}\nEtag: {etag}\nX-Ratelimit-Remaining: {remaining}\nX-Ratelimit-Reset: {reset}\n\n[]"),
            );
        }

        fn calls(&self) -> usize {
            std::fs::read_to_string(self.path("calls")).map(|c| c.lines().count()).unwrap_or(0)
        }
    }

    fn setup() -> (Store, Id, Stub) {
        let store = Store::memory().unwrap();
        let p = store.add_project("demo", "/tmp/demo-gh", Some("main"), None).unwrap();
        store
            .update_project(p.id, &ProjectPatch { tracker: Some(Tracker::Github { repo: "o/r".into() }), ..Default::default() })
            .unwrap();
        (store, p.id, Stub::new())
    }

    fn issue_json(n: i64, title: &str, labels: &[&str], updated: &str, body: &str) -> String {
        let labels: Vec<String> = labels.iter().map(|l| format!("{{\"name\":\"{l}\"}}")).collect();
        format!(
            r#"{{"number":{n},"title":"{title}","body":"{body}","state":"open","labels":[{}],"assignees":[],"user":{{"login":"alice"}},"html_url":"https://github.com/o/r/issues/{n}","updated_at":"{updated}"}}"#,
            labels.join(",")
        )
    }

    const T0: &str = "2024-03-01T12:00:00Z";
    const T1: &str = "2024-03-02T12:00:00Z";

    fn all_issues(store: &Store, p: Id) -> Vec<agentbench_core::Issue> {
        store.issues().unwrap().into_iter().filter(|i| i.project_id == p).collect()
    }

    fn sync(store: &Store, p: Id, stub: &Stub, now: i64) -> Outcome {
        sync_project(store, &stub.gh, p, now, false)
    }

    #[test]
    fn issues_and_pull_requests_appear_once_and_a_second_sync_changes_nothing() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{},{}]", issue_json(1, "First", &["ready", "priority:high"], T0, ""), issue_json(2, "Second", &["bug"], T0, "")));
        stub.put(
            "pulls.json",
            r#"[{"number":9,"title":"Fix","body":"Closes #1","user":{"login":"bob"},"head":{"ref":"fix-1","sha":"abc"},"base":{"ref":"main"},"draft":false,"html_url":"u","created_at":"2024-03-01T12:00:00Z","updated_at":"2024-03-01T12:00:00Z"}]"#,
        );
        assert_eq!(sync(&store, p, &stub, 1_000), Outcome::Synced);
        let first = all_issues(&store, p);
        assert_eq!(first.len(), 2);
        let one = first.iter().find(|i| i.external_number == Some(1)).unwrap();
        assert_eq!(one.state, IssueState::Review);
        assert_eq!(one.priority, 3);
        assert_eq!(one.labels, ["ready", "priority:high"]);
        assert_eq!(one.external_author.as_deref(), Some("alice"));
        let two = first.iter().find(|i| i.external_number == Some(2)).unwrap();
        assert_eq!(two.state, IssueState::Backlog);
        assert_eq!(store.pr_count(p, "open").unwrap(), 1);
        assert_eq!(store.pr_issue(p, 9).unwrap(), Some(one.id));

        stub.probe("200 OK", "\"e2\"", 4000, 4_000_000_000);
        assert_eq!(sync(&store, p, &stub, 2_000), Outcome::Synced);
        assert_eq!(all_issues(&store, p), first);
        assert_eq!(store.pr_count(p, "open").unwrap(), 1);
    }

    #[test]
    fn an_edited_issue_updates_the_mirror_and_a_deleted_label_disappears() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{}]", issue_json(1, "Old", &["ready", "bug"], T0, "")));
        sync(&store, p, &stub, 1_000);
        stub.put("issues.json", &format!("[{}]", issue_json(1, "New title", &["ready"], T1, "new body")));
        stub.probe("200 OK", "\"e2\"", 4000, 4_000_000_000);
        sync(&store, p, &stub, 2_000);
        let i = &all_issues(&store, p)[0];
        assert_eq!((i.title.as_str(), i.body.as_str()), ("New title", "new body"));
        assert_eq!(i.labels, ["ready"]);
        assert_eq!(all_issues(&store, p).len(), 1);
    }

    #[test]
    fn a_closed_issue_becomes_done_and_a_not_planned_one_is_removed() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{},{}]", issue_json(1, "A", &[], T0, ""), issue_json(2, "B", &[], T0, "")));
        sync(&store, p, &stub, 1_000);
        stub.put("issues.json", "[]");
        stub.put("issue_1.json", &issue_json(1, "A", &[], T1, "").replace("\"open\"", "\"closed\",\"state_reason\":\"completed\""));
        stub.put("issue_2.json", &issue_json(2, "B", &[], T1, "").replace("\"open\"", "\"closed\",\"state_reason\":\"not_planned\""));
        stub.probe("200 OK", "\"e2\"", 4000, 4_000_000_000);
        sync(&store, p, &stub, 2_000);
        let issues = all_issues(&store, p);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].external_number, Some(1));
        assert_eq!(issues[0].state, IssueState::Done);
    }

    #[test]
    fn a_comment_is_added_once_and_an_edit_updates_it() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{}]", issue_json(1, "A", &[], T0, "")));
        let comment = |body: &str| {
            format!(r#"[{{"id":77,"issue_url":"https://api.github.com/repos/o/r/issues/1","user":{{"login":"carol"}},"body":"{body}","html_url":"https://github.com/o/r/issues/1#c77","created_at":"{T0}"}}]"#)
        };
        stub.put("comments.json", &comment("hello"));
        sync(&store, p, &stub, 1_000);
        stub.probe("200 OK", "\"e2\"", 4000, 4_000_000_000);
        sync(&store, p, &stub, 2_000);
        let id = all_issues(&store, p)[0].id;
        let comments = store.comments(id).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].external_id.as_deref(), Some("77"));
        assert_eq!(comments[0].author, "carol");
        stub.put("comments.json", &comment("hello again"));
        stub.probe("200 OK", "\"e3\"", 4000, 4_000_000_000);
        sync(&store, p, &stub, 3_000);
        let comments = store.comments(id).unwrap();
        assert_eq!((comments.len(), comments[0].body.as_str()), (1, "hello again"));
    }

    #[test]
    fn pull_request_comments_land_on_the_linked_issue() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{}]", issue_json(1, "A", &[], T0, "")));
        stub.put(
            "pulls.json",
            r#"[{"number":9,"title":"Fix","body":"fixes #1","user":{"login":"bob"},"head":{"ref":"x","sha":"abc"},"base":{"ref":"main"},"html_url":"u","created_at":"2024-03-01T12:00:00Z","updated_at":"2024-03-01T12:00:00Z"}]"#,
        );
        stub.put("comments.json", r#"[{"id":5,"issue_url":"https://api.github.com/repos/o/r/issues/9","user":{"login":"dan"},"body":"lgtm","html_url":"u","created_at":"2024-03-01T12:00:00Z"}]"#);
        sync(&store, p, &stub, 1_000);
        let id = all_issues(&store, p)[0].id;
        assert_eq!(store.comments(id).unwrap().len(), 1);
    }

    #[test]
    fn dependencies_and_sub_issues_become_depends_on() {
        let (store, p, stub) = setup();
        stub.put(
            "issues.json",
            &format!(
                "[{},{},{}]",
                issue_json(1, "base", &[], T0, ""),
                issue_json(2, "needs one", &[], T0, "Depends on #1"),
                issue_json(3, "epic", &[], T0, "").replace("\"updated_at\"", "\"parent_issue_url\":null,\"updated_at\""),
            ),
        );
        sync(&store, p, &stub, 1_000);
        let by = |n: i64| all_issues(&store, p).into_iter().find(|i| i.external_number == Some(n)).unwrap();
        assert_eq!(by(2).depends_on, vec![by(1).id]);
        assert_eq!(by(2).waiting_on, vec![by(1).id]);
    }

    #[test]
    fn an_unchanged_etag_costs_one_call_and_keeps_the_mirror() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{}]", issue_json(1, "A", &[], T0, "")));
        sync(&store, p, &stub, 1_000);
        assert_eq!(load_state(&store, p).etag.as_deref(), Some("\"e1\""));
        let before = stub.calls();
        stub.put("probe", "HTTP/2.0 304 Not Modified\nEtag: \"e1\"\nX-Ratelimit-Remaining: 4000\n\n");
        assert_eq!(sync(&store, p, &stub, 2_000), Outcome::Unchanged);
        assert_eq!(stub.calls(), before + 1);
        let args = std::fs::read_to_string(stub.path("calls")).unwrap();
        assert!(args.lines().last().unwrap().contains("If-None-Match: \"e1\""));
        assert_eq!(all_issues(&store, p).len(), 1);
        assert_eq!(load_state(&store, p).last_sync, Some(2_000));
    }

    #[test]
    fn an_exhausted_rate_limit_backs_off_until_the_reset() {
        let (store, p, stub) = setup();
        stub.probe("403 Forbidden", "\"e1\"", 0, 5_000);
        assert_eq!(sync(&store, p, &stub, 1_000), Outcome::RateLimited(5_000_000));
        let calls = stub.calls();
        assert_eq!(sync(&store, p, &stub, 2_000), Outcome::Skipped);
        assert_eq!(stub.calls(), calls);
        let state = load_state(&store, p);
        assert_eq!(state.backoff_until, Some(5_000_000));
        stub.probe("200 OK", "\"e1\"", 4000, 4_000_000_000);
        assert_eq!(sync(&store, p, &stub, 5_000_001), Outcome::Synced);
        assert_eq!(load_state(&store, p).backoff_until, None);
    }

    #[test]
    fn remaining_zero_on_a_good_response_also_backs_off() {
        let (store, p, stub) = setup();
        stub.probe("200 OK", "\"e1\"", 0, 9_000);
        assert_eq!(sync(&store, p, &stub, 1_000), Outcome::RateLimited(9_000_000));
        assert!(all_issues(&store, p).is_empty());
    }

    #[test]
    fn a_failing_gh_sets_the_status_backs_off_and_keeps_the_old_mirror() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{}]", issue_json(1, "A", &[], T0, "")));
        sync(&store, p, &stub, 1_000);
        stub.put("exit", "1");
        stub.put("exit_msg", "gh: boom");
        stub.probe("200 OK", "\"e2\"", 4000, 4_000_000_000);
        let out = sync(&store, p, &stub, 100_000);
        assert!(matches!(&out, Outcome::Failed(e) if e.contains("boom")), "{out:?}");
        let state = load_state(&store, p);
        assert_eq!(state.failures, 1);
        assert_eq!(state.error.as_deref(), Some("gh: boom"));
        assert_eq!(state.backoff_until, Some(100_000 + FAILURE_BACKOFF_MS));
        assert_eq!(state.last_sync, Some(1_000));
        assert_eq!(all_issues(&store, p).len(), 1);
        assert_eq!(sync(&store, p, &stub, 110_000), Outcome::Skipped);
        let _ = sync_project(&store, &stub.gh, p, 120_000, true);
        assert_eq!(load_state(&store, p).failures, 2);
        assert_eq!(load_state(&store, p).backoff_until, Some(120_000 + 2 * FAILURE_BACKOFF_MS));
    }

    #[test]
    fn a_failure_midway_leaves_the_mirror_untouched() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{}]", issue_json(1, "A", &[], T0, "")));
        sync(&store, p, &stub, 1_000);
        stub.put("issues.json", &format!("[{}]", issue_json(1, "Changed", &[], T1, "")));
        stub.put("comments.json", "not json");
        stub.probe("200 OK", "\"e2\"", 4000, 4_000_000_000);
        assert!(matches!(sync(&store, p, &stub, 100_000), Outcome::Failed(_)));
        assert_eq!(all_issues(&store, p)[0].title, "A");
        assert_eq!(load_state(&store, p).etag.as_deref(), Some("\"e1\""));
    }

    #[test]
    fn edits_to_a_mirrored_issue_go_to_the_outbox_instead_of_failing() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{}]", issue_json(4, "A", &[], T0, "")));
        sync(&store, p, &stub, 1_000);
        assert!(store.outbox(Some(p)).unwrap().is_empty());
        let id = all_issues(&store, p)[0].id;
        store.update_issue(id, &IssuePatch { state: Some(IssueState::Doing), labels: Some(vec!["x".into()]), ..Default::default() }).unwrap();
        store.add_comment(id, &NewComment { author: None, body: "note".into() }).unwrap();
        let out = store.outbox(Some(p)).unwrap();
        assert_eq!(out.iter().map(|o| o.kind.as_str()).collect::<Vec<_>>(), ["update_issue", "comment"]);
        assert!(out.iter().all(|o| o.status == "pending"));
        assert!(out[0].payload.contains("\"number\":4"));
        let internal = store.add_issue(&agentbench_core::NewIssue { project_id: p, title: "mine".into(), ..Default::default() }).unwrap();
        store.update_issue(internal.id, &IssuePatch { priority: Some(2), ..Default::default() }).unwrap();
        assert_eq!(store.outbox(None).unwrap().len(), 2);
    }

    #[test]
    fn a_local_state_survives_a_sync_when_github_did_not_change() {
        let (store, p, stub) = setup();
        stub.put("issues.json", &format!("[{}]", issue_json(1, "A", &["ready"], T0, "")));
        sync(&store, p, &stub, 1_000);
        let id = all_issues(&store, p)[0].id;
        store.update_issue(id, &IssuePatch { state: Some(IssueState::Doing), ..Default::default() }).unwrap();
        stub.probe("200 OK", "\"e2\"", 4000, 4_000_000_000);
        sync(&store, p, &stub, 2_000);
        assert_eq!(store.issue(id).unwrap().state, IssueState::Doing);
    }

    #[test]
    fn iso_round_trips_with_parse_time() {
        for ms in [0, 1_709_296_215_000, 1_700_000_000_000] {
            assert_eq!(parse_time(&iso(ms)), ms);
        }
        assert_eq!(iso(1_709_296_215_000), "2024-03-01T12:30:15Z");
    }

    #[test]
    fn a_non_github_project_is_refused_without_calling_gh() {
        let store = Store::memory().unwrap();
        let p = store.add_project("x", "/tmp/x-gh", Some("main"), None).unwrap();
        let stub = Stub::new();
        assert!(matches!(sync(&store, p.id, &stub, 1), Outcome::Failed(_)));
        assert_eq!(stub.calls(), 0);
        let _: &Path = stub.dir.path();
    }
}
