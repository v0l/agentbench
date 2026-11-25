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

fn six_sentence_agent(name: &str) -> Box<dyn agentbench_server::agent::Agent> {
    Box::new(agentbench_server::agent::JsonCommand {
        name: name.into(),
        label: Some("Two sentences".into()),
        program: "sh".into(),
        args: vec![
            "-c".into(),
            r#"read p; echo '{"text":"I started two workers just now. They are still running. The first is on the login fix. The second is on the readme. Neither has asked for help. I will tell you when they finish."}'"#
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

fn six_sentence_supervisor(rig: &Rig) -> SessionId {
    let project = rig.store.supervisor_project().unwrap().expect("the supervisor project");
    let tree = rig.store.main_worktree(project.id).unwrap();
    rig.store.add_session(tree.id, "glib", "Supervisor", None, None).unwrap().id
}

#[test]
fn a_spoken_take_reaches_the_supervisor_and_its_reply_is_spoken_back() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let rig = rig_with_models(vec![six_sentence_agent("glib")], models);
    let expected = six_sentence_supervisor(&rig);
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
    assert_eq!(sid, expected, "the two sentence agent is the supervisor's");
    h.run_steps(4);

    let ctx = h.ctx.clone();
    h.state_mut().voice_test_take(ASK, &ctx).expect("the take was read");
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
    assert!(spoken.len() >= 2, "the reply was spoken a sentence at a time: {spoken:?}");
    assert!(
        spoken[0].contains("two workers") && !spoken[0].contains("still running"),
        "the first sentence is spoken before the reply ends: {spoken:?}"
    );
    assert!(transcript.iter().any(|l| l.starts_with("assistant: ")), "{transcript:?}");
}

#[test]
fn the_talk_key_holds_to_talk_and_toggles_with_the_microphone() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let rig = rig_with_models(vec![six_sentence_agent("glib")], models);
    for (hold, tag) in [(true, "hold"), (false, "toggle")] {
        let mut h = harness_cfg(
            vec![
                ("url".into(), rig.url.clone()),
                ("voice_converse".into(), "1".into()),
                ("voice_mode".into(), "agentbench".into()),
                ("voice_model".into(), "whisper-base.en".into()),
                ("voice_hold".into(), if hold { "1" } else { "0" }.into()),
            ],
            Cfg { size: [1280.0, 800.0], ..Cfg::default() },
        );
        h.state_mut().voice_test_silent();
        until(&mut h, "the overview", |h| h.state().overview().is_some());
        h.state_mut().open_supervisor();
        until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
        let sid = h.state().supervisor_chat().unwrap();
        h.run_steps(4);

        use agentbench::voice::Talk;
        let ctx = h.ctx.clone();
        let press = h.state_mut().voice_test_talk_key(ASK, &ctx, true);
        assert_eq!(press, Some(Talk::Start), "{tag}: a press opens the take");
        let release = h.state_mut().voice_test_talk_key(ASK, &ctx, false);
        if hold {
            assert_eq!(release, Some(Talk::Finish), "{tag}: the release ends the take");
        } else {
            assert_eq!(release, Some(Talk::Nothing), "{tag}: a tap does not end the take");
            let press = h.state_mut().voice_test_talk_key(ASK, &ctx, true);
            assert_eq!(press, Some(Talk::Finish), "{tag}: the next press ends it");
        }
        until(&mut h, "the words", |h| {
            h.state().transcript_talk(sid).iter().any(|l| l.starts_with("user: "))
        });
        let said = h.state().transcript_talk(sid);
        eprintln!("{tag} transcript:\n{}", said.join("\n"));
        let ask = said.iter().find(|l| l.starts_with("user: ")).unwrap();
        assert!(ask.to_lowercase().contains("running"), "{tag}: {ask}");
    }
}

#[test]
fn speaking_over_the_reply_drops_the_rest_of_it() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let rig = rig_with_models(vec![six_sentence_agent("glib")], models);
    let expected = six_sentence_supervisor(&rig);
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
    until(&mut h, "the first sentence being read with more queued", |h| {
        h.state().voice_test_reading() && h.state().voice_test_waiting() >= 2
    });
    let queued_before = h.state().voice_test_waiting();
    assert!(queued_before >= 2, "later sentences were waiting: {queued_before}");

    h.state_mut().voice_test_barge(BARGE, &ctx).expect("the take was read");
    assert!(!h.state().voice_test_reading(), "the audio stopped at once");
    assert_eq!(h.state().voice_test_waiting(), 0, "the rest of that reply was dropped");
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
