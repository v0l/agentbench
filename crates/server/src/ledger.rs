use crate::store::{OpenItem, Store};
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
    matches!(kind, "judge" | "check_in" | "watch" | "groom" | "main_dirty" | "question" | "user")
}

fn clip(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(CLIP) {
        Some((at, _)) => format!("{}...", &line[..at]),
        None => line.to_string(),
    }
}

pub fn delivered(store: &Store, supervisor: SessionId, source: &str, worker: Option<SessionId>, text: &str) {
    let kind = kind_of(source);
    if !is_tracked(kind) {
        return;
    }
    let target = worker.map(|w| w.to_string());
    let already = store.open_items(supervisor).unwrap_or_default().iter().any(|i| {
        i.kind == kind && i.target == target && (kind != "user" && i.text == clip(text) || kind == "judge")
    });
    if already {
        if kind == "judge" {
            let stale: Vec<_> = store
                .open_items(supervisor)
                .unwrap_or_default()
                .iter()
                .filter(|i| i.kind == kind && i.target == target)
                .map(|i| i.id)
                .collect();
            let _ = store.close_open_items(&stale);
        } else {
            return;
        }
    }
    let head = (kind == "judge")
        .then(|| worker.and_then(|w| store.session(w).ok()).and_then(|s| crate::autopilot::head_of(store, &s)))
        .flatten();
    let _ = store.add_open_item(supervisor, kind, target.as_deref(), head.as_deref(), &clip(text));
}

pub fn duplicate_judge(store: &Store, worker: SessionId) -> bool {
    store.session(worker).is_ok_and(|s| crate::autopilot::already_judged(store, &s))
}

fn close_where(store: &Store, supervisor: SessionId, pick: impl Fn(&OpenItem) -> bool) {
    let ids: Vec<_> =
        store.open_items(supervisor).unwrap_or_default().iter().filter(|i| pick(i)).map(|i| i.id).collect();
    if !ids.is_empty() {
        let _ = store.close_open_items(&ids);
    }
}

pub fn closed_by(store: &Store, tool: &str, args: &Value, supervisor: SessionId) {
    let target = args.get("id").and_then(Value::as_str).map(|s| s.trim().to_string());
    let on = |kinds: &'static [&'static str]| {
        close_where(store, supervisor, |i| {
            kinds.contains(&i.kind.as_str()) && target.as_ref().is_some_and(|t| i.target.as_ref() == Some(t))
        })
    };
    match tool {
        "judge_session" => on(&["judge"]),
        "check_in" => on(&["check_in"]),
        "answer_session" | "escalate_question" => on(&["question"]),
        "unwatch_session" | "read_session" => on(&["watch"]),
        "file_issue" | "start_session" | "update_issue" | "comment_on_issue" => {
            close_where(store, supervisor, |i| i.kind == "user")
        }
        "groom_done" => close_where(store, supervisor, |i| i.kind == "groom"),
        "main_checkout" => close_where(store, supervisor, |i| i.kind == "main_dirty"),
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

pub fn sweep(store: &Store, supervisor: SessionId) {
    let settled: Vec<_> = store
        .open_items(supervisor)
        .unwrap_or_default()
        .into_iter()
        .filter(|i| i.kind != "user")
        .filter(|i| i.target.as_deref().and_then(|t| t.parse().ok()).is_some_and(|t| target_settled(store, t, &i.kind)))
        .map(|i| i.id)
        .collect();
    if !settled.is_empty() {
        let _ = store.close_open_items(&settled);
    }
}

pub fn close_replied(store: &Store, supervisor: SessionId) {
    let users: Vec<OpenItem> =
        store.open_items(supervisor).unwrap_or_default().into_iter().filter(|i| i.kind == "user").collect();
    if users.is_empty() {
        return;
    }
    let replies: Vec<i64> = store
        .messages(supervisor, 0)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| matches!(m.entry, Entry::Assistant { .. }))
        .map(|m| m.created_at)
        .collect();
    let ids: Vec<_> =
        users.iter().filter(|u| replies.iter().any(|r| *r >= u.delivered_at)).map(|u| u.id).collect();
    if !ids.is_empty() {
        let _ = store.close_open_items(&ids);
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
        let ids: Vec<_> = open.iter().map(|i| i.id).collect();
        let _ = store.close_open_items(&ids);
        return Hold::GiveUp(lines);
    }
    let _ = store.set_setting(&key, &(sent + 1).to_string());
    Hold::Nudge(format!(
        "You still have {} open items:\n{}\nClose each one by calling one of: judge_session, check_in, answer_session, escalate_question, file_issue, start_session, update_issue, comment_on_issue, or by replying, or write what blocks it into a tracker issue, then end the turn.",
        open.len(),
        lines.join("\n")
    ))
}
