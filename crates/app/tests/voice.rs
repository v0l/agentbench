mod common;

use common::*;
use agentbench_core::SessionId;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::PathBuf;
use std::time::Duration;

const ASK: &[u8] = include_bytes!("fixtures/voice/ask.wav");
const BARGE: &[u8] = include_bytes!("fixtures/voice/barge.wav");
const SILENCE: &[u8] = include_bytes!("fixtures/voice/silence.wav");
const CLAP: &[u8] = include_bytes!("fixtures/voice/clap.wav");
const BREATH: &[u8] = include_bytes!("fixtures/voice/breath.wav");
const COUGH: &[u8] = include_bytes!("fixtures/voice/cough.wav");
const YES: &[u8] = include_bytes!("fixtures/voice/yes.wav");
const STOP: &[u8] = include_bytes!("fixtures/voice/stop.wav");

fn models_dir() -> Option<PathBuf> {
    speech_engine()?;
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
    let log = rig.engine_log();
    assert!(log.contains("agentbench starts"), "the take and the reply went through the engine child: {log}");
    assert!(log.contains("/v1/audio/transcriptions") && log.contains("/v1/audio/speech"), "{log}");
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
        until(&mut h, "the conversation", |h| h.state().voice_test_conversing());

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

fn hands_free_chat(rig: &Rig, sink: agentbench::voice::Sink, size: [f32; 2]) -> (Harness<'static, agentbench::App>, SessionId) {
    hands_free_chat_with(rig, sink, size, "whisper-base.en")
}

fn hands_free_chat_with(
    rig: &Rig,
    sink: agentbench::voice::Sink,
    size: [f32; 2],
    model: &str,
) -> (Harness<'static, agentbench::App>, SessionId) {
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_converse".into(), "1".into()),
            ("voice_hands_free".into(), "1".into()),
            ("voice_mode".into(), "agentbench".into()),
            ("voice_model".into(), model.into()),
        ],
        Cfg { size, ..Cfg::default() },
    );
    h.state_mut().voice_test_silent();
    h.state_mut().voice_test_hands_free(sink);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_supervisor();
    until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    let sid = h.state().supervisor_chat().unwrap();
    h.run_steps(4);
    (h, sid)
}

fn clip(wav: &[u8]) -> Vec<f32> {
    let (pcm, rate) = agentbench::voice::read_wav(wav).unwrap();
    let mut pcm = agentbench::voice::to_whisper_rate(&pcm, rate);
    pcm.extend(vec![0.0005f32; agentbench::voice::RATE as usize * 4]);
    pcm
}

fn room(seconds: f32) -> Vec<f32> {
    vec![0.0005f32; (agentbench::voice::RATE as f32 * seconds) as usize]
}

#[test]
fn hands_free_starts_a_take_when_you_speak_and_the_words_reach_the_supervisor() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let rig = rig_with_models(vec![six_sentence_agent("glib")], models);
    six_sentence_supervisor(&rig);
    for (size, tag) in [([1280.0, 800.0], "1280"), ([390.0, 844.0], "390")] {
        let (mut h, sid) = hands_free_chat(&rig, agentbench::voice::Sink::Headphones, size);
        until(&mut h, "the microphone to open", |h| h.state().hands_free_on());
        until_label(&mut h, "hands free listening");
        shot(&mut h, &format!("hands-free-listening-{tag}"));
        h.state_mut().voice_test_hear(&room(2.0));
        h.run_steps(3);
        assert!(!h.state().voice_test_in_speech(), "{tag}: room is not speech");
        if tag == "390" {
            continue;
        }
        h.state_mut().voice_test_hear(&clip(ASK));
        until(&mut h, "the spoken turn", |h| {
            h.state().transcript_talk(sid).iter().any(|l| l.starts_with("user: "))
        });
        let said = h.state().transcript_talk(sid);
        let ask = said.iter().find(|l| l.starts_with("user: ")).unwrap();
        assert!(ask.to_lowercase().contains("running"), "{ask}");
        assert!(h.state().hands_free_on(), "the microphone stays open after the take");
    }
}

#[test]
fn hands_free_ignores_noise_and_still_sends_a_short_word() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let mut whispers = vec!["whisper-base.en"];
    if models.join("whisper-large-v3-turbo/model.safetensors").is_file() {
        whispers.push("whisper-large-v3-turbo");
    }
    let rig = rig_with_models(vec![six_sentence_agent("glib")], models);
    six_sentence_supervisor(&rig);
    let noises: [(&str, &[u8]); 4] = [("silence", SILENCE), ("clap", CLAP), ("breath", BREATH), ("cough", COUGH)];
    let words: [(&str, &[u8]); 2] = [("yes", YES), ("stop", STOP)];
    for model in whispers {
        let (mut h, sid) = hands_free_chat_with(&rig, agentbench::voice::Sink::Headphones, [1280.0, 800.0], model);
        until(&mut h, "the microphone to open", |h| h.state().hands_free_on());
        h.state_mut().voice_test_hear(&room(2.0));
        h.run_steps(3);
        let before = h.state().transcript_talk(sid).iter().filter(|l| l.starts_with("user: ")).count();
        let turns = |h: &Harness<'_, agentbench::App>| {
            h.state().transcript_talk(sid).into_iter().filter(|l| l.starts_with("user: ")).skip(before).collect::<Vec<_>>()
        };
        for (name, wav) in noises {
            let seen = h.state().voice_test_ignored_count();
            h.state_mut().voice_test_hear(&clip(wav));
            if name == "silence" {
                h.run_steps(20);
                assert!(!h.state().voice_test_in_speech(), "{model}: silence is not speech");
            } else {
                until(&mut h, "the take to be ignored", |h| {
                    assert_eq!(turns(h), Vec::<String>::new(), "{model}: {name} was sent as words");
                    h.state().voice_test_ignored_count() > seen
                });
                assert_eq!(h.state().talk_state(), Some(agentbench::voice::IGNORED));
            }
            h.run_steps(20);
            assert_eq!(turns(&h), Vec::<String>::new(), "{model}: {name} was sent as words");
            if name == "clap" && model == "whisper-base.en" {
                until_label(&mut h, "hands free listening");
                shot(&mut h, "hands-free-ignored-1280");
            }
            assert!(h.state().hands_free_on(), "{model}: the microphone stays open after {name}");
        }
        let seen = h.state().voice_test_ignored_count();
        for (n, (name, wav)) in words.into_iter().enumerate() {
            h.state_mut().voice_test_hear(&clip(wav));
            until(&mut h, "the spoken word", |h| {
                assert_eq!(h.state().voice_test_ignored_count(), seen, "{model}: {name} was ignored as noise");
                turns(h).len() > n
            });
            let said = turns(&h)[n].to_lowercase();
            assert!(said.contains(name), "{model}: {said}");
        }
    }
}

#[test]
fn the_ignored_noise_chip_fits_a_phone() {
    let rig = rig_with(vec![six_sentence_agent("glib")]);
    six_sentence_supervisor(&rig);
    let (mut h, _) = hands_free_chat(&rig, agentbench::voice::Sink::Headphones, [390.0, 844.0]);
    until(&mut h, "the microphone to open", |h| h.state().hands_free_on());
    h.state_mut().voice_test_ignored();
    h.run_steps(4);
    assert_eq!(h.state().talk_state(), Some(agentbench::voice::IGNORED));
    let chip = h.get_by_label("hands free listening");
    assert!(chip.rect().right() <= 390.0, "the chip runs off a phone");
    shot(&mut h, "hands-free-ignored-390");
}

#[test]
fn hands_free_speech_during_a_reply_stops_the_reader_and_clears_the_queue() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let rig = rig_with_models(vec![six_sentence_agent("glib")], models);
    six_sentence_supervisor(&rig);
    let (mut h, sid) = hands_free_chat(&rig, agentbench::voice::Sink::Headphones, [1280.0, 800.0]);
    until(&mut h, "the microphone to open", |h| h.state().hands_free_on());
    h.state_mut().voice_test_send("tell me about every worker running right now, in detail");
    until(&mut h, "the first sentence being read with more queued", |h| {
        h.state().voice_test_reading() && h.state().voice_test_waiting() >= 2
    });
    let speech = clip(BARGE);
    let tail = agentbench::voice::RATE as usize * 4;
    h.state_mut().voice_test_hear(&speech[..speech.len() - tail]);
    until(&mut h, "speech to be heard", |h| h.state().voice_test_in_speech());
    assert!(!h.state().voice_test_reading(), "the audio stopped at once");
    assert_eq!(h.state().voice_test_waiting(), 0, "the rest of that reply was dropped");
    h.state_mut().voice_test_hear(&speech[speech.len() - tail..]);
    until(&mut h, "the second question", |h| {
        h.state().transcript_talk(sid).iter().filter(|l| l.starts_with("user: ")).count() >= 2
    });
    assert!(h.state().hands_free_on());
}

#[test]
fn hands_free_closes_the_microphone_when_you_leave_the_chat_or_press_the_key() {
    let rig = rig_with(vec![six_sentence_agent("glib")]);
    six_sentence_supervisor(&rig);
    let (mut h, _) = hands_free_chat(&rig, agentbench::voice::Sink::Headphones, [1280.0, 800.0]);
    until(&mut h, "the microphone to open", |h| h.state().hands_free_on());

    let ctx = h.ctx.clone();
    let key = h.state_mut().voice_test_talk_key(ASK, &ctx, true);
    assert_eq!(key, Some(agentbench::voice::Talk::Finish));
    h.run_steps(4);
    assert!(!h.state().hands_free_on(), "the talk key pauses it");
    let key = h.state_mut().voice_test_talk_key(ASK, &ctx, true);
    assert_eq!(key, Some(agentbench::voice::Talk::Start));
    until(&mut h, "the microphone to open again", |h| h.state().hands_free_on());

    h.state_mut().blank_chat_in_home();
    h.run_steps(4);
    assert!(!h.state().hands_free_on(), "leaving the chat closes the microphone");
    h.state_mut().open_supervisor();
    until(&mut h, "the microphone to open on return", |h| h.state().hands_free_on());
}

#[test]
fn hands_free_refuses_speakers_and_asks_once_when_the_output_is_unknown() {
    let rig = rig_with(vec![six_sentence_agent("glib")]);
    six_sentence_supervisor(&rig);
    let speakers = agentbench::voice::Sink::Speakers("Built-in Audio".into());
    let (mut h, _) = hands_free_chat(&rig, speakers, [1280.0, 800.0]);
    until(&mut h, "the refusal", |h| h.state().voice_test_notice().is_some());
    let why = h.state().voice_test_notice().unwrap();
    assert!(why.contains("Built-in Audio") && why.contains("headphones"), "{why}");
    h.run_steps(4);
    assert!(!h.state().hands_free_on());
    shot(&mut h, "hands-free-refused-1280");

    let (mut h, _) = hands_free_chat(&rig, agentbench::voice::Sink::Unknown, [390.0, 844.0]);
    until(&mut h, "the question", |h| h.state().asking_headphones());
    h.run_steps(4);
    shot(&mut h, "hands-free-ask-390");
    until_label(&mut h, "speakers");
    assert!(!h.state().hands_free_on());
    h.get_by_label("headphones").click();
    until(&mut h, "the microphone to open", |h| h.state().hands_free_on());
    assert!(!h.state().asking_headphones());
}

#[test]
fn hands_free_on_speakers_opens_with_echo_cancelling_on() {
    let rig = rig_with(vec![six_sentence_agent("glib")]);
    six_sentence_supervisor(&rig);
    let speakers = agentbench::voice::Sink::Speakers("Built-in Audio".into());
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_converse".into(), "1".into()),
            ("voice_hands_free".into(), "1".into()),
            ("voice_echo_cancel".into(), "1".into()),
            ("voice_mode".into(), "agentbench".into()),
        ],
        Cfg::default(),
    );
    h.state_mut().voice_test_silent();
    h.state_mut().voice_test_hands_free(speakers);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_supervisor();
    until(&mut h, "the microphone to open", |h| h.state().hands_free_on());
    assert!(h.state().voice_test_notice().is_none());
}

#[test]
fn the_hands_free_settings_fit_at_both_sizes() {
    let rig = rig();
    for (tag, size) in [("1280", [1280.0, 800.0]), ("390", [390.0, 844.0])] {
        let mut h = harness_cfg(
            vec![
                ("url".into(), rig.url.clone()),
                ("voice_converse".into(), "1".into()),
                ("voice_hands_free".into(), "1".into()),
            ],
            Cfg { size, ..Cfg::default() },
        );
        until(&mut h, "the overview", |h| h.state().overview().is_some());
        h.state_mut().open_settings_page("speech");
        h.run_steps(2);
        until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
        h.run_steps(4);
        for label in ["hands free", "echo cancelling"] {
            let n = h.get_by_label(label);
            assert!(n.rect().right() <= size[0], "{label} runs off a {tag} screen");
        }
        h.get_by_label("hands free").scroll_to_me();
        h.run_steps(4);
        shot(&mut h, &format!("hands-free-settings-{tag}"));
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
    until(&mut h, "the conversation", |h| h.state().voice_test_conversing());

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
