//! The spoken conversation with the supervisor, end to end in the window.
//!
//! A recorded take goes in through the same reader a finished microphone take uses, Whisper reads
//! it (on the scratch server, from the models on this machine), the words go to the supervisor as
//! a spoken turn, and the reply comes back a sentence at a time through the same queue the sound
//! card would have pulled from. Nothing opens a microphone or a speaker, and the servers are the
//! test's own: a private home folder, database and token, on a port the kernel picked. Only the
//! model files are shared with the machine's own install.

mod common;

use common::*;
use agentbench_core::SessionId;
use egui_kittest::kittest::Queryable;
use std::path::PathBuf;
use std::time::Duration;

const ASK: &[u8] = include_bytes!("fixtures/voice/ask.wav");
const BARGE: &[u8] = include_bytes!("fixtures/voice/barge.wav");

fn models_dir() -> Option<PathBuf> {
    let dir = std::env::var("AGENTBENCH_MODELS").ok().map(PathBuf::from).or_else(|| {
        Some(dirs_home()?.join(".local/share/agentbench/models"))
    })?;
    (dir.join("whisper-base.en/config.json").is_file() && dir.join("phonon/model.safetensors").is_file())
        .then_some(dir)
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// A supervisor that answers with two sentences, so the reply is spoken in two pieces and there is
/// something left to drop when the user speaks over it.
fn two_sentence_agent(name: &str) -> Box<dyn agentbench_server::agent::Agent> {
    Box::new(agentbench_server::agent::JsonCommand {
        name: name.into(),
        label: Some("Two sentences".into()),
        program: "sh".into(),
        args: vec![
            "-c".into(),
            r#"read p; echo '{"text":"I started two workers just now. They are still running."}'"#
                .into(),
            "sh".into(),
        ],
        resume_args: vec![],
        model_args: vec![],
        prompt_on_stdin: true,
        session: None,
        text: Some("/text".into()),
        error: Some("/error".into()),
        input_tokens: None,
        output_tokens: None,
        plain: false,
    })
}

/// The supervisor chat, made before the window starts so the agent that answers in two sentences is
/// the one the supervisor uses.
fn two_sentence_supervisor(rig: &Rig) -> SessionId {
    let project = rig.store.supervisor_project().unwrap().expect("the supervisor project");
    let tree = rig.store.main_worktree(project.id).unwrap();
    rig.store.add_session(tree.id, "glib", "Supervisor", None, None).unwrap().id
}

/// The supervisor's reply, sent a sentence at a time, as the window queues it for the sound card.
#[test]
fn a_spoken_take_reaches_the_supervisor_and_its_reply_is_spoken_back() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let rig = rig_with_models(vec![two_sentence_agent("glib")], models);
    let expected = two_sentence_supervisor(&rig);
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_converse".into(), "1".into()),
            ("voice_mode".into(), "agentbench".into()),
            ("voice_model".into(), "whisper-base.en".into()),
        ],
        Cfg { size: [1280.0, 800.0], ..Cfg::default() },
    );
    h.state_mut().voice_test_silent();
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_supervisor();
    until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    let sid = h.state().supervisor_chat().unwrap();
    eprintln!("supervisor sid {sid} expected {expected}");
    assert_eq!(sid, expected, "the two sentence agent is the supervisor's");
    h.run_steps(4);

    let ctx = h.ctx.clone();
    h.state_mut().voice_test_take(ASK, &ctx).expect("the take was read");
    for _ in 0..60 {
        h.run_steps(2);
        if let Some(e) = h.state().last_error() {
            eprintln!("notice: {e}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("transcript now: {:?}", h.state().transcript_talk(sid));
    eprintln!("converse state: {:?}", h.state().voice_test_state());
    eprintln!("supervisor: {:?}", h.state().supervisor_chat());
    eprintln!(
        "speaking {} rows {}",
        h.state().speaking(),
        h.state().transcript_len(sid)
    );
    let stored: Vec<String> = rig
        .store
        .messages(sid, 0)
        .unwrap_or_default()
        .into_iter()
        .map(|m| format!("{:?}", m.entry))
        .collect();
    eprintln!("stored: {stored:?}");
    until(&mut h, "the words", |h| {
        h.state().transcript_talk(sid).iter().any(|l| l.contains("running"))
    });
    let said = h.state().transcript_talk(sid);
    eprintln!("transcript after the take:\n{}", said.join("\n"));
    let ask = said.iter().find(|l| l.starts_with("user: ")).expect("a user turn").clone();
    assert!(ask.to_lowercase().contains("running"), "{ask}");

    until(&mut h, "the reply", |h| {
        h.state().transcript_talk(sid).iter().any(|l| l.starts_with("assistant: "))
    });
    let mut spoken: Vec<String> = Vec::new();
    for _ in 0..200 {
        spoken.extend(std::mem::take(&mut h.state_mut().voice_test_spoken()));
        if spoken.len() >= 2 {
            break;
        }
        h.run_steps(2);
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("spoken, in order: {spoken:?}");
    let transcript = h.state().transcript_talk(sid);
    eprintln!("transcript at the end:\n{}", transcript.join("\n"));
    assert!(transcript.iter().any(|l| l.starts_with("assistant: ")), "{transcript:?}");
    assert!(spoken.len() >= 2, "the reply was spoken a sentence at a time: {spoken:?}");
}

/// Speaking over the reply stops the audio and drops what of it had not been said yet.
#[test]
fn speaking_over_the_reply_drops_the_rest_of_it() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let rig = rig_with_models(vec![two_sentence_agent("glib")], models);
    let expected = two_sentence_supervisor(&rig);
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_converse".into(), "1".into()),
            ("voice_mode".into(), "agentbench".into()),
            ("voice_model".into(), "whisper-base.en".into()),
        ],
        Cfg { size: [1280.0, 800.0], ..Cfg::default() },
    );
    h.state_mut().voice_test_silent();
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_supervisor();
    until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    let sid = h.state().supervisor_chat().unwrap();
    assert_eq!(sid, expected);
    h.run_steps(4);

    let ctx = h.ctx.clone();
    h.state_mut().voice_test_send("tell me about every worker running right now, in detail");
    until(&mut h, "the reply", |h| {
        h.state().transcript_talk(sid).iter().any(|l| l.starts_with("assistant: "))
    });
    for _ in 0..40 {
        h.run_steps(2);
        std::thread::sleep(Duration::from_millis(50));
    }
    let before = h.state().transcript_talk(sid);
    eprintln!("transcript before the barge in:\n{}", before.join("\n"));
    assert!(
        before.iter().filter(|l| l.starts_with("assistant: ")).count() == 1,
        "one reply before the barge in: {before:?}"
    );

    h.state_mut().voice_test_barge(BARGE, &ctx).expect("the take was read");
    assert!(!h.state().voice_test_reading(), "the audio stopped at once");
    assert_eq!(h.state().voice_test_pending(), 0, "the rest of that reply was dropped");
    until(&mut h, "the second question", |h| {
        h.state().transcript_talk(sid).iter().filter(|l| l.starts_with("user: ")).count() >= 2
    });
    let after = h.state().transcript_talk(sid);
    eprintln!("transcript after the barge in:\n{}", after.join("\n"));
    assert!(
        after.iter().filter(|l| l.starts_with("user: ")).count() >= 2,
        "the barge in was heard as a turn: {after:?}"
    );
}
