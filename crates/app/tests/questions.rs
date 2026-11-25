use agentbench::App;
use agentbench_core::Entry;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT as _, Queryable};

mod common;
use common::*;

struct Seeded {
    rig: Rig,
    boss: agentbench_core::SessionId,
    early: agentbench_core::Id,
}

fn seeded() -> Seeded {
    let rig = rig();
    let sup = agentbench_server::projects::ensure_supervisor(&rig.store, &rig._dir.path().join("supervisor")).unwrap();
    let tree = rig.store.main_worktree(sup.id).unwrap();
    let boss = rig.store.add_session(tree.id, "echo", "Supervisor", None, None).unwrap();
    let project = rig.store.projects().unwrap().into_iter().find(|p| p.name == "demo").unwrap();
    let tree = rig.store.main_worktree(project.id).unwrap();
    let worker = rig.store.add_session(tree.id, "echo", "say hello", None, None).unwrap();
    use agentbench_server::ledger::{closed_by, delivered};
    delivered(&rig.store, boss.id, "user", None, "how is the login fix going");
    let early = rig.store.add_message(boss.id, &Entry::user("how is the login fix going")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    rig.store.add_message(boss.id, &Entry::Assistant { text: "It is in review. I judged it this morning.".into() }).unwrap();
    for n in 0..40 {
        rig.store.add_message(boss.id, &Entry::Assistant { text: format!("filler reply {n}\n\nmore words to take room\n\nand more") }).unwrap();
    }
    agentbench_server::ledger::close_replied(&rig.store, boss.id);
    delivered(&rig.store, boss.id, "question", Some(worker.id), "which database should the cache use");
    closed_by(&rig.store, "answer_session", &serde_json::json!({"id": worker.id.to_string(), "answer": "sqlite, it is already a dependency"}), boss.id, &serde_json::Value::Null);
    delivered(&rig.store, boss.id, "judge", Some(worker.id), "judge the worker on the README issue");
    Seeded { rig, boss: boss.id, early: early.id }
}

fn open_log(rig: &Rig, size: [f32; 2]) -> Harness<'static, App> {
    let mut h = harness_at(rig.url.clone(), size);
    until(&mut h, "the overview", |h| h.state().overview().is_some_and(|o| o.supervisor.is_some()));
    let sid = h.state().supervisor_chat().unwrap();
    h.state_mut().open_supervisor_chat(sid);
    h.run_steps(5);
    if h.state().drawer_open() {
        click(&mut h, size[0] - 10.0, 600.0);
        h.run_steps(5);
    }
    until(&mut h, "the open count", |h| h.query_by_label("questions log, 1 open").is_some());
    h.get_by_label("questions log").click();
    until(&mut h, "the items", |h| h.state().questions_log().is_some_and(|l| l.items.len() == 3));
    h.run_steps(5);
    h
}

fn sideways(h: &Harness<'_, App>, size: [f32; 2]) -> Vec<String> {
    h.root()
        .children_recursive()
        .filter(|n| n.accesskit_node().bounding_box().is_some())
        .filter(|n| n.rect().left() < -0.5 || n.rect().right() > size[0] + 0.5)
        .map(|n| format!("{:?} {:?} {:?}", n.accesskit_node().role(), n.accesskit_node().label(), n.rect()))
        .collect()
}

#[test]
fn the_log_shows_each_question_over_its_answer_with_the_open_one_first() {
    let s = seeded();
    let h = open_log(&s.rig, [1280.0, 800.0]);
    let mut h = h;
    shot(&mut h, "questions-log-1280");
    assert!(h.query_by_label_contains("judge the worker on the README issue").is_some());
    assert!(h.query_by_label_contains("you: how is the login fix going").is_some());
    assert!(h.query_by_label_contains("replied. It is in review.").is_some());
    assert!(h.query_by_label_contains("answered. sqlite, it is already a dependency").is_some());
    assert!(h.query_by_label_contains("waiting").is_some(), "the open item shows its age");
    let judge_y = h.get_by_label_contains("judge the worker on the README").rect().top();
    let user_y = h.get_by_label_contains("you: how is the login fix going").rect().top();
    assert!(judge_y < user_y, "open items sort first");
    let bad = sideways(&h, [1280.0, 800.0]);
    assert!(bad.is_empty(), "{bad:?}");
}

#[test]
fn the_log_filters_by_state_kind_and_search() {
    let s = seeded();
    let mut h = open_log(&s.rig, [1280.0, 800.0]);
    h.get_by_label("log state open").click();
    until(&mut h, "open only", |h| h.state().questions_log().is_some_and(|l| l.items.len() == 1));
    h.get_by_label("log state all").click();
    h.get_by_label("log kind workers").click();
    until(&mut h, "worker questions", |h| {
        h.state().questions_log().is_some_and(|l| l.items.len() == 1 && l.items[0].kind == "question")
    });
    h.get_by_label("log kind any kind").click();
    until(&mut h, "everything", |h| h.state().questions_log().is_some_and(|l| l.items.len() == 3));
    h.get_by_label("log search").click();
    h.run_steps(2);
    h.event(egui::Event::Text("login".into()));
    until(&mut h, "the search", |h| {
        h.state().questions_log().is_some_and(|l| l.items.len() == 1 && l.items[0].kind == "user")
    });
}

#[test]
fn tapping_an_item_jumps_to_its_place_in_the_transcript() {
    let s = seeded();
    let mut h = open_log(&s.rig, [1280.0, 800.0]);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.boss) > 40);
    h.run_steps(10);
    let user = h.state().questions_log().unwrap().items.iter().find(|i| i.kind == "user").unwrap().id;
    h.get_by_label_contains(&format!("log item {user}:")).click();
    until(&mut h, "the jump", |h| !h.state().transcript_seeking(s.boss) && h.state().last_seek_target(s.boss).is_some());
    h.run_steps(5);
    let target = h.state().last_seek_target(s.boss).unwrap();
    assert_eq!(target / agentbench_core::SHOWN_PER_ROW, s.early, "the user message the item came from");
    assert!(h.query_by_label_contains("I judged it this morning").is_some(), "the reply under the question is on screen");
    assert!(h.query_by_label_contains("filler reply 39").is_none(), "scrolled away from the bottom");
}

#[test]
fn the_log_fits_a_phone() {
    for size in [[390.0, 844.0], [412.0, 900.0]] {
        let s = seeded();
        let mut h = open_log(&s.rig, size);
        assert!(!h.state().drawer_open());
        shot(&mut h, &format!("questions-log-{}", size[0] as u32));
        assert!(h.query_by_label("log search").is_some());
        let bad = sideways(&h, size);
        assert!(bad.is_empty(), "questions log at {size:?}: {bad:?}");
        let user = h.state().questions_log().unwrap().items.iter().find(|i| i.kind == "user").unwrap().id;
        h.get_by_label_contains(&format!("log item {user}:")).click();
        until(&mut h, "the jump", |h| h.state().last_seek_target(s.boss).is_some());
    }
}
