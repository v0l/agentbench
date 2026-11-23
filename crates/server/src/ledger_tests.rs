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
    closed_by(&w.store, "judge_session", &serde_json::json!({"id": w.worker.id.to_string()}), w.boss.id);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
}

#[test]
fn a_tool_for_another_session_leaves_the_item_open() {
    let w = world();
    delivered(&w.store, w.boss.id, "judge", Some(w.worker.id), "judge it");
    closed_by(&w.store, "judge_session", &serde_json::json!({"id": "other"}), w.boss.id);
    closed_by(&w.store, "check_in", &serde_json::json!({"id": w.worker.id.to_string()}), w.boss.id);
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
    closed_by(&w.store, "judge_session", &serde_json::json!({"id": w.worker.id.to_string()}), w.boss.id);
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
    closed_by(&w.store, "file_issue", &serde_json::json!({"title": "t"}), w.boss.id);
    assert!(w.store.open_items(w.boss.id).unwrap().is_empty());
}

#[test]
fn file_issue_closes_only_the_oldest_user_item() {
    let w = world();
    for t in ["one", "two", "three"] {
        delivered(&w.store, w.boss.id, "user", None, t);
    }
    closed_by(&w.store, "file_issue", &serde_json::json!({"title": "t"}), w.boss.id);
    let open = w.store.open_items(w.boss.id).unwrap();
    assert_eq!(open.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["two", "three"]);
}

#[test]
fn start_session_closes_one_user_item() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "one");
    delivered(&w.store, w.boss.id, "user", None, "two");
    closed_by(&w.store, "start_session", &serde_json::json!({}), w.boss.id);
    assert_eq!(w.store.open_items(w.boss.id).unwrap().len(), 1);
}

#[test]
fn update_issue_and_comment_on_issue_close_nothing() {
    let w = world();
    delivered(&w.store, w.boss.id, "user", None, "one");
    delivered(&w.store, w.boss.id, "user", None, "two");
    closed_by(&w.store, "update_issue", &serde_json::json!({"id": 1}), w.boss.id);
    closed_by(&w.store, "comment_on_issue", &serde_json::json!({"id": 1}), w.boss.id);
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
}
