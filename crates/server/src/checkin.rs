use crate::autopilot::{self, Autopilot, clip, flag, number};
use crate::runner::{Origin, Runner};
use crate::store::{StoreError, Worker};
use agentbench_core::{Entry, Id, Message, NewSession, Place, Session, SessionId, clock_hm, span};
use std::collections::HashSet;

const MIN_DIRTY_GROWTH: usize = 2;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub command: String,
    pub exit: Option<i32>,
    pub failed: bool,
    pub results: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transcript {
    pub turn_started: i64,
    pub runs: Vec<Run>,
    pub compactions: u32,
}

fn key(command: &str) -> String {
    command.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn is_check(command: &str) -> bool {
    ["test", "build", "check", "clippy", "make"].iter().any(|w| command.contains(w))
}

fn is_test(command: &str) -> bool {
    command.contains("test")
}

fn exit_code(output: &str) -> Option<i32> {
    let at = output.rfind("[exit ")?;
    output[at + 6..].split(|c: char| !(c.is_ascii_digit() || c == '-')).next()?.parse().ok()
}

fn run_of(input: &serde_json::Value, output: Option<&str>, error: bool) -> Option<Run> {
    let command = key(input.get("command")?.as_str()?);
    let out = output.unwrap_or_default();
    let exit = exit_code(out);
    let results: Vec<String> = out
        .lines()
        .filter(|l| l.contains("test result:"))
        .map(|l| l.trim().chars().take(200).collect())
        .collect();
    let failed = error
        || exit.is_some_and(|c| c != 0)
        || results.iter().any(|l| l.contains("FAILED"));
    Some(Run { command, exit, failed, results })
}

pub fn read(messages: &[Message], fallback: i64) -> Transcript {
    let turn_at = messages
        .iter()
        .rposition(|m| matches!(&m.entry, Entry::User { from, .. } if from.is_empty()));
    let turn_started = turn_at.map(|i| messages[i].created_at).unwrap_or(fallback);
    let mut runs = Vec::new();
    let mut compactions = 0;
    for (i, m) in messages.iter().enumerate() {
        match &m.entry {
            Entry::Tool { name, input, output, error, .. } if name.eq_ignore_ascii_case("bash") => {
                if output.is_some()
                    && let Some(run) = run_of(input, output.as_deref(), *error)
                {
                    runs.push(run);
                }
            }
            Entry::Summary { .. } | Entry::Section { .. } if turn_at.is_none_or(|t| i > t) => {
                compactions += 1;
            }
            _ => {}
        }
    }
    Transcript { turn_started, runs, compactions }
}

impl Transcript {
    pub fn last_runs(&self, n: usize) -> &[Run] {
        &self.runs[self.runs.len().saturating_sub(n)..]
    }

    pub fn last_results(&self, n: usize) -> Vec<String> {
        let all: Vec<&String> = self.runs.iter().flat_map(|r| r.results.iter()).collect();
        all[all.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect()
    }

    pub fn repeated_failure(&self, times: u32) -> Option<String> {
        if times == 0 {
            return None;
        }
        let last = self.runs.iter().rev().find(|r| is_check(&r.command))?;
        let streak = self
            .runs
            .iter()
            .rev()
            .filter(|r| r.command == last.command)
            .take_while(|r| r.failed)
            .count() as u32;
        (streak >= times).then(|| last.command.clone())
    }

    pub fn regression(&self) -> Option<String> {
        let last = self.runs.iter().rev().find(|r| is_test(&r.command))?;
        let earlier_pass = self.runs.iter().any(|r| r.command == last.command && !r.failed);
        (last.failed && earlier_pass).then(|| last.command.clone())
    }
}

#[derive(Default)]
pub struct Check {
    turn: i64,
    next_at: i64,
    early: HashSet<&'static str>,
    baseline: Option<(i64, usize)>,
}

#[derive(Clone, Debug)]
pub struct StuckSignal {
    pub session: SessionId,
    pub issue: Option<Id>,
    pub reasons: Vec<String>,
}

pub type StuckHook = Arc<dyn Fn(&StuckSignal) + Send + Sync>;

fn stuck_key(id: SessionId) -> String {
    format!("check_in_stuck_{id}")
}

pub fn is_stuck(store: &crate::store::Store, id: SessionId) -> bool {
    store.setting(&stuck_key(id)).ok().flatten().as_deref() == Some("1")
}

struct Facts {
    commits_this_turn: usize,
    last_commit: i64,
    dirty: usize,
}

fn facts(path: &Path, turn_started: i64) -> Facts {
    let commits = crate::git::commit_times(path, 200);
    Facts {
        commits_this_turn: commits.iter().filter(|t| **t + 1000 > turn_started).count(),
        last_commit: commits.first().copied().unwrap_or(0),
        dirty: crate::git::dirty_count(path).unwrap_or(0),
    }
}

fn report(
    session: &Session,
    first_on_issue: i64,
    t: &Transcript,
    f: &Facts,
    now: i64,
    reasons: &[String],
    note: &str,
) -> String {
    let mut out = format!(
        "\"{title}\"{issue}, model {model}{stuck}\n",
        title = session.title,
        issue = session.issue_id.map(|i| format!(", issue #{i}")).unwrap_or_default(),
        model = session.model.as_deref().unwrap_or("default"),
        stuck = if reasons.is_empty() { String::new() } else { " STUCK".to_string() },
    );
    for r in reasons {
        out.push_str(&format!("- stuck: {r}\n"));
    }
    if !note.is_empty() {
        out.push_str(&format!("{note}\n"));
    }
    out.push_str(&format!(
        "Time in this turn: {}. Total time on the issue: {}.\n",
        span(now - t.turn_started),
        span(now - first_on_issue)
    ));
    out.push_str(&format!(
        "Commits this turn: {}, last commit {}. Dirty files: {}. Compactions this turn: {}.\n",
        f.commits_this_turn,
        if f.last_commit == 0 { "none".to_string() } else { clock_hm(f.last_commit) },
        f.dirty,
        t.compactions
    ));
    out.push_str("Last bash commands:\n");
    let runs = t.last_runs(3);
    if runs.is_empty() {
        out.push_str("  (none)\n");
    }
    for r in runs {
        let exit = r.exit.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
        out.push_str(&format!("  $ {} (exit {exit})\n", clip(&r.command, 160)));
    }
    out.push_str("Last test results:\n");
    let results = t.last_results(3);
    if results.is_empty() {
        out.push_str("  (none)\n");
    }
    for l in results {
        out.push_str(&format!("  {l}\n"));
    }
    out.push_str(&format!(
        "Answer with the check_in tool, id={}, action continue, message (with `text`, sent to the worker) or escalate (stops it and starts a takeover on the top model in the same worktree).",
        session.id
    ));
    out
}

pub fn escalate(runner: &Runner, session: &Session, brief: &str) -> Result<Session, StoreError> {
    let store = runner.store();
    let worker: Option<Worker> = store.workers()?.into_iter().find(|w| w.session == session.id);
    if worker.is_some() {
        store.remove_worker(session.id)?;
    }
    runner.interrupt(session.id);
    let parent = session.parent.or_else(|| runner.supervisor_session().map(|s| s.id));
    let prompt = format!(
        "You take over from session {id} in this worktree on the top model; it was stopped because it was stuck. Run git log and git status first, keep what works, and finish the job. Do not repeat what failed.\n\n{brief}",
        id = session.id
    );
    let new = runner.start_session(NewSession {
        parent,
        project_id: session.project_id,
        agent: session.agent.clone(),
        place: Place::Existing { worktree_id: session.worktree_id },
        title: Some(format!("takeover: {}", session.title)),
        model: Some("high".into()),
        issue_id: session.issue_id,
        prompt: Some(prompt),
        ..Default::default()
    })?;
    if let Some(w) = worker {
        store.add_worker(new.id, w.issue, w.project)?;
    }
    store.set_setting(&stuck_key(session.id), "0")?;
    Ok(new)
}

impl Autopilot {
    pub fn on_stuck(&self, hook: StuckHook) {
        *self.stuck_hook.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    fn is_top(&self, session: &Session) -> bool {
        let Some(model) = session.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) else {
            return false;
        };
        let store = self.store();
        model.eq_ignore_ascii_case("high")
            || self.runner.quotas.level_model("high").as_deref() == Some(model)
            || crate::providers::level_model(store, &session.agent, "high").as_deref() == Some(model)
    }

    pub(crate) fn check_in(&self, w: &Worker, session: &Session, now: i64) -> bool {
        let store = self.store();
        if (self.is_top(session) && !flag(store, autopilot::CHECK_IN_ALL))
            || crate::quota::paused_until(store, session.id).is_some()
        {
            return false;
        }
        let Some(sup) = self.runner.supervisor_session() else { return false };
        let Ok(tree) = store.worktree(session.worktree_id) else { return false };
        let minutes = |key, default| number::<u64>(store, key, default) as i64 * 60_000;
        let interval = minutes(autopilot::CHECK_IN_MINUTES, autopilot::DEFAULT_CHECK_IN_MINUTES);
        let quiet_limit = minutes(autopilot::STUCK_NO_COMMIT_MINUTES, autopilot::DEFAULT_STUCK_NO_COMMIT_MINUTES);
        let failed_limit = number(store, autopilot::STUCK_FAILED_RUNS, autopilot::DEFAULT_STUCK_FAILED_RUNS);
        let compaction_limit = number(store, autopilot::STUCK_COMPACTIONS, autopilot::DEFAULT_STUCK_COMPACTIONS);
        let t = read(&store.messages(session.id, 0).unwrap_or_default(), session.created_at);
        let f = facts(Path::new(&tree.path), t.turn_started);

        let mut signals: Vec<(&'static str, String)> = Vec::new();
        if let Some(cmd) = t.repeated_failure(failed_limit) {
            signals.push(("failed_runs", format!("`{}` failed {failed_limit} or more times in a row", clip(&cmd, 120))));
        }
        if let Some(cmd) = t.regression() {
            signals.push(("regression", format!("`{}` passed earlier in this session and now fails", clip(&cmd, 120))));
        }
        if compaction_limit > 0 && t.compactions >= compaction_limit {
            signals.push(("compactions", format!("{} compactions in one turn", t.compactions)));
        }

        let (due, fresh) = {
            let mut checks = self.checks.lock().unwrap_or_else(|e| e.into_inner());
            let c = checks.entry(session.id).or_default();
            if c.turn != t.turn_started {
                *c = Check { turn: t.turn_started, next_at: t.turn_started + interval, ..Default::default() };
            }
            if c.baseline.is_none_or(|b| b.0 != f.last_commit) {
                c.baseline = Some((f.last_commit, f.dirty));
            }
            let grown = c.baseline.is_some_and(|b| f.dirty >= b.1 + MIN_DIRTY_GROWTH);
            if quiet_limit > 0 && grown && now - f.last_commit.max(t.turn_started) >= quiet_limit {
                signals.push(("no_commit", format!("no commit for {} while the dirty files grew to {}", span(now - f.last_commit.max(t.turn_started)), f.dirty)));
            }
            let due = interval > 0 && now >= c.next_at;
            if due {
                c.next_at = now + interval;
            }
            let fresh: Vec<&'static str> =
                signals.iter().map(|s| s.0).filter(|k| c.early.insert(k)).collect();
            (due, fresh)
        };
        let stuck = !signals.is_empty();
        let _ = store.set_setting(&stuck_key(session.id), if stuck { "1" } else { "0" });
        if !due && fresh.is_empty() {
            return false;
        }

        let reasons: Vec<String> = signals.iter().map(|s| s.1.clone()).collect();
        let first_on_issue = session
            .issue_id
            .and_then(|i| {
                store.sessions().ok()?.iter().filter(|s| s.issue_id == Some(i)).map(|s| s.created_at).min()
            })
            .unwrap_or(session.created_at);
        let escalating = stuck && !fresh.is_empty() && flag(store, autopilot::AUTO_ESCALATE);
        let note = if escalating { "Auto-escalate is on: the session was stopped and a takeover started on the top model." } else { "" };
        let body = report(session, first_on_issue, &t, &f, now, &reasons, note);
        if stuck {
            if !fresh.is_empty() {
                crate::process::stuck(store, session, &reasons);
            }
            let hook = self.stuck_hook.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(hook) = hook {
                hook(&StuckSignal { session: session.id, issue: session.issue_id, reasons: reasons.clone() });
            }
        }
        let origin = Origin::new("check_in", Some(session.id));
        let told = self.runner.tell(&sup, &body, origin);
        self.log(
            Some(w.project),
            Some(w.issue),
            &format!(
                "check_in on #{}{}{}",
                w.issue,
                if stuck { ": stuck" } else { "" },
                if told.is_err() { " (the supervisor could not be told)" } else { "" }
            ),
        );
        if escalating {
            match escalate(&self.runner, session, &body) {
                Ok(new) => {
                    self.log(Some(w.project), Some(w.issue), &format!("auto-escalated #{} to the top model in session {}", w.issue, new.id));
                    return true;
                }
                Err(e) => self.log(Some(w.project), Some(w.issue), &format!("could not escalate #{}: {}", w.issue, clip(&e.0, 160))),
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(id: i64, command: &str, output: &str, error: bool) -> Message {
        Message {
            id,
            session_id: Default::default(),
            created_at: id * 1000,
            entry: Entry::Tool {
                call_id: id.to_string(),
                name: "bash".into(),
                input: json!({ "command": command }),
                output: Some(output.into()),
                error,
            },
        }
    }

    fn said(id: i64, from: &[&str]) -> Message {
        Message {
            id,
            session_id: Default::default(),
            created_at: id * 1000,
            entry: Entry::User { text: "go".into(), attachments: vec![], from: from.iter().map(|s| s.to_string()).collect() },
        }
    }

    fn fails(id: i64, command: &str) -> Message {
        bash(id, command, "test result: FAILED. 0 passed\n[exit 101 in 2.0s]", true)
    }

    fn passes(id: i64, command: &str) -> Message {
        bash(id, command, "test result: ok. 4 passed\n[exit 0 in 2.0s]", false)
    }

    #[test]
    fn exit_codes_and_test_results_are_read_from_bash_output() {
        let t = read(&[said(1, &[]), fails(2, "cargo  test"), passes(3, "cargo test -p x")], 0);
        assert_eq!(t.runs[0].command, "cargo test");
        assert_eq!((t.runs[0].exit, t.runs[0].failed), (Some(101), true));
        assert_eq!((t.runs[1].exit, t.runs[1].failed), (Some(0), false));
        assert_eq!(t.last_results(1), vec!["test result: ok. 4 passed".to_string()]);
        assert_eq!(t.turn_started, 1000);
    }

    #[test]
    fn three_failures_of_one_command_in_a_row_are_repeated_failure() {
        let m = [said(1, &[]), fails(2, "cargo test"), fails(3, "cargo test"), bash(4, "ls", "x", false)];
        assert_eq!(read(&m, 0).repeated_failure(3), None);
        let m = [said(1, &[]), fails(2, "cargo test"), fails(3, "cargo test"), bash(4, "ls", "x", false), fails(5, "cargo test")];
        assert_eq!(read(&m, 0).repeated_failure(3).as_deref(), Some("cargo test"));
        let m = [fails(2, "cargo test"), fails(3, "cargo test"), passes(4, "cargo test"), fails(5, "cargo test")];
        assert_eq!(read(&m, 0).repeated_failure(3), None, "a pass breaks the streak");
    }

    #[test]
    fn a_test_that_passed_and_now_fails_is_a_regression() {
        let m = [passes(1, "cargo test"), fails(2, "cargo test")];
        assert_eq!(read(&m, 0).regression().as_deref(), Some("cargo test"));
        let m = [fails(1, "cargo test"), fails(2, "cargo test")];
        assert_eq!(read(&m, 0).regression(), None);
        let m = [passes(1, "cargo test"), fails(2, "cargo test"), passes(3, "cargo test")];
        assert_eq!(read(&m, 0).regression(), None);
    }

    #[test]
    fn compactions_count_only_from_the_last_typed_message() {
        let s = |id| Message { entry: Entry::Summary { text: "s".into() }, ..said(id, &[]) };
        let m = [said(1, &[]), s(2), said(3, &["judge"]), s(4), said(5, &[]), s(6), s(7)];
        assert_eq!(read(&m, 0).compactions, 2);
    }
}
