use crate::store::{Closing, NewLogItem, OpenItem, Store};
use agentbench_core::supervisor_log::{LOG_CLIP, LOG_DAYS_DEFAULT, LOG_DAYS_SETTING, clip_chars, first_sentence};
use agentbench_core::{Entry, IssueState, SessionId};
use serde_json::Value;
use std::path::Path;

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;

pub const MAX_NUDGES: u32 = 3;
const PENDING_AFTER_MS: i64 = 60_000;
const CLIP: usize = 160;

fn nudges_key(session: SessionId) -> String {
    format!("open_nudges_{session}")
}

fn kind_of(source: &str) -> &str {
    match source.split_whitespace().next().unwrap_or(source) {
        "main_checkout" => "main_dirty",
        other => other,
    }
}

fn is_tracked(kind: &str) -> bool {
    matches!(kind, "judge" | "check_in" | "watch" | "groom" | "judge_issues" | "main_dirty" | "question" | "user")
}

fn clip(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(CLIP) {
        Some((at, _)) => format!("{}...", &line[..at]),
        None => line.to_string(),
    }
}

fn short(text: &str) -> String {
    clip_chars(text.trim(), LOG_CLIP).0
}

pub fn delivered(store: &Store, supervisor: SessionId, source: &str, worker: Option<SessionId>, text: &str) {
    let kind = kind_of(source);
    let tracked = is_tracked(kind);
    let target = worker.map(|w| w.to_string());
    let already = tracked
        && store.open_items(supervisor).unwrap_or_default().iter().any(|i| {
            i.kind == kind && i.target == target && (kind != "user" && i.text == clip(text) || kind == "judge")
        });
    if already {
        if kind == "judge" {
            let stale: Vec<OpenItem> = store
                .open_items(supervisor)
                .unwrap_or_default()
                .into_iter()
                .filter(|i| i.kind == kind && i.target == target)
                .collect();
            close_items(store, &stale, &Closing::new("superseded").outcome("a newer judge request replaced this one"));
        } else {
            return;
        }
    }
    let log_id = log_arrival(store, supervisor, source, kind, worker, text, tracked);
    if !tracked {
        return;
    }
    let head = (kind == "judge")
        .then(|| worker.and_then(|w| store.session(w).ok()).and_then(|s| crate::autopilot::head_of(store, &s)))
        .flatten();
    let _ = store.add_open_item(supervisor, kind, target.as_deref(), head.as_deref(), &clip(text), log_id);
}

fn log_arrival(
    store: &Store,
    supervisor: SessionId,
    source: &str,
    kind: &str,
    worker: Option<SessionId>,
    text: &str,
    needs_answer: bool,
) -> Option<agentbench_core::Id> {
    prune(store);
    let asking = worker.and_then(|w| store.session(w).ok());
    let named = source.split_once(' ').map(|(_, name)| name.trim()).filter(|n| !n.is_empty());
    let project_id = asking.as_ref().map(|s| s.project_id).or_else(|| {
        named.and_then(|n| store.projects().ok()?.into_iter().find(|p| p.name == n).map(|p| p.id))
    });
    let asker = match (kind, &asking) {
        ("user", _) => "user".to_string(),
        (_, Some(s)) => s.id.to_string(),
        _ => "autopilot".to_string(),
    };
    let item = NewLogItem {
        supervisor: Some(supervisor),
        kind,
        asker: &asker,
        asker_title: asking.as_ref().map(|s| s.title.as_str()).unwrap_or_default(),
        session: worker,
        project_id,
        issue_id: asking.as_ref().and_then(|s| s.issue_id),
        question: text.trim(),
        needs_answer,
    };
    store.add_log(&item).ok()
}

pub fn log_days(store: &Store) -> i64 {
    store
        .setting(LOG_DAYS_SETTING)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(LOG_DAYS_DEFAULT)
}

fn prune(store: &Store) {
    let days = log_days(store);
    if days > 0 {
        let _ = store.prune_log(crate::store::now() - days * 86_400_000);
    }
}

fn close_items(store: &Store, items: &[OpenItem], closing: &Closing) {
    if items.is_empty() {
        return;
    }
    for item in items {
        if let Some(log) = item.log_id {
            let _ = store.close_log(log, closing);
        }
    }
    let ids: Vec<_> = items.iter().map(|i| i.id).collect();
    let _ = store.close_open_items(&ids);
}

pub fn duplicate_judge(store: &Store, worker: SessionId) -> bool {
    store.session(worker).is_ok_and(|s| crate::autopilot::already_judged(store, &s))
}

fn close_where(store: &Store, supervisor: SessionId, closing: &Closing, pick: impl Fn(&OpenItem) -> bool) {
    let items: Vec<OpenItem> =
        store.open_items(supervisor).unwrap_or_default().into_iter().filter(|i| pick(i)).collect();
    close_items(store, &items, closing);
}

fn arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).map(str::trim).unwrap_or_default()
}

pub fn closing_for(tool: &str, args: &Value, out: &Value) -> Closing {
    let summary = arg(args, "answer_summary");
    let mut c = match tool {
        "judge_session" => {
            let mut outcome = arg(args, "reason").to_string();
            let followup = arg(args, "followup");
            if !followup.is_empty() {
                outcome = format!("{outcome}. Followup: {followup}");
            }
            Closing::new(tool).detail(arg(args, "verdict")).outcome(short(&outcome))
        }
        "check_in" => Closing::new(tool).detail(arg(args, "action").to_lowercase()).outcome(short(arg(args, "text"))),
        "answer_session" => Closing::new(tool).outcome(short(arg(args, "answer"))),
        "escalate_question" => Closing::new(tool).outcome(short(arg(args, "recommendation"))),
        "file_issue" => {
            let n = out.get("filed").and_then(Value::as_i64);
            let mut c = Closing::new(tool)
                .detail(n.map(|n| format!("#{n}")).unwrap_or_default())
                .outcome(short(arg(args, "title")));
            c.issue_id = n;
            c
        }
        "start_session" => {
            let id = out.get("session").and_then(Value::as_str).unwrap_or_default();
            let title = out.get("title").and_then(Value::as_str).unwrap_or_default();
            let mut c = Closing::new(tool).detail(id.chars().take(8).collect::<String>()).outcome(short(title));
            c.issue_id = args.get("issue").and_then(Value::as_i64);
            c
        }
        "groom_done" => Closing::new(tool).outcome(short(arg(args, "summary"))),
        "judge_issue" => Closing::new(tool).outcome(short(arg(args, "reason"))),
        "main_checkout" => Closing::new(tool).detail(arg(args, "action")).outcome(short(arg(args, "message"))),
        other => Closing::new(other),
    };
    if !summary.is_empty() {
        c.outcome = short(summary);
    }
    c
}

pub fn worker_resumed(store: &Store, worker: SessionId) {
    let stale = store.open_items_targeting("judge", &worker.to_string()).unwrap_or_default();
    close_items(store, &stale, &Closing::new("resumed").outcome(RESUMED_OUTCOME));
}

const RESUMED_OUTCOME: &str = "worker resumed, judge on next turn end";

pub fn closed_by(store: &Store, tool: &str, args: &Value, supervisor: SessionId, out: &Value) {
    let target = args.get("id").and_then(Value::as_str).map(|s| s.trim().to_string());
    let closing = closing_for(tool, args, out);
    let on = |kinds: &'static [&'static str]| {
        close_where(store, supervisor, &closing, |i| {
            kinds.contains(&i.kind.as_str()) && target.as_ref().is_some_and(|t| i.target.as_ref() == Some(t))
        })
    };
    match tool {
        "judge_session" => on(&["judge"]),
        "check_in" => {
            on(&["check_in"]);
            let running = target
                .as_ref()
                .and_then(|t| t.parse::<SessionId>().ok())
                .and_then(|id| store.session(id).ok())
                .is_some_and(|s| s.status == agentbench_core::SessionStatus::Running);
            if running {
                on(&["judge"]);
            }
        }
        "answer_session" | "escalate_question" => on(&["question"]),
        "unwatch_session" | "read_session" => on(&["watch"]),
        "file_issue" | "start_session" => {
            let oldest = store.open_items(supervisor).unwrap_or_default().into_iter().find(|i| i.kind == "user");
            if let Some(item) = oldest {
                close_items(store, &[item], &closing);
            }
        }
        "groom_done" => close_where(store, supervisor, &closing, |i| i.kind == "groom"),
        "judge_issue" if crate::judging::all_answered(store) => {
            close_where(store, supervisor, &closing, |i| i.kind == "judge_issues")
        }
        "main_checkout" => close_where(store, supervisor, &closing, |i| i.kind == "main_dirty"),
        _ => {}
    }
}

fn target_settled(store: &Store, target: SessionId, kind: &str) -> bool {
    let Ok(session) = store.session(target) else { return true };
    if session.issue_id.is_some_and(|i| store.issue(i).is_ok_and(|i| i.state == IssueState::Done)) {
        return true;
    }
    if !matches!(kind, "judge" | "main_dirty") {
        return false;
    }
    let (Ok(tree), Ok(project)) = (store.worktree(session.worktree_id), store.project(session.project_id)) else {
        return false;
    };
    let Some(branch) = tree.branch.as_deref().filter(|_| !tree.main) else { return false };
    let main = Path::new(&project.path);
    crate::git::is_merged(main, branch)
}

fn settled_why(store: &Store, target: SessionId) -> &'static str {
    match store.session(target) {
        Err(_) => "the session is gone",
        Ok(s) if s.issue_id.is_some_and(|i| store.issue(i).is_ok_and(|i| i.state == IssueState::Done)) => {
            "the issue is done"
        }
        Ok(_) => "the branch is merged",
    }
}

pub fn sweep(store: &Store, supervisor: SessionId) {
    let settled: Vec<(OpenItem, SessionId)> = store
        .open_items(supervisor)
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.kind != "user")
        .filter_map(|i| {
            let t: SessionId = i.target.as_deref()?.parse().ok()?;
            target_settled(store, t, &i.kind).then_some((i, t))
        })
        .collect();
    for (item, target) in settled {
        close_items(store, &[item], &Closing::new("settled").outcome(settled_why(store, target)));
    }
}

fn replies(store: &Store, supervisor: SessionId) -> Vec<(i64, String)> {
    let rows = store.messages(supervisor, 0).unwrap_or_default();
    agentbench_core::shown(&rows)
        .into_iter()
        .filter_map(|m| match m.entry {
            Entry::Assistant { text } => Some((m.created_at, text)),
            _ => None,
        })
        .collect()
}

fn reply_after(replies: &[(i64, String)], at: i64) -> Option<&str> {
    replies.iter().find(|(t, _)| *t >= at).map(|(_, text)| text.as_str())
}

pub fn close_replied(store: &Store, supervisor: SessionId) {
    let users: Vec<OpenItem> =
        store.open_items(supervisor).unwrap_or_default().into_iter().filter(|i| i.kind == "user").collect();
    let waiting = store.log_waiting_for_reply(supervisor).unwrap_or_default();
    if users.is_empty() && waiting.is_empty() {
        return;
    }
    let said = replies(store, supervisor);
    for u in users {
        if let Some(text) = reply_after(&said, u.delivered_at) {
            let closing = Closing { reply: short(text), ..Closing::new("replied").outcome(first_sentence(text)) };
            close_items(store, &[u], &closing);
        }
    }
    for (id, arrived) in waiting {
        if let Some(text) = reply_after(&said, arrived) {
            let _ = store.set_log_outcome(id, &first_sentence(text), &short(text));
        }
    }
}

fn waited(ms: i64) -> String {
    let minutes = (ms / 60_000).max(0);
    match minutes {
        0 => "under a minute".to_string(),
        1 => "1 minute".to_string(),
        n => format!("{n} minutes"),
    }
}

fn line(n: usize, item: &OpenItem, now: i64) -> String {
    let who = match item.kind.as_str() {
        "user" => "user message".to_string(),
        kind => match &item.target {
            Some(t) => format!("{kind} for session {t}"),
            None => kind.to_string(),
        },
    };
    format!("{n}. {who}, waited {}: {}", waited(now - item.delivered_at), item.text)
}

pub fn pending_block(store: &Store, supervisor: SessionId, now: i64) -> String {
    sweep(store, supervisor);
    close_replied(store, supervisor);
    let old: Vec<OpenItem> = store
        .open_items(supervisor)
        .unwrap_or_default()
        .into_iter()
        .filter(|i| now - i.delivered_at >= PENDING_AFTER_MS)
        .collect();
    if old.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = old.iter().enumerate().map(|(n, i)| line(n + 1, i, now)).collect();
    format!(
        "<pending>\nThese items reached you earlier and are still open, oldest first. Close each with its action before you end the turn.\n{}\n</pending>",
        lines.join("\n")
    )
}

pub enum Hold {
    Nudge(String),
    GiveUp(Vec<String>),
    Clear,
}

pub const HOLD_SETTING: &str = "supervisor_hold_open_items";

fn holds(kind: &str) -> bool {
    matches!(kind, "judge" | "check_in" | "main_dirty" | "question" | "user")
}

pub fn at_turn_end(store: &Store, supervisor: SessionId, now: i64) -> Hold {
    close_replied(store, supervisor);
    sweep(store, supervisor);
    let off = store.setting(HOLD_SETTING).ok().flatten().is_some_and(|v| v == "0");
    let open: Vec<OpenItem> =
        store.open_items(supervisor).unwrap_or_default().into_iter().filter(|i| holds(&i.kind)).collect();
    let key = nudges_key(supervisor);
    if open.is_empty() || off {
        let _ = store.set_setting(&key, "0");
        return Hold::Clear;
    }
    let sent: u32 = store.setting(&key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0);
    let lines: Vec<String> = open.iter().enumerate().map(|(n, i)| line(n + 1, i, now)).collect();
    if sent >= MAX_NUDGES {
        let _ = store.set_setting(&key, "0");
        close_items(store, &open, &Closing::new("stale").outcome(format!("still open after {MAX_NUDGES} reminders")));
        return Hold::GiveUp(lines);
    }
    let _ = store.set_setting(&key, &(sent + 1).to_string());
    Hold::Nudge(format!(
        "You still have {} open items:\n{}\nClose each one by calling one of: judge_session, check_in, answer_session, escalate_question, file_issue or start_session (each closes the oldest user ask), or by replying. A comment on an issue (comment_on_issue) or update_issue does not close an ask. Or write what blocks it into a tracker issue, then end the turn.",
        open.len(),
        lines.join("\n")
    ))
}
