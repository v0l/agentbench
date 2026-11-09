use crate::runner::{Origin, Runner};
use crate::store::{Judged, Store, StoreError, Worker};
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
const AUTO_CLOSE: &str = "autopilot_auto_close";
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    ConcludedPass,
    ConcludedFail,
    NeedsFollowup,
    WaitingOnUser,
    Blocked,
}

impl Verdict {
    pub const ALL: [Verdict; 5] = [
        Verdict::ConcludedPass,
        Verdict::ConcludedFail,
        Verdict::NeedsFollowup,
        Verdict::WaitingOnUser,
        Verdict::Blocked,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::ConcludedPass => "concluded_pass",
            Verdict::ConcludedFail => "concluded_fail",
            Verdict::NeedsFollowup => "needs_followup",
            Verdict::WaitingOnUser => "waiting_on_user",
            Verdict::Blocked => "blocked",
        }
    }

    pub fn parse(s: &str) -> Option<Verdict> {
        Verdict::ALL.into_iter().find(|v| v.as_str() == s.trim())
    }
}

const JUDGE_MINUTES: &str = "autopilot_judge_minutes";
pub const DEFAULT_JUDGE_MINUTES: u64 = 15;

pub const JUDGING_RULES: &str = "Decide from what the worker actually did, not its tone. \
     Idle after \"let me know if you want X\" is needs_followup unless X is a real user decision. \
     A claim of passing tests without test output in the transcript is not a pass. \
     Leftover items the worker could do itself mean needs_followup. \
     Check the diff against the issue's acceptance criteria.";

pub fn head_of(store: &Store, session: &Session) -> Option<String> {
    let tree = store.worktree(session.worktree_id).ok()?;
    crate::git::head(Path::new(&tree.path))
}

pub fn current_verdict(store: &Store, session: &Session) -> Option<Judged> {
    let v = store.latest_verdict(session.id).ok().flatten()?;
    (v.head == head_of(store, session)).then_some(v)
}

fn verdict_line(v: &Judged) -> String {
    format!("supervisor {} judged {}: {}", v.judge, v.verdict, clip(&v.reason, 300))
}

fn diff_summary(store: &Store, session: &Session) -> String {
    let (Ok(project), Ok(tree)) = (store.project(session.project_id), store.worktree(session.worktree_id)) else {
        return "the worktree is gone".into();
    };
    if tree.main {
        return "it works in the main checkout, so there is no branch diff".into();
    }
    let base = project.branch.clone().or_else(|| crate::git::current_branch(Path::new(&project.path)));
    let Some(base) = base else { return "the default branch is unknown".into() };
    match crate::git::diff_stat(Path::new(&tree.path), &base) {
        Ok(s) if s.trim().is_empty() => format!("no changes against {base}"),
        Ok(s) => s,
        Err(e) => format!("git diff failed: {}", clip(&e, 200)),
    }
}

fn test_runs(store: &Store, session: agentbench_core::SessionId) -> Vec<String> {
    let mut runs = Vec::new();
    for m in agentbench_core::shown(&store.messages(session, 0).unwrap_or_default()) {
        let Entry::Tool { input, output, error, .. } = m.entry else { continue };
        let Some(command) = input.get("command").and_then(|c| c.as_str()) else { continue };
        if !command.contains("test") {
            continue;
        }
        let out = output.unwrap_or_default();
        let tail: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).rev().take(4).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        runs.push(format!(
            "$ {}{}\n{}",
            clip(command, 200),
            if error { " (tool error)" } else { "" },
            tail.iter().map(|l| format!("  {}", clip(l, 200))).collect::<Vec<_>>().join("\n")
        ));
    }
    let skip = runs.len().saturating_sub(6);
    runs.into_iter().skip(skip).collect()
}

pub fn judge_request(store: &Store, session: &Session) -> String {
    let issue = session.issue_id.and_then(|i| store.issue(i).ok());
    let about = match &issue {
        Some(i) => format!(
            "Issue #{}: {}\n\nAcceptance criteria (the issue body):\n{}",
            i.id,
            i.title,
            if i.body.trim().is_empty() { "(empty)" } else { i.body.trim() }
        ),
        None => "No issue.".into(),
    };
    let reply = last_reply(store, session.id);
    let tests = test_runs(store, session.id);
    let tests = if tests.is_empty() {
        "No test command appears in the transcript.".to_string()
    } else {
        tests.join("\n")
    };
    format!(
        "Judge worker {id} (\"{title}\"), its turn ended.\n\n{about}\n\nIts last reply:\n{reply}\n\nDiff stat of its branch (head {head}):\n{diff}\n\nTest runs in its transcript:\n{tests}\n\n\
         Call judge_session id={id} with verdict concluded_pass, concluded_fail, needs_followup (give `followup`, the message the worker gets), waiting_on_user or blocked, and a reason. \
         Nothing is reviewed, merged or closed until you do. {rules}",
        id = session.id,
        title = session.title,
        reply = if reply.trim().is_empty() { "(empty)" } else { reply.trim() },
        head = head_of(store, session).map(|h| h.chars().take(10).collect::<String>()).unwrap_or_else(|| "unknown".into()),
        diff = diff_summary(store, session),
        rules = JUDGING_RULES,
    )
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

pub fn last_reply(store: &Store, session: agentbench_core::SessionId) -> String {
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
        if !flag(store, ENABLED) || !flag(store, AUTO_CLOSE) {
            return;
        }
        let supervisor = store.supervisor_project().ok().flatten().map(|p| p.id);
        let tracked: HashSet<_> = store.workers().unwrap_or_default().iter().map(|w| w.session).collect();
        let sessions = store.sessions().unwrap_or_default();
        for s in &sessions {
            if Some(s.project_id) == supervisor
                || s.parent.is_none()
                || tracked.contains(&s.id)
                || self.runner.is_running(s.id)
                || s.status != SessionStatus::Idle
                || level(store, s.project_id) < Automation::Ship
            {
                continue;
            }
            let Some(v) = current_verdict(store, s).filter(|v| v.verdict == Verdict::ConcludedPass.as_str()) else {
                continue;
            };
            let why = match s.issue_id {
                Some(i) => match store.issue(i) {
                    Ok(issue) if issue.state == IssueState::Done && self.branch_is_merged(s) => {
                        format!("#{i} is done and its branch is merged")
                    }
                    _ => continue,
                },
                None => {
                    let grace = number(store, GRACE_MINUTES, DEFAULT_GRACE_MINUTES) as i64 * 60_000;
                    if now - s.updated_at < grace || !self.branch_is_merged_or_none(s) {
                        continue;
                    }
                    "it is idle past the grace period and nothing is pending".to_string()
                }
            };
            self.close_finished(s, &format!("{why}; {}", verdict_line(&v)));
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
        let running = self.runner.is_running(w.session);
        match w.state.as_str() {
            "working" | "asking" => {
                if running {
                    self.watch(w, &session, now);
                } else {
                    self.finished(w, &session, now);
                }
            }
            "judging" | "held" | "parked" | "ready" if running => {
                let _ = store.set_worker(w.session, "working", false);
                if w.state == "parked" {
                    self.drop_labels(w.issue, &[BLOCKED, NEEDS_USER]);
                }
                self.log(Some(w.project), Some(w.issue), &format!("#{} is working again", w.issue));
            }
            "judging" | "held" => {
                if let Some(v) = store.latest_verdict(w.session).ok().flatten().filter(|v| !v.acted) {
                    if v.head == head_of(store, &session) {
                        self.act(w, &session, &v);
                    } else {
                        self.log(
                            Some(w.project),
                            Some(w.issue),
                            &format!("#{}: its branch changed after the verdict, so it is judged again", w.issue),
                        );
                        self.ask(w, &session, now);
                    }
                } else if w.state == "held" {
                    if w.asked_at == 0 && self.runner.supervisor_session().is_some() {
                        self.ask(w, &session, now);
                    }
                } else if now - w.asked_at >= number(store, JUDGE_MINUTES, DEFAULT_JUDGE_MINUTES) as i64 * 60_000 {
                    let _ = store.set_worker(w.session, "held", false);
                    self.log(
                        Some(w.project),
                        Some(w.issue),
                        &format!("#{}: the supervisor gave no verdict in time; nothing is reviewed, merged or closed", w.issue),
                    );
                    self.notify(&format!("#{} finished but the supervisor has not judged it; nothing merges until it does", w.issue));
                }
            }
            "ready" => {
                if issue.state == IssueState::Done {
                    let _ = store.remove_worker(w.session);
                } else if w.green && flag(store, ENABLED) && level(store, w.project) >= Automation::Ship {
                    match current_verdict(store, &session) {
                        Some(v) if v.verdict == Verdict::ConcludedPass.as_str() => self.merge(w, &session, &v),
                        _ => {
                            self.log(
                                Some(w.project),
                                Some(w.issue),
                                &format!("#{}: its branch changed after the verdict, so it is judged again", w.issue),
                            );
                            self.ask(w, &session, now);
                        }
                    }
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
        let unplanned = number::<u32>(store, &format!("no_plan_{}", w.session), 0) >= crate::agent::progress::ESCALATE_AT;
        if !unplanned && now - session.updated_at < stuck_ms {
            return;
        }
        let minutes = stuck_ms / 60_000;
        if w.nudges < number(store, MAX_NUDGES, DEFAULT_MAX_NUDGES) {
            let _ = store.bump_worker(w.session, "nudges");
            let text = if unplanned {
                "Autopilot: you have read many files and written no plan. Write your implementation steps with todo_write now and start the first one.".to_string()
            } else {
                format!("Autopilot: no output from you for {minutes} minutes. If something blocks you, say what; otherwise carry on and finish with a conclusion.")
            };
            let _ = self.runner.tell(session, &text, Origin::new("autopilot", Some(w.session)));
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

    fn finished(&self, w: &Worker, session: &Session, now: i64) {
        let store = self.store();
        match session.status {
            SessionStatus::Failed => {
                self.block(w, &last_error(store, w.session), false);
                let _ = store.remove_worker(w.session);
            }
            SessionStatus::Stopped => {
                self.block(w, "the session was stopped before it finished", false);
                let _ = store.remove_worker(w.session);
            }
            _ => self.ask(w, session, now),
        }
    }

    fn ask(&self, w: &Worker, session: &Session, now: i64) {
        let store = self.store();
        let _ = store.settle_verdicts(w.session);
        let Some(sup) = self.runner.supervisor_session() else {
            let _ = store.set_worker_asked(w.session, 0);
            let _ = store.set_worker(w.session, "held", false);
            self.log(
                Some(w.project),
                Some(w.issue),
                &format!("#{} finished but no supervisor is running to judge it; nothing is reviewed, merged or closed", w.issue),
            );
            self.notify(&format!("#{} finished but no supervisor is running to judge it", w.issue));
            return;
        };
        let origin = Origin::new("judge", Some(w.session));
        match self.runner.tell(&sup, &judge_request(store, session), origin) {
            Ok(()) => {
                let _ = store.set_worker_asked(w.session, now.max(1));
                self.log(Some(w.project), Some(w.issue), &format!("#{} finished, asked the supervisor for a verdict", w.issue));
            }
            Err(e) => {
                let _ = store.set_worker_asked(w.session, 0);
                let _ = store.set_worker(w.session, "held", false);
                self.log(Some(w.project), Some(w.issue), &format!("#{}: could not ask the supervisor: {}", w.issue, clip(&e.0, 160)));
                self.notify(&format!("#{} finished but the supervisor could not be asked to judge it", w.issue));
            }
        }
    }

    fn act(&self, w: &Worker, session: &Session, v: &Judged) {
        let store = self.store();
        let _ = store.settle_verdicts(w.session);
        let Some(verdict) = Verdict::parse(&v.verdict) else { return };
        let line = verdict_line(v);
        match verdict {
            Verdict::ConcludedPass | Verdict::ConcludedFail => {
                let green = verdict == Verdict::ConcludedPass;
                self.set_state(w.issue, IssueState::Review);
                let _ = store.set_worker(w.session, "ready", green);
                self.log(
                    Some(w.project),
                    Some(w.issue),
                    &format!("#{} moved to review{}; {line}", w.issue, if green { "" } else { ", will not merge" }),
                );
                if !green || level(store, w.project) < Automation::Ship {
                    self.notify(&format!("#{} is ready for review", w.issue));
                }
            }
            Verdict::NeedsFollowup => {
                let followup = v.followup.clone().filter(|f| !f.trim().is_empty()).unwrap_or_else(|| v.reason.clone());
                self.rework(w, session, &followup, &line);
            }
            Verdict::WaitingOnUser => {
                let _ = store.set_worker(w.session, "parked", false);
                self.block(w, &format!("{}; {line}", v.reason), true);
            }
            Verdict::Blocked => {
                self.block(w, &format!("{}; {line}", v.reason), false);
                let _ = store.remove_worker(w.session);
            }
        }
    }

    fn rework(&self, w: &Worker, session: &Session, followup: &str, line: &str) {
        let store = self.store();
        if w.reworks >= MAX_REWORKS {
            self.block(w, &format!("still not finished after {MAX_REWORKS} follow-ups; {line}"), false);
            self.notify(&format!("#{} is blocked: not finished after {MAX_REWORKS} follow-ups", w.issue));
            let _ = store.remove_worker(w.session);
            return;
        }
        let _ = store.bump_worker(w.session, "reworks");
        let _ = store.set_worker(w.session, "working", false);
        let text = format!(
            "Conclusion check from the supervisor: {}\n\nDo it now and finish what you can do yourself, then end with a conclusion and a final `DONE #{} tests:<pass|fail> <result>` line.",
            followup.trim(),
            w.issue
        );
        match self.runner.tell(session, &text, Origin::new("autopilot", Some(w.session))) {
            Ok(()) => self.log(
                Some(w.project),
                Some(w.issue),
                &format!("sent #{} back to finish ({} of {MAX_REWORKS}); {line}", w.issue, w.reworks + 1),
            ),
            Err(e) => {
                self.block(w, &format!("could not send it back: {}", e.0), false);
                let _ = store.remove_worker(w.session);
            }
        }
    }

    fn merge(&self, w: &Worker, session: &Session, v: &Judged) {
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
                let closed = flag(store, AUTO_CLOSE);
                if closed {
                    self.runner.interrupt(w.session);
                    self.runner.end(w.session);
                    let _ = store.delete_session(w.session);
                }
                let line = verdict_line(v);
                self.set_state(w.issue, IssueState::Done);
                self.comment(
                    w.issue,
                    &if closed {
                        format!("autopilot: {} and closed session {}; {line}.", shipped.describe(), w.session)
                    } else {
                        format!("autopilot: {}; {line}.", shipped.describe())
                    },
                );
                self.log(Some(w.project), Some(w.issue), &format!("{}, #{} done; {line}", shipped.describe(), w.issue));
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
                parent: self.runner.supervisor_session().map(|s| s.id),
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
            goal: None,
            off_vision: false,
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

    fn script_agent(script: &str) -> Box<dyn crate::agent::Agent> {
        named_agent("echo", script)
    }

    fn named_agent(name: &str, script: &str) -> Box<dyn crate::agent::Agent> {
        Box::new(JsonCommand {
            name: name.into(),
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
        boss: Option<agentbench_core::SessionId>,
        _dir: tempfile::TempDir,
    }

    fn rig(script: &str) -> Rig {
        let r = rig_alone(script);
        let sup = crate::projects::ensure_supervisor(&r.store, &r._dir.path().join("sup")).unwrap();
        let tree = r.store.main_worktree(sup.id).unwrap();
        let boss = r.store.add_session(tree.id, "boss", "boss", None, None).unwrap();
        Rig { boss: Some(boss.id), ..r }
    }

    fn rig_alone(script: &str) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("demo");
        crate::git::testing::repo(&repo);
        let store = Store::memory().unwrap();
        let project = crate::projects::add(&store, &repo, None, false).unwrap();
        let boss = named_agent("boss", r#"cat >/dev/null; echo '{"text":"noted"}'"#);
        let runner = Runner::new(store.clone(), vec![script_agent(script), boss]);
        store.set_setting("autorun_agent", "echo").unwrap();
        Rig { ap: Autopilot::new(runner), store, project: project.id, repo, boss: None, _dir: dir }
    }

    fn judge(
        r: &Rig,
        sid: agentbench_core::SessionId,
        verdict: &str,
        followup: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        let mut args = serde_json::json!({"id": sid.to_string(), "verdict": verdict, "reason": format!("because {verdict}")});
        if let Some(f) = followup {
            args["followup"] = serde_json::json!(f);
        }
        r.ap.runner.tools().call("judge_session", &args, r.boss.unwrap())
    }

    async fn boss_told(r: &Rig) -> Vec<String> {
        idle(r, r.boss.unwrap()).await;
        r.store
            .messages(r.boss.unwrap(), 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::User { text, .. } => Some(text),
                _ => None,
            })
            .flat_map(|t| t.split("Judge worker ").skip(1).map(str::to_string).collect::<Vec<_>>())
            .collect()
    }

    fn said_to_boss(r: &Rig) -> Vec<String> {
        r.store
            .messages(r.boss.unwrap(), 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::User { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    fn feed(r: &Rig) -> Vec<String> {
        r.store.autopilot_log(200).unwrap().into_iter().map(|a| a.text).collect()
    }

    async fn idle(r: &Rig, sid: agentbench_core::SessionId) {
        for _ in 0..400 {
            if !r.ap.runner.is_running(sid) {
                tokio::time::sleep(Duration::from_millis(60)).await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("session never finished");
    }

    async fn passed(r: &Rig, sid: agentbench_core::SessionId) {
        r.ap.tick();
        idle(r, r.boss.unwrap()).await;
        judge(r, sid, "concluded_pass", None).unwrap();
        r.ap.tick();
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

    fn auto_close(r: &Rig) {
        r.store.set_setting(AUTO_CLOSE, "1").unwrap();
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

    #[tokio::test]
    async fn a_done_issue_with_a_merged_branch_loses_its_session_and_worktree_only_when_auto_close_is_on() {
        let r = rig(DONE);
        let (a, sid) = worked_on(&r).await;
        passed(&r, sid).await;
        let tree = r.store.worktree(r.store.session(sid).unwrap().worktree_id).unwrap();
        assert!(std::path::Path::new(&tree.path).exists());
        set_level(&r.store, r.project, Some(Automation::Ship));
        crate::git::merge_branch(&r.repo, tree.branch.as_deref().unwrap()).unwrap();
        r.ap.set_state(a, IssueState::Done);
        r.ap.tick();
        assert!(r.store.session(sid).is_ok(), "session stays open by default");
        assert!(std::path::Path::new(&tree.path).exists(), "worktree stays by default");
        auto_close(&r);
        r.ap.tick();
        assert!(r.store.session(sid).is_err(), "session closed");
        assert!(!std::path::Path::new(&tree.path).exists(), "worktree removed");
        let closed = feed(&r).into_iter().find(|l| l.contains("closed session")).unwrap();
        assert!(closed.contains("judged concluded_pass: because concluded_pass"), "{closed}");
    }

    #[tokio::test]
    async fn a_session_waiting_on_the_user_is_never_closed() {
        let r = rig(DONE);
        let (a, sid) = worked_on(&r).await;
        passed(&r, sid).await;
        let tree = r.store.worktree(r.store.session(sid).unwrap().worktree_id).unwrap();
        set_level(&r.store, r.project, Some(Automation::Ship));
        auto_close(&r);
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
        passed(&r, sid).await;
        auto_close(&r);
        r.ap.set_state(a, IssueState::Done);
        set_level(&r.store, r.project, Some(Automation::Ship));
        r.ap.tick();
        assert!(r.store.session(sid).is_ok());
    }

    #[tokio::test]
    async fn below_ship_nothing_is_closed() {
        let r = rig(DONE);
        let (a, sid) = worked_on(&r).await;
        passed(&r, sid).await;
        auto_close(&r);
        let tree = r.store.worktree(r.store.session(sid).unwrap().worktree_id).unwrap();
        crate::git::merge_branch(&r.repo, tree.branch.as_deref().unwrap()).unwrap();
        r.ap.set_state(a, IssueState::Done);
        r.ap.tick();
        assert!(r.store.session(sid).is_ok());
    }

    async fn loose_session(r: &Rig, parent: Option<agentbench_core::SessionId>) -> Session {
        on(r);
        set_level(&r.store, r.project, Some(Automation::Ship));
        let tree = r.store.main_worktree(r.project).unwrap();
        let s = r
            .ap
            .runner
            .start_session(NewSession {
                parent,
                project_id: r.project,
                agent: "echo".into(),
                place: Place::Existing { worktree_id: tree.id },
                prompt: Some("go".into()),
                ..Default::default()
            })
            .unwrap();
        idle(r, s.id).await;
        r.store.session(s.id).unwrap()
    }

    #[tokio::test]
    async fn a_judged_session_with_no_issue_is_closed_only_after_the_grace_period() {
        let r = rig(DONE);
        let s = loose_session(&r, r.boss).await;
        let at = s.updated_at;
        auto_close(&r);
        r.ap.tick_at(at + 31 * 60_000);
        assert!(r.store.session(s.id).is_ok(), "a DONE line alone closes nothing");
        judge(&r, s.id, "concluded_pass", None).unwrap();
        r.ap.tick_at(at + 60_000);
        assert!(r.store.session(s.id).is_ok(), "within the grace period");
        r.ap.tick_at(at + 31 * 60_000);
        assert!(r.store.session(s.id).is_err(), "closed after it");
        assert!(feed(&r).iter().any(|l| l.contains("closed session") && l.contains("judged concluded_pass")));
    }

    #[tokio::test]
    async fn the_sweep_never_closes_a_user_started_session() {
        let r = rig(DONE);
        let s = loose_session(&r, None).await;
        auto_close(&r);
        r.store.add_verdict(s.id, None, head_of(&r.store, &s).as_deref(), "concluded_pass", "x", None, "", r.boss.unwrap()).unwrap();
        r.ap.tick_at(s.updated_at + 120 * 60_000);
        assert!(r.store.session(s.id).is_ok());
    }

    #[tokio::test]
    async fn the_sweep_leaves_an_idle_session_judged_anything_but_pass_open() {
        let r = rig(r#"read p; echo '{"text":"Pushed. Let me know if you want X."}'"#);
        let s = loose_session(&r, r.boss).await;
        auto_close(&r);
        r.ap.tick_at(s.updated_at + 120 * 60_000);
        assert!(r.store.session(s.id).is_ok(), "no verdict");
        judge(&r, s.id, "waiting_on_user", None).unwrap();
        r.ap.tick_at(s.updated_at + 120 * 60_000);
        assert!(r.store.session(s.id).is_ok(), "waiting on the user");
    }

    #[tokio::test]
    async fn the_loop_skips_projects_below_autowork() {
        let r = rig(DONE);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        for l in [Automation::Manual, Automation::Assist, Automation::Delegate] {
            set_level(&r.store, r.project, Some(l));
            r.ap.tick();
            assert!(r.store.sessions().unwrap().iter().all(|s| s.issue_id.is_none()), "nothing started at {l:?}");
        }
        set_level(&r.store, r.project, None);
        apply(&r.store, &AutopilotPatch { default_level: Some(Automation::Autowork), ..Default::default() }).unwrap();
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing, "the default reaches autowork");
    }

    #[tokio::test]
    async fn autowork_leaves_a_passed_branch_in_review_and_ship_merges_and_pushes_it() {
        let r = rig(DONE);
        let bare = bare_remote(&r);
        let (a, sid) = worked_on(&r).await;
        passed(&r, sid).await;
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Review, "autowork does not merge");
        assert!(!r.repo.join("feature.txt").exists());
        set_level(&r.store, r.project, Some(Automation::Ship));
        r.ap.tick();
        assert!(r.repo.join("feature.txt").exists(), "merged into the main checkout");
        assert!(remote_has(&bare, "feature.txt"), "pushed");
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Done);
        assert!(r.store.session(sid).is_ok(), "session stays open by default");
        assert!(r.store.comments(a).unwrap().iter().any(|c| c.body.contains("merged") && c.body.contains("judged concluded_pass")));
    }

    #[tokio::test]
    async fn a_failed_push_keeps_the_issue_in_review() {
        let r = rig(DONE);
        let missing = r._dir.path().join("gone.git");
        assert!(std::process::Command::new("git").arg("-C").arg(&r.repo).args(["remote", "add", "origin"]).arg(&missing).status().unwrap().success());
        let (a, sid) = worked_on(&r).await;
        set_level(&r.store, r.project, Some(Automation::Ship));
        passed(&r, sid).await;
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
            let (a, sid) = worked_on(&r).await;
            set_level(&r.store, r.project, Some(level));
            passed(&r, sid).await;
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
    const LOOSE: &str = r#"read p; case "$p" in *"Conclusion check"*) echo '{"text":"Added X too. DONE #1 tests:pass ok"}';; *) echo '{"text":"Pushed. Let me know if you want X. All tests passed."}';; esac"#;

    async fn finished_worker(script: &str) -> (Rig, Id, agentbench_core::SessionId) {
        let r = rig(script);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        set_level(&r.store, r.project, Some(Automation::Ship));
        auto_close(&r);
        r.ap.tick();
        settle(&r).await;
        let sid = r.store.workers().unwrap()[0].session;
        (r, a, sid)
    }

    #[tokio::test]
    async fn a_loose_reply_only_asks_the_supervisor_until_it_judges() {
        let (r, a, sid) = finished_worker(LOOSE).await;
        for _ in 0..3 {
            r.ap.tick();
        }
        let told = boss_told(&r).await;
        assert_eq!(told.len(), 1, "asked once: {told:?}");
        assert_eq!(said_to_boss(&r).len(), 1, "one message for the turn end: {:?}", said_to_boss(&r));
        let ask = &told[0];
        assert!(ask.contains(&sid.to_string()) && ask.contains("judge_session"), "{ask}");
        assert!(ask.contains("Let me know if you want X"), "the full reply: {ask}");
        assert!(ask.contains("Diff stat") && ask.contains("Test runs"), "{ask}");
        let froms: Vec<Vec<String>> = r
            .store
            .messages(r.boss.unwrap(), 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::User { text, from, .. } if text.contains("Judge worker ") => Some(from),
                _ => None,
            })
            .collect();
        assert!(
            froms.iter().all(|f| f.iter().any(|s| s.starts_with("judge "))),
            "a judge request is a system message: {froms:?}"
        );
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing, "not review");
        assert!(r.store.session(sid).is_ok(), "not closed");
        assert!(r.store.comments(a).unwrap().iter().all(|c| !c.body.contains("merged")));
        assert_eq!(r.store.workers().unwrap()[0].state, "judging");
    }

    #[tokio::test]
    async fn concluded_pass_moves_to_review_and_ship_merges() {
        let (r, a, sid) = finished_worker(DONE).await;
        let tree = r.store.worktree(r.store.session(sid).unwrap().worktree_id).unwrap();
        commit_in(&tree.path, "feature.txt");
        r.ap.tick();
        judge(&r, sid, "concluded_pass", None).unwrap();
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Review);
        r.ap.tick();
        assert!(r.repo.join("feature.txt").exists());
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Done);
        assert!(feed(&r).iter().any(|l| l.contains("done") && l.contains("judged concluded_pass: because concluded_pass")));
        let c = r.store.comments(a).unwrap();
        assert!(c.iter().any(|c| c.body.contains("concluded_pass") && c.body.contains("> Done. DONE #1")), "the judged reply is posted");
    }

    #[tokio::test]
    async fn concluded_fail_goes_to_review_and_never_merges() {
        let (r, a, sid) = finished_worker(DONE).await;
        let tree = r.store.worktree(r.store.session(sid).unwrap().worktree_id).unwrap();
        commit_in(&tree.path, "feature.txt");
        r.ap.tick();
        judge(&r, sid, "concluded_fail", None).unwrap();
        for _ in 0..3 {
            r.ap.tick();
        }
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Review);
        assert!(!r.repo.join("feature.txt").exists());
        assert!(r.store.session(sid).is_ok());
    }

    #[tokio::test]
    async fn needs_followup_sends_the_message_and_the_worker_carries_on() {
        let (r, a, sid) = finished_worker(LOOSE).await;
        r.ap.tick();
        judge(&r, sid, "needs_followup", Some("Add X yourself, it is in scope.")).unwrap();
        r.ap.tick();
        let w = r.store.workers().unwrap().remove(0);
        assert_eq!(w.reworks, 1);
        settle(&r).await;
        let sent = agentbench_core::shown(&r.store.messages(sid, 0).unwrap());
        assert!(sent.iter().any(|m| matches!(&m.entry, Entry::User { text, .. } if text.contains("Add X yourself"))));
        assert!(last_reply(&r.store, sid).contains("Added X too"));
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing, "judged again, not reviewed");
        assert_eq!(boss_told(&r).await.len(), 2, "asked again after the follow-up");
    }

    #[tokio::test]
    async fn followups_past_the_cap_block_the_issue_without_closing_or_merging() {
        let (r, a, sid) = finished_worker(r#"read p; echo '{"text":"Next I can add Y."}'"#).await;
        for _ in 0..=MAX_REWORKS {
            r.ap.tick();
            judge(&r, sid, "needs_followup", Some("do Y")).unwrap();
            r.ap.tick();
            settle(&r).await;
        }
        let i = r.store.issue(a).unwrap();
        assert!(i.labels.contains(&"blocked".to_string()), "{:?}", i.labels);
        assert_ne!(i.state, IssueState::Review);
        assert!(r.store.session(sid).is_ok());
        assert!(r.store.workers().unwrap().is_empty());
        assert!(r.store.comments(a).unwrap().iter().any(|c| c.body.contains("follow-ups")));
    }

    #[tokio::test]
    async fn waiting_on_user_labels_and_notifies_once() {
        let (r, a, sid) = finished_worker(r#"read p; echo '{"text":"Which vendor key should I use?"}'"#).await;
        r.ap.tick();
        judge(&r, sid, "waiting_on_user", None).unwrap();
        r.ap.tick();
        r.ap.tick();
        let i = r.store.issue(a).unwrap();
        assert!(i.labels.contains(&"needs-user".to_string()) && i.labels.contains(&"blocked".to_string()));
        let notices = r.store.messages(r.boss.unwrap(), 0).unwrap().into_iter().filter(|m| matches!(&m.entry, Entry::Notice { text } if text.contains("needs you"))).count();
        assert_eq!(notices, 1);
        assert!(r.store.session(sid).is_ok());
        assert_eq!(r.store.sessions().unwrap().iter().filter(|s| s.issue_id == Some(a)).count(), 1, "not restarted");
    }

    #[tokio::test]
    async fn blocked_labels_the_issue() {
        let (r, a, sid) = finished_worker(DONE).await;
        r.ap.tick();
        judge(&r, sid, "blocked", None).unwrap();
        r.ap.tick();
        let i = r.store.issue(a).unwrap();
        assert!(i.labels.contains(&"blocked".to_string()) && !i.labels.contains(&"needs-user".to_string()));
        assert!(r.store.workers().unwrap().is_empty());
        assert!(r.store.comments(a).unwrap().iter().any(|c| c.body.contains("because blocked")));
    }

    #[tokio::test]
    async fn with_no_supervisor_nothing_moves_and_the_user_is_told_once() {
        let r = rig_alone(DONE);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        set_level(&r.store, r.project, Some(Automation::Ship));
        auto_close(&r);
        r.ap.tick();
        settle(&r).await;
        let sid = r.store.workers().unwrap()[0].session;
        for _ in 0..3 {
            r.ap.tick();
        }
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing);
        assert!(r.store.session(sid).is_ok());
        assert_eq!(r.store.workers().unwrap()[0].state, "held");
        assert_eq!(feed(&r).iter().filter(|l| l.contains("no supervisor is running")).count(), 1);
    }

    #[tokio::test]
    async fn a_supervisor_that_does_not_answer_in_time_holds_the_worker_and_tells_the_user_once() {
        let (r, a, _) = finished_worker(DONE).await;
        let now = crate::store::now();
        r.ap.tick_at(now);
        r.ap.tick_at(now + 16 * 60_000);
        r.ap.tick_at(now + 40 * 60_000);
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing);
        assert_eq!(r.store.workers().unwrap()[0].state, "held");
        assert_eq!(feed(&r).iter().filter(|l| l.contains("no verdict in time")).count(), 1);
        assert_eq!(boss_told(&r).await.len(), 1, "not asked again on every tick");
    }

    #[tokio::test]
    async fn a_new_commit_voids_the_verdict() {
        let (r, a, sid) = finished_worker(DONE).await;
        let tree = r.store.worktree(r.store.session(sid).unwrap().worktree_id).unwrap();
        r.ap.tick();
        judge(&r, sid, "concluded_pass", None).unwrap();
        commit_in(&tree.path, "late.txt");
        r.ap.tick();
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing, "stale verdict not acted on");
        assert_eq!(boss_told(&r).await.len(), 2, "judged again");
        assert!(!r.repo.join("late.txt").exists());
        let s = r.store.session(sid).unwrap();
        assert!(current_verdict(&r.store, &s).is_none());
    }

    #[tokio::test]
    async fn with_three_todo_issues_and_a_cap_of_two_the_third_starts_when_one_finishes() {
        let r = rig(DONE);
        let a = todo(&r, "a", 3, &[]);
        let b = todo(&r, "b", 2, &[]);
        let c = todo(&r, "c", 1, &[]);
        r.ap.tick();
        let started = |r: &Rig| {
            let mut v: Vec<_> = r.store.sessions().unwrap().iter().filter_map(|s| s.issue_id).collect();
            v.sort();
            v
        };
        assert!(started(&r).is_empty(), "off by default");
        on(&r);
        r.ap.tick();
        assert_eq!(started(&r), [a, b]);
        assert_eq!(r.store.issue(a).unwrap().state, IssueState::Doing);
        assert_eq!(r.store.issue(c).unwrap().state, IssueState::Todo);
        r.ap.tick();
        assert_eq!(started(&r).len(), 2, "still capped while both run");
        settle(&r).await;
        r.ap.tick();
        assert_eq!(started(&r), [a, b, c], "the third starts with no user input");
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
        let sid = r.store.sessions().unwrap().iter().find(|s| s.issue_id == Some(a)).unwrap().id;
        idle(&r, r.boss.unwrap()).await;
        let notices: Vec<_> = said_to_boss(&r).into_iter().filter(|t| t.contains(&sid.to_string()) && t.contains("changed state to failed")).collect();
        assert_eq!(notices.len(), 1, "{notices:?}");
        r.ap.tick();
        assert_eq!(r.store.sessions().unwrap().iter().filter(|s| s.issue_id == Some(a)).count(), 1, "not started again");
    }

    #[tokio::test]
    async fn a_session_that_only_reads_is_stuck_even_though_it_is_not_silent() {
        let r = rig(r#"read p; sleep 30"#);
        let a = todo(&r, "a", 0, &[]);
        on(&r);
        r.ap.tick();
        let w = r.store.workers().unwrap().remove(0);
        let now = r.store.session(w.session).unwrap().updated_at;
        r.ap.tick_at(now);
        assert_eq!(r.store.workers().unwrap()[0].nudges, 0, "recent output, no gap flagged");
        r.store.set_setting(&format!("no_plan_{}", w.session), "45").unwrap();
        r.ap.tick_at(now);
        assert_eq!(r.store.workers().unwrap()[0].nudges, 1);
        let _ = a;
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
        let (r, a, sid) = finished_worker(DONE).await;
        std::fs::write(r.repo.join("scratch.txt"), "x").unwrap();
        passed(&r, sid).await;
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
