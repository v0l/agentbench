use super::*;
use agentbench_core::Session;
use std::process::Command;

struct World {
    store: Store,
    boss: Session,
    worker: Session,
    main: std::path::PathBuf,
    project: i64,
    _dir: tempfile::TempDir,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("r");
    crate::git::testing::repo(&main);
    let store = Store::memory().unwrap();
    let p = crate::projects::add(&store, &main, None, false).unwrap();
    let tree = store.main_worktree(p.id).unwrap();
    let sup = crate::projects::ensure_supervisor(&store, &dir.path().join("sup")).unwrap();
    let sup_tree = store.main_worktree(sup.id).unwrap();
    let boss = store.add_session(sup_tree.id, "boss", "boss", None, None).unwrap();
    let worker = store.add_session(tree.id, "worker", "worker", None, None).unwrap();
    store.set_parent(worker.id, Some(boss.id)).unwrap();
    World { store, boss, worker, main, project: p.id, _dir: dir }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

const LATER: i64 = 120_000;

#[test]
fn an_item_older_than_a_minute_shows_in_the_pending_block_oldest_first() {
    let w = world();
    let at = crate::store::now();
    delivered(&w.store, w.boss.id, "user", None, "where is the thing");
    delivered(&w.store, w.boss.id, "check_in", Some(w.worker.id), "check on it");
    assert_eq!(pending_block(&w.store, w.boss.id, at), "", "fresh items are not listed");
    let block = pending_block(&w.store, w.boss.id, at + LATER);
    assert!(block.starts_with("<pending>") && block.ends_with("</pending>"), "{block}");
    assert!(block.find("where is the thing").unwrap() < block.find("check on it").unwrap(), "{block}");
    assert!(block.contains("waited 2 minutes"), "{block}");
}

#[test]
fn judge_session_closes_the_judge_item() {
    let w = world();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "judge it");
    closed_by(&w.store, "judge_session", &serde_json::json!({"id": w.worker.id.to_string()}), w.boss.id, &serde_json::Value::Null);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
}

#[test]
fn a_worker_resuming_closes_its_judge_item_and_the_next_turn_end_opens_a_new_one() {
    let w = world();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "judge it");
    delivered(&w.store, w.boss.id, "check_in", Some(w.worker.id), "check on it");
    let judge_log = w.store.open_items(w.boss.id).unwrap()[0].log_id.unwrap();
    worker_resumed(&w.store, w.worker.id);
    let open = w.store.open_items(w.boss.id).unwrap();
    assert_eq!(open.iter().map(|i| i.kind.as_str()).collect::<Vec<_>>(), ["check_in"]);
    let log = w.store.log_item(judge_log).unwrap().unwrap();
    assert_eq!(log.action, "resumed");
    assert_eq!(log.outcome, "worker resumed, judge on next turn end");
    let block = pending_block(&w.store, w.boss.id, crate::store::now() + LATER);
    assert!(!block.contains("judge for session"), "{block}");
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "judge again");
    let kinds: Vec<_> = w.store.open_items(w.boss.id).unwrap().into_iter().map(|i| i.kind).collect();
    assert_eq!(kinds, ["check_in", "judge"]);
}

#[test]
fn check_in_closes_the_judge_item_of_a_running_session() {
    let w = world();
    w.store.set_status(w.worker.id, agentbench_core::SessionStatus::Running).unwrap();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "judge it");
    closed_by(&w.store, "check_in", &serde_json::json!({"id": w.worker.id.to_string(), "action": "continue"}), w.boss.id, &serde_json::Value::Null);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
}

#[test]
fn check_in_keeps_the_judge_item_of_an_idle_session() {
    let w = world();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "judge it");
    closed_by(&w.store, "check_in", &serde_json::json!({"id": w.worker.id.to_string(), "action": "continue"}), w.boss.id, &serde_json::Value::Null);
    assert_eq!(w.store.open_items(w.boss.id).unwrap().len(), 1);
}

#[test]
fn a_tool_for_another_session_leaves_the_item_open() {
    let w = world();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "judge it");
    closed_by(&w.store, "judge_session", &serde_json::json!({"id": "other"}), w.boss.id, &serde_json::Value::Null);
    closed_by(&w.store, "check_in", &serde_json::json!({"id": w.worker.id.to_string()}), w.boss.id, &serde_json::Value::Null);
    assert_eq!(w.store.open_items(w.boss.id).unwrap().len(), 1);
}

#[test]
fn a_merged_branch_closes_its_judge_item() {
    let w = world();
    let tree = crate::projects::add_worktree(&w.store, w.project, "feat", None).unwrap();
    let session = w.store.add_session(tree.id, "worker", "feat", None, None).unwrap();
    std::fs::write(Path::new(&tree.path).join("a.txt"), "a").unwrap();
    git(Path::new(&tree.path), &["add", "."]);
    git(Path::new(&tree.path), &["commit", "-qm", "work"]);
    delivered(&w.store, w.boss.id, "judge", Some(session.id), "judge feat");
    sweep(&w.store, w.boss.id);
    assert_eq!(w.store.open_items(w.boss.id).unwrap().len(), 1, "unmerged stays open");
    git(&w.main, &["merge", "-q", "--no-edit", "feat"]);
    sweep(&w.store, w.boss.id);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
}

#[test]
fn a_removed_session_closes_its_items() {
    let w = world();
    let tree = crate::projects::add_worktree(&w.store, w.project, "gone", None).unwrap();
    let session = w.store.add_session(tree.id, "worker", "gone", None, None).unwrap();
    delivered(&w.store, w.boss.id, "check_in", Some(session.id), "check");
    w.store.delete_session(session.id).unwrap();
    sweep(&w.store, w.boss.id);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
}

#[test]
fn a_second_judge_item_for_one_worker_replaces_the_first() {
    let w = world();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "first");
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "second");
    let open = w.store.open_items(w.boss.id).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].text, "second");
}

#[test]
fn a_user_message_closes_only_on_a_supervisor_reply() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "hello");
    closed_by(&w.store, "judge_session", &serde_json::json!({"id": w.worker.id.to_string()}), w.boss.id, &serde_json::Value::Null);
    close_replied(&w.store, w.boss.id);
    assert_eq!(w.store.open_items(w.boss.id).unwrap().len(), 1);
    std::thread::sleep(std::time::Duration::from_millis(5));
    w.store.add_message(w.boss.id, &Entry::Assistant { text: "done".into() }).unwrap();
    close_replied(&w.store, w.boss.id);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
}

#[test]
fn a_wire_logged_reply_closes_the_user_ask() {
    use agentbench_core::{Said, Sent};
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "what about the sessions");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let tool_only = Sent {
        message: serde_json::json!({"role": "assistant", "content": "", "tool_calls": [{"id": "1"}]}),
        said: Said::Plain,
    };
    w.store.add_message(w.boss.id, &Entry::Wire { messages: vec![tool_only] }).unwrap();
    close_replied(&w.store, w.boss.id);
    assert_eq!(w.store.open_items(w.boss.id).unwrap().len(), 1);
    let reply = Sent { message: serde_json::json!({"role": "assistant", "content": "closed them"}), said: Said::Plain };
    w.store.add_message(w.boss.id, &Entry::Wire { messages: vec![reply] }).unwrap();
    close_replied(&w.store, w.boss.id);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
}

#[test]
fn file_issue_closes_an_open_user_item() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "please track this");
    closed_by(&w.store, "file_issue", &serde_json::json!({"title": "t"}), w.boss.id, &serde_json::Value::Null);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
}

#[test]
fn file_issue_closes_only_the_oldest_user_item() {
    let w = world();
    for t in ["one", "two", "three"] {
        delivered(&w.store, w.boss.id, "user", None, t);
    }
    closed_by(&w.store, "file_issue", &serde_json::json!({"title": "t"}), w.boss.id, &serde_json::Value::Null);
    let open = w.store.open_items(w.boss.id).unwrap();
    assert_eq!(open.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["two", "three"]);
}

#[test]
fn start_session_closes_one_user_item() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "one");
    delivered(&w.store, w.boss.id, "user", None, "two");
    closed_by(&w.store, "start_session", &serde_json::json!({}), w.boss.id, &serde_json::Value::Null);
    assert_eq!(w.store.open_items(w.boss.id).unwrap().len(), 1);
}

#[test]
fn update_issue_and_comment_on_issue_close_nothing() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "one");
    delivered(&w.store, w.boss.id, "user", None, "two");
    closed_by(&w.store, "update_issue", &serde_json::json!({"id": 1}), w.boss.id, &serde_json::Value::Null);
    closed_by(&w.store, "comment_on_issue", &serde_json::json!({"id": 1}), w.boss.id, &serde_json::Value::Null);
    assert_eq!(w.store.open_items(w.boss.id).unwrap().len(), 2);
}

#[test]
fn a_wire_logged_user_message_after_the_verdict_needs_a_new_one() {
    let w = world();
    let s = w.store.session(w.worker.id).unwrap();
    let head = crate::autopilot::head_of(&w.store, &s);
    w.store.add_verdict(s.id, None, head.as_deref(), "concluded_pass", "x", None, "", w.boss.id).unwrap();
    assert!(crate::autopilot::already_judged(&w.store, &s));
    std::thread::sleep(std::time::Duration::from_millis(5));
    crate::test_support::wire_user(&w.store, s.id, "one more thing");
    assert!(!crate::autopilot::already_judged(&w.store, &s));
}

#[test]
fn the_turn_end_nudges_three_times_then_gives_up_with_the_list() {
    let w = world();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "judge it");
    let now = crate::store::now();
    for _ in 0..MAX_NUDGES {
        match at_turn_end(&w.store, w.boss.id, now) {
            Hold::Nudge(text) => {
                assert!(text.contains("You still have 1 open items") && text.contains("judge it"), "{text}")
            }
            _ => panic!("expected a nudge"),
        }
    }
    match at_turn_end(&w.store, w.boss.id, now) {
        Hold::GiveUp(lines) => assert!(lines[0].contains("judge it")),
        _ => panic!("expected the give up"),
    }
    assert!(matches!(at_turn_end(&w.store, w.boss.id, now), Hold::Clear));
    let log = only_log(&w);
    assert_eq!((log.action.as_str(), log.outcome.as_str()), ("stale", "still open after 3 reminders"));
}

fn only_log(w: &World) -> agentbench_core::LogItem {
    let all = w.store.log_items(&Default::default()).unwrap().items;
    assert_eq!(all.len(), 1, "{all:?}");
    all.into_iter().next().unwrap()
}

fn closes_with(tool: &str, kind: &str, args: serde_json::Value, out: serde_json::Value) -> agentbench_core::LogItem {
    let w = world();
    delivered(&w.store, w.boss.id, kind, Some(w.worker.id), &format!("{kind} for the worker"));
    let open = only_log(&w);
    assert!(open.open() && open.needs_answer, "{open:?}");
    assert_eq!(open.asker, w.worker.id.to_string());
    assert_eq!(open.project_id, Some(w.project));
    let mut args = args;
    args["id"] = serde_json::json!(w.worker.id.to_string());
    closed_by(&w.store, tool, &args, w.boss.id, &out);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
    let log = only_log(&w);
    assert!(!log.open(), "{log:?}");
    assert_eq!(log.action, tool);
    log
}

#[test]
fn judge_session_records_the_verdict_and_the_followup() {
    let log = closes_with(
        "judge_session",
        "judge",
        serde_json::json!({"verdict": "needs_followup", "reason": "no tests ran", "followup": "run the suite"}),
        serde_json::Value::Null,
    );
    assert_eq!(log.detail, "needs_followup");
    assert_eq!(log.outcome, "no tests ran. Followup: run the suite");
}

#[test]
fn check_in_records_its_action_and_text() {
    let log = closes_with("check_in", "check_in", serde_json::json!({"action": "Message", "text": "commit now"}), serde_json::Value::Null);
    assert_eq!((log.detail.as_str(), log.outcome.as_str()), ("message", "commit now"));
}

#[test]
fn answer_session_records_the_answer_sent() {
    let log = closes_with("answer_session", "question", serde_json::json!({"answer": "use sqlite"}), serde_json::Value::Null);
    assert_eq!(log.outcome, "use sqlite");
}

#[test]
fn escalate_question_records_the_recommendation() {
    let log = closes_with("escalate_question", "question", serde_json::json!({"recommendation": "say yes"}), serde_json::Value::Null);
    assert_eq!(log.outcome, "say yes");
}

#[test]
fn an_answer_summary_replaces_the_outcome() {
    let log = closes_with(
        "judge_session",
        "judge",
        serde_json::json!({"verdict": "concluded_pass", "reason": "long reason", "answer_summary": "merged it"}),
        serde_json::Value::Null,
    );
    assert_eq!(log.outcome, "merged it");
}

#[test]
fn file_issue_records_the_new_issue_number() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "please track the login bug");
    closed_by(&w.store, "file_issue", &serde_json::json!({"title": "Login bug"}), w.boss.id, &serde_json::json!({"filed": 42}));
    let log = only_log(&w);
    assert_eq!((log.action.as_str(), log.detail.as_str(), log.outcome.as_str()), ("file_issue", "#42", "Login bug"));
    assert_eq!(log.asker, "user");
    assert_eq!(log.issue_id, Some(42));
}

#[test]
fn start_session_records_the_new_session() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "start on it");
    let out = serde_json::json!({"session": "0123456789abcdef", "title": "Fix the login"});
    closed_by(&w.store, "start_session", &serde_json::json!({}), w.boss.id, &out);
    let log = only_log(&w);
    assert_eq!((log.detail.as_str(), log.outcome.as_str()), ("01234567", "Fix the login"));
}

#[test]
fn a_reply_closes_the_user_message_with_its_first_sentence() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "how is the release going");
    std::thread::sleep(std::time::Duration::from_millis(5));
    w.store
        .add_message(w.boss.id, &Entry::Assistant { text: "It shipped at noon. The tag is v2.".into() })
        .unwrap();
    close_replied(&w.store, w.boss.id);
    let log = only_log(&w);
    assert_eq!(log.action, "replied");
    assert_eq!(log.outcome, "It shipped at noon.");
    assert_eq!(log.reply, "It shipped at noon. The tag is v2.");
}

#[test]
fn a_filed_ask_also_gets_the_reply_that_followed() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "track it");
    closed_by(&w.store, "file_issue", &serde_json::json!({"title": "t"}), w.boss.id, &serde_json::json!({"filed": 7}));
    std::thread::sleep(std::time::Duration::from_millis(5));
    w.store.add_message(w.boss.id, &Entry::Assistant { text: "Filed it as 7. More later.".into() }).unwrap();
    close_replied(&w.store, w.boss.id);
    let log = only_log(&w);
    assert_eq!((log.action.as_str(), log.outcome.as_str()), ("file_issue", "t"));
    assert_eq!(log.reply, "Filed it as 7. More later.");
}

#[test]
fn a_second_judge_request_supersedes_the_first_in_the_log() {
    let w = world();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "first");
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "second");
    let items = w.store.log_items(&Default::default()).unwrap().items;
    assert_eq!(items.len(), 2);
    assert_eq!((items[0].question.as_str(), items[0].open()), ("second", true));
    assert_eq!((items[1].question.as_str(), items[1].action.as_str()), ("first", "superseded"));
}

#[test]
fn a_removed_session_settles_its_log_item() {
    let w = world();
    let tree = crate::projects::add_worktree(&w.store, w.project, "gone2", None).unwrap();
    let session = w.store.add_session(tree.id, "worker", "gone", None, None).unwrap();
    delivered(&w.store, w.boss.id, "check_in", Some(session.id), "check");
    w.store.delete_session(session.id).unwrap();
    sweep(&w.store, w.boss.id);
    let log = only_log(&w);
    assert_eq!((log.action.as_str(), log.outcome.as_str()), ("settled", "the session is gone"));
}

#[test]
fn a_message_that_needs_no_answer_is_kept_and_marked() {
    let w = world();
    delivered(&w.store, w.boss.id, "autopilot", Some(w.worker.id), "paused for pressure");
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
    let log = only_log(&w);
    assert!(!log.needs_answer && !log.open());
    assert_eq!(log.action, "no_action_needed");
    let q = agentbench_core::LogQuery { needs_answer: Some(true), ..Default::default() };
    assert!(w.store.log_items(&q).unwrap().items.is_empty());
}

#[test]
fn a_groom_request_is_logged_under_its_project() {
    let w = world();
    let name = w.store.project(w.project).unwrap().name;
    delivered(&w.store, w.boss.id, &format!("groom {name}"), None, "groom it");
    let log = only_log(&w);
    assert_eq!((log.kind.as_str(), log.asker.as_str(), log.project_id), ("groom", "autopilot", Some(w.project)));
    closed_by(&w.store, "groom_done", &serde_json::json!({"summary": "promoted two"}), w.boss.id, &serde_json::Value::Null);
    assert_eq!(only_log(&w).outcome, "promoted two");
}

#[test]
fn the_log_keeps_only_the_configured_days() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "old one");
    w.store.set_setting(LOG_DAYS_SETTING, "1").unwrap();
    w.store.prune_log(crate::store::now() + 1).unwrap();
    delivered(&w.store, w.boss.id, "user", None, "new one");
    assert_eq!(only_log(&w).question, "new one");
}

#[test]
fn a_merge_lands_on_the_judge_item_of_that_worker() {
    let w = world();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "judge it");
    w.store.set_log_merge(w.worker.id, "merged feat into the main branch").unwrap();
    assert_eq!(only_log(&w).merge, "merged feat into the main branch");
}
