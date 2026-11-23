use crate::store::Store;

pub const DAY_MS: i64 = 86_400_000;

fn day_key(now: i64) -> String {
    format!("untrusted_turns_{}", now.div_euclid(DAY_MS))
}

pub fn turns_today(store: &Store, now: i64) -> u64 {
    store.setting(&day_key(now)).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0)
}

pub fn turn_cap(store: &Store) -> u64 {
    store
        .setting(crate::autopilot::UNTRUSTED_TURNS)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(crate::autopilot::DEFAULT_UNTRUSTED_TURNS)
}

pub fn turns_left(store: &Store, now: i64) -> bool {
    turns_today(store, now) < turn_cap(store)
}

pub fn count_turn(store: &Store, now: i64) {
    let _ = store.set_setting(&day_key(now), &(turns_today(store, now) + 1).to_string());
}

pub fn count_session_turn(store: &Store, session: &agentbench_core::Session, now: i64) {
    if session.issue_id.and_then(|i| store.issue(i).ok()).is_some_and(|i| i.trust.is_untrusted()) {
        count_turn(store, now);
    }
}

pub const CAP_NOTE: &str = "Kept in backlog: the daily cap on agent turns spent on untrusted issues is used up. It is picked up again after the cap resets.";

pub fn startable(store: &Store, issues: Vec<agentbench_core::Issue>, now: i64) -> Vec<agentbench_core::Issue> {
    let mut left = turn_cap(store).saturating_sub(turns_today(store, now));
    let mut out = Vec::with_capacity(issues.len());
    for i in issues {
        let approved = i.source != agentbench_core::IssueSource::Github || !i.trust.is_untrusted() || i.ready_by.is_some();
        if i.state == agentbench_core::IssueState::Todo && !approved {
            continue;
        }
        if i.state == agentbench_core::IssueState::Todo && i.trust.is_untrusted() {
            if left == 0 {
                let noted = store.comments(i.id).unwrap_or_default().last().is_some_and(|c| c.body == CAP_NOTE);
                if !noted {
                    let _ = store.add_comment_local(i.id, &agentbench_core::NewComment { author: Some("agentbench".into()), body: CAP_NOTE.into() });
                }
                continue;
            }
            left -= 1;
        }
        out.push(i);
    }
    out
}
