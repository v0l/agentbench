mod common;

use agentbench_core::SessionId;
use common::*;
use egui_kittest::Harness;
use serde_json::{Value, json};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::Message;

const ASK: &[u8] = include_bytes!("fixtures/voice/ask.wav");
const BARGE: &[u8] = include_bytes!("fixtures/voice/barge.wav");
const FRAME: usize = 3200;

#[derive(Default)]
struct Seen {
    start: Option<Value>,
    frames: Vec<(Instant, usize)>,
    controls: Vec<Value>,
}

struct Fake {
    url: String,
    seen: Arc<Mutex<Seen>>,
    send: Sender<Message>,
}

impl Fake {
    fn start() -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (send, out): (Sender<Message>, Receiver<Message>) = channel();
        let s = seen.clone();
        std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept(tcp).unwrap();
            ws.get_ref().set_read_timeout(Some(Duration::from_millis(3))).unwrap();
            loop {
                for m in out.try_iter() {
                    if ws.send(m).is_err() {
                        return;
                    }
                }
                match ws.read() {
                    Ok(Message::Text(t)) => {
                        let v: Value = serde_json::from_str(&t).unwrap();
                        let mut seen = s.lock().unwrap();
                        match v["type"].as_str() {
                            Some("start") => {
                                seen.start = Some(v);
                                drop(seen);
                                let ready = json!({"type": "ready", "session": "s1", "sample_rate": 24000, "frame_samples": FRAME, "frame_bytes": FRAME * 2});
                                ws.send(Message::Text(ready.to_string().into())).unwrap();
                            }
                            Some("say") => {
                                seen.controls.push(v);
                                drop(seen);
                                ws.send(Message::Text(json!({"type": "queued", "tokens": 5}).to_string().into())).unwrap();
                            }
                            Some("interrupt") => {
                                seen.controls.push(v);
                                drop(seen);
                                let said = json!({"type": "said", "text": " I started two workers", "unsaid_tokens": 40});
                                ws.send(Message::Text(said.to_string().into())).unwrap();
                            }
                            _ => seen.controls.push(v),
                        }
                    }
                    Ok(Message::Binary(b)) => s.lock().unwrap().frames.push((Instant::now(), b.len())),
                    Ok(Message::Close(_)) => return,
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(_) => return,
                }
            }
        });
        Fake { url, seen, send }
    }

    fn reply(&self, level: f32) {
        let pcm = agent_voice::voice::audio::to_pcm16(&vec![level; FRAME]);
        self.send.send(Message::Binary(pcm.into())).unwrap();
    }

    fn text(&self, words: &str) {
        self.send.send(Message::Text(json!({"type": "text", "frame": 1, "text": words}).to_string().into())).unwrap();
    }

    fn says(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .controls
            .iter()
            .filter(|v| v["type"] == "say")
            .map(|v| v["text"].as_str().unwrap().to_string())
            .collect()
    }

    fn interrupts(&self) -> usize {
        self.seen.lock().unwrap().controls.iter().filter(|v| v["type"] == "interrupt").count()
    }
}

fn models_dir() -> Option<PathBuf> {
    let dir = std::env::var("AGENTBENCH_MODELS")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/share/agentbench/models")))?;
    dir.join("whisper-base.en/config.json").is_file().then_some(dir)
}

fn agent(name: &str, script: &str) -> Box<dyn agentbench_server::agent::Agent> {
    Box::new(agentbench_server::agent::JsonCommand {
        name: name.into(),
        label: Some("Scripted".into()),
        program: "sh".into(),
        args: vec!["-c".into(), script.into(), "sh".into()],
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

const THREE_SENTENCES: &str = r#"read p; sleep 2; echo '{"text":"I started two workers just now. They are still running. I will tell you when they finish."}'"#;

fn supervisor(rig: &Rig, agent: &str) {
    let project = rig.store.supervisor_project().unwrap().expect("the supervisor project");
    let tree = rig.store.main_worktree(project.id).unwrap();
    rig.store.add_session(tree.id, agent, "Supervisor", None, None).unwrap();
}

fn duplex_chat(rig: &Rig, fake: &Fake, size: [f32; 2]) -> (Harness<'static, agentbench::App>, SessionId) {
    duplex_chat_with(rig, fake, size, &[])
}

fn relay_chat(rig: &Rig, fake: &Fake, size: [f32; 2], says: bool) -> (Harness<'static, agentbench::App>, SessionId) {
    duplex_chat_with(rig, fake, size, &[("voice_duplex_free", "0"), ("voice_duplex_says", if says { "1" } else { "0" })])
}

fn duplex_chat_with(
    rig: &Rig,
    fake: &Fake,
    size: [f32; 2],
    extra: &[(&str, &str)],
) -> (Harness<'static, agentbench::App>, SessionId) {
    let mut cfg = vec![
        ("url".into(), rig.url.clone()),
        ("voice_converse".into(), "1".into()),
        ("voice_hands_free".into(), "1".into()),
        ("voice_mode".into(), "agentbench".into()),
        ("voice_model".into(), "whisper-base.en".into()),
    ];
    cfg.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let mut h = harness_cfg(cfg, Cfg { size, ..Cfg::default() });
    h.state_mut().voice_test_silent();
    h.state_mut().voice_test_hands_free(agentbench::voice::Sink::Headphones);
    h.state_mut().voice_test_duplex(&fake.url);
    h.state_mut().voice_test_speech().duplex_voice = voice_clip_file();
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_supervisor();
    until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    let sid = h.state().supervisor_chat().unwrap();
    until(&mut h, "the duplex session", |h| h.state().voice_test_duplex_on());
    (h, sid)
}

fn room(seconds: f32) -> Vec<f32> {
    vec![0.0005f32; (agentbench::voice::RATE as f32 * seconds) as usize]
}

fn clip(wav: &[u8]) -> Vec<f32> {
    let (pcm, rate) = agentbench::voice::read_wav(wav).unwrap();
    let mut pcm = agentbench::voice::to_whisper_rate(&pcm, rate);
    pcm.extend(room(4.0));
    pcm
}

fn hear_live(h: &mut Harness<'static, agentbench::App>, pcm: &[f32]) {
    let block = agentbench::voice::RATE as usize / 50;
    let start = Instant::now();
    for (i, chunk) in pcm.chunks(block).enumerate() {
        let due = start + Duration::from_millis(20 * i as u64);
        while Instant::now() < due {
            std::thread::sleep(Duration::from_millis(1));
        }
        h.state_mut().voice_test_hear(chunk);
        h.step();
    }
}

#[test]
fn hands_free_with_a_duplex_model_streams_frames_of_6400_bytes_every_133_ms() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let (mut h, _) = duplex_chat(&rig, &fake, [1280.0, 800.0]);
    let start = fake.seen.lock().unwrap().start.clone().expect("a start message");
    assert_eq!(start["mode"], "free", "Haru replies in its own words by default");
    assert_eq!(start["model"], "Haru-4B");
    assert!(start["card"]["situation"].as_str().unwrap().contains("own words"), "{start}");
    assert!(!start["card"].to_string().contains("relay"), "{start}");

    hear_live(&mut h, &room(3.0));
    h.run_steps(4);
    std::thread::sleep(Duration::from_millis(100));
    let frames = fake.seen.lock().unwrap().frames.clone();
    eprintln!("{} frames in 3 s", frames.len());
    assert!((21..=23).contains(&frames.len()), "three seconds is 22.5 frames: {}", frames.len());
    assert!(frames.iter().all(|(_, n)| *n == 6400), "every frame is 6400 bytes");
    let mut gaps: Vec<f64> =
        frames.windows(2).map(|w| (w[1].0 - w[0].0).as_secs_f64() * 1000.0).collect();
    gaps.sort_by(f64::total_cmp);
    let median = gaps[gaps.len() / 2];
    eprintln!("median gap {median:.1} ms, min {:.1}, max {:.1}", gaps[0], gaps[gaps.len() - 1]);
    assert!((113.0..=153.0).contains(&median), "frames leave about every 133 ms: {median}");
    assert_eq!(h.state().voice_test_duplex_sent(), frames.len());
}

#[test]
fn reply_frames_play_in_order_and_a_late_frame_plays_silence() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let (mut h, _) = duplex_chat(&rig, &fake, [1280.0, 800.0]);
    hear_live(&mut h, &clip(ASK)[..agentbench::voice::RATE as usize]);
    assert!(h.state().voice_test_in_speech());
    let level = |f: &[f32]| (f.iter().sum::<f32>() / f.len() as f32 * 10.0).round() / 10.0;
    for l in [0.1, 0.2, 0.3] {
        fake.reply(l);
    }
    let t = Instant::now();
    let first = loop {
        h.step();
        let f = h.state().voice_test_duplex_pull().expect("a manual player");
        assert_eq!(f.len(), FRAME);
        if level(&f) != 0.0 {
            break f;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "no reply frames arrived");
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut played = vec![level(&first)];
    for _ in 0..3 {
        played.push(level(&h.state().voice_test_duplex_pull().unwrap()));
    }
    assert_eq!(played, vec![0.1, 0.2, 0.3, 0.0], "in order, then silence for the late fourth");
    fake.reply(0.4);
    fake.reply(0.5);
    std::thread::sleep(Duration::from_millis(200));
    h.step();
    let next = level(&h.state().voice_test_duplex_pull().unwrap());
    assert_eq!(next, 0.5, "the late frame is dropped so the delay does not grow");
}

#[test]
fn in_relay_mode_replies_are_read_by_text_to_speech_while_the_model_listens() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let (mut h, sid) = relay_chat(&rig, &fake, [1280.0, 800.0], false);
    let start = fake.seen.lock().unwrap().start.clone().unwrap();
    assert_eq!(start["mode"], "relay");
    assert!(start["card"]["relationship"].as_str().unwrap().contains("relay"));
    h.state_mut().voice_test_send("what are the workers doing");
    until(&mut h, "the reply", |h| h.state().transcript_talk(sid).iter().any(|l| l.starts_with("assistant: ")));
    let mut spoken = Vec::new();
    let t = Instant::now();
    while spoken.is_empty() && t.elapsed() < Duration::from_secs(20) {
        spoken.extend(h.state_mut().voice_test_spoken());
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(spoken.first().is_some_and(|s| s.contains("two workers")), "{spoken:?}");
    assert!(fake.says().is_empty(), "nothing is sent to the model to say");
    assert!(h.state().voice_test_duplex_on(), "the model still listens");
}

#[test]
fn in_free_mode_the_reply_is_shown_but_neither_said_nor_read_aloud() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let (mut h, sid) = duplex_chat_with(&rig, &fake, [1280.0, 800.0], &[("voice_duplex_says", "1")]);
    h.state_mut().voice_test_send("what are the workers doing");
    until(&mut h, "the reply", |h| h.state().transcript_talk(sid).iter().any(|l| l.starts_with("assistant: ")));
    let mut spoken = Vec::new();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(4) {
        spoken.extend(h.state_mut().voice_test_spoken());
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(spoken, Vec::<String>::new(), "no text to speech over Haru");
    assert_eq!(fake.says(), Vec::<String>::new(), "no say, even with replies in its voice on");
    assert_eq!(h.state().voice_test_duplex_says(), Vec::<String>::new());
    assert!(h.state().transcript_talk(sid).iter().any(|l| l.contains("I started two workers just now.")));
    assert!(h.state().voice_test_duplex_on());
}

#[test]
fn in_free_mode_what_haru_says_is_shown_in_the_chat_as_the_voice_model() {
    use egui_kittest::kittest::Queryable;
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    for (tag, size) in [("1280", [1280.0, 800.0]), ("390", [390.0, 844.0])] {
        let fake = Fake::start();
        let (mut h, sid) = duplex_chat(&rig, &fake, size);
        h.state_mut().voice_test_send("what are the workers doing");
        until(&mut h, "the reply", |h| h.state().transcript_talk(sid).iter().any(|l| l.starts_with("assistant: ")));
        for w in [" Sure,", " I can", " help with", " that."] {
            fake.text(w);
        }
        let label = "voice model said: Sure, I can help with that.";
        until_label(&mut h, label);
        let r = h.get_by_label(label).rect();
        assert!(r.right() <= size[0] + 0.5 && r.left() >= 0.0, "Haru's words run off a {tag} screen: {r:?}");
        shot(&mut h, &format!("duplex-haru-words-{tag}"));
    }
}

#[test]
fn in_relay_mode_nothing_from_the_model_plays_unless_a_say_is_queued() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let (mut h, _) = relay_chat(&rig, &fake, [1280.0, 800.0], true);
    let ask = clip(ASK);
    let second = agentbench::voice::RATE as usize;
    hear_live(&mut h, &ask[..second]);
    assert!(h.state().voice_test_in_speech());
    let before = h.state().voice_test_duplex_muted();
    for _ in 0..10 {
        fake.reply(0.3);
    }
    let t = Instant::now();
    while h.state().voice_test_duplex_muted() < before + 10 {
        assert!(t.elapsed() < Duration::from_secs(10), "babble while the user spoke was played");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..10 {
        assert!(h.state().voice_test_duplex_pull().unwrap().iter().all(|v| *v == 0.0), "relay babble was played");
    }
    hear_live(&mut h, &ask[second..]);
    assert!(!h.state().voice_test_in_speech());
    let sid = h.state().supervisor_chat().unwrap();
    h.state_mut().voice_test_send("what are the workers doing");
    until(&mut h, "the says", |_| !fake.says().is_empty());
    fake.reply(0.3);
    fake.reply(0.3);
    let t = Instant::now();
    loop {
        h.step();
        if h.state().voice_test_duplex_pull().unwrap().iter().any(|v| *v != 0.0) {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "a queued say plays");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(h.state().transcript_talk(sid).iter().any(|l| l.starts_with("user: ")));
}

#[test]
fn nothing_the_model_says_is_played_before_the_user_speaks() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let (mut h, _) = duplex_chat(&rig, &fake, [1280.0, 800.0]);
    let start = fake.seen.lock().unwrap().start.clone().unwrap();
    assert!(start.get("speak_first").is_none(), "{start}");
    for _ in 0..10 {
        fake.reply(0.3);
    }
    let t = Instant::now();
    while h.state().voice_test_duplex_muted() < 10 {
        assert!(t.elapsed() < Duration::from_secs(10), "the frames never arrived");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    for _ in 0..10 {
        let f = h.state().voice_test_duplex_pull().unwrap();
        assert!(f.iter().all(|v| *v == 0.0), "a frame was played with no user speech");
    }
    assert_eq!(h.state().talk_state(), Some("listening"), "babble is not the model speaking");

    hear_live(&mut h, &clip(ASK)[..agentbench::voice::RATE as usize]);
    fake.reply(0.3);
    fake.reply(0.3);
    let t = Instant::now();
    loop {
        h.step();
        if h.state().voice_test_duplex_pull().unwrap().iter().any(|v| *v != 0.0) {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "once the user spoke the reply plays");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn speak_first_plays_the_model_before_the_user_speaks() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_converse".into(), "1".into()),
            ("voice_hands_free".into(), "1".into()),
            ("voice_mode".into(), "agentbench".into()),
            ("voice_duplex_speak_first".into(), "1".into()),
        ],
        Cfg::default(),
    );
    h.state_mut().voice_test_silent();
    h.state_mut().voice_test_hands_free(agentbench::voice::Sink::Headphones);
    h.state_mut().voice_test_duplex(&fake.url);
    h.state_mut().voice_test_speech().duplex_voice = voice_clip_file();
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_supervisor();
    until(&mut h, "the duplex session", |h| h.state().voice_test_duplex_on());
    assert_eq!(fake.seen.lock().unwrap().start.clone().unwrap()["speak_first"], true);
    fake.reply(0.3);
    fake.reply(0.3);
    let t = Instant::now();
    while !h.state().voice_test_duplex_pull().unwrap().iter().any(|v| *v != 0.0) {
        assert!(t.elapsed() < Duration::from_secs(10), "speak first plays at once");
        h.step();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(h.state().voice_test_duplex_muted(), 0);
}

#[test]
fn without_a_voice_clip_the_duplex_model_is_not_used_and_the_chip_says_so() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    for (tag, size) in [("1280", [1280.0, 800.0]), ("390", [390.0, 844.0])] {
        let mut h = harness_cfg(
            vec![
                ("url".into(), rig.url.clone()),
                ("voice_converse".into(), "1".into()),
                ("voice_hands_free".into(), "1".into()),
                ("voice_mode".into(), "agentbench".into()),
            ],
            Cfg { size, ..Cfg::default() },
        );
        h.state_mut().voice_test_silent();
        h.state_mut().voice_test_hands_free(agentbench::voice::Sink::Headphones);
        h.state_mut().voice_test_duplex(&fake.url);
        until(&mut h, "the overview", |h| h.state().overview().is_some());
        h.state_mut().open_supervisor();
        until(&mut h, "the clip line", |h| h.state().talk_state() == Some("record your voice clip first"));
        until_label(&mut h, "hands free listening");
        chip_fits(&h, size, "no clip");
        shot(&mut h, &format!("duplex-no-clip-{tag}"));
        h.run_steps(20);
        assert!(fake.seen.lock().unwrap().start.is_none(), "no session is opened");
        assert_eq!(h.state().voice_test_duplex_url(), None);
    }
}

#[test]
fn a_voice_setting_edited_on_disk_survives_the_settings_window_and_the_next_save() {
    let rig = rig();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.ron");
    let write = |pairs: &[(&str, &str)]| {
        let all: std::collections::HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        std::fs::write(&path, ron::ser::to_string_pretty(&all, Default::default()).unwrap()).unwrap();
    };
    write(&[("voice_duplex_choice", "haru"), ("voice_hang_secs", "3")]);
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_duplex_choice".into(), "haru".into()),
            ("voice_hang_secs".into(), "3".into()),
        ],
        Cfg::default(),
    );
    h.state_mut().watch_speech_file(path.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_settings_page("speech");
    until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
    h.state_mut().voice_test_settings_speech().unwrap().hang_secs = 7.0;
    std::thread::sleep(Duration::from_millis(20));
    write(&[("voice_duplex_choice", "haru"), ("voice_hang_secs", "3"), ("voice_duplex_voice", "/disk/me.wav"), ("voice_headphones", "1")]);
    until(&mut h, "the disk edit", |h| h.state().voice_test_settings().duplex_voice == "/disk/me.wav");
    assert_eq!(h.state_mut().voice_test_settings_speech().unwrap().duplex_voice, "/disk/me.wav", "the open window follows it");
    h.state_mut().settings_save();
    h.run_steps(4);
    let s = h.state().voice_test_settings();
    assert_eq!(s.duplex_voice, "/disk/me.wav", "saving the window does not undo it");
    assert_eq!(s.headphones, Some(true));
    assert_eq!(s.hang_secs, 7.0, "what was changed in the window is kept");
    let mut saved = Mem::default();
    h.state().store(&mut saved);
    assert_eq!(saved.0["voice_duplex_voice"], "/disk/me.wav");
    assert_eq!(saved.0["voice_hang_secs"], "7");
}

fn chip_fits(h: &Harness<'static, agentbench::App>, size: [f32; 2], what: &str) {
    use egui_kittest::kittest::Queryable;
    let r = h.get_by_label("hands free listening").rect();
    assert!(r.right() <= size[0] && r.left() >= 0.0, "{what}: the talk chip runs off a {} wide screen: {r:?}", size[0]);
}

#[test]
fn a_supervisor_reply_goes_to_the_engine_as_say_messages_and_the_indicator_follows() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    for (tag, size) in [("1280", [1280.0, 800.0]), ("390", [390.0, 844.0])] {
        let fake = Fake::start();
        let (mut h, sid) = relay_chat(&rig, &fake, size, true);
        until(&mut h, "listening", |h| h.state().talk_state() == Some("listening"));
        until_label(&mut h, "hands free listening");
        shot(&mut h, &format!("duplex-listening-{tag}"));
        chip_fits(&h, size, "listening");
        h.state_mut().voice_test_send("what are the workers doing");
        until(&mut h, "the supervisor to think", |h| h.state().talk_state() == Some("supervisor thinking"));
        shot(&mut h, &format!("duplex-thinking-{tag}"));
        chip_fits(&h, size, "thinking");
        until(&mut h, "the reply", |h| {
            h.state().transcript_talk(sid).iter().any(|l| l.starts_with("assistant: "))
        });
        until(&mut h, "three says", |_| fake.says().len() >= 3);
        assert_eq!(
            fake.says(),
            ["I started two workers just now.", "They are still running.", "I will tell you when they finish."]
        );
        assert_eq!(h.state().voice_test_duplex_says(), fake.says());
        assert_eq!(h.state_mut().voice_test_spoken(), Vec::<String>::new(), "text to speech stays quiet");
        until(&mut h, "the model speaking", |h| h.state().talk_state() == Some("model speaking"));
        shot(&mut h, &format!("duplex-speaking-{tag}"));
        chip_fits(&h, size, "speaking");
    }
}

#[test]
fn an_utterance_becomes_a_supervisor_turn_and_a_barge_in_sends_interrupt() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let rig = rig_with_models(vec![agent("glib", THREE_SENTENCES)], models);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let (mut h, sid) = relay_chat(&rig, &fake, [1280.0, 800.0], true);

    let before = h.state().voice_test_duplex_sent();
    let ask = clip(ASK);
    hear_live(&mut h, &ask);
    until(&mut h, "the spoken turn", |h| {
        h.state().transcript_talk(sid).iter().any(|l| l.starts_with("user: "))
    });
    let said = h.state().transcript_talk(sid);
    let turn = said.iter().find(|l| l.starts_with("user: ")).unwrap();
    eprintln!("turn: {turn}");
    assert!(turn.to_lowercase().contains("running"), "{turn}");
    let sent = h.state().voice_test_duplex_sent() - before;
    assert!(sent >= ask.len() * 3 / 2 / FRAME - 1, "the same microphone went to the engine: {sent}");
    assert_eq!(fake.interrupts(), 0, "nothing was queued, so speech is no barge in");

    until(&mut h, "the says", |_| fake.says().len() >= 3);
    let barge = clip(BARGE);
    hear_live(&mut h, &barge[..agentbench::voice::RATE as usize]);
    until(&mut h, "the interrupt", |_| fake.interrupts() == 1);
    hear_live(&mut h, &barge[agentbench::voice::RATE as usize..]);
    until(&mut h, "the second turn", |h| {
        h.state().transcript_talk(sid).iter().filter(|l| l.starts_with("user: ")).count() >= 2
    });
    let turns = h.state().transcript_talk(sid);
    let second = turns.iter().filter(|l| l.starts_with("user: ")).nth(1).unwrap();
    eprintln!("second turn: {second}");
    assert!(second.contains("I heard only: \"I started two workers\""), "{second}");
    assert_eq!(fake.interrupts(), 1, "one interrupt per barge in");
}

#[test]
fn in_free_mode_an_utterance_still_becomes_a_supervisor_turn() {
    let Some(models) = models_dir() else {
        eprintln!("skipped: no speech models on this machine");
        return;
    };
    let rig = rig_with_models(vec![agent("glib", THREE_SENTENCES)], models);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let (mut h, sid) = duplex_chat(&rig, &fake, [1280.0, 800.0]);
    assert_eq!(fake.seen.lock().unwrap().start.clone().unwrap()["mode"], "free");
    hear_live(&mut h, &clip(ASK));
    until(&mut h, "the spoken turn", |h| h.state().transcript_talk(sid).iter().any(|l| l.starts_with("user: ")));
    let turn = h.state().transcript_talk(sid).into_iter().find(|l| l.starts_with("user: ")).unwrap();
    assert!(turn.to_lowercase().contains("running"), "{turn}");
    until(&mut h, "the reply", |h| h.state().transcript_talk(sid).iter().any(|l| l.starts_with("assistant: ")));
    h.run_steps(20);
    assert!(fake.says().is_empty(), "{:?}", fake.says());
}

#[test]
fn the_recorded_voice_clip_is_kept_and_sent_with_the_start() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let fake = Fake::start();
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_converse".into(), "1".into()),
            ("voice_hands_free".into(), "1".into()),
            ("voice_mode".into(), "agentbench".into()),
        ],
        Cfg::default(),
    );
    h.state_mut().voice_test_silent();
    h.state_mut().voice_test_hands_free(agentbench::voice::Sink::Headphones);
    h.state_mut().voice_test_clip_dir(dir.path().to_path_buf());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let ctx = h.ctx.clone();
    let tone: Vec<f32> = (0..agentbench::voice::RATE as usize * 3).map(|i| 0.3 * (i as f32 * 0.07).sin()).collect();
    let path = h.state_mut().voice_test_record_clip(&ctx, &tone).expect("the clip");
    assert!(path.starts_with(dir.path().to_str().unwrap()), "{path}");
    let (kept, rate) = agentbench::voice::read_wav(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!((kept.len(), rate), (tone.len(), agentbench::voice::RATE));
    h.state_mut().voice_test_duplex(&fake.url);
    h.state_mut().open_supervisor();
    until(&mut h, "the duplex session", |h| h.state().voice_test_duplex_on());
    let start = fake.seen.lock().unwrap().start.clone().unwrap();
    let voice = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, start["voice"].as_str().unwrap()).unwrap();
    assert_eq!(voice.len(), 3 * 24_000 * 2, "three seconds at 24 kHz, 16 bit");
}

#[test]
fn the_duplex_settings_fit_at_both_sizes() {
    use egui_kittest::kittest::Queryable;
    let rig = rig();
    for (tag, size) in [("1280", [1280.0, 800.0]), ("390", [390.0, 844.0])] {
        let mut h = harness_cfg(
            vec![
                ("url".into(), rig.url.clone()),
                ("voice_converse".into(), "1".into()),
                ("voice_hands_free".into(), "1".into()),
                ("voice_duplex_url".into(), "http://gpu:8080".into()),
            ],
            Cfg { size, ..Cfg::default() },
        );
        until(&mut h, "the overview", |h| h.state().overview().is_some());
        h.state_mut().open_settings_page("speech");
        h.run_steps(2);
        until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
        h.run_steps(4);
        h.get_by_label("Haru replies in its own words").scroll_to_me();
        h.run_steps(4);
        for label in ["Haru replies in its own words", "model speaks first", "record"] {
            let r = h.get_by_label(label).rect();
            assert!(r.right() <= size[0], "{label} runs off a {tag} screen: {r:?}");
        }
        shot(&mut h, &format!("duplex-settings-{tag}"));
    }
}

#[test]
#[ignore = "needs a running inference-bench duplex server in AGENTBENCH_DUPLEX_URL"]
fn a_real_duplex_server_takes_the_start_frames_say_and_interrupt() {
    let url = std::env::var("AGENTBENCH_DUPLEX_URL").expect("AGENTBENCH_DUPLEX_URL");
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_converse".into(), "1".into()),
            ("voice_hands_free".into(), "1".into()),
            ("voice_mode".into(), "agentbench".into()),
        ],
        Cfg::default(),
    );
    h.state_mut().voice_test_silent();
    h.state_mut().voice_test_hands_free(agentbench::voice::Sink::Headphones);
    h.state_mut().voice_test_duplex(&url);
    h.state_mut().voice_test_speech().duplex_voice = voice_clip_file();
    h.state_mut().voice_test_duplex_model(&std::env::var("AGENTBENCH_DUPLEX_MODEL").unwrap_or_default());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_supervisor();
    let t = Instant::now();
    while !h.state().voice_test_duplex_on() {
        assert!(t.elapsed() < Duration::from_secs(120), "no ready: {:?}", h.state().voice_test_notice());
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    hear_live(&mut h, &room(2.0));
    let mut heard = 0;
    for _ in 0..30 {
        let f = h.state().voice_test_duplex_pull().unwrap();
        assert_eq!(f.len(), FRAME);
        heard += f.iter().any(|v| *v != 0.0) as usize;
        h.step();
        std::thread::sleep(Duration::from_millis(133));
    }
    eprintln!("{} frames sent, {heard} of 30 pulled frames had sound", h.state().voice_test_duplex_sent());
    assert!(heard > 0, "the server sent audio back");
    let sid = h.state().supervisor_chat().unwrap();
    h.state_mut().voice_test_send("what are the workers doing");
    until(&mut h, "the reply", |h| h.state().transcript_talk(sid).iter().any(|l| l.starts_with("assistant: ")));
    until(&mut h, "the says", |h| h.state().voice_test_duplex_says().len() >= 3);
    assert!(h.state().voice_test_notice().is_none_or(|n| !n.contains("duplex")), "{:?}", h.state().voice_test_notice());
}

#[test]
fn with_no_duplex_model_hands_free_keeps_text_to_speech() {
    let rig = rig_with(vec![agent("glib", THREE_SENTENCES)]);
    supervisor(&rig, "glib");
    let mut h = harness_cfg(
        vec![
            ("url".into(), rig.url.clone()),
            ("voice_converse".into(), "1".into()),
            ("voice_hands_free".into(), "1".into()),
            ("voice_mode".into(), "agentbench".into()),
        ],
        Cfg::default(),
    );
    h.state_mut().voice_test_silent();
    h.state_mut().voice_test_hands_free(agentbench::voice::Sink::Headphones);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_supervisor();
    until(&mut h, "the microphone to open", |h| h.state().hands_free_on());
    let sid = h.state().supervisor_chat().unwrap();
    h.state_mut().voice_test_send("what are the workers doing");
    until(&mut h, "the reply", |h| {
        h.state().transcript_talk(sid).iter().any(|l| l.starts_with("assistant: "))
    });
    let mut spoken = Vec::new();
    let t = Instant::now();
    while spoken.is_empty() && t.elapsed() < Duration::from_secs(20) {
        spoken.extend(h.state_mut().voice_test_spoken());
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(spoken[0].contains("two workers"), "{spoken:?}");
    assert!(!h.state().voice_test_duplex_on());
}
