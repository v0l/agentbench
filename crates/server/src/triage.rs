use crate::runner::Runner;
use crate::store::{Store, StoreError, now};
use crate::titles::{Target, ask, without_thinking};
use agentbench_core::{
    Change, Entry, Id, Issue, IssuePatch, IssueState, NewIssue, Proposal, ProposalKind,
    ProposalState, Session, TriagePatch, TriageState,
};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

const BUDGET: &str = "triage_budget";
const EVERY: &str = "triage_every_hours";
const LAST_RUN: &str = "triage_last_run";
const STATUS: &str = "triage_status";
const RUNNING: &str = "triage_running";

pub const DEFAULT_BUDGET: u64 = 24_000;
const OUTPUT_TOKENS: u64 = 3_000;
const PATIENCE: Duration = Duration::from_secs(300);
const CHARS_PER_TOKEN: u64 = 4;
const BODY_CLIP: usize = 500;
const COMMENT_CLIP: usize = 300;
const SESSION_CLIP: usize = 1_200;
const STEP_CLIP: usize = 140;
const SESSION_ENTRIES: usize = 24;
const RECENT_DAYS: i64 = 14;
const CHECK_EVERY: Duration = Duration::from_secs(600);

const INSTRUCTIONS: &str = "You triage the issue trackers of several software projects for their owner. \
You are given, per project, an optional note on what matters, its open issues with their comments, \
its recently finished issues, and recent agent session transcripts with tool calls. Propose changes; \
nothing you say is applied until the owner accepts it. Look for: duplicate issues (within or across \
projects), stale issues (no activity, or already fixed according to a finished issue or a session), \
missing or inconsistent labels and priorities, and one failure showing up in several projects \
(propose one cross-cutting issue instead of many). Only propose what the evidence supports. \
Reply with JSON only, shaped as {\"proposals\": [...]}. Each proposal has \"kind\" (one of \
duplicate, stale, labels, cross_cutting), \"summary\" (one sentence), \"issues\" (the issue ids that \
are the evidence), \"sessions\" (session ids that are evidence, may be empty) and \"action\":\n\
- duplicate: {\"keep\": id, \"duplicates\": [ids]}\n\
- stale: {\"issue\": id, \"reason\": \"...\"}\n\
- labels: {\"issue\": id, \"labels\": [\"...\"], \"priority\": number} (either field may be left out)\n\
- cross_cutting: {\"project\": id, \"title\": \"...\", \"body\": \"...\"} (the project to file the shared issue in)\n\
Use only ids that appear in the input. If nothing is worth proposing reply {\"proposals\": []}.";

fn key_note(project: Id) -> String {
    format!("triage_note_{project}")
}

fn number(store: &Store, key: &str, default: u64) -> u64 {
    store.setting(key).ok().flatten().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn project_ids(store: &Store) -> Vec<(Id, String)> {
    store
        .projects()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !p.builtin())
        .map(|p| (p.id, p.name))
        .collect()
}

pub fn state(store: &Store) -> TriageState {
    let notes = project_ids(store)
        .into_iter()
        .filter_map(|(id, _)| {
            store.setting(&key_note(id)).ok().flatten().filter(|n| !n.trim().is_empty()).map(|n| (id, n))
        })
        .collect();
    TriageState {
        proposals: store.proposals(ProposalState::Pending).unwrap_or_default(),
        running: store.setting(RUNNING).ok().flatten().as_deref() == Some("1"),
        budget_tokens: number(store, BUDGET, DEFAULT_BUDGET),
        every_hours: number(store, EVERY, 0),
        last_run: store.setting(LAST_RUN).ok().flatten().and_then(|v| v.parse().ok()),
        last_status: store.setting(STATUS).ok().flatten().unwrap_or_default(),
        notes,
    }
}

pub fn apply(store: &Store, patch: &TriagePatch) -> Result<TriageState, StoreError> {
    if let Some(n) = patch.budget_tokens {
        store.set_setting(BUDGET, &n.to_string())?;
    }
    if let Some(n) = patch.every_hours {
        store.set_setting(EVERY, &n.to_string())?;
    }
    if let (Some(project), Some(note)) = (patch.note_project, &patch.note) {
        store.project(project)?;
        if note.trim().is_empty() {
            store.delete_setting(&key_note(project))?;
        } else {
            store.set_setting(&key_note(project), note.trim())?;
        }
    }
    Ok(state(store))
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.len() <= max {
        return text.to_string();
    }
    format!("{}…", &text[..text.floor_char_boundary(max)])
}

fn one_line(text: &str, max: usize) -> String {
    clip(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

fn days_ago(at: i64) -> i64 {
    ((now() - at) / 86_400_000).max(0)
}

fn session_text(store: &Store, s: &Session) -> String {
    let messages = store.messages(s.id, 0).unwrap_or_default();
    let skip = messages.len().saturating_sub(SESSION_ENTRIES);
    let mut lines = Vec::new();
    for m in messages.into_iter().skip(skip) {
        match m.entry {
            Entry::User { text, .. } => lines.push(format!("user: {}", one_line(&text, STEP_CLIP))),
            Entry::Assistant { text } => {
                lines.push(format!("agent: {}", one_line(&text, STEP_CLIP)))
            }
            Entry::Tool { name, input, output, error, .. } => {
                let out = output.map(|o| one_line(&o, STEP_CLIP)).unwrap_or_default();
                lines.push(format!(
                    "tool {name}({}){} -> {out}",
                    one_line(&input.to_string(), STEP_CLIP),
                    if error { " FAILED" } else { "" }
                ));
            }
            Entry::Error { text } => lines.push(format!("error: {}", one_line(&text, STEP_CLIP))),
            _ => {}
        }
    }
    let mut text = lines.join("\n");
    if text.len() > SESSION_CLIP {
        let from = text.ceil_char_boundary(text.len() - SESSION_CLIP);
        text = format!("…{}", &text[from..]);
    }
    text
}

fn issue_text(store: &Store, i: &Issue) -> String {
    let mut out = format!(
        "#{} [{}] priority {} labels [{}] idle {}d: {}\n",
        i.id,
        i.state.as_str(),
        i.priority,
        i.labels.join(", "),
        days_ago(i.updated_at),
        i.title
    );
    if !i.body.trim().is_empty() {
        out.push_str(&format!("  {}\n", one_line(&i.body, BODY_CLIP)));
    }
    let comments = store.comments(i.id).unwrap_or_default();
    for c in comments.iter().rev().take(3).rev() {
        out.push_str(&format!("  comment by {}: {}\n", c.author, one_line(&c.body, COMMENT_CLIP)));
    }
    out
}

pub struct Gathered {
    pub text: String,
    pub issues: usize,
    pub sessions: usize,
}

pub fn gather(store: &Store, budget_tokens: u64) -> Gathered {
    let limit = (budget_tokens * CHARS_PER_TOKEN) as usize;
    let issues = store.issues().unwrap_or_default();
    let sessions = store.sessions().unwrap_or_default();
    let mut out = String::new();
    let (mut n_issues, mut n_sessions) = (0, 0);
    let projects = project_ids(store);
    let mut blocks: Vec<Vec<String>> = Vec::new();
    for (pid, name) in &projects {
        let mut lines = vec![format!("== Project {name} (id {pid}) ==\n")];
        if let Some(note) = store.setting(&key_note(*pid)).ok().flatten() {
            lines.push(format!("Owner's note on what matters: {}\n", one_line(&note, 400)));
        }
        let mine: Vec<&Issue> = issues.iter().filter(|i| i.project_id == *pid).collect();
        for i in mine.iter().filter(|i| i.state != IssueState::Done) {
            lines.push(issue_text(store, i));
        }
        let mut finished: Vec<&&Issue> = mine.iter().filter(|i| i.state == IssueState::Done).collect();
        finished.sort_by_key(|i| std::cmp::Reverse(i.updated_at));
        for i in finished.into_iter().take(8) {
            lines.push(format!("finished #{}: {}\n", i.id, i.title));
        }
        let open: BTreeSet<Id> =
            mine.iter().filter(|i| i.state != IssueState::Done).map(|i| i.id).collect();
        for s in sessions.iter().filter(|s| s.project_id == *pid) {
            let tied = s.issue_id.is_some_and(|i| open.contains(&i));
            if !tied && days_ago(s.updated_at) > RECENT_DAYS {
                continue;
            }
            let body = session_text(store, s);
            if body.is_empty() {
                continue;
            }
            lines.push(format!(
                "session {} ({}, issue {}, idle {}d): {}\n{}\n",
                s.id,
                s.status.as_str(),
                s.issue_id.map(|i| format!("#{i}")).unwrap_or("none".into()),
                days_ago(s.updated_at),
                one_line(&s.title, 80),
                body
            ));
        }
        blocks.push(lines);
    }
    let mut cursor = vec![0usize; blocks.len()];
    let mut full = false;
    while !full {
        full = true;
        for (b, lines) in blocks.iter().enumerate() {
            if cursor[b] >= lines.len() {
                continue;
            }
            full = false;
            let line = &lines[cursor[b]];
            if out.len() + line.len() > limit && cursor[b] > 0 {
                cursor[b] = lines.len();
                continue;
            }
            cursor[b] += 1;
            if line.starts_with('#') {
                n_issues += 1;
            } else if line.starts_with("session ") {
                n_sessions += 1;
            }
            let _ = &mut out;
            out_push(&mut out, b, line);
        }
    }
    let ordered = reorder(&out, &blocks.len());
    Gathered { text: ordered, issues: n_issues, sessions: n_sessions }
}

fn out_push(out: &mut String, block: usize, line: &str) {
    out.push_str(&format!("\u{1}{block}\u{2}{line}"));
}

fn reorder(raw: &str, blocks: &usize) -> String {
    let mut by: Vec<String> = vec![String::new(); *blocks];
    for part in raw.split('\u{1}').filter(|p| !p.is_empty()) {
        if let Some((b, line)) = part.split_once('\u{2}')
            && let Ok(b) = b.parse::<usize>()
        {
            by[b].push_str(line);
        }
    }
    by.join("\n")
}

fn ids(v: &Value) -> Vec<Id> {
    v.as_array()
        .map(|a| a.iter().filter_map(|x| x.as_i64().or_else(|| x.as_str()?.trim_start_matches('#').parse().ok())).collect())
        .unwrap_or_default()
}

fn id(v: &Value) -> Option<Id> {
    v.as_i64().or_else(|| v.as_str()?.trim().trim_start_matches('#').parse().ok())
}

fn fingerprint(kind: ProposalKind, issues: &[Id], extra: &str) -> String {
    let set: BTreeSet<Id> = issues.iter().copied().collect();
    format!("{}:{}:{extra}", kind.as_str(), set.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(","))
}

pub fn parse(raw: &str) -> Vec<Value> {
    let text = without_thinking(raw);
    let text = text.trim();
    let slice = |open: char, close: char| {
        let (a, b) = (text.find(open)?, text.rfind(close)?);
        (a < b).then(|| &text[a..=b])
    };
    if let Some(Ok(v)) = slice('{', '}').map(serde_json::from_str::<Value>)
        && let Some(list) = v.get("proposals").and_then(Value::as_array)
    {
        return list.clone();
    }
    if let Some(Ok(Value::Array(list))) = slice('[', ']').map(serde_json::from_str::<Value>) {
        return list;
    }
    Vec::new()
}

pub fn build(store: &Store, raw: &Value) -> Option<(String, Proposal)> {
    let kind = ProposalKind::parse(raw.get("kind")?.as_str()?)?;
    let summary = one_line(raw.get("summary")?.as_str()?, 300);
    let action = raw.get("action")?;
    let known = |i: Id| store.issue(i).ok();
    let mut evidence: Vec<Id> = ids(raw.get("issues").unwrap_or(&Value::Null))
        .into_iter()
        .filter(|i| known(*i).is_some())
        .collect();
    let mut changes = Vec::new();
    let mut extra = String::new();
    match kind {
        ProposalKind::Duplicate => {
            let keep = known(id(action.get("keep")?)?)?;
            let dups: Vec<Issue> = ids(action.get("duplicates")?)
                .into_iter()
                .filter(|d| *d != keep.id)
                .filter_map(known)
                .collect();
            if dups.is_empty() {
                return None;
            }
            evidence.push(keep.id);
            for d in &dups {
                evidence.push(d.id);
                changes.push(Change::Comment {
                    issue_id: d.id,
                    body: format!("Duplicate of #{}: {}", keep.id, keep.title),
                });
                changes.push(Change::Update {
                    issue_id: d.id,
                    patch: IssuePatch { state: Some(IssueState::Done), ..Default::default() },
                });
            }
            changes.push(Change::Comment {
                issue_id: keep.id,
                body: format!(
                    "Duplicates merged here: {}",
                    dups.iter().map(|d| format!("#{}", d.id)).collect::<Vec<_>>().join(", ")
                ),
            });
        }
        ProposalKind::Stale => {
            let issue = known(id(action.get("issue")?)?)?;
            let reason = one_line(action.get("reason").and_then(Value::as_str).unwrap_or(""), 300);
            evidence.push(issue.id);
            changes.push(Change::Comment {
                issue_id: issue.id,
                body: format!("Closed as stale: {reason}"),
            });
            changes.push(Change::Update {
                issue_id: issue.id,
                patch: IssuePatch { state: Some(IssueState::Done), ..Default::default() },
            });
        }
        ProposalKind::Labels => {
            let issue = known(id(action.get("issue")?)?)?;
            let labels: Option<Vec<String>> = action.get("labels").and_then(Value::as_array).map(|a| {
                a.iter().filter_map(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
            });
            let priority = action.get("priority").and_then(Value::as_i64);
            let labels = labels.filter(|l| *l != issue.labels);
            let priority = priority.filter(|p| *p != issue.priority);
            if labels.is_none() && priority.is_none() {
                return None;
            }
            evidence.push(issue.id);
            changes.push(Change::Update {
                issue_id: issue.id,
                patch: IssuePatch { labels, priority, ..Default::default() },
            });
        }
        ProposalKind::CrossCutting => {
            let project = id(action.get("project")?)?;
            store.project(project).ok()?;
            let title = one_line(action.get("title")?.as_str()?, 120);
            if title.is_empty() || evidence.len() < 2 {
                return None;
            }
            let body = action.get("body").and_then(Value::as_str).unwrap_or("").trim().to_string();
            let refs = evidence.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ");
            changes.push(Change::Create {
                issue: NewIssue {
                    project_id: project,
                    title: title.clone(),
                    body: format!("{body}\n\nSeen in: {refs}"),
                    state: Some(IssueState::Backlog),
                    priority: None,
                    labels: vec![],
                },
            });
            for i in &evidence {
                changes.push(Change::Comment {
                    issue_id: *i,
                    body: format!("Part of a cross-cutting issue: {title}"),
                });
            }
            extra = title.to_lowercase();
        }
    }
    let evidence: Vec<Id> = evidence.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
    if evidence.is_empty() || changes.is_empty() {
        return None;
    }
    let mut projects: Vec<Id> =
        evidence.iter().filter_map(|i| known(*i)).map(|i| i.project_id).collect();
    if let Some(Change::Create { issue }) = changes.first() {
        projects.push(issue.project_id);
    }
    let projects: Vec<Id> = projects.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
    let sessions: Vec<String> = raw
        .get("sessions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let known_sessions: Vec<String> = store
        .sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.id.to_string())
        .collect();
    let sessions = sessions.into_iter().filter(|s| known_sessions.contains(s)).collect();
    let print = fingerprint(kind, &evidence, &extra);
    Some((
        print,
        Proposal {
            id: 0,
            kind,
            summary,
            issues: evidence,
            sessions,
            projects,
            changes,
            state: ProposalState::Pending,
            created_at: now(),
        },
    ))
}

pub async fn run(store: &Store, target: &Target) -> Result<String, String> {
    let budget = number(store, BUDGET, DEFAULT_BUDGET);
    let gathered = gather(store, budget);
    if gathered.issues == 0 {
        return Ok("no open issues to read".into());
    }
    let raw = ask(target, INSTRUCTIONS, &gathered.text, OUTPUT_TOKENS.min(budget), PATIENCE).await?;
    let mut added = 0;
    let mut seen: HashMap<String, ()> = HashMap::new();
    for item in parse(&raw) {
        let Some((print, proposal)) = build(store, &item) else { continue };
        if seen.insert(print.clone(), ()).is_some() || store.proposal_known(&print).map_err(|e| e.0)? {
            continue;
        }
        store.add_proposal(&print, &proposal).map_err(|e| e.0)?;
        added += 1;
    }
    Ok(format!(
        "read {} issues and {} sessions, proposed {added} changes",
        gathered.issues, gathered.sessions
    ))
}

pub fn start(runner: Runner) -> bool {
    let store = runner.store().clone();
    let Some(target) = runner.titles().and_then(|t| t.target()) else { return false };
    if store.setting(RUNNING).ok().flatten().as_deref() == Some("1") {
        return false;
    }
    let _ = store.set_setting(RUNNING, "1");
    let _ = store.set_setting(LAST_RUN, &now().to_string());
    tokio::spawn(async move {
        let status = match run(&store, &target).await {
            Ok(s) => s,
            Err(e) => format!("failed: {e}"),
        };
        let _ = store.set_setting(STATUS, &status);
        let _ = store.set_setting(RUNNING, "0");
        let _ = store.log_autopilot(None, None, &format!("triage: {status}"));
        runner.changed.notify_waiters();
    });
    true
}

pub fn spawn(runner: Runner) {
    let _ = runner.store().set_setting(RUNNING, "0");
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CHECK_EVERY).await;
            let s = state(runner.store());
            let due = s.every_hours > 0
                && s.last_run.is_none_or(|t| now() - t >= s.every_hours as i64 * 3_600_000);
            if due {
                start(runner.clone());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::titles::fake::{completion, server};
    use agentbench_core::NewComment;

    fn two_projects() -> (Store, Id, Id, Id, Id) {
        let store = Store::memory().unwrap();
        let a = store.add_project("alpha", "/tmp/alpha", Some("main"), None).unwrap();
        let b = store.add_project("beta", "/tmp/beta", Some("main"), None).unwrap();
        let mk = |p: Id, t: &str| {
            store
                .add_issue(&NewIssue { project_id: p, title: t.into(), ..Default::default() })
                .unwrap()
                .id
        };
        let i1 = mk(a.id, "Login loops on firefox");
        let i2 = mk(b.id, "Firefox login redirect loop");
        (store, a.id, b.id, i1, i2)
    }

    fn target(url: String) -> Target {
        Target { url, model: "tiny".into(), key: String::new() }
    }

    fn reply(i1: Id, i2: Id) -> String {
        completion(&format!(
            r#"```json
{{"proposals": [
 {{"kind": "duplicate", "summary": "Same login loop", "issues": [{i1}, {i2}], "sessions": [],
   "action": {{"keep": {i1}, "duplicates": [{i2}]}}}},
 {{"kind": "labels", "summary": "Needs a bug label", "issues": [{i1}],
   "action": {{"issue": {i1}, "labels": ["bug"], "priority": 3}}}},
 {{"kind": "stale", "summary": "ghost", "issues": [9999], "action": {{"issue": 9999, "reason": "x"}}}}
]}}
```"#
        ))
    }

    #[tokio::test]
    async fn a_run_proposes_changes_across_two_projects_with_evidence() {
        let (store, a, b, i1, i2) = two_projects();
        store
            .add_comment(i1, &NewComment { author: None, body: "still happening".into() })
            .unwrap();
        apply(&store, &TriagePatch { note_project: Some(a), note: Some("auth bugs first".into()), ..Default::default() }).unwrap();
        let (url, seen) = server(vec![(200, reply(i1, i2))]);
        let said = run(&store, &target(url)).await.unwrap();
        assert!(said.contains("proposed 2"), "{said}");
        let prompt = seen.lock().unwrap()[0]["messages"][1]["content"].as_str().unwrap().to_string();
        assert!(prompt.contains("Firefox login redirect loop") && prompt.contains("Login loops"));
        assert!(prompt.contains("auth bugs first") && prompt.contains("still happening"));
        let proposals = state(&store).proposals;
        assert_eq!(proposals.len(), 2);
        let dup = &proposals[0];
        assert_eq!(dup.kind, ProposalKind::Duplicate);
        assert_eq!(dup.issues, vec![i1, i2]);
        assert_eq!(dup.projects, vec![a, b]);
    }

    #[tokio::test]
    async fn a_rejected_proposal_is_not_proposed_again() {
        let (store, _, _, i1, i2) = two_projects();
        let (url, _) = server(vec![(200, reply(i1, i2)), (200, reply(i1, i2))]);
        run(&store, &target(url.clone())).await.unwrap();
        let first = store.proposals(ProposalState::Pending).unwrap();
        store.decide_proposal(first[0].id, ProposalState::Rejected).unwrap();
        let said = run(&store, &target(url)).await.unwrap();
        assert!(said.contains("proposed 0"), "{said}");
        let pending = store.proposals(ProposalState::Pending).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].kind, ProposalKind::Labels);
    }

    #[test]
    fn the_budget_limits_what_the_model_reads() {
        let (store, ..) = two_projects();
        let small = gather(&store, 40);
        let big = gather(&store, 24_000);
        assert!(small.text.len() < big.text.len());
        assert!(small.issues < big.issues, "{} vs {}", small.issues, big.issues);
    }

    #[test]
    fn a_reply_that_is_not_json_gives_no_proposals() {
        assert!(parse("sorry, no").is_empty());
        assert_eq!(parse("<think>x</think>[{\"kind\":\"stale\"}]").len(), 1);
    }
}
