mod common;

use agentbench::voice::engine::{Gpu, INSTALL};
use common::*;
use egui_kittest::Harness;
use egui_kittest::kittest::NodeT;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tungstenite::Message;

const FAKE_ENV: &str = "AGENTBENCH_FAKE_ENGINE";
const HOLDER_ENV: &str = "AGENTBENCH_ENGINE_HOLDER";
const FRAME: usize = 3200;

fn card(index: u32, free_gb: u64) -> Gpu {
    Gpu { index, name: format!("Test card {index}"), total_mib: 96 * 1024, free_mib: free_gb * 1024 }
}

fn fake_config() -> (u16, String) {
    let args: Vec<String> = std::env::args().collect();
    let config = std::env::var("FAKE_CONFIG").ok().or_else(|| {
        args.iter().position(|a| a == "--config").and_then(|i| args.get(i + 1)).cloned()
    });
    let text = std::fs::read_to_string(config.expect("a config")).unwrap();
    let value = |key: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(key)?.trim().strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
            .unwrap()
    };
    let port = value("bind").rsplit(':').next().unwrap().parse().unwrap();
    (port, value("id"))
}

fn respond(s: &mut TcpStream, code: u16, body: Value) {
    let b = body.to_string();
    let _ = write!(s, "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len());
}

fn duplex(tcp: TcpStream, key: &str) {
    let want = format!("Bearer {key}");
    let callback = |req: &tungstenite::handshake::server::Request, resp| {
        let ok = req.headers().get("authorization").and_then(|v| v.to_str().ok()) == Some(want.as_str());
        if ok { Ok(resp) } else { Err(tungstenite::http::Response::builder().status(401).body(None).unwrap()) }
    };
    let Ok(mut ws) = tungstenite::accept_hdr(tcp, callback) else { return };
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(&t).unwrap_or_default();
                if v["type"] == "start" {
                    let ready = json!({"type": "ready", "session": "s1", "sample_rate": 24000, "frame_samples": FRAME, "frame_bytes": FRAME * 2});
                    let _ = ws.send(Message::Text(ready.to_string().into()));
                }
            }
            Ok(Message::Binary(_)) => {
                let pcm = agent_voice::voice::audio::to_pcm16(&vec![0.2f32; FRAME]);
                if ws.send(Message::Binary(pcm.into())).is_err() {
                    return;
                }
            }
            Ok(Message::Close(_)) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

fn serve_fake() {
    let mode = std::env::var("FAKE_MODE").unwrap_or_default();
    let (port, id) = fake_config();
    let key = std::env::var("INFERENCE_BENCH_API_KEYS").unwrap_or_default();
    if let Ok(out) = std::env::var("FAKE_SEEN") {
        let seen = json!({
            "pid": std::process::id(),
            "cuda": std::env::var("CUDA_VISIBLE_DEVICES").ok(),
            "key": key,
            "port": port,
            "args": std::env::args().collect::<Vec<_>>(),
        });
        std::fs::write(out, seen.to_string()).unwrap();
    }
    eprintln!("fake engine: loading {id}");
    if mode == "crash" {
        eprintln!("\u{1b}[31mERROR\u{1b}[0m CUDA error: out of memory while loading {id}");
        std::process::exit(3);
    }
    let started = Instant::now();
    let load = Duration::from_millis(std::env::var("FAKE_LOAD_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500));
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    for s in listener.incoming() {
        let Ok(mut s) = s else { continue };
        let mut head = [0u8; 512];
        let n = s.peek(&mut head).unwrap_or(0);
        let line = String::from_utf8_lossy(&head[..n]).lines().next().unwrap_or_default().to_string();
        let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
        if path.starts_with("/v1/duplex") {
            let key = key.clone();
            std::thread::spawn(move || duplex(s, &key));
            continue;
        }
        let mut reader = BufReader::new(s.try_clone().unwrap());
        let mut auth = String::new();
        loop {
            let mut l = String::new();
            if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                break;
            }
            if l.to_lowercase().starts_with("authorization:") {
                auth = l.split_once(':').unwrap().1.trim().to_string();
            }
        }
        if path != "/health" && auth != format!("Bearer {key}") {
            respond(&mut s, 401, json!({"error": "no key"}));
            continue;
        }
        let loaded = started.elapsed() >= load;
        match path.as_str() {
            "/health" => respond(&mut s, 200, json!({"status": "ok"})),
            "/v1/capabilities" if mode == "noduplex" => {
                respond(&mut s, 200, json!({"routes": [{"path": "/v1/chat/completions"}]}))
            }
            "/v1/capabilities" => respond(&mut s, 200, json!({"routes": [{"path": "/v1/duplex"}, {"path": "/v1/models"}]})),
            "/v1/models" => respond(
                &mut s,
                200,
                json!({"data": [{"id": id, "type": "duplex", "loaded": loaded, "status": if loaded { "loaded" } else { "loading" }}]}),
            ),
            _ => respond(&mut s, 404, json!({})),
        }
    }
}

#[test]
fn fake_engine_main() {
    if std::env::var_os(FAKE_ENV).is_some() {
        serve_fake();
    }
}

struct Kit {
    dir: tempfile::TempDir,
    program: PathBuf,
    model: PathBuf,
    seen: PathBuf,
}

impl Kit {
    fn new(mode: &str) -> Kit {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("Haru-4B");
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join("config.json"), "{}").unwrap();
        std::fs::write(model.join("model.safetensors"), "").unwrap();
        let seen = dir.path().join("seen.json");
        let program = dir.path().join("inference-bench-server");
        let exe = std::env::current_exe().unwrap();
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo inference-bench-server 0.0.0-fake; exit 0; fi\nexport {FAKE_ENV}=1 FAKE_MODE={mode} FAKE_SEEN='{}' FAKE_CONFIG=\"$2\"\nexec '{}' --exact fake_engine_main --nocapture --test-threads=1\n",
            seen.display(),
            exe.display()
        );
        std::fs::write(&program, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        Kit { dir, program, model, seen }
    }

    fn engine_dir(&self) -> PathBuf {
        self.dir.path().join("engine")
    }

    fn seen(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.seen).unwrap()).unwrap()
    }

    fn attach(&self, h: &mut Harness<'static, agentbench::App>, gpus: Vec<Gpu>) {
        h.state_mut().voice_test_local_engine(&self.program, &self.model, self.engine_dir(), gpus);
    }
}

fn alive(pid: u32) -> bool {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    let state = stat.rsplit_once(") ").map(|(_, r)| r.chars().next().unwrap_or('X')).unwrap_or('X');
    !matches!(state, 'Z' | 'X')
}

fn wait_gone(pid: u32, within: Duration) -> bool {
    let t = Instant::now();
    while t.elapsed() < within {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
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

const REPLY: &str = r#"read p; sleep 1; echo '{"text":"I started two workers just now. They are still running."}'"#;

fn supervisor(rig: &Rig) {
    let project = rig.store.supervisor_project().unwrap().expect("the supervisor project");
    let tree = rig.store.main_worktree(project.id).unwrap();
    rig.store.add_session(tree.id, "glib", "Supervisor", None, None).unwrap();
}

fn app(rig: &Rig, size: [f32; 2]) -> Harness<'static, agentbench::App> {
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
    h.state_mut().voice_test_speech().duplex_voice = voice_clip_file();
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h
}

fn texts(h: &Harness<'_, agentbench::App>) -> String {
    let mut out = String::new();
    for n in h.root().children_recursive() {
        let a = n.accesskit_node();
        out.push_str(&a.label().unwrap_or_default());
        out.push('\n');
        out.push_str(&a.value().unwrap_or_default());
        out.push('\n');
    }
    out
}

fn notice(h: &Harness<'_, agentbench::App>) -> String {
    h.state().voice_test_notice().unwrap_or_default()
}

#[test]
fn hands_free_starts_the_engine_and_talks_to_it_without_showing_its_port_or_key() {
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("ok");
    let mut h = app(&rig, [1280.0, 800.0]);
    kit.attach(&mut h, vec![card(0, 2), card(1, 90)]);
    assert!(h.state().voice_test_engine().pid().is_none(), "nothing runs before hands free is in use");
    h.state_mut().open_supervisor();
    until(&mut h, "the engine to start", |h| h.state().voice_test_engine().pid().is_some());
    until(&mut h, "the loading line", |h| {
        h.state().talk_state() == Some("loading") && h.state().voice_progress().is_some_and(|p| p.starts_with("loading the voice model, "))
    });
    until_label(&mut h, "hands free listening");
    shot(&mut h, "engine-loading-1280");
    until(&mut h, "the duplex session", |h| h.state().voice_test_duplex_on());
    let seen = kit.seen();
    assert_eq!(seen["cuda"], "1", "the card with the most free memory");
    assert_eq!(std::env::var_os("CUDA_VISIBLE_DEVICES"), None, "set in the child's environment only");
    let (key, port) = (seen["key"].as_str().unwrap().to_string(), seen["port"].as_u64().unwrap());
    assert!(key.len() >= 32, "a random key: {key}");
    let url = h.state().voice_test_duplex_url().unwrap();
    assert_eq!(url, format!("http://127.0.0.1:{port}"));
    let config = std::fs::read_to_string(kit.engine_dir().join("engine.toml")).unwrap();
    assert!(config.contains(&format!("127.0.0.1:{port}")) && !config.contains(&key), "{config}");
    assert!(std::fs::read_to_string(kit.engine_dir().join("engine.log")).unwrap().contains("fake engine: loading Haru-4B"));
    let mut sound = 0;
    let (speech, rate) = agentbench::voice::read_wav(include_bytes!("fixtures/voice/speech.wav")).unwrap();
    let speech = agentbench::voice::to_whisper_rate(&speech, rate);
    for chunk in speech.chunks(1600).take(40) {
        h.state_mut().voice_test_hear(chunk);
        h.step();
        std::thread::sleep(Duration::from_millis(40));
        sound += h.state().voice_test_duplex_pull().is_some_and(|f| f.iter().any(|v| *v != 0.0)) as usize;
    }
    assert!(sound > 0, "reply frames came back with sound");
    let on_screen = texts(&h);
    h.state_mut().open_settings_page("speech");
    until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
    h.run_steps(4);
    let settings = texts(&h);
    for shown in [&on_screen, &settings] {
        assert!(!shown.contains(&key), "the key is never shown");
        assert!(!shown.contains(&format!(":{port}")) && !shown.contains(&port.to_string()), "the port is never shown");
    }
    assert!(settings.contains("loaded on GPU 1 (Test card 1)"), "{settings}");
}

#[test]
fn switching_hands_free_off_stops_the_engine() {
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("ok");
    let mut h = app(&rig, [1280.0, 800.0]);
    kit.attach(&mut h, vec![card(1, 90)]);
    h.state_mut().open_supervisor();
    until(&mut h, "the duplex session", |h| h.state().voice_test_duplex_on());
    let pid = kit.seen()["pid"].as_u64().unwrap() as u32;
    assert!(alive(pid));
    h.state_mut().voice_test_speech().hands_free = false;
    until(&mut h, "the engine to stop", |h| h.state().voice_test_engine().pid().is_none());
    assert!(wait_gone(pid, Duration::from_secs(10)), "the engine process exits");
    assert!(!h.state().voice_test_duplex_on());
}

#[test]
fn an_idle_engine_stops_after_the_timeout_and_zero_keeps_it() {
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("ok");
    let mut h = app(&rig, [1280.0, 800.0]);
    kit.attach(&mut h, vec![card(1, 90)]);
    h.state_mut().voice_test_speech().engine_idle_secs = 0;
    h.state_mut().open_supervisor();
    until(&mut h, "the duplex session", |h| h.state().voice_test_duplex_on());
    h.state_mut().voice_test_pause();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(h.state().voice_test_engine().pid().is_some(), "0 keeps it loaded");
    h.state_mut().voice_test_speech().engine_idle_secs = 2;
    let pid = h.state().voice_test_engine().pid().unwrap();
    let t = Instant::now();
    until(&mut h, "the idle stop", |h| h.state().voice_test_engine().pid().is_none());
    assert!(t.elapsed() < Duration::from_secs(5));
    assert!(wait_gone(pid, Duration::from_secs(10)));
}

#[test]
fn a_child_that_dies_is_restarted_once_then_left_off_with_its_log() {
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("crash");
    let mut h = app(&rig, [1280.0, 800.0]);
    kit.attach(&mut h, vec![card(1, 90)]);
    h.state_mut().open_supervisor();
    until(&mut h, "the first death", |h| notice(h).contains("starting it again"));
    assert_eq!(h.state().voice_test_engine().starts, 1);
    until(&mut h, "the second death", |h| notice(h).contains("left off"));
    let n = notice(&h);
    assert!(n.contains("CUDA error: out of memory") && !n.contains('\u{1b}'), "the log tail, without colour: {n}");
    assert_eq!(h.state().voice_test_engine().starts, 2, "restarted once, no more");
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(4) {
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(h.state().voice_test_engine().starts, 2);
    assert!(h.state().hands_free_on(), "hands free carries on with speech to text and read aloud");
}

#[test]
fn an_engine_without_duplex_support_says_how_to_update() {
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("noduplex");
    let mut h = app(&rig, [1280.0, 800.0]);
    kit.attach(&mut h, vec![card(1, 90)]);
    h.state_mut().open_supervisor();
    until(&mut h, "the refusal", |h| notice(h).contains("cannot serve duplex"));
    assert!(notice(&h).contains(INSTALL));
    assert!(h.state().voice_test_engine().pid().is_none(), "the engine is stopped");
}

#[test]
fn a_busy_card_is_refused_with_the_numbers_and_nothing_starts() {
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("ok");
    let mut h = app(&rig, [1280.0, 800.0]);
    kit.attach(&mut h, vec![card(0, 2), card(1, 90)]);
    h.state_mut().voice_test_speech().duplex_gpu = "0".into();
    h.state_mut().open_supervisor();
    until(&mut h, "the refusal", |h| notice(h).contains("GPU 0"));
    let n = notice(&h);
    assert!(n.contains("2.0 GB free of 96.0 GB") && n.contains("16.0 GB"), "{n}");
    assert_eq!(h.state().voice_test_engine().starts, 0);
    assert!(!kit.seen.exists());
}

#[test]
fn a_missing_engine_says_what_to_install() {
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("ok");
    let mut h = app(&rig, [1280.0, 800.0]);
    kit.attach(&mut h, vec![card(1, 90)]);
    h.state_mut().voice_test_speech().engine_program = "/no/such/inference-bench-server".into();
    h.state_mut().open_supervisor();
    until(&mut h, "the install line", |h| notice(h).contains(INSTALL));
    assert_eq!(h.state().voice_test_engine().starts, 0);
}

#[test]
fn a_set_server_address_launches_nothing() {
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("ok");
    let mut h = app(&rig, [1280.0, 800.0]);
    kit.attach(&mut h, vec![card(1, 90)]);
    h.state_mut().voice_test_speech().duplex_url = "http://127.0.0.1:9/v1".into();
    h.state_mut().open_supervisor();
    until(&mut h, "the microphone to open", |h| h.state().hands_free_on());
    h.run_steps(20);
    assert_eq!(h.state().voice_test_duplex_url().as_deref(), Some("http://127.0.0.1:9/v1"));
    assert_eq!(h.state().voice_test_engine().starts, 0);
    assert!(!kit.seen.exists());
}

#[test]
fn a_missing_model_says_what_is_missing_and_falls_back_to_text_to_speech() {
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("ok");
    std::fs::remove_file(kit.model.join("model.safetensors")).unwrap();
    let mut h = app(&rig, [1280.0, 800.0]);
    kit.attach(&mut h, vec![card(1, 90)]);
    h.state_mut().open_supervisor();
    until(&mut h, "the missing file", |h| notice(h).contains("has no model.safetensors"));
    assert_eq!(h.state().voice_test_engine().starts, 0);
    let sid = h.state().supervisor_chat().unwrap();
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
    assert!(!h.state().voice_test_duplex_on());
}

fn real_engine() -> Option<(String, String)> {
    Some((std::env::var("AGENTBENCH_REAL_ENGINE").ok()?, std::env::var("AGENTBENCH_REAL_MODEL").ok()?))
}

fn real_gpu() -> u32 {
    std::env::var("AGENTBENCH_REAL_GPU").ok().and_then(|g| g.parse().ok()).unwrap_or(1)
}

fn smi_used(gpu: u32) -> u64 {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=index,memory.used,uuid", "--format=csv,noheader,nounits"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let Some((used, uuid)) = text.lines().find_map(|l| {
        let mut p = l.split(',').map(str::trim);
        (p.next()?.parse::<u32>().ok()? == gpu).then(|| Some((p.next()?.parse::<u64>().ok()?, p.next()?.to_string())))?
    }) else {
        return 0;
    };
    let apps = std::process::Command::new("nvidia-smi")
        .args(["--query-compute-apps=pid,used_memory,gpu_uuid", "--format=csv,noheader,nounits"])
        .output()
        .unwrap();
    let own: u64 = String::from_utf8_lossy(&apps.stdout)
        .lines()
        .filter_map(|l| {
            let p: Vec<&str> = l.split(',').map(str::trim).collect();
            (p.len() == 3 && p[2] == uuid && p[0].parse::<u32>().ok()? == std::process::id()).then(|| p[1].parse::<u64>().ok())?
        })
        .sum();
    used.saturating_sub(own)
}

fn twenty_seconds() -> Vec<f32> {
    let rate = agentbench::voice::RATE;
    let mut pcm = Vec::new();
    while pcm.len() < rate as usize * 20 {
        for wav in [&include_bytes!("fixtures/voice/speech.wav")[..], include_bytes!("fixtures/voice/ask.wav"), include_bytes!("fixtures/voice/barge.wav")] {
            let (p, r) = agentbench::voice::read_wav(wav).unwrap();
            pcm.extend(agentbench::voice::to_whisper_rate(&p, r));
            pcm.extend(vec![0.0005f32; rate as usize]);
        }
    }
    pcm.truncate(rate as usize * 20);
    pcm
}

#[test]
fn the_speech_check_passes_recorded_speech_and_fails_white_noise() {
    for wav in [&include_bytes!("fixtures/voice/speech.wav")[..], include_bytes!("fixtures/voice/ask.wav"), include_bytes!("fixtures/voice/barge.wav")] {
        let (pcm, rate) = agentbench::voice::read_wav(wav).unwrap();
        let s = sound_of(&pcm, rate);
        assert!(speech_like(&s), "speech: {s:?}");
    }
    let mut seed = 1u32;
    let noise: Vec<f32> = (0..48_000)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1u32 << 24) as f32 * 0.6 - 0.3
        })
        .collect();
    let s = sound_of(&noise, 24_000);
    assert!(!speech_like(&s), "white noise: {s:?}");
    let hum: Vec<f32> = (0..48_000).map(|i| 0.2 * (i as f32 * 0.05).sin()).collect();
    let s = sound_of(&hum, 24_000);
    assert!(!speech_like(&s), "a steady tone: {s:?}");
    assert!(!speech_like(&sound_of(&vec![0.0; 48_000], 24_000)), "silence");
}

#[test]
#[ignore = "needs the real engine and model in AGENTBENCH_REAL_ENGINE and AGENTBENCH_REAL_MODEL, on a free GPU"]
fn the_real_model_loads_answers_with_sound_and_frees_its_gpu() {
    let (program, model) = real_engine().expect("AGENTBENCH_REAL_ENGINE and AGENTBENCH_REAL_MODEL");
    let gpu = real_gpu();
    let before = smi_used(gpu);
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let mut h = app(&rig, [1280.0, 800.0]);
    h.state_mut().voice_test_real_engine(&program, &model, gpu);
    let t = Instant::now();
    h.state_mut().open_supervisor();
    let mut lines = std::collections::BTreeSet::new();
    while !h.state().voice_test_duplex_on() {
        assert!(t.elapsed() < Duration::from_secs(300), "no ready: {:?}", h.state().voice_test_notice());
        if let Some(p) = h.state().voice_progress() {
            lines.insert(p);
        }
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    let ready = t.elapsed();
    let load = h.state().voice_test_engine().load_time.unwrap();
    let pid = h.state().voice_test_engine().pid().unwrap();
    let held = smi_used(gpu);
    eprintln!("progress lines: {:?}", lines.iter().take(3).collect::<Vec<_>>());
    eprintln!("model loaded in {:.1} s, session ready {:.1} s after switch on, GPU {gpu} {before} MiB before, {held} MiB loaded", load.as_secs_f32(), ready.as_secs_f32());
    assert!(!lines.is_empty(), "a loading line was shown");
    let clip = twenty_seconds();
    let block = agentbench::voice::RATE as usize / 50;
    let start = Instant::now();
    let (mut pulled, mut sound) = (0, 0);
    let mut played = Vec::new();
    let mut next_pull = Instant::now();
    for (i, chunk) in clip.chunks(block).enumerate() {
        let due = start + Duration::from_millis(20 * i as u64);
        while Instant::now() < due {
            std::thread::sleep(Duration::from_millis(1));
        }
        h.state_mut().voice_test_hear(chunk);
        h.step();
        if Instant::now() >= next_pull {
            next_pull += Duration::from_millis(133);
            let f = h.state().voice_test_duplex_pull().unwrap();
            pulled += 1;
            sound += f.iter().any(|v| v.abs() > 1e-4) as usize;
            played.extend(f);
        }
    }
    let sent = h.state().voice_test_duplex_sent();
    let muted = h.state().voice_test_duplex_muted();
    let heard = sound_of(&played, 24_000);
    eprintln!("20 s streamed: {sent} frames sent, {pulled} pulled, {sound} with sound, {muted} muted before speech, played {heard:?}");
    if let Ok(out) = std::env::var("AGENTBENCH_REAL_PLAYED") {
        std::fs::write(out, agentbench::voice::wav_bytes(&played, 24_000)).unwrap();
    }
    assert!(sent >= 140, "{sent}");
    assert!(sound > pulled / 4, "frames came back with sound: {sound} of {pulled}");
    assert!(speech_like(&heard), "what was played is not speech like: {heard:?}");
    h.state_mut().voice_test_speech().duplex_replies = true;
    let sid = h.state().supervisor_chat().unwrap();
    h.state_mut().voice_test_send("what are the workers doing");
    let t = Instant::now();
    while h.state().voice_test_duplex_says().len() < 2 {
        assert!(t.elapsed() < Duration::from_secs(60), "no says: {:?}", h.state().voice_test_notice());
        h.state_mut().voice_test_hear(&vec![0.0005f32; agentbench::voice::RATE as usize / 50]);
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut reply = Vec::new();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(8) {
        h.state_mut().voice_test_hear(&vec![0.0005f32; agentbench::voice::RATE as usize * 133 / 1000]);
        h.step();
        reply.extend(h.state().voice_test_duplex_pull().unwrap());
        std::thread::sleep(Duration::from_millis(133));
    }
    let said = sound_of(&reply, 24_000);
    eprintln!("said reply in {sid:?}: {said:?}");
    if let Ok(out) = std::env::var("AGENTBENCH_REAL_PLAYED") {
        std::fs::write(format!("{out}.reply.wav"), agentbench::voice::wav_bytes(&reply, 24_000)).unwrap();
    }
    assert!(said.rms < 0.01 || speech_like(&said), "the said reply is noise: {said:?}");
    assert!(h.state().voice_test_notice().is_none_or(|n| !n.contains("voice model")), "{:?}", h.state().voice_test_notice());
    h.state_mut().voice_test_speech().hands_free = false;
    until(&mut h, "the engine to stop", |h| h.state().voice_test_engine().pid().is_none());
    assert!(wait_gone(pid, Duration::from_secs(20)), "the engine exits");
    let t = Instant::now();
    while smi_used(gpu) > before + 200 && t.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(200));
    }
    let after = smi_used(gpu);
    let apps = std::process::Command::new("nvidia-smi").args(["--query-compute-apps=pid,used_memory", "--format=csv,noheader"]).output().unwrap();
    eprintln!("after switch off: GPU {gpu} {after} MiB besides this test, compute apps: {}", String::from_utf8_lossy(&apps.stdout).replace('\n', "; "));
    assert!(after <= before + 200, "the memory is freed: {after} MiB against {before} before");
}

#[test]
fn the_local_voice_settings_fit_at_both_sizes() {
    use egui_kittest::kittest::Queryable;
    let rig = rig();
    let kit = Kit::new("ok");
    for (tag, size) in [("1280", [1280.0, 800.0]), ("390", [390.0, 844.0])] {
        let mut h = app(&rig, size);
        kit.attach(&mut h, vec![card(0, 2), card(1, 90)]);
        h.state_mut().open_settings_page("speech");
        h.run_steps(2);
        until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
        h.run_steps(4);
        h.get_by_label("replies in its voice").scroll_to_me();
        h.run_steps(4);
        let shown = texts(&h);
        for want in ["Haru-4B (local)", "model found in", "loads on GPU 1 (Test card 1): it has the most free memory, 90.0 GB", "engine: inference-bench-server 0.0.0-fake"] {
            assert!(shown.contains(want), "{tag}: {want} is missing");
        }
        for label in ["replies in its voice", "model speaks first", "record again", "unload after"] {
            let r = h.get_by_label(label).rect();
            assert!(r.right() <= size[0] && r.left() >= 0.0, "{label} runs off a {tag} screen: {r:?}");
        }
        shot(&mut h, &format!("engine-settings-{tag}"));
    }
}

fn hold_an_engine() {
    let dir = PathBuf::from(std::env::var(HOLDER_ENV).unwrap());
    let rig = rig_with(vec![agent("glib", REPLY)]);
    supervisor(&rig);
    let kit = Kit::new("ok");
    let mut h = app(&rig, [1280.0, 800.0]);
    match real_engine() {
        Some((program, model)) => h.state_mut().voice_test_real_engine(&program, &model, real_gpu()),
        None => kit.attach(&mut h, vec![card(1, 90)]),
    }
    h.state_mut().open_supervisor();
    if real_engine().is_some() {
        until(&mut h, "the engine to start", |h| h.state().voice_test_engine().pid().is_some());
    } else {
        until(&mut h, "the duplex session", |h| h.state().voice_test_duplex_on());
    }
    let pid = h.state().voice_test_engine().pid().unwrap();
    std::fs::write(dir.join("engine.pid"), pid.to_string()).unwrap();
    loop {
        h.step();
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn engine_holder_main() {
    if std::env::var_os(HOLDER_ENV).is_some() {
        hold_an_engine();
    }
}

#[test]
fn killing_the_app_with_sigkill_takes_the_engine_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "engine_holder_main", "--nocapture", "--test-threads=1"])
        .env(HOLDER_ENV, dir.path())
        .env("AGENTBENCH_UI_LOCK_INHERITED", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid_file = dir.path().join("engine.pid");
    let t = Instant::now();
    while !pid_file.exists() {
        assert!(t.elapsed() < Duration::from_secs(120), "the app never started its engine");
        assert!(app.try_wait().unwrap().is_none(), "the app exited early");
        std::thread::sleep(Duration::from_millis(100));
    }
    let engine: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    assert!(alive(engine));
    app.kill().unwrap();
    app.wait().unwrap();
    assert!(wait_gone(engine, Duration::from_secs(5)), "the engine {engine} outlived the app it belonged to");
}
