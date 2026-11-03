use crate::runner::Runner;
use crate::store::{Store, StoreError, Worker};
use agentbench_core::{
    Automation, AutopilotPatch, AutopilotState, Entry, Id, Issue, IssuePatch, IssueState,
    NewComment, NewSession, Place, Project, ProjectPatch, Risky, Session, SessionStatus,
    issue_prompt,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ENABLED: &str = "autopilot";
const CAP: &str = "autopilot_cap";
const PROJECT_CAP: &str = "autopilot_project_cap";
const STUCK_MINUTES: &str = "autopilot_stuck_minutes";
const MAX_NUDGES: &str = "autopilot_max_nudges";
const MERGE: &str = "autopilot_merge";
const IDLE: &str = "autopilot_idle";
const AGENT: &str = "autopilot_agent";
const DEFAULT_LEVEL: &str = "automation_default";
const MIGRATED: &str = "automation_migrated";

pub const DEFAULT_CAP: u32 = 4;
pub const DEFAULT_PROJECT_CAP: u32 = 2;
pub const DEFAULT_STUCK_MINUTES: u64 = 15;
pub const DEFAULT_MAX_NUDGES: u32 = 2;
const GRACE_MINUTES: &str = "autopilot_grace_minutes";
pub const DEFAULT_GRACE_MINUTES: u64 = 30;
const MAX_REWORKS: u32 = 3;
const MAX_STUCK_RESTARTS: u32 = 2;
const TICK_EVERY: Duration = Duration::from_secs(30);
const DEPLOY_LIMIT: Duration = Duration::from_secs(20 * 60);
const NEEDS_USER: &str = "needs-user";
const BLOCKED: &str = "blocked";
const AUTHOR: &str = "autopilot";

fn flag(store: &Store, key: &str) -> bool {
    store.setting(key).ok().flatten().as_deref() == Some("1")
}

fn number<T: std::str::FromStr>(store: &Store, key: &str, default: T) -> T {
    store.setting(key).ok().flatten().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn project_key(id: Id) -> String {
    format!("autopilot_project_{id}")
}

fn old_override(store: &Store, id: Id) -> Option<bool> {
    match store.setting(&project_key(id)).ok().flatten().as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

pub fn default_level(store: &Store) -> Automation {
    store
        .setting(DEFAULT_LEVEL)
        .ok()
        .flatten()
        .and_then(|v| Automation::parse(&v))
        .unwrap_or_default()
}

pub fn level_of(store: &Store, project: &Project) -> Automation {
    project.level(default_level(store))
}

pub fn level(store: &Store, project: Id) -> Automation {
    store.project(project).map(|p| level_of(store, &p)).unwrap_or_else(|_| default_level(store))
}

pub fn allows(store: &Store, project: Id, action: Risky) -> bool {
    store.project(project).is_ok_and(|p| p.allows(default_level(store), action))
}

pub fn enabled_for(store: &Store, project: Id) -> bool {
    flag(store, ENABLED) && level(store, project) >= Automation::Autowork
}

pub fn migrate(store: &Store) -> Result<(), StoreError> {
    if flag(store, MIGRATED) {
        return Ok(());
    }
    let global = flag(store, ENABLED);
    let running = if flag(store, MERGE) { Automation::Ship } else { Automation::Autowork };
    let projects: Vec<Project> =
        store.projects()?.into_iter().filter(|p| !p.builtin()).collect();
    let mut any_on = false;
    for p in &projects {
        let set = match (old_override(store, p.id), global) {
            (Some(true), _) => Some(running),
            (Some(false), true) => Some(Automation::Delegate),
            _ => None,
        };
        any_on |= set == Some(running);
        if let Some(level) = set
            && p.automation.is_none()
        {
            store.update_project(
                p.id,
                &ProjectPatch { automation: Some(Some(level)), ..Default::default() },
            )?;
        }
        store.delete_setting(&project_key(p.id))?;
    }
    if global {
        store.set_setting(DEFAULT_LEVEL, running.as_str())?;
    }
    if any_on {
        store.set_setting(ENABLED, "1")?;
    }
    store.delete_setting(MERGE)?;
    store.set_setting(MIGRATED, "1")
}

pub fn state(store: &Store) -> AutopilotState {
    let workers = store.workers().unwrap_or_default();
    AutopilotState {
        enabled: flag(store, ENABLED),
        cap: number(store, CAP, DEFAULT_CAP),
        project_cap: number(store, PROJECT_CAP, DEFAULT_PROJECT_CAP),
        stuck_minutes: number(store, STUCK_MINUTES, DEFAULT_STUCK_MINUTES),
        max_nudges: number(store, MAX_NUDGES, DEFAULT_MAX_NUDGES),
        default_level: default_level(store),
        idle: flag(store, IDLE),
        workers: workers.iter().filter(|w| is_active(&w.state)).count() as u32,
        activity: store.autopilot_log(30).unwrap_or_default(),
    }
}

pub fn apply(store: &Store, patch: &AutopilotPatch) -> Result<AutopilotState, StoreError> {
    if let Some(e) = patch.enabled {
        store.set_setting(ENABLED, if e { "1" } else { "0" })?;
        store.set_setting(IDLE, "0")?;
        store.log_autopilot(None, None, if e { "autopilot resumed" } else { "autopilot paused" })?;
    }
    if let Some(n) = patch.cap {
        store.set_setting(CAP, &n.to_string())?;
    }
    if let Some(n) = patch.project_cap {
        store.set_setting(PROJECT_CAP, &n.to_string())?;
    }
    if let Some(n) = patch.stuck_minutes {
        store.set_setting(STUCK_MINUTES, &n.to_string())?;
    }
    if let Some(n) = patch.max_nudges {
        store.set_setting(MAX_NUDGES, &n.to_string())?;
    }
    if let Some(l) = patch.default_level {
        store.set_setting(DEFAULT_LEVEL, l.as_str())?;
        store.set_setting(IDLE, "0")?;
        store.log_autopilot(None, None, &format!("default automation level set to {}", l.label()))?;
    }
    Ok(state(store))
}

pub fn set_project(
    store: &Store,
    id: Id,
    patch: &ProjectPatch,
) -> Result<Project, StoreError> {
    let before = store.project(id)?;
    let after = store.update_project(id, patch)?;
    let default = default_level(store);
    if before.automation != after.automation {
        let word = match after.automation {
            Some(l) => l.label().to_string(),
            None => format!("the default ({})", default.label()),
        };
        store.set_setting(IDLE, "0")?;
        store.log_autopilot(Some(id), None, &format!("{} set to {word}", after.name))?;
    }
    if before.risky != after.risky {
        let on: Vec<&str> = after.risky.iter().map(|r| r.label()).collect();
        let text = if on.is_empty() { "none".to_string() } else { on.join(", ") };
        store.log_autopilot(Some(id), None, &format!("{} risky actions: {text}", after.name))?;
    }
    Ok(after)
}

pub fn summary(store: &Store) -> String {
    let default = default_level(store);
    let mut out = format!(
        "Global default: {} ({}). Autopilot loop: {}.\n",
        default.label(),
        default.about(),
        if flag(store, ENABLED) { "on" } else { "paused" }
    );
    for p in store.projects().unwrap_or_default().iter().filter(|p| !p.builtin()) {
        let own = if p.automation.is_some() { "" } else { ", the default" };
        let risky: Vec<&str> = p.risky.iter().map(|r| r.as_str()).collect();
        let risky = if risky.is_empty() { "none".to_string() } else { risky.join(", ") };
        out.push_str(&format!(
            "- {} (id {}): {}{own}; risky actions on: {risky}\n",
            p.name,
            p.id,
            p.level(default).label()
        ));
    }
    out
}

fn is_active(state: &str) -> bool {
    matches!(state, "working" | "asking")
}

pub struct Picked {
    pub issues: Vec<Issue>,
    pub skipped: Vec<(Id, &'static str)>,
}

pub struct Room<'a> {
    pub enabled: &'a dyn Fn(Id) -> bool,
    pub git_repo: &'a dyn Fn(Id) -> bool,
    pub live: &'a HashSet<Id>,
    pub running_by_project: &'a HashMap<Id, u32>,
    pub running: u32,
    pub cap: u32,
    pub project_cap: u32,
}

pub fn pick(issues: &[Issue], room: &Room) -> Picked {
    let mut todo: Vec<&Issue> = issues
        .iter()
        .filter(|i| i.state == IssueState::Todo && (room.enabled)(i.project_id))
        .collect();
    todo.sort_by(|a, b| {
        b.priority.cmp(&a.priority).then(a.created_at.cmp(&b.created_at)).then(a.id.cmp(&b.id))
    });
    let mut by_project = room.running_by_project.clone();
    let mut running = room.running;
    let mut out = Picked { issues: Vec::new(), skipped: Vec::new() };
    for i in todo {
        let has = |l: &str| i.labels.iter().any(|x| x.eq_ignore_ascii_case(l));
        if has(NEEDS_USER) {
            out.skipped.push((i.id, "labelled needs-user"));
        } else if has(BLOCKED) {
            out.skipped.push((i.id, "labelled blocked"));
        } else if room.live.contains(&i.id) {
            out.skipped.push((i.id, "it already has a live session"));
        } else if !(room.git_repo)(i.project_id) {
            out.skipped.push((i.id, "its project is not a git repo"));
        } else if running >= room.cap || *by_project.get(&i.project_id).unwrap_or(&0) >= room.project_cap
        {
            continue;
        } else {
            running += 1;
            *by_project.entry(i.project_id).or_default() += 1;
            out.issues.push(i.clone());
        }
    }
    out
}

#[derive(Debug, PartialEq)]
pub enum Verdict {
    Concluded { green: bool },
    Leftover(Vec<String>),
    Blocked { reason: String, user: bool },
    Asked(String),
    Empty,
}

const USER_ONLY: [&str; 24] = [
    "secret",
    "credential",
    "api key",
    "password",
    "token",
    "decision",
    "decide",
    "hardware",
    "device",
    "deploy",
    "install on",
    "send email",
    "send an email",
    "send a message",
    "external message",
    "spend",
    "money",
    "purchase",
    "permission",
    "approval",
    "the user",
    "user's",
    "out of scope",
    "access to",
];

pub fn needs_user(text: &str) -> bool {
    let t = text.to_lowercase();
    USER_ONLY.iter().any(|k| t.contains(k))
}

const LEFTOVER_HEADS: [&str; 13] = [
    "needs more work",
    "not done",
    "what is left",
    "todo",
    "to do",
    "to-do",
    "next steps",
    "remaining",
    "left to do",
    "follow-up",
    "follow-ups",
    "follow ups",
    "still to do",
];

fn bullet(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with("- [x]") || t.starts_with("* [x]") {
        return None;
    }
    for p in ["- [ ]", "* [ ]", "- ", "* ", "• "] {
        if let Some(rest) = t.strip_prefix(p) {
            return Some(rest.trim());
        }
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && t[digits..].starts_with(". ") {
        return Some(t[digits + 2..].trim());
    }
    None
}

fn head_of(line: &str) -> bool {
    let t = line.trim().trim_start_matches(['#', '*', '`', ' ']).trim_end_matches([':', '*', '`', ' ']);
    let t = t.to_lowercase();
    LEFTOVER_HEADS.iter().any(|h| t == *h || t.starts_with(&format!("{h} ")) || t.starts_with(&format!("{h}:")))
}

pub fn judge(reply: &str) -> Verdict {
    let reply = reply.trim();
    if reply.is_empty() {
        return Verdict::Empty;
    }
    let lines: Vec<&str> = reply.lines().collect();
    for line in &lines {
        let t = line.trim().trim_start_matches(['*', '`', '#', ' ']);
        if let Some(rest) = t.strip_prefix("BLOCKED") {
            let reason = rest.trim_start_matches(|c: char| c == '#' || c.is_ascii_digit() || c == ' ');
            let reason = reason.trim().to_string();
            return Verdict::Blocked { user: needs_user(&reason), reason };
        }
    }
    let mut items = Vec::new();
    let mut under_head = false;
    for line in &lines {
        if head_of(line) {
            under_head = true;
            continue;
        }
        match bullet(line) {
            Some(item) if under_head || line.trim_start().contains("[ ]") => {
                items.push(item.to_string())
            }
            Some(_) => {}
            None if line.trim().is_empty() => {}
            None => under_head = false,
        }
    }
    if !items.is_empty() {
        let doable: Vec<String> = items.iter().filter(|i| !needs_user(i)).cloned().collect();
        return if doable.is_empty() {
            Verdict::Blocked { reason: items.join("; "), user: true }
        } else {
            Verdict::Leftover(doable)
        };
    }
    let last = lines.iter().rev().find(|l| !l.trim().is_empty()).copied().unwrap_or("");
    if last.trim_end().ends_with('?') && !reply.contains("DONE #") {
        return Verdict::Asked(last.trim().to_string());
    }
    let lower = reply.to_lowercase();
    let done_line = lower.lines().rev().find(|l| l.contains("done #") && l.contains("tests:"));
    if let Some(l) = done_line {
        if l.contains("tests:pass") {
            return Verdict::Concluded { green: true };
        }
        if l.contains("tests:fail") {
            return Verdict::Concluded { green: false };
        }
    }
    let failed = lower.contains("tests:fail")
        || lower.contains("tests fail")
        || lower.contains("test failed")
        || lower.contains("tests failed")
        || lower.contains("failing");
    let passed = lower.contains("tests:pass")
        || lower.contains("tests pass")
        || lower.contains("all tests")
        || lower.contains("passed")
        || lower.contains("passing");
    Verdict::Concluded { green: passed && !failed }
}

fn worker_prompt(issue: &Issue, comments: &[agentbench_core::Comment]) -> String {
    format!(
        "{}\nThis is an unattended autopilot run. Stay in your worktree and on this issue, commit your work on your branch, and run the tests. \
         Finish the work: do every test, verification and follow-up you can do yourself before you reply. Your last reply must be a conclusion: \
         either rigid (verified, tests pass, nothing left) or needs more work, listing only what you genuinely could not do \
         (a user decision, credentials, hardware or a real-world action, or out of scope). Never end with a todo list of things you could have done. \
         Never deploy, send messages, spend money or touch secrets: say what is needed instead. \
         End with one line: `DONE #{} tests:<pass|fail> <one-line result>`, or `BLOCKED #{} <reason>` if you cannot go on.",
        issue_prompt(issue, comments),
        issue.id,
        issue.id,
    )
}

fn last_reply(store: &Store, session: agentbench_core::SessionId) -> String {
    agentbench_core::shown(&store.messages(session, 0).unwrap_or_default())
        .into_iter()
        .rev()
        .find_map(|m| match m.entry {
            Entry::Assistant { text } => Some(text),
            _ => None,
        })
        .unwrap_or_default()
}

fn last_error(store: &Store, session: agentbench_core::SessionId) -> String {
    agentbench_core::shown(&store.messages(session, 0).unwrap_or_default())
        .into_iter()
        .rev()
        .find_map(|m| match m.entry {
            Entry::Error { text } => Some(text),
            _ => None,
        })
        .unwrap_or_else(|| "the session failed".into())
}

fn clip(text: &str, max: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    format!("{}...", t.chars().take(max).collect::<String>())
}

#[derive(Clone)]
pub struct Autopilot {
    runner: Runner,
    said: Arc<Mutex<HashSet<String>>>,
}

impl Autopilot {
    pub fn new(runner: Runner) -> Autopilot {
        Autopilot { runner, said: Default::default() }
    }

    fn store(&self) -> &Store {
        self.runner.store()
    }

    fn log(&self, project: Option<Id>, issue: Option<Id>, text: &str) {
        let _ = self.store().log_autopilot(project, issue, text);
        self.runner.changed.notify_waiters();
    }

    fn log_once(&self, project: Option<Id>, issue: Option<Id>, text: &str) {
        let key = format!("{issue:?}/{text}");
        if self.said.lock().unwrap_or_else(|e| e.into_inner()).insert(key) {
            self.log(project, issue, text);
        }
    }

    fn comment(&self, issue: Id, body: &str) {
        let _ = self.store().add_comment(
            issue,
            &NewComment { author: Some(AUTHOR.into()), body: body.to_string() },
        );
    }

    fn notify(&self, text: &str) {
        crate::tools::desktop_notice("autopilot", text);
        if let Some(s) = self.runner.supervisor_session() {
            let _ =
                self.store().add_message(s.id, &Entry::Notice { text: format!("notice: {text}") });
        }
    }

    fn set_state(&self, issue: Id, state: IssueState) {
        let _ = self
            .store()
            .update_issue(issue, &IssuePatch { state: Some(state), ..Default::default() });
    }

    fn add_labels(&self, issue: Id, add: &[&str]) -> bool {
        let Ok(i) = self.store().issue(issue) else { return false };
        let mut labels = i.labels.clone();
        for a in add {
            if !labels.iter().any(|l| l == a) {
                labels.push(a.to_string());
            }
        }
        if labels == i.labels {
            return false;
        }
        let _ = self
            .store()
            .update_issue(issue, &IssuePatch { labels: Some(labels), ..Default::default() });
        true
    }

    fn drop_labels(&self, issue: Id, remove: &[&str]) {
        let Ok(i) = self.store().issue(issue) else { return };
        let labels: Vec<String> =
            i.labels.iter().filter(|l| !remove.contains(&l.as_str())).cloned().collect();
        if labels != i.labels {
            let _ = self
                .store()
                .update_issue(issue, &IssuePatch { labels: Some(labels), ..Default::default() });
        }
    }

    fn block(&self, w: &Worker, reason: &str, user: bool) {
        let labels: &[&str] = if user { &[BLOCKED, NEEDS_USER] } else { &[BLOCKED] };
        let fresh = self.add_labels(w.issue, labels);
        self.comment(
            w.issue,
            &format!(
                "autopilot: blocked{} (session {}): {}",
                if user { ", needs you" } else { "" },
                w.session,
                clip(reason, 1500)
            ),
        );
        self.log(
            Some(w.project),
            Some(w.issue),
            &format!("blocked #{}{}: {}", w.issue, if user { " (needs you)" } else { "" }, clip(reason, 200)),
        );
        if user && fresh {
            self.notify(&format!("#{} needs you: {}", w.issue, clip(reason, 200)));
        }
    }

    pub fn tick(&self) {
        self.tick_at(crate::store::now());
    }

    pub fn tick_at(&self, now: i64) {
        let store = self.store();
        let workers = store.workers().unwrap_or_default();
        if !flag(store, ENABLED) && workers.is_empty() {
            return;
        }
        for w in &workers {
            self.reconcile(w, now);
        }
        self.sweep(now);
        self.start_more();
    }

    fn sweep(&self, now: i64) {
        let store = self.store();
        if !flag(store, ENABLED) {
            return;
        }
        let supervisor = store.supervisor_project().ok().flatten().map(|p| p.id);
        let tracked: HashSet<_> = store.workers().unwrap_or_default().iter().map(|w| w.session).collect();
        let sessions = store.sessions().unwrap_or_default();
        for s in &sessions {
            if Some(s.project_id) == supervisor
                || tracked.contains(&s.id)
                || self.runner.is_running(s.id)
                || s.status == SessionStatus::Waiting
                || level(store, s.project_id) < Automation::Ship
            {
                continue;
            }
            let reason = match s.issue_id {
                Some(i) => match store.issue(i) {
                    Ok(issue) if issue.state == IssueState::Done && self.branch_is_merged(s) => {
                        format!("#{i} is done and its branch is merged")
                    }
                    _ => continue,
                },
                None => {
                    let grace = number(store, GRACE_MINUTES, DEFAULT_GRACE_MINUTES) as i64 * 60_000;
                    let finished = matches!(s.status, SessionStatus::Idle)
                        && now - s.updated_at >= grace
                        && matches!(judge(&last_reply(store, s.id)), Verdict::Concluded { .. });
                    if !finished || !self.branch_is_merged_or_none(s) {
                        continue;
                    }
                    "it finished with a conclusion and nothing is pending".to_string()
                }
            };
            self.close_finished(s, &reason);
        }
    }

    fn branch_is_merged(&self, s: &Session) -> bool {
        let store = self.store();
        let (Ok(project), Ok(tree)) = (store.project(s.project_id), store.worktree(s.worktree_id)) else {
            return false;
        };
        match tree.branch.as_deref().filter(|_| !tree.main) {
            Some(b) => crate::git::is_merged(Path::new(&project.path), b),
            None => false,
        }
    }

    fn branch_is_merged_or_none(&self, s: &Session) -> bool {
        match self.store().worktree(s.worktree_id) {
            Ok(tree) if tree.main || tree.branch.is_none() => true,
            Ok(_) => self.branch_is_merged(s),
            Err(_) => false,
        }
    }

    fn close_finished(&self, s: &Session, reason: &str) {
        let store = self.store();
        let Ok(tree) = store.worktree(s.worktree_id) else { return };
        let others_busy = store
            .sessions()
            .unwrap_or_default()
            .iter()
            .any(|o| o.id != s.id && o.worktree_id == s.worktree_id && self.runner.is_running(o.id));
        if others_busy {
            return;
        }
        let project = store.project(s.project_id).ok();
        let removal = if tree.main {
            Ok(())
        } else {
            crate::projects::remove_worktree(store, tree.id, false)
        };
        if let Err(e) = removal {
            self.log_once(
                Some(s.project_id),
                s.issue_id,
                &format!("could not clean up {}: {}", tree.label(), clip(&e.0, 160)),
            );
            return;
        }
        self.runner.end(s.id);
        let _ = store.delete_session(s.id);
        if let (Some(b), Some(p), false) = (tree.branch.as_deref(), &project, tree.main) {
            let _ = crate::git::delete_branch(Path::new(&p.path), b);
        }
        self.log(
            Some(s.project_id),
            s.issue_id,
            &format!(
                "closed session {} ({}){}",
                s.id,
                reason,
                if tree.main { String::new() } else { format!(" and removed worktree {}", tree.label()) }
            ),
        );
    }

    fn reconcile(&self, w: &Worker, now: i64) {
        let store = self.store();
        let Ok(session) = store.session(w.session) else {
            let _ = store.remove_worker(w.session);
            return;
        };
        let Ok(issue) = store.issue(w.issue) else {
            let _ = store.remove_worker(w.session);
            return;
        };
        match w.state.as_str() {
            "working" | "asking" => {
                if self.runner.is_running(w.session) {
                    self.watch(w, &session, now);
                } else {
                    self.finished(w, &session);
                }
            }
            "ready" => {
                if issue.state == IssueState::Done {
                    let _ = store.remove_worker(w.session);
                } else if w.green && flag(store, ENABLED) && level(store, w.project) >= Automation::Ship {
                    self.merge(w, &session);
                }
            }
            "merge_failed" => {
                if issue.state == IssueState::Done {
                    let _ = store.remove_worker(w.session);
                }
            }
            _ => {}
        }
    }

    fn watch(&self, w: &Worker, session: &Session, now: i64) {
        let store = self.store();
        if session.status == SessionStatus::Waiting {
            if w.state != "asking" {
                let _ = store.set_worker(w.session, "asking", false);
                self.block(w, "the session is waiting for an answer from you", true);
            }
            return;
        }
        if w.state == "asking" {
            let _ = store.set_worker(w.session, "working", false);
            self.drop_labels(w.issue, &[BLOCKED, NEEDS_USER]);
            self.log(Some(w.project), Some(w.issue), &format!("#{} answered, working again", w.issue));
        }
        let stuck_ms = number(store, STUCK_MINUTES, DEFAULT_STUCK_MINUTES) as i64 * 60_000;
        if now - session.updated_at < stuck_ms {
            return;
        }
        let minutes = stuck_ms / 60_000;
        if w.nudges < number(store, MAX_NUDGES, DEFAULT_MAX_NUDGES) {
            let _ = store.bump_worker(w.session, "nudges");
            let text = format!(
                "Autopilot: no output from you for {minutes} minutes. If something blocks you, say what; otherwise carry on and finish with a conclusion."
            );
            let _ = self.runner.send(session, &text);
            self.log(
                Some(w.project),
                Some(w.issue),
                &format!("nudged #{} after {minutes} minutes of silence ({} of {})", w.issue, w.nudges + 1, number(store, MAX_NUDGES, DEFAULT_MAX_NUDGES)),
            );
            return;
        }
        self.runner.interrupt(w.session);
        let _ = store.remove_worker(w.session);
        let key = format!("autopilot_stuck_{}", w.issue);
        let restarts = number::<u32>(store, &key, 0) + 1;
        let _ = store.set_setting(&key, &restarts.to_string());
        self.comment(
            w.issue,
            &format!("autopilot: session {} stopped after {} nudges with no output for {minutes} minutes.", w.session, w.nudges),
        );
        if restarts >= MAX_STUCK_RESTARTS {
            self.set_state(w.issue, IssueState::Todo);
            self.add_labels(w.issue, &[BLOCKED]);
            self.comment(w.issue, "autopilot: stuck twice, labelled blocked so it is not started again.");
            self.notify(&format!("#{} got stuck twice and is blocked", w.issue));
        } else {
            self.set_state(w.issue, IssueState::Todo);
        }
        self.log(
            Some(w.project),
            Some(w.issue),
            &format!("stopped stuck session on #{} and returned it to todo", w.issue),
        );
    }

    fn finished(&self, w: &Worker, session: &Session) {
        let store = self.store();
        match session.status {
            SessionStatus::Failed => {
                self.block(w, &last_error(store, w.session), false);
                let _ = store.remove_worker(w.session);
                return;
            }
            SessionStatus::Stopped => {
                self.block(w, "the session was stopped before it finished", false);
                let _ = store.remove_worker(w.session);
                return;
            }
            _ => {}
        }
        let reply = last_reply(store, w.session);
        match judge(&reply) {
            Verdict::Concluded { green } => {
                self.set_state(w.issue, IssueState::Review);
                let _ = store.set_worker(w.session, "ready", green);
                self.comment(
                    w.issue,
                    &format!("autopilot: session {} concluded{}.\n\n{}", w.session, if green { " with tests passing" } else { " without a green result" }, clip(&reply, 1200)),
                );
                self.log(
                    Some(w.project),
                    Some(w.issue),
                    &format!(
                        "#{} finished, moved to review{}",
                        w.issue,
                        if green { "" } else { " (no green result, will not merge)" }
                    ),
                );
                if !green || level(store, w.project) < Automation::Ship {
                    self.notify(&format!("#{} is ready for review", w.issue));
                }
            }
            Verdict::Leftover(items) => self.rework(
                w,
                session,
                &format!("your last reply lists work you can do yourself: {}", items.join("; ")),
            ),
            Verdict::Empty => self.rework(w, session, "your last reply was empty"),
            Verdict::Asked(q) => {
                self.block(w, &format!("the session asked: {q}"), true);
                let _ = store.set_worker(w.session, "asking", false);
            }
            Verdict::Blocked { reason, user } => {
                self.block(w, &reason, user);
                let _ = store.remove_worker(w.session);
            }
        }
    }

    fn rework(&self, w: &Worker, session: &Session, why: &str) {
        let store = self.store();
        if w.reworks >= MAX_REWORKS {
            self.block(w, &format!("no conclusion after {MAX_REWORKS} reminders: {why}"), false);
            let _ = store.remove_worker(w.session);
            return;
        }
        let _ = store.bump_worker(w.session, "reworks");
        let text = format!(
            "Conclusion check: {why}. Do it now: run the tests, write the missing test, verify, and finish what you can do yourself. \
             Then end with a conclusion: rigid, or needs more work listing only what you genuinely cannot do, and a final `DONE #{} tests:<pass|fail> <result>` line.",
            w.issue
        );
        match self.runner.send(session, &text) {
            Ok(()) => self.log(
                Some(w.project),
                Some(w.issue),
                &format!("sent #{} back to finish: {}", w.issue, clip(why, 160)),
            ),
            Err(e) => {
                self.block(w, &format!("could not send it back: {}", e.0), false);
                let _ = store.remove_worker(w.session);
            }
        }
    }

    fn merge(&self, w: &Worker, session: &Session) {
        let store = self.store();
        match ship(store, session) {
            Ok(shipped) if matches!(shipped.pushed, Some(Err(_))) => {
                let why = format!("{}; the issue stays in review until it is pushed", shipped.describe());
                let _ = store.set_worker(w.session, "merge_failed", true);
                self.comment(w.issue, &format!("autopilot: {why}"));
                self.log(Some(w.project), Some(w.issue), &format!("#{}: {}", w.issue, clip(&why, 200)));
                self.notify(&format!("#{} merged but not pushed", w.issue));
            }
            Ok(shipped) => {
                self.runner.interrupt(w.session);
                self.runner.end(w.session);
                let _ = store.delete_session(w.session);
                self.set_state(w.issue, IssueState::Done);
                self.comment(
                    w.issue,
                    &format!("autopilot: {} and closed session {}.", shipped.describe(), w.session),
                );
                self.log(Some(w.project), Some(w.issue), &format!("{}, #{} done", shipped.describe(), w.issue));
                if let Some(Err(e)) = &shipped.deployed {
                    self.notify(&format!("#{} merged but the deploy failed: {}", w.issue, clip(e, 160)));
                }
            }
            Err(Unshipped::NoBranch) => {
                self.log_once(Some(w.project), Some(w.issue), &format!("#{} has no branch to merge", w.issue));
            }
            Err(Unshipped::Failed(why)) => {
                let _ = store.set_worker(w.session, "merge_failed", true);
                self.comment(w.issue, &format!("autopilot: merge failed: {why}"));
                self.log(Some(w.project), Some(w.issue), &format!("merge of #{} failed: {}", w.issue, clip(&why, 200)));
                self.notify(&format!("#{} could not be merged: {}", w.issue, clip(&why, 160)));
            }
        }
    }

    fn start_more(&self) {
        let store = self.store();
        let workers = store.workers().unwrap_or_default();
        let issues = store.issues().unwrap_or_default();
        let mut live: HashSet<Id> = workers.iter().map(|w| w.issue).collect();
        for s in store.sessions().unwrap_or_default() {
            if let Some(i) = s.issue_id
                && self.runner.is_running(s.id)
            {
                live.insert(i);
            }
        }
        let mut by_project: HashMap<Id, u32> = HashMap::new();
        let mut running = 0;
        for w in workers.iter().filter(|w| is_active(&w.state) && self.runner.is_running(w.session)) {
            running += 1;
            *by_project.entry(w.project).or_default() += 1;
        }
        let repos: HashMap<Id, bool> = store
            .projects()
            .unwrap_or_default()
            .into_iter()
            .map(|p| (p.id, !p.builtin() && crate::git::main_worktree(Path::new(&p.path)).is_ok()))
            .collect();
        let enabled = |p: Id| enabled_for(store, p);
        let git_repo = |p: Id| repos.get(&p).copied().unwrap_or(false);
        let room = Room {
            enabled: &enabled,
            git_repo: &git_repo,
            live: &live,
            running_by_project: &by_project,
            running,
            cap: number(store, CAP, DEFAULT_CAP),
            project_cap: number(store, PROJECT_CAP, DEFAULT_PROJECT_CAP),
        };
        let picked = pick(&issues, &room);
        for (id, why) in &picked.skipped {
            let project = issues.iter().find(|i| i.id == *id).map(|i| i.project_id);
            self.log_once(project, Some(*id), &format!("skipped #{id}: {why}"));
        }
        let agent = store
            .setting(AGENT)
            .ok()
            .flatten()
            .or_else(|| store.setting("autorun_agent").ok().flatten())
            .unwrap_or_else(|| "native".into());
        if !picked.issues.is_empty() && !self.runner.agent(&agent).is_some_and(|a| a.available()) {
            self.log_once(None, None, &format!("cannot start workers: agent {agent} is not ready"));
            return;
        }
        let mut started = 0;
        for issue in &picked.issues {
            let comments = store.comments(issue.id).unwrap_or_default();
            let new = NewSession {
                project_id: issue.project_id,
                agent: agent.clone(),
                place: Place::New { branch: String::new() },
                issue_id: Some(issue.id),
                prompt: Some(worker_prompt(issue, &comments)),
                ..Default::default()
            };
            match self.runner.start_session(new) {
                Ok(s) => {
                    started += 1;
                    self.set_state(issue.id, IssueState::Doing);
                    let _ = store.add_worker(s.id, issue.id, issue.project_id);
                    self.log(
                        Some(issue.project_id),
                        Some(issue.id),
                        &format!(
                            "started a worker on #{} {} (priority {}, {} of {} running)",
                            issue.id,
                            clip(&issue.title, 60),
                            issue.priority,
                            running + started,
                            room.cap
                        ),
                    );
                }
                Err(e) => {
                    self.add_labels(issue.id, &[BLOCKED]);
                    self.comment(issue.id, &format!("autopilot could not start a worker: {}", e.0));
                    self.log(
                        Some(issue.project_id),
                        Some(issue.id),
                        &format!("could not start #{}: {}", issue.id, clip(&e.0, 160)),
                    );
                }
            }
        }
        let pending = store
            .workers()
            .unwrap_or_default()
            .iter()
            .any(|w| is_active(&w.state) && self.runner.is_running(w.session));
        if started > 0 {
            let _ = store.set_setting(IDLE, "0");
        } else if !pending && !flag(store, IDLE) {
            let _ = store.set_setting(IDLE, "1");
            self.log(None, None, "idle: no eligible issues left");
            self.notify("autopilot is idle: no eligible issues left");
        }
    }
}

pub enum Unshipped {
    NoBranch,
    Failed(String),
}

#[derive(Debug)]
pub struct Shipped {
    pub branch: String,
    pub pushed: Option<Result<String, String>>,
    pub deployed: Option<Result<String, String>>,
}

impl Shipped {
    pub fn describe(&self) -> String {
        let mut out = format!("merged {} into the main branch", self.branch);
        match &self.pushed {
            Some(Ok(to)) => out.push_str(&format!(", pushed to {to}")),
            Some(Err(e)) => out.push_str(&format!(", push failed: {}", clip(e, 200))),
            None => out.push_str(", no remote to push to"),
        }
        match &self.deployed {
            Some(Ok(_)) => out.push_str(", deployed"),
            Some(Err(e)) => out.push_str(&format!(", deploy failed: {}", clip(e, 200))),
            None => {}
        }
        out
    }
}

pub fn ship(store: &Store, session: &Session) -> Result<Shipped, Unshipped> {
    let fail = |s: String| Unshipped::Failed(s);
    let (Ok(project), Ok(tree)) =
        (store.project(session.project_id), store.worktree(session.worktree_id))
    else {
        return Err(fail("the project or worktree is gone".into()));
    };
    let Some(branch) = tree.branch.clone().filter(|_| !tree.main) else {
        return Err(Unshipped::NoBranch);
    };
    let main = Path::new(&project.path);
    if let Some(want) = &project.branch
        && crate::git::current_branch(main).as_deref() != Some(want.as_str())
    {
        return Err(fail(format!("the main checkout is not on {want}")));
    }
    if crate::git::is_dirty(main).unwrap_or(true) {
        return Err(fail("the main checkout has uncommitted changes".into()));
    }
    if crate::git::is_dirty(Path::new(&tree.path)).unwrap_or(false) {
        return Err(fail(format!("{branch} has uncommitted changes")));
    }
    crate::git::merge_branch(main, &branch).map_err(fail)?;
    if !crate::git::is_merged(main, &branch) {
        return Err(fail(format!("{branch} is not in the main branch after the merge")));
    }
    let target = project.branch.clone().or_else(|| crate::git::current_branch(main));
    let pushed = match (crate::git::push_remote(main), target) {
        (Some(remote), Some(b)) => {
            Some(crate::git::push(main, &remote, &b).map(|()| format!("{remote}/{b}")))
        }
        _ => None,
    };
    let deployed = match &project.deploy {
        Some(cmd) if pushed.as_ref().is_none_or(Result::is_ok)
            && project.allows(default_level(store), Risky::Deploy) =>
        {
            Some(run_deploy(main, cmd, DEPLOY_LIMIT))
        }
        _ => None,
    };
    Ok(Shipped { branch, pushed, deployed })
}

pub fn run_deploy(dir: &Path, command: &str, limit: Duration) -> Result<String, String> {
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {command}: {e}"))?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() >= limit => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{command} ran past {} minutes and was stopped", limit.as_secs() / 60));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.to_string()),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let lines: Vec<&str> = text.trim().lines().collect();
    let tail = clip(&lines[lines.len().saturating_sub(20)..].join("\n"), 2000);
    if out.status.success() {
        Ok(tail)
    } else {
        Err(format!("{command} exited with {}: {tail}", out.status))
    }
}

pub fn spawn(runner: Runner) {
    let ap = Autopilot::new(runner.clone());
    tokio::spawn(async move {
        loop {
            let woken = runner.changed.notified();
            let this = ap.clone();
            let _ = tokio::task::spawn_blocking(move || this.tick()).await;
            tokio::select! {
                _ = woken => {}
                _ = tokio::time::sleep(TICK_EVERY) => {}
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::JsonCommand;
    use crate::store::testing;

    fn issue(id: Id, project: Id, priority: i64, created: i64, state: IssueState) -> Issue {
        Issue {
            id,
            project_id: project,
            title: format!("issue {id}"),
            body: String::new(),
            state,
            priority,
            labels: vec![],
            created_at: created,
            updated_at: created,
        }
    }

    struct Fixture {
        live: HashSet<Id>,
        by_project: HashMap<Id, u32>,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture { live: HashSet::new(), by_project: HashMap::new() }
        }

        fn pick(&self, issues: &[Issue], running: u32, cap: u32, project_cap: u32) -> Picked {
            let enabled = |_: Id| true;
            let repo = |_: Id| true;
            pick(
                issues,
                &Room {
                    enabled: &enabled,
                    git_repo: &repo,
                    live: &self.live,
                    running_by_project: &self.by_project,
                    running,
                    cap,
                    project_cap,
                },
            )
        }
    }

    fn ids(p: &Picked) -> Vec<Id> {
        p.issues.iter().map(|i| i.id).collect()
    }

    #[test]
    fn the_picker_takes_todo_by_priority_then_oldest_and_never_backlog() {
        let issues = [
            issue(1, 1, 0, 10, IssueState::Todo),
            issue(2, 1, 5, 30, IssueState::Todo),
            issue(3, 1, 5, 20, IssueState::Todo),
            issue(4, 1, 9, 5, IssueState::Backlog),
            issue(5, 1, 9, 5, IssueState::Doing),
        ];
        let p = Fixture::new().pick(&issues, 0, 10, 10);
        assert_eq!(ids(&p), [3, 2, 1]);
    }

    #[test]
    fn the_picker_stops_at_the_global_and_project_caps() {
        let issues = [
            issue(1, 1, 3, 1, IssueState::Todo),
            issue(2, 1, 2, 2, IssueState::Todo),
            issue(3, 2, 1, 3, IssueState::Todo),
            issue(4, 2, 0, 4, IssueState::Todo),
        ];
        let f = Fixture::new();
        assert_eq!(ids(&f.pick(&issues, 0, 2, 5)), [1, 2]);
        assert_eq!(ids(&f.pick(&issues, 1, 2, 5)), [1]);
        assert!(ids(&f.pick(&issues, 2, 2, 5)).is_empty());
        assert_eq!(ids(&f.pick(&issues, 0, 10, 1)), [1, 3], "one per project");
        let mut busy = Fixture::new();
        busy.by_project.insert(1, 2);
        assert_eq!(ids(&busy.pick(&issues, 2, 10, 2)), [3, 4]);
    }

    #[test]
    fn the_picker_skips_needs_user_blocked_live_and_non_repo_issues() {
        let mut needs = issue(1, 1, 9, 1, IssueState::Todo);
        needs.labels = vec!["needs-user".into()];
        let mut blocked = issue(2, 1, 8, 2, IssueState::Todo);
        blocked.labels = vec!["Blocked".into()];
        let live = issue(3, 1, 7, 3, IssueState::Todo);
        let elsewhere = issue(4, 2, 6, 4, IssueState::Todo);
        let fine = issue(5, 1, 0, 5, IssueState::Todo);
        let mut f = Fixture::new();
        f.live.insert(3);
        let enabled = |_: Id| true;
        let repo = |p: Id| p == 1;
        let p = pick(
            &[needs, blocked, live, elsewhere, fine],
            &Room {
                enabled: &enabled,
                git_repo: &repo,
                live: &f.live,
                running_by_project: &f.by_project,
                running: 0,
                cap: 10,
                project_cap: 10,
            },
        );
        assert_eq!(ids(&p), [5]);
        let reasons: Vec<_> = p.skipped.iter().map(|s| s.0).collect();
        assert_eq!(reasons, [1, 2, 3, 4]);
    }

    fn set_level(s: &Store, id: Id, level: Option<Automation>) {
        set_project(s, id, &ProjectPatch { automation: Some(level), ..Default::default() }).unwrap();
    }

    #[test]
    fn a_projects_own_level_beats_the_global_default() {
        let s = Store::memory().unwrap();
        let (a, _) = testing::project(&s, "/a");
        let b = s.add_project("other", "/b", Some("main"), None).unwrap();
        assert_eq!(default_level(&s), Automation::Delegate);
        assert_eq!((level(&s, a.id), level(&s, b.id)), (Automation::Delegate, Automation::Delegate));
        apply(&s, &AutopilotPatch { default_level: Some(Automation::Ship), ..Default::default() }).unwrap();
        set_level(&s, b.id, Some(Automation::Manual));
        assert_eq!((level(&s, a.id), level(&s, b.id)), (Automation::Ship, Automation::Manual));
        apply(&s, &AutopilotPatch { default_level: Some(Automation::Assist), ..Default::default() }).unwrap();
        assert_eq!(level(&s, b.id), Automation::Manual, "the default does not move a project's own level");
        set_level(&s, b.id, None);
        assert_eq!(level(&s, b.id), Automation::Assist, "cleared, it follows the default again");
        let st = state(&s);
        assert_eq!((st.cap, st.project_cap, st.enabled, st.default_level), (4, 2, false, Automation::Assist));
        assert!(st.activity.iter().any(|a| a.text.contains("other set to manual")));
    }

    #[test]
    fn the_loop_runs_a_project_only_when_on_and_at_autowork_or_above() {
        let s = Store::memory().unwrap();
        let (a, _) = testing::project(&s, "/a");
        apply(&s, &AutopilotPatch { enabled: Some(true), ..Default::default() }).unwrap();
        assert!(!enabled_for(&s, a.id), "delegate is below autowork");
        set_level(&s, a.id, Some(Automation::Autowork));
        assert!(enabled_for(&s, a.id));
        apply(&s, &AutopilotPatch { enabled: Some(false), ..Default::default() }).unwrap();
        assert!(!enabled_for(&s, a.id), "the pause key stops everything");
    }

    #[test]
    fn risky_actions_need_full_auto_and_their_own_toggle() {
        let s = Store::memory().unwrap();
        let (a, _) = testing::project(&s, "/a");
        for l in Automation::ALL {
            set_project(&s, a.id, &ProjectPatch { automation: Some(Some(l)), risky: Some(vec![]), ..Default::default() }).unwrap();
            for r in Risky::ALL {
                assert!(!allows(&s, a.id, r), "{r:?} off at {l:?}");
            }
            set_project(&s, a.id, &ProjectPatch { risky: Some(vec![Risky::Deploy]), ..Default::default() }).unwrap();
            for r in Risky::ALL {
                let want = l == Automation::FullAuto && r == Risky::Deploy;
                assert_eq!(allows(&s, a.id, r), want, "{r:?} at {l:?}");
            }
        }
        apply(&s, &AutopilotPatch { default_level: Some(Automation::FullAuto), ..Default::default() }).unwrap();
        set_level(&s, a.id, None);
        assert!(allows(&s, a.id, Risky::Deploy) && !allows(&s, a.id, Risky::SpendMoney), "toggles are per project even at a full auto default");
    }

    #[test]
    fn with_nothing_set_the_migration_leaves_delegate_and_the_loop_off() {
        let s = Store::memory().unwrap();
        let (a, _) = testing::project(&s, "/a");
        migrate(&s).unwrap();
        let st = state(&s);
        assert_eq!((st.default_level, st.enabled), (Automation::Delegate, false));
        assert_eq!(s.project(a.id).unwrap().automation, None);
    }

    #[test]
    fn the_old_autopilot_switches_become_levels_once() {
        let s = Store::memory().unwrap();
        let (a, _) = testing::project(&s, "/a");
        let b = s.add_project("b", "/b", Some("main"), None).unwrap();
        let c = s.add_project("c", "/c", Some("main"), None).unwrap();
        s.set_setting(&project_key(a.id), "1").unwrap();
        s.set_setting(&project_key(b.id), "0").unwrap();
        s.set_setting(MERGE, "1").unwrap();
        migrate(&s).unwrap();
        assert_eq!(s.project(a.id).unwrap().automation, Some(Automation::Ship), "on with merge");
        assert_eq!(s.project(b.id).unwrap().automation, None, "off under a global off changes nothing");
        assert_eq!(s.project(c.id).unwrap().automation, None);
        assert_eq!(default_level(&s), Automation::Delegate);
        assert!(state(&s).enabled, "the loop keeps running the project that was on");
        assert!(enabled_for(&s, a.id) && !enabled_for(&s, c.id));
        assert!(s.setting(&project_key(a.id)).unwrap().is_none() && s.setting(MERGE).unwrap().is_none());
        set_level(&s, a.id, Some(Automation::Assist));
        migrate(&s).unwrap();
        assert_eq!(s.project(a.id).unwrap().automation, Some(Automation::Assist), "runs once");

        let s = Store::memory().unwrap();
        let (a, _) = testing::project(&s, "/a");
        let b = s.add_project("b", "/b", Some("main"), None).unwrap();
        s.set_setting(ENABLED, "1").unwrap();
        s.set_setting(&project_key(b.id), "0").unwrap();
        migrate(&s).unwrap();
        assert_eq!(default_level(&s), Automation::Autowork, "a global on becomes the default");
        assert_eq!(level(&s, a.id), Automation::Autowork);
        assert_eq!(s.project(b.id).unwrap().automation, Some(Automation::Delegate), "its own off is kept");
    }

    #[test]
    fn a_reply_with_a_list_of_self_doable_work_is_not_a_conclusion() {
        let v = judge("Implemented it.\n\nTodo:\n- run the full test suite\n- add a test for Y\n");
        assert_eq!(v, Verdict::Leftover(vec!["run the full test suite".into(), "add a test for Y".into()]));
        let v = judge("Next steps:\n1. verify the merge locally\n2. need the API key from the user\n");
        assert_eq!(v, Verdict::Leftover(vec!["verify the merge locally".into()]));
    }

    #[test]
    fn a_list_of_user_only_items_is_blocked_on_the_user() {
        let v = judge("Needs more work:\n- needs the user's decision on the schema\n- credentials for the staging db\n");
        assert!(matches!(v, Verdict::Blocked { user: true, .. }), "{v:?}");
        let v = judge("BLOCKED #7 need an api key to continue");
        assert_eq!(v, Verdict::Blocked { reason: "need an api key to continue".into(), user: true });
        let v = judge("BLOCKED #7 the build is broken upstream");
        assert!(matches!(v, Verdict::Blocked { user: false, .. }));
    }

    #[test]
    fn a_rigid_reply_concludes_and_a_question_does_not() {
        assert_eq!(judge("All done.\nDONE #3 branch tests:pass fixed it"), Verdict::Concluded { green: true });
        assert_eq!(judge("DONE #3 tests:fail one case left"), Verdict::Concluded { green: false });
        assert_eq!(judge("Finished, nothing else."), Verdict::Concluded { green: false });
        assert!(matches!(judge("Which database should I use?"), Verdict::Asked(_)));
        assert_eq!(judge("  "), Verdict::Empty);
        assert_eq!(judge("- [x] done\n- [x] tested\nDONE #1 tests:pass ok"), Verdict::Concluded { green: true });
    }

    fn script_agent(script: &str) -> Box<dyn crate::agent::Agent> {
        Box::new(JsonCommand {
            name: "echo".into(),
            label: None,
            program: "sh".into(),
            args: vec!["-c".into(), script.into(), "sh".into()],
            resume_args: vec!["{session}".into()],
            model_args: vec![],
            prompt_on_stdin: true,
            session: Some("/session".into()),
            text: Some("/text".into()),
            error: Some("/error".into()),
            input_tokens: None,
            output_tokens: None,
            plain: false,
        })
    }

    struct Rig {
        ap: Autopilot,
        store: Store,
        project: Id,
        repo: std::path::PathBuf,
        _dir: tempfile::TempDir,
    }

    fn rig(script: &str) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("demo");
        crate::git::testing::repo(&repo);
        let store = Store::memory().unwrap();
        let project = crate::projects::add(&store, &repo, None, false).unwrap();
        let runner = Runner::new(store.clone(), vec![script_agent(script)]);
        store.set_setting("autorun_agent", "echo").unwrap();
        Rig { ap: Autopilot::new(runner), store, project: project.id, repo, _dir: dir }
    }

    fn todo(r: &Rig, title: &str, priority: i64, labels: &[&str]) -> Id {
        r.store
            .add_issue(&agentbench_core::NewIssue {
                project_id: r.project,
                title: title.into(),
                state: Some(IssueState::Todo),
                priority: Some(priority),
                labels: labels.iter().map(|l| l.to_string()).collect(),
                ..Default::default()
            })
            .unwrap()
            .id
    }

    fn on(r: &Rig) {
        apply(
            &r.store,
            &AutopilotPatch { enabled: Some(true), cap: Some(2), project_cap: Some(5), ..Default::default() },
        )
        .unwrap();
        set_level(&r.store, r.project, Some(Automation::Autowork));
    }

    fn commit_in(path: &str, file: &str) {
        std::fs::write(std::path::Path::new(path).join(file), "x").unwrap();
        for args in [vec!["add", "."], vec!["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "work"]] {
            assert!(std::process::Command::new("git").arg("-C").arg(path).args(&args).status().unwrap().success());
        }
    }

    fn bare_remote(r: &Rig) -> std::path::PathBuf {
        let bare = r._dir.path().join("remote.git");
        assert!(std::process::Command::new("git").args(["init", "-q", "--bare"]).arg(&bare).status().unwrap().success());
        assert!(std::process::Command::new("git").arg("-C").arg(&r.repo).args(["remote", "add", "origin"]).arg(&bare).status().unwrap().success());
        bare
    }

    fn remote_has(bare: &std::path::Path, file: &str) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(bare)
            .args(["cat-file", "-e", &format!("main:{file}")])
            .status()
            .unwrap()
            .success()
    }

    async fn worked_on(r: &Rig) -> (Id, agentbench_core::SessionId) {
        let a = todo(r, "a", 0, &[]);
        on(r);
        r.ap.tick();
        let w = r.store.workers().unwrap().remove(0);
        let tree = r.store.worktree(r.store.session(w.session).unwrap().worktree_id).unwrap();
        settle(r).await;
        commit_in(&tree.path, "feature.txt");
        (a, w.session)
    }

    #[test]
    fn a_done_line_with_passing_tests_is_green_even_when_the_prose_mentions_failing() {
        let reply = "Wrote a failing test first, then fixed it.\n\nDONE #168 tests:pass all suites green";
        assert_eq!(judge(reply), Verdict::Concluded { green: true });
        assert_eq!(judge("tests passed before\nDONE #9 tests:fail one left"), Verdict::Concluded { green: false });
    }

    #[tokio::test]
    async fn a_done_issue_with_a_merged_branch_loses_its_session_and_worktree() {
        let r = rig(DONE);
        let (a, sid) = worked_on(&r).await;
        r.ap.tick();
        let tree = r.store.worktree(r.store.session(sid).unwrap().worktree_id).unwrap();
        assert!(std::path::Path::new(&tree.path).exists());
        set_level(&r.store, r.project, Some(Automation::Ship));
        crate::git::merge_branch(&r.repo, tree.branch.as_deref().unwrap()).unwrap();
        r.ap.set_state(a, IssueState::Done);
        r.ap.tick();
        assert!(r.store.session(sid).is_err(), "session closed");
        assert!(!std::path::Path::new(&tree.path).exists(), "worktree removed");
        assert!(r.store.autopilot_log(50).unwrap().iter().any(|l| l.text.contains("closed session")));
    }

    #[tokio::test]
    async fn a_session_waiting_on_the_user_is_never_closed() {
        let r = rig(DONE);
        let (a, sid) = worked_on(&r).await;
        r.ap.tick();
        let tree = r.store.worktree(r.store.session(sid).unwrap().worktree_id).unwrap();
        set_level(&r.store, r.project, Some(Automation::Ship));
        crate::git::merge_branch(&r.repo, tree.branch.as_deref().unwrap()).unwrap();
        r.ap.set_state(a, IssueState::Done);
        r.store.set_status(sid, SessionStatus::Waiting).unwrap();
        r.ap.tick();
        assert!(r.store.session(sid).is_ok(), "still open");
        assert!(std::path::Path::new(&tree.path).exists());
    }

    #[tokio::test]
    async fn an_unmerged_branch_keeps_its_session_even_when_the_issue_is_done() {
        let r = rig(DONE);
        let (a, sid) = worked_on(&r).await;
        r.ap.tick();
        set_level(&r.store, r.project, Some(Automation::Autowork));
        r.ap.set_state(a, IssueState::Done);
        set_level(&r.store, r.project, Some(Automation::Ship));
        r.ap.tick();
        assert!(r.store.session(sid).is_ok());
    }

    #[tokio::test]
    async fn below_ship_nothing_is_closed() {
        let r = rig(DONE);
        let (a, sid) = worked_on(&r).await;
        r.ap.tick();
        let tree = r.store.worktree(r.store.session(sid).unwrap().worktree_id).unwrap();
        crate::git::merge_branch(&r.repo, tree.branch.as_deref().unwrap()).unwrap();
        r.ap.set_state(a, IssueState::Done);
        r.ap.tick();
        assert!(r.store.session(sid).is_ok());
    }

    #[tokio::test]
    async fn a_finished_session_with_no_issue_is_closed_after_the_grace_period() {
        let r = rig(DONE);
        on(&r);
        set_level(&r.store, r.project, Some(Automation::Ship));
        let tree = r.store.main_worktree(r.project).unwrap();
        let s = r
            .ap
            .runner
            .start_session(NewSession {
                project_id: r.project,
                agent: "echo".into(),
                place: Place::Existing { worktree_id: tree.id },
                prompt: Some("go".into()),
                ..Default::default()
            })
            .unwrap();
        for _ in 0..200 {
            if !r.ap.runner.is_running(s.id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let at = r.store.session(s.id).unwrap().updated_at;
        r.ap.tick_at(at + 60_000);
        assert!(r.store.session(s.id).is_ok(), "within the grace period");
        r.ap.tick_at(at + 31 * 60_000);
        assert!(r.store.session(s.id).is_err(), "closed after it");
    }

    #[tokio::test]
    async fn the_loop_skips_projects_below_autowork() {
        let r = rig(DONE);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        for l in [Automation::Manual, Automation::Assist, Automation::Delegate] {
            set_level(&r.store, r.project, Some(l));
            r.ap.tick();
            assert!(r.store.sessions().unwrap().is_empty(), "nothing started at {l:?}");
        }
        set_level(&r.store, r.project, None);
        apply(&r.store, &AutopilotPatch { default_level: Some(Automation::Autowork), ..Default::default() }).unwrap();
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing, "the default reaches autowork");
    }

    #[tokio::test]
    async fn autowork_leaves_a_green_branch_in_review_and_ship_merges_and_pushes_it() {
        let r = rig(DONE);
        let bare = bare_remote(&r);
        let (a, sid) = worked_on(&r).await;
        r.ap.tick();
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Review, "autowork does not merge");
        assert!(!r.repo.join("feature.txt").exists());
        set_level(&r.store, r.project, Some(Automation::Ship));
        r.ap.tick();
        assert!(r.repo.join("feature.txt").exists(), "merged into the main checkout");
        assert!(remote_has(&bare, "feature.txt"), "pushed");
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Done);
        assert!(r.store.session(sid).is_err(), "session closed");
    }

    #[tokio::test]
    async fn a_failed_push_keeps_the_issue_in_review() {
        let r = rig(DONE);
        let missing = r._dir.path().join("gone.git");
        assert!(std::process::Command::new("git").arg("-C").arg(&r.repo).args(["remote", "add", "origin"]).arg(&missing).status().unwrap().success());
        let (a, _) = worked_on(&r).await;
        set_level(&r.store, r.project, Some(Automation::Ship));
        r.ap.tick();
        r.ap.tick();
        assert!(r.repo.join("feature.txt").exists());
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Review);
        assert_eq!(r.store.workers().unwrap()[0].state, "merge_failed");
        assert!(r.store.comments(a).unwrap().iter().any(|c| c.body.contains("push failed")));
    }

    #[tokio::test]
    async fn the_loop_deploys_only_at_full_auto_with_the_deploy_toggle_on() {
        for (level, toggles, want) in [
            (Automation::Ship, vec![Risky::Deploy], false),
            (Automation::FullAuto, vec![], false),
            (Automation::FullAuto, vec![Risky::Publish], false),
            (Automation::FullAuto, vec![Risky::Deploy], true),
        ] {
            let r = rig(DONE);
            let mark = r._dir.path().join("deployed");
            set_project(
                &r.store,
                r.project,
                &ProjectPatch { deploy: Some(format!("touch {}", mark.display())), risky: Some(toggles.clone()), ..Default::default() },
            )
            .unwrap();
            let (a, _) = worked_on(&r).await;
            set_level(&r.store, r.project, Some(level));
            r.ap.tick();
            r.ap.tick();
            assert_eq!(r.store.issue(a).unwrap().state, IssueState::Done);
            assert_eq!(mark.exists(), want, "{level:?} with {toggles:?}");
        }
    }

    async fn settle(r: &Rig) {
        for _ in 0..400 {
            if r.store.workers().unwrap().iter().all(|w| !r.ap.runner.is_running(w.session)) {
                tokio::time::sleep(Duration::from_millis(60)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("workers never finished");
    }

    const DONE: &str = r#"read p; sleep 1; echo "{\"text\":\"Done. DONE #1 tests:pass ok\"}""#;

    #[tokio::test]
    async fn with_three_todo_issues_and_a_cap_of_two_the_third_starts_when_one_finishes() {
        let r = rig(DONE);
        let a = todo(&r, "a", 3, &[]);
        let b = todo(&r, "b", 2, &[]);
        let c = todo(&r, "c", 1, &[]);
        r.ap.tick();
        assert!(r.store.sessions().unwrap().is_empty(), "off by default");
        on(&r);
        r.ap.tick();
        let started = |r: &Rig| {
            let mut v: Vec<_> = r.store.sessions().unwrap().iter().filter_map(|s| s.issue_id).collect();
            v.sort();
            v
        };
        assert_eq!(started(&r), [a, b]);
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing);
        assert_eq!(r.store.issue(c).unwrap().state, IssueState::Todo);
        r.ap.tick();
        assert_eq!(started(&r).len(), 2, "still capped while both run");
        settle(&r).await;
        r.ap.tick();
        assert_eq!(started(&r), [a, b, c], "the third starts with no user input");
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Review);
    }

    #[tokio::test]
    async fn a_needs_user_or_blocked_issue_is_never_started() {
        let r = rig(DONE);
        let n = todo(&r, "needs me", 9, &["needs-user"]);
        let b = todo(&r, "blocked", 8, &["blocked"]);
        let ok = todo(&r, "fine", 0, &[]);
        on(&r);
        r.ap.tick();
        r.ap.tick();
        let sessions = r.store.sessions().unwrap();
        assert_eq!(sessions.iter().filter_map(|s| s.issue_id).collect::<Vec<_>>(), [ok]);
        assert_eq!(r.store.issue(n).unwrap().state, IssueState::Todo);
        assert_eq!(r.store.issue(b).unwrap().state, IssueState::Todo);
        let feed = r.store.autopilot_log(50).unwrap();
        assert!(feed.iter().any(|a| a.text.contains(&format!("skipped #{n}"))));
        assert_eq!(feed.iter().filter(|a| a.text.contains(&format!("skipped #{n}"))).count(), 1);
    }

    #[tokio::test]
    async fn a_finished_worker_moves_its_issue_to_review() {
        let r = rig(DONE);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        r.ap.tick();
        settle(&r).await;
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Review);
        let w = &r.store.workers().unwrap()[0];
        assert_eq!((w.state.as_str(), w.green), ("ready", true));
        assert!(r.store.comments(a).unwrap().iter().any(|c| c.body.contains("concluded with tests passing")));
    }

    #[tokio::test]
    async fn a_worker_that_failed_blocks_its_issue_with_a_comment() {
        let r = rig(r#"read p; echo '{"error":"quota exceeded"}'"#);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        r.ap.tick();
        settle(&r).await;
        r.ap.tick();
        let i = r.store.issue(a).unwrap();
        assert!(i.labels.contains(&"blocked".to_string()));
        assert!(!i.labels.contains(&"needs-user".to_string()));
        assert!(r.store.comments(a).unwrap().iter().any(|c| c.body.contains("blocked") && c.body.contains("quota exceeded")));
        assert!(r.store.workers().unwrap().is_empty());
        r.ap.tick();
        assert_eq!(r.store.sessions().unwrap().len(), 1, "not started again");
    }

    #[tokio::test]
    async fn a_worker_that_asks_for_a_credential_gets_needs_user_and_is_not_restarted() {
        let r = rig(r#"read p; echo '{"text":"BLOCKED #1 I need an api key for the vendor"}'"#);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        r.ap.tick();
        settle(&r).await;
        r.ap.tick();
        r.ap.tick();
        let i = r.store.issue(a).unwrap();
        assert!(i.labels.contains(&"needs-user".to_string()) && i.labels.contains(&"blocked".to_string()));
        assert_eq!(r.store.sessions().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_reply_with_a_leftover_list_is_sent_back_not_moved_to_review() {
        let script = r#"read p; case "$p" in *"Conclusion check"*) echo '{"text":"Ran everything. DONE #1 tests:pass ok"}';; *) printf '%s\n' '{"text":"Built it.\n\nTodo:\n- run the tests\n- add a test for Z"}';; esac"#;
        let r = rig(script);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        r.ap.tick();
        settle(&r).await;
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing, "not review");
        let w = r.store.workers().unwrap().remove(0);
        assert_eq!((w.state.as_str(), w.reworks), ("working", 1));
        settle(&r).await;
        let sent = agentbench_core::shown(&r.store.messages(w.session, 0).unwrap());
        assert!(sent.iter().any(|m| matches!(&m.entry, Entry::User { text, .. } if text.contains("Conclusion check") && text.contains("run the tests"))));
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Review);
    }

    #[tokio::test]
    async fn a_silent_session_is_nudged_then_stopped_and_the_issue_returns_to_todo() {
        let r = rig(r#"read p; sleep 30"#);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        r.ap.tick();
        let sid = r.store.workers().unwrap()[0].session;
        let started = r.store.session(sid).unwrap().updated_at;
        let later = started + 16 * 60_000;
        r.ap.tick_at(later);
        let w = r.store.workers().unwrap().remove(0);
        assert_eq!(w.nudges, 1);
        assert!(r.ap.runner.is_running(sid));
        let later = r.store.session(sid).unwrap().updated_at + 16 * 60_000;
        r.ap.tick_at(later);
        let later = r.store.session(sid).unwrap().updated_at + 16 * 60_000;
        r.ap.tick_at(later);
        assert!(r.store.workers().unwrap().is_empty(), "given up on");
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Todo);
        assert!(r.store.comments(a).unwrap().iter().any(|c| c.body.contains("stopped after 2 nudges")));
        for _ in 0..100 {
            if !r.ap.runner.is_running(sid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(!r.ap.runner.is_running(sid));
    }

    #[tokio::test]
    async fn a_dirty_main_checkout_stops_the_merge_instead_of_forcing_it() {
        let r = rig(DONE);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        set_level(&r.store, r.project, Some(Automation::Ship));
        r.ap.tick();
        settle(&r).await;
        std::fs::write(r.repo.join("scratch.txt"), "x").unwrap();
        r.ap.tick();
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Review);
        assert_eq!(r.store.workers().unwrap()[0].state, "merge_failed");
    }

    #[tokio::test]
    async fn it_goes_idle_once_when_nothing_is_left() {
        let r = rig(DONE);
        on(&r);
        r.ap.tick();
        r.ap.tick();
        let idle = |r: &Rig| r.store.autopilot_log(50).unwrap().iter().filter(|a| a.text.starts_with("idle")).count();
        assert_eq!(idle(&r), 1);
        assert!(state(&r.store).idle);
        todo(&r, "new", 0, &[]);
        r.ap.tick();
        assert!(!state(&r.store).idle);
    }
}
