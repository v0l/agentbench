use crate::store::{Store, now};
use agentbench_core::{Id, Issue, IssuePatch, IssueState, NewComment, NewIssue, Session, SessionId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

pub const LABEL: &str = "process";
pub const PROJECT_SETTING: &str = "process_project";
pub const PER_TURN: u32 = 3;
pub const REPEATS: u32 = 3;
pub const DIGEST_LAST: &str = "process_digest_last";
pub const DAY_MS: i64 = 86_400_000;
const STALE_NOTE_DAYS: u64 = 30;
const AUTHOR: &str = "process";

pub const CATEGORIES: [(&str, &str); 9] = [
    ("harness_bug", "harness bug"),
    ("tool", "tool missing or awkward"),
    ("brief", "brief unclear or wrong"),
    ("instruction_conflict", "instruction conflict"),
    ("permission", "permission or credential gap"),
    ("environment", "environment (GPU, ports, restarts)"),
    ("wasted_time", "wasted time"),
    ("user_correction", "user correction"),
    ("other", "other"),
];

pub fn category(raw: &str) -> &'static str {
    let r = raw.trim().to_lowercase().replace([' ', '-'], "_");
    let found = CATEGORIES.iter().find(|(k, label)| r == *k || r == label.replace([' ', '-'], "_"));
    if let Some((k, _)) = found {
        return k;
    }
    let has = |w: &str| r.contains(w);
    if has("harness") || has("bug") {
        "harness_bug"
    } else if has("tool") {
        "tool"
    } else if has("brief") {
        "brief"
    } else if has("conflict") || has("instruction") {
        "instruction_conflict"
    } else if has("permission") || has("credential") || has("secret") {
        "permission"
    } else if has("environment") || has("gpu") || has("port") || has("restart") {
        "environment"
    } else if has("wasted") || has("time") || has("loop") {
        "wasted_time"
    } else if has("correction") || has("user") {
        "user_correction"
    } else {
        "other"
    }
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub category: String,
    pub title: String,
    pub what_happened: String,
    pub cost: String,
    pub suggestion: String,
    pub key: String,
    pub source: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Meta {
    category: String,
    #[serde(default)]
    key: String,
    #[serde(default)]
    text: String,
    count: u32,
    #[serde(default)]
    minutes: f64,
}

#[derive(Debug, PartialEq)]
pub enum Filed {
    New(Id),
    Bumped(Id, u32),
}

impl Filed {
    pub fn id(&self) -> Id {
        match self {
            Filed::New(i) | Filed::Bumped(i, _) => *i,
        }
    }
}

fn meta_key(issue: Id) -> String {
    format!("process_meta_{issue}")
}

fn meta(store: &Store, issue: &Issue) -> Meta {
    store
        .setting(&meta_key(issue.id))
        .ok()
        .flatten()
        .and_then(|m| serde_json::from_str(&m).ok())
        .unwrap_or_else(|| Meta {
            category: String::new(),
            key: String::new(),
            text: format!("{} {}", bare_title(&issue.title), issue.body),
            count: count_in(&issue.title),
            minutes: 0.0,
        })
}

fn save_meta(store: &Store, issue: Id, m: &Meta) {
    let _ = store.set_setting(&meta_key(issue), &serde_json::to_string(m).unwrap_or_default());
}

static COUNT: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"\s*\(x(\d+)\)\s*$").unwrap());

pub fn count_in(title: &str) -> u32 {
    COUNT.captures(title).and_then(|c| c[1].parse().ok()).unwrap_or(1)
}

fn bare_title(title: &str) -> String {
    COUNT.replace(title, "").trim().to_string()
}

fn titled(title: &str, count: u32) -> String {
    let bare = bare_title(title);
    if count > 1 { format!("{bare} (x{count})") } else { bare }
}

static COST: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)(\d+(?:\.\d+)?)\s*(k|m)?\s*(hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s|tokens?)\b").unwrap()
});

pub fn minutes(cost: &str) -> f64 {
    COST.captures_iter(cost)
        .map(|c| {
            let n: f64 = c[1].parse().unwrap_or(0.0);
            let unit = c[3].to_lowercase();
            let scale = match c.get(2).map(|m| m.as_str().to_lowercase()) {
                Some(s) if s == "k" => 1_000.0,
                Some(s) if s == "m" && unit.starts_with("token") => 1_000_000.0,
                _ => 1.0,
            };
            if unit.starts_with('h') {
                n * 60.0
            } else if unit.starts_with("token") {
                n * scale / 10_000.0
            } else if unit.starts_with('s') {
                n / 60.0
            } else {
                n
            }
        })
        .sum()
}

const STOP: [&str; 40] = [
    "the", "and", "for", "was", "were", "are", "this", "that", "with", "when", "then", "than", "from", "into", "onto", "one", "again",
    "its", "has", "had", "have", "not", "but", "you", "your", "our", "out", "too", "very", "each", "time", "times", "came", "back",
    "did", "does", "they", "them", "there", "their",
];

fn stem(w: &str) -> String {
    for suffix in ["ing", "ed", "es", "s"] {
        if let Some(s) = w.strip_suffix(suffix)
            && s.len() >= 3
        {
            return s.to_string();
        }
    }
    w.to_string()
}

fn words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() >= 3 && !STOP.contains(w))
        .map(stem)
        .collect()
}

pub fn similar(a: &str, b: &str) -> bool {
    let (a, b) = (words(a), words(b));
    let shared = a.intersection(&b).count() as f64;
    let (small, all) = (a.len().min(b.len()) as f64, a.union(&b).count() as f64);
    if small < 2.0 {
        return false;
    }
    shared / all >= 0.5 || (small >= 4.0 && shared / small >= 0.7)
}

pub fn queue_project(store: &Store, fallback: Option<Id>) -> Option<Id> {
    let projects = store.projects().ok()?;
    let set: Option<Id> = store.setting(PROJECT_SETTING).ok().flatten().and_then(|v| v.trim().parse().ok());
    if let Some(p) = set.and_then(|id| projects.iter().find(|p| p.id == id)) {
        return Some(p.id);
    }
    if let Some(p) = projects.iter().find(|p| !p.builtin() && p.name.eq_ignore_ascii_case("agentbench")) {
        return Some(p.id);
    }
    fallback.and_then(|f| projects.iter().find(|p| p.id == f && !p.builtin())).map(|p| p.id)
}

pub fn open_reports(store: &Store, project: Id) -> Vec<Issue> {
    store
        .issues()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.project_id == project && i.state.is_open() && i.labels.iter().any(|l| l.eq_ignore_ascii_case(LABEL)))
        .collect()
}

fn context(store: &Store, session: Option<&Session>) -> String {
    let Some(s) = session else { return "Reported by the harness.".into() };
    let project = store.project(s.project_id).map(|p| p.name).unwrap_or_default();
    let model = [s.usage.model.as_str(), s.model.as_deref().unwrap_or_default()]
        .into_iter()
        .find(|m| !m.is_empty())
        .unwrap_or("default");
    let issue = s.issue_id.map(|i| format!(", issue #{i}")).unwrap_or_default();
    format!("Session {} ({}, {}) in {project}{issue}, model {model}.", s.id, s.agent, s.title)
}

fn occurrence(store: &Store, r: &Report, session: Option<&Session>) -> String {
    let mut out = format!("{}\n\n{}", r.what_happened.trim(), context(store, session));
    if !r.cost.trim().is_empty() {
        out.push_str(&format!("\nCost: {}", r.cost.trim()));
    }
    if !r.suggestion.trim().is_empty() {
        out.push_str(&format!("\nSuggestion: {}", r.suggestion.trim()));
    }
    if !r.source.is_empty() {
        out.push_str(&format!("\nSource: {}", r.source));
    }
    out
}

pub fn file(store: &Store, r: &Report, session: Option<&Session>) -> Result<Filed, String> {
    let what = r.what_happened.trim();
    if what.is_empty() {
        return Err("missing `what_happened`".into());
    }
    let project = queue_project(store, session.map(|s| s.project_id)).ok_or("no project to file process reports in")?;
    let cat = category(&r.category);
    let title = Some(r.title.trim()).filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(|| {
        let first = what.lines().next().unwrap_or(what);
        let clipped: String = first.chars().take(80).collect();
        clipped.trim_end_matches('.').to_string()
    });
    let text = format!("{title} {what}");
    let found = open_reports(store, project).into_iter().find(|i| {
        let m = meta(store, i);
        (!r.key.is_empty() && m.key == r.key)
            || ((m.category.is_empty() || m.category == cat) && (similar(&text, &m.text) || similar(&title, &bare_title(&i.title))))
    });
    let body = occurrence(store, r, session);
    if let Some(issue) = found {
        let mut m = meta(store, &issue);
        m.count = m.count.max(count_in(&issue.title)) + 1;
        m.minutes += minutes(&r.cost);
        if m.category.is_empty() {
            m.category = cat.into();
        }
        if m.key.is_empty() {
            m.key = r.key.clone();
        }
        store
            .update_issue(issue.id, &IssuePatch { title: Some(titled(&issue.title, m.count)), ..Default::default() })
            .map_err(|e| e.0)?;
        store
            .add_comment(issue.id, &NewComment { author: Some(AUTHOR.into()), body: format!("Seen again (x{}).\n\n{body}", m.count) })
            .map_err(|e| e.0)?;
        save_meta(store, issue.id, &m);
        return Ok(Filed::Bumped(issue.id, m.count));
    }
    let label = CATEGORIES.iter().find(|(k, _)| *k == cat).map_or(cat, |(_, l)| l);
    let issue = store
        .add_issue(&NewIssue {
            project_id: project,
            title: title.clone(),
            body: format!("Category: {label}\n\n{body}"),
            state: Some(IssueState::Backlog),
            labels: vec![LABEL.into()],
            ..Default::default()
        })
        .map_err(|e| e.0)?;
    save_meta(store, issue.id, &Meta { category: cat.into(), key: r.key.clone(), text, count: 1, minutes: minutes(&r.cost) });
    Ok(Filed::New(issue.id))
}

static TURNS: LazyLock<Mutex<HashMap<SessionId, (Id, u32)>>> = LazyLock::new(Default::default);

fn turn_of(store: &Store, session: SessionId) -> Id {
    store
        .messages(session, 0)
        .unwrap_or_default()
        .iter()
        .rev()
        .find(|m| matches!(m.entry, agentbench_core::Entry::User { .. }))
        .map_or(0, |m| m.id)
}

pub fn report(store: &Store, r: &Report, session: &Session) -> Result<Filed, String> {
    let turn = turn_of(store, session.id);
    {
        let mut turns = TURNS.lock().unwrap_or_else(|e| e.into_inner());
        let slot = turns.entry(session.id).or_insert((turn, 0));
        if slot.0 != turn {
            *slot = (turn, 0);
        }
        if slot.1 >= PER_TURN {
            return Err(format!("at most {PER_TURN} process reports per turn; put the rest in one report"));
        }
        slot.1 += 1;
    }
    file(store, r, Some(session))
}

static SEEN: LazyLock<Mutex<HashMap<(SessionId, String), u32>>> = LazyLock::new(Default::default);

pub fn tally(session: SessionId, key: &str) -> u32 {
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let n = seen.entry((session, key.to_string())).or_default();
    *n += 1;
    *n
}

pub fn capture(store: &Store, session: Option<&Session>, key: &str, category: &str, title: &str, what: &str) -> Option<Filed> {
    let r = Report {
        category: category.into(),
        title: title.into(),
        what_happened: what.into(),
        key: key.into(),
        source: "auto-capture".into(),
        ..Default::default()
    };
    match file(store, &r, session) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!("could not file a process report: {e}");
            None
        }
    }
}

pub fn capture_once(store: &Store, session: &Session, key: &str, category: &str, title: &str, what: &str) -> Option<Filed> {
    (tally(session.id, &format!("once:{key}")) == 1).then(|| capture(store, Some(session), key, category, title, what)).flatten()
}

pub fn repeated(store: &Store, session: &Session, key: &str, category: &str, title: &str, what: &str) -> Option<Filed> {
    (tally(session.id, key) == REPEATS).then(|| capture(store, Some(session), key, category, title, what)).flatten()
}

pub fn undelivered(store: &Store, session: &Session, to: SessionId) -> Option<Filed> {
    capture(
        store,
        Some(session),
        "send-message-undelivered",
        "harness_bug",
        "Final send_message to the parent comes back not delivered",
        &format!("A send_message from {} to its parent {to} starting with DONE came back \"not delivered\"; the worker had to put it in its reply instead.", session.id),
    )
}

pub fn restarted(store: &Store, sessions: usize) -> Option<Filed> {
    (sessions > 0)
        .then(|| {
            capture(
                store,
                None,
                "server-restart-stopped-turns",
                "environment",
                "Server restarts stop running agent turns",
                &format!("A server restart stopped {sessions} running turn(s); they were resumed with a restart prompt."),
            )
        })
        .flatten()
}

static TOOL_ERROR: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"[0-9]+").unwrap());

pub fn tool_error(store: &Store, session: &Session, tool: &str, output: &str) -> Option<Filed> {
    if matches!(tool, "bash" | "bash_output") {
        return None;
    }
    let first = output.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
    let shape: String = TOOL_ERROR.replace_all(first, "N").chars().take(80).collect();
    let key = format!("tool-error:{tool}:{shape}");
    repeated(
        store,
        session,
        &key,
        "tool",
        &format!("{tool} keeps failing: {}", shape.trim()),
        &format!("{tool} returned the same error {REPEATS} times in one session: {}", first.chars().take(300).collect::<String>()),
    )
}

pub fn usage(store: &Store, session: &Session, delta: &agentbench_core::Usage) {
    let total = &session.usage;
    if delta.bash_edits > 0 && total.bash_edits >= REPEATS as u64 {
        capture_once(
            store,
            session,
            "warning-bash-edit",
            "tool",
            "Agents keep editing files from bash after the harness note",
            &format!("The \"that command edited a file from bash, use edit_file\" note fired {} times in one session and the agent kept doing it.", total.bash_edits),
        );
    }
    if delta.bash_sleeps > 0 && total.bash_sleeps >= REPEATS as u64 {
        capture_once(
            store,
            session,
            "warning-bash-sleep",
            "tool",
            "Agents keep sleeping in bash after the harness refusal",
            &format!("bash refused a sleep {} times in one session and the agent kept trying.", total.bash_sleeps),
        );
    }
}

pub fn from_reply(store: &Store, session: &Session, reply: &str) -> Vec<Filed> {
    reply
        .lines()
        .filter_map(|l| {
            let l = l.trim().trim_start_matches(['-', '*', '>', ' ']);
            let lower = l.to_lowercase();
            lower.starts_with("process:").then(|| l["process:".len()..].trim().to_string())
        })
        .filter(|t| !t.is_empty())
        .filter(|t| tally(session.id, &format!("reply:{}", crate::memory::hash_note(t))) == 1)
        .take(PER_TURN as usize)
        .filter_map(|t| {
            let r = Report { category: category(&t).into(), what_happened: t, source: "process: line in a reply".into(), ..Default::default() };
            file(store, &r, Some(session)).ok()
        })
        .collect()
}

static CORRECTION: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\bwhy (didn'?t|did not|don'?t|do not|isn'?t|wasn'?t|haven'?t|hasn'?t|are|did) you\b|\bwhy (is|was) (it|this|that) (not|still)\b|\bi thought (that|this|it) was clear\b|\bcome on\b|\bi (already |just )?(told|asked) you\b|\b(as|like) i (said|asked)\b|\bhow many times\b|\bi said\b|(^|[.!?]\s+)(always|never|don'?t ever|stop)\s+\w+",
    )
    .unwrap()
});

pub fn is_correction(text: &str) -> bool {
    CORRECTION.is_match(text)
}

pub fn correction(store: &Store, session: &Session, text: &str) -> Option<Filed> {
    let text = text.trim();
    if text.starts_with('/') || !is_correction(text) {
        return None;
    }
    let clipped: String = text.chars().take(600).collect();
    let title: String = text.lines().next().unwrap_or(text).chars().take(80).collect();
    let r = Report {
        category: "user_correction".into(),
        title: format!("User correction: {}", title.trim()),
        what_happened: format!("The user wrote: \"{clipped}\""),
        suggestion: "Decide where the fix belongs: global AGENTS.md, a supervisor note, a project note, or a guard rule.".into(),
        source: "user message".into(),
        ..Default::default()
    };
    file(store, &r, Some(session)).ok()
}

pub struct Ranked {
    pub issue: Issue,
    pub count: u32,
    pub minutes: f64,
    pub category: String,
}

pub fn ranked(store: &Store, project: Id, since: i64) -> Vec<Ranked> {
    let mut out: Vec<Ranked> = open_reports(store, project)
        .into_iter()
        .filter(|i| i.updated_at >= since)
        .map(|i| {
            let m = meta(store, &i);
            Ranked { count: m.count.max(count_in(&i.title)), minutes: m.minutes, category: m.category, issue: i }
        })
        .collect();
    out.sort_by(|a, b| {
        b.count.cmp(&a.count).then(b.minutes.total_cmp(&a.minutes)).then(a.issue.id.cmp(&b.issue.id))
    });
    out
}

fn note_files(store: &Store) -> Vec<(String, String, std::time::SystemTime)> {
    let root = store.memory_root();
    let supervisor = store.supervisor_project().ok().flatten().map(|p| p.id.to_string());
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(root) else { return out };
    for d in dirs.flatten() {
        let path = d.path();
        let marker = std::fs::read_to_string(path.join(".project")).unwrap_or_default();
        let name = d.file_name().to_string_lossy().into_owned();
        let ours = name == crate::memory::GLOBAL || supervisor.as_deref().is_some_and(|s| marker.trim() == s);
        if !path.is_dir() || !ours {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&path) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let body = std::fs::read_to_string(&p).unwrap_or_default();
            let modified = f.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            out.push((format!("{name}/{stem}"), body, modified));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn note_suggestions(store: &Store, now_ms: i64) -> Vec<String> {
    let notes = note_files(store);
    let mut out = Vec::new();
    let stale = std::time::Duration::from_secs(STALE_NOTE_DAYS * 86_400);
    let now = std::time::UNIX_EPOCH + std::time::Duration::from_millis(now_ms.max(0) as u64);
    for (name, _, modified) in &notes {
        if now.duration_since(*modified).is_ok_and(|age| age >= stale) {
            out.push(format!("{name}: unchanged for over {STALE_NOTE_DAYS} days; check it still holds, or drop it"));
        }
    }
    for (i, (a, body_a, _)) in notes.iter().enumerate() {
        for (b, body_b, _) in &notes[i + 1..] {
            if similar(body_a, body_b) {
                out.push(format!("{a} and {b} look like the same rule; merge them"));
            }
        }
    }
    out
}

pub fn rules_path(store: &Store) -> std::path::PathBuf {
    crate::guards::path(store)
}

pub fn digest(store: &Store, now_ms: i64) -> Option<String> {
    let project = queue_project(store, None)?;
    let since = store.setting(DIGEST_LAST).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
    let items = ranked(store, project, since);
    let notes = note_suggestions(store, now_ms);
    if items.is_empty() && notes.is_empty() {
        return None;
    }
    let mut lines = vec![format!(
        "Daily process digest: {} new or bumped process report(s), ranked by count then cost.",
        items.len()
    )];
    for r in &items {
        let cost = if r.minutes > 0.0 { format!(", about {:.0} min lost", r.minutes) } else { String::new() };
        let guard = if r.count >= 2 { " Make it a guard." } else { "" };
        let cat = if r.category.is_empty() { String::new() } else { format!(", {}", r.category) };
        lines.push(format!("- #{} {} (count {}{cost}{cat}).{guard}", r.issue.id, bare_title(&r.issue.title), r.count));
    }
    if !notes.is_empty() {
        lines.push("Notes to prune:".into());
        lines.extend(notes.iter().map(|n| format!("- {n}")));
    }
    lines.push(format!(
        "End each item in exactly one outcome: a note or AGENTS.md edit, a rule added to {} (guards read only that file), a todo issue with the concrete fix, or closed as noise. Then tell the user in one line what changed.",
        rules_path(store).display()
    ));
    Some(lines.join("\n"))
}

pub fn due(store: &Store, now_ms: i64) -> bool {
    let last: i64 = store.setting(DIGEST_LAST).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
    now_ms - last >= DAY_MS
}

pub fn send_digest(runner: &crate::runner::Runner, now_ms: i64) -> bool {
    let store = runner.store();
    if !due(store, now_ms) {
        return false;
    }
    let Some(sup) = runner.supervisor_session() else { return false };
    let text = digest(store, now_ms);
    let _ = store.set_setting(DIGEST_LAST, &now_ms.to_string());
    match text {
        Some(t) => {
            if let Err(e) = runner.tell(&sup, &t, crate::runner::Origin::new("process", None)) {
                tracing::warn!("the process digest waits for the supervisor: {}", e.0);
            }
            true
        }
        None => false,
    }
}

pub fn spawn(runner: crate::runner::Runner) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            let r = runner.clone();
            let _ = tokio::task::spawn_blocking(move || send_digest(&r, now())).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing;

    fn setup() -> (Store, Session) {
        let s = Store::memory().unwrap();
        let (_, main) = testing::project(&s, "/a");
        s.add_project("agentbench", "/ab", Some("main"), None).unwrap();
        let session = s.add_session(main.id, "native", "w", None, None).unwrap();
        (s, session)
    }

    fn rep(what: &str, cost: &str) -> Report {
        Report { category: "harness bug".into(), what_happened: what.into(), cost: cost.into(), ..Default::default() }
    }

    fn agentbench(s: &Store) -> Id {
        s.projects().unwrap().into_iter().find(|p| p.name == "agentbench").unwrap().id
    }

    #[test]
    fn a_report_files_a_labelled_backlog_issue_with_no_goal_in_the_agentbench_project() {
        let (s, w) = setup();
        let f = report(&s, &rep("send_message DONE to the parent came back not delivered", "20 min"), &w).unwrap();
        let i = s.issue(f.id()).unwrap();
        assert_eq!((i.state, i.goal.clone(), i.labels.clone()), (IssueState::Backlog, None, vec![LABEL.to_string()]));
        assert_eq!(i.project_id, agentbench(&s));
        assert!(i.body.contains(&w.id.to_string()) && i.body.contains("harness bug") && i.body.contains("20 min"), "{}", i.body);
    }

    #[test]
    fn a_duplicate_report_bumps_the_count_instead_of_filing_again() {
        let (s, w) = setup();
        let first = report(&s, &rep("send_message DONE to the parent came back not delivered", "10 min"), &w).unwrap();
        let again = file(&s, &rep("The DONE send_message to my parent was not delivered", "1 hour"), Some(&w)).unwrap();
        assert_eq!(again, Filed::Bumped(first.id(), 2));
        let i = s.issue(first.id()).unwrap();
        assert!(i.title.ends_with("(x2)"), "{}", i.title);
        assert_eq!(s.comments(i.id).unwrap().len(), 1);
        assert_eq!(open_reports(&s, agentbench(&s)).len(), 1);
        let other = file(&s, &rep("tp tests fail when run in parallel on one GPU", ""), Some(&w)).unwrap();
        assert!(matches!(other, Filed::New(_)));
        let r = ranked(&s, agentbench(&s), 0);
        assert_eq!(r[0].issue.id, first.id());
        assert!((r[0].minutes - 70.0).abs() < 0.01);
    }

    #[test]
    fn the_rate_limit_holds_per_turn() {
        let (s, w) = setup();
        let topics = ["alpha ports collide", "bravo credentials missing", "charlie brief unclear", "delta model looped"];
        for t in &topics[..3] {
            report(&s, &rep(&format!("{t} again and again here"), ""), &w).unwrap();
        }
        let e = report(&s, &rep("delta model looped again and again here", ""), &w).unwrap_err();
        assert!(e.contains("at most 3"), "{e}");
        s.add_message(w.id, &agentbench_core::Entry::User { text: "next".into(), attachments: vec![], from: vec![] }).unwrap();
        assert!(report(&s, &rep("delta model looped again and again here", ""), &w).is_ok(), "a new turn resets it");
    }

    #[test]
    fn a_repeated_warning_files_once_at_the_threshold() {
        let (s, w) = setup();
        let mut w = w;
        for n in 1..=5u64 {
            w.usage.bash_edits = n;
            usage(&s, &w, &agentbench_core::Usage { bash_edits: 1, ..Default::default() });
            let filed = open_reports(&s, agentbench(&s)).len();
            assert_eq!(filed, usize::from(n >= 3), "after {n}");
        }
        let i = &open_reports(&s, agentbench(&s))[0];
        assert!(!i.title.contains("(x"), "{}", i.title);
    }

    #[test]
    fn a_tool_error_repeated_three_times_files_once() {
        let (s, w) = setup();
        for n in 0..4 {
            tool_error(&s, &w, "edit_file", &format!("error: no match at line {}", 7000 + n));
        }
        assert!(tool_error(&s, &w, "bash", "exit 1").is_none() && tool_error(&s, &w, "bash", "exit 1").is_none() && tool_error(&s, &w, "bash", "exit 1").is_none(), "a failing command is not a harness error");
        let open = open_reports(&s, agentbench(&s));
        assert_eq!(open.len(), 1);
        assert!(open[0].title.contains("edit_file keeps failing"), "{}", open[0].title);
    }

    #[test]
    fn a_correction_files_one_report_and_plain_messages_none() {
        let (s, w) = setup();
        assert!(correction(&s, &w, "please add a dark mode toggle").is_none());
        assert!(correction(&s, &w, "/compact").is_none());
        let f = correction(&s, &w, "Why didn't you run the tests? I thought that was clear.").unwrap();
        let i = s.issue(f.id()).unwrap();
        assert!(i.title.starts_with("User correction"), "{}", i.title);
        assert!(i.body.contains("user correction"));
        assert_eq!(open_reports(&s, agentbench(&s)).len(), 1);
        assert!(is_correction("Never push to main from a worker."));
        assert!(is_correction("come on, use edit_file"));
    }

    #[test]
    fn a_process_line_in_a_reply_is_filed_once() {
        let (s, w) = setup();
        let reply = "DONE #3 tests:pass ok\nprocess: the tp tests fail when run in parallel on one GPU, cost 30 min";
        assert_eq!(from_reply(&s, &w, reply).len(), 1);
        assert!(from_reply(&s, &w, reply).is_empty(), "the same reply is not filed twice");
    }

    #[test]
    fn the_digest_ranks_by_count_then_cost_and_marks_repeats_for_a_guard() {
        let (s, w) = setup();
        let small = file(&s, &rep("ports collide between worker test servers", "5 min"), Some(&w)).unwrap();
        let big = file(&s, &rep("a worker on the default model looped for hours", "2.5 hours"), Some(&w)).unwrap();
        let twice = file(&s, &rep("send_message DONE to the parent came back not delivered", "1 min"), Some(&w)).unwrap();
        file(&s, &rep("The DONE send_message to the parent was not delivered", "1 min"), Some(&w)).unwrap();
        let d = digest(&s, now()).unwrap();
        let pos = |id: Id| d.find(&format!("#{id} ")).unwrap();
        assert!(pos(twice.id()) < pos(big.id()) && pos(big.id()) < pos(small.id()), "{d}");
        let line = d.lines().find(|l| l.contains(&format!("#{} ", twice.id()))).unwrap();
        assert!(line.contains("Make it a guard"), "{line}");
        assert!(!d.lines().find(|l| l.contains(&format!("#{} ", big.id()))).unwrap().contains("guard"));
        assert!(d.contains("guards.toml") && d.contains("closed as noise"));
    }

    #[test]
    fn the_digest_is_sent_to_the_supervisor_once_a_day() {
        let (s, w) = setup();
        let dir = tempfile::tempdir().unwrap();
        let sup = crate::projects::ensure_supervisor(&s, &dir.path().join("sup")).unwrap();
        let boss = s.add_session(s.main_worktree(sup.id).unwrap().id, "native", "boss", None, None).unwrap();
        let runner = crate::runner::Runner::new(s.clone(), vec![]);
        let t = now();
        assert!(!send_digest(&runner, t), "nothing to say yet");
        file(&s, &rep("ports collide between worker test servers", "5 min"), Some(&w)).unwrap();
        assert!(!send_digest(&runner, t + 1000), "not a day since the last one");
        assert!(send_digest(&runner, t + DAY_MS));
        assert!(!send_digest(&runner, t + DAY_MS + 1000), "once a day");
        let told = s.deferred(boss.id).unwrap();
        assert_eq!(told.len(), 1);
        assert!(told[0].text.starts_with("Daily process digest"), "{}", told[0].text);
    }

    #[test]
    fn minutes_reads_rough_costs() {
        assert_eq!(minutes("2.5 hours"), 150.0);
        assert_eq!(minutes("about 20 min"), 20.0);
        assert_eq!(minutes("50k tokens"), 5.0);
        assert_eq!(minutes(""), 0.0);
    }

    #[test]
    fn process_issues_are_never_picked_by_the_autopilot() {
        let (s, w) = setup();
        let f = report(&s, &rep("ports collide between worker test servers", ""), &w).unwrap();
        let issues = s.issues().unwrap();
        let enabled = |_: Id| true;
        let vision = |_: Id| Some(agentbench_core::Vision::default());
        let live = HashSet::new();
        let by = HashMap::new();
        let room = crate::autopilot::Room {
            enabled: &enabled,
            git_repo: &enabled,
            vision: &vision,
            live: &live,
            running_by_project: &by,
            running: 0,
            cap: 10,
            project_cap: 10,
        };
        assert!(crate::autopilot::pick(&issues, &room).issues.iter().all(|i| i.id != f.id()));
        s.update_issue(f.id(), &IssuePatch { state: Some(IssueState::Todo), ..Default::default() }).unwrap();
        let issues = s.issues().unwrap();
        assert!(crate::autopilot::pick(&issues, &room).issues.iter().all(|i| i.id != f.id()), "no goal, still not picked");
    }
}
