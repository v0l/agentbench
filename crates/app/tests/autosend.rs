mod common;

use agentbench::App;
use agentbench_core::SessionId;
use common::*;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::time::Duration;

fn chat(on: bool) -> (Rig, Harness<'static, App>, SessionId) {
    let rig = rig();
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_auto_send".into(), if on { "1" } else { "0" }.into()),
            ("voice_auto_send_secs".into(), "3".into()),
        ],
        Cfg::default(),
    );
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.state_mut().voice_test_clock(100.0);
    h.run_steps(2);
    (rig, h, session.id)
}

fn said(h: &mut Harness<'static, App>, text: &str) {
    let ctx = h.ctx.clone();
    h.state_mut().voice_test_final(&ctx, text);
    h.run_steps(3);
}

fn started(h: &mut Harness<'static, App>) {
    said(h, "fix the parser");
    assert!(h.state().voice_test_countdown(), "the take started no countdown");
}

fn sent(h: &Harness<'_, App>, sid: SessionId, text: &str) -> usize {
    h.state().transcript_talk(sid).iter().filter(|l| **l == format!("user: {text}")).count()
}

fn wait_out(h: &mut Harness<'static, App>, to: f64) {
    h.state_mut().voice_test_clock(to);
    for _ in 0..40 {
        h.run_steps(2);
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn a_final_transcript_starts_a_visible_countdown() {
    let (_rig, mut h, sid) = chat(true);
    said(&mut h, "fix the parser");
    assert!(h.state().voice_test_countdown());
    assert_eq!(h.state().draft_of(sid), "fix the parser");
    h.get_by_label_contains("Sending in 3");
}

#[test]
fn the_countdown_sends_once_after_the_delay() {
    let (_rig, mut h, sid) = chat(true);
    said(&mut h, "fix the parser");
    wait_out(&mut h, 101.0);
    assert_eq!(sent(&h, sid, "fix the parser"), 0, "not before the delay");
    wait_out(&mut h, 104.0);
    until(&mut h, "the message", |h| sent(h, sid, "fix the parser") >= 1);
    wait_out(&mut h, 130.0);
    assert_eq!(sent(&h, sid, "fix the parser"), 1, "sent exactly once");
    assert_eq!(h.state().draft_of(sid), "");
    assert!(!h.state().voice_test_countdown());
}

#[test]
fn typing_cancels_the_countdown() {
    let (_rig, mut h, sid) = chat(true);
    started(&mut h);
    h.event(egui::Event::Text(" now".into()));
    h.run_steps(3);
    wait_out(&mut h, 110.0);
    assert!(!h.state().voice_test_countdown());
    assert_eq!(sent(&h, sid, "fix the parser now"), 0);
    assert!(h.state().draft_of(sid).starts_with("fix the parser"));
}

#[test]
fn a_tap_on_the_draft_cancels_the_countdown() {
    let (_rig, mut h, sid) = chat(true);
    started(&mut h);
    click(&mut h, 800.0, 735.0);
    h.run_steps(3);
    wait_out(&mut h, 110.0);
    assert!(!h.state().voice_test_countdown());
    assert_eq!(sent(&h, sid, "fix the parser"), 0);
    assert_eq!(h.state().draft_of(sid), "fix the parser");
}

#[test]
fn a_new_take_cancels_the_countdown() {
    let (_rig, mut h, sid) = chat(true);
    started(&mut h);
    let ctx = h.ctx.clone();
    let _ = h.state_mut().voice_test_take(&[], &ctx);
    assert!(!h.state().voice_test_countdown());
    wait_out(&mut h, 110.0);
    assert_eq!(sent(&h, sid, "fix the parser"), 0);
}

#[test]
fn the_sending_countdown_and_the_closing_chip_never_show_together() {
    let (_rig, mut h, _sid) = chat(true);
    started(&mut h);
    h.get_by_label_contains("Sending in");
    let ctx = h.ctx.clone();
    let _ = h.state_mut().voice_test_take(&[], &ctx);
    h.run_steps(3);
    assert!(!h.state().voice_test_countdown());
    assert!(h.query_by_label_contains("Sending in").is_none());
}

#[test]
fn leaving_the_chat_cancels_the_countdown() {
    let (_rig, mut h, sid) = chat(true);
    started(&mut h);
    h.state_mut().open_supervisor();
    until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    wait_out(&mut h, 110.0);
    assert!(!h.state().voice_test_countdown());
    assert_eq!(sent(&h, sid, "fix the parser"), 0);
}

#[test]
fn nothing_worth_sending_is_never_sent() {
    let (_rig, mut h, sid) = chat(true);
    for text in ["", "   ", "/new", "you", "[BLANK_AUDIO]"] {
        said(&mut h, text);
        assert!(!h.state().voice_test_countdown(), "{text:?} started a countdown");
        wait_out(&mut h, 120.0);
        assert_eq!(sent(&h, sid, text.trim()), 0, "{text:?} was sent");
    }
    assert_eq!(h.state().transcript_len(sid), 3);
}

#[test]
fn with_auto_send_off_nothing_is_sent() {
    let (_rig, mut h, sid) = chat(false);
    said(&mut h, "fix the parser");
    assert!(!h.state().voice_test_countdown());
    wait_out(&mut h, 130.0);
    assert_eq!(sent(&h, sid, "fix the parser"), 0);
    assert_eq!(h.state().draft_of(sid), "fix the parser");
}

#[test]
fn desktop_defaults_to_no_auto_send() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    assert_eq!(h.state().voice_test_auto_send(), cfg!(target_os = "android"));
}
