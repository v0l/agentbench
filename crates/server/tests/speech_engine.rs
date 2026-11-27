use agentbench_server::engine::Settings;
use axum::extract::{Multipart, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const FAKE_ENV: &str = "AGENTBENCH_FAKE_SPEECH_ENGINE";
const FRAME: usize = 1920;

fn config_value(text: &str, key: &str) -> Vec<String> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix(key)?.trim().strip_prefix('='))
        .map(|v| v.trim().trim_matches('"').to_string())
        .collect()
}

#[derive(Clone)]
struct Fake {
    key: String,
    ids: Vec<String>,
    kinds: Vec<String>,
    seen: PathBuf,
    used: Arc<Mutex<Vec<String>>>,
}

fn note(f: &Fake, what: Value) {
    let mut line = what.to_string();
    line.push('\n');
    use std::io::Write;
    let _ = std::fs::OpenOptions::new().create(true).append(true).open(&f.seen).and_then(|mut o| o.write_all(line.as_bytes()));
}

fn authed(f: &Fake, h: &HeaderMap) -> bool {
    h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) == Some(format!("Bearer {}", f.key).as_str())
}

async fn transcriptions(State(f): State<Fake>, h: HeaderMap, mut form: Multipart) -> Response {
    if !authed(&f, &h) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "no key"}}))).into_response();
    }
    let mut fields = serde_json::Map::new();
    let mut samples = 0usize;
    let mut rate = 0u32;
    let mut first = 0.0f32;
    while let Ok(Some(field)) = form.next_field().await {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            let b = field.bytes().await.unwrap();
            let tag = u16::from_le_bytes([b[20], b[21]]);
            rate = u32::from_le_bytes([b[24], b[25], b[26], b[27]]);
            samples = (b.len() - 44) / if tag == 3 { 4 } else { 2 };
            if tag == 3 && b.len() >= 48 {
                first = f32::from_le_bytes([b[44], b[45], b[46], b[47]]);
            }
            fields.insert("tag".into(), json!(tag));
        } else {
            fields.insert(name, json!(field.text().await.unwrap()));
        }
    }
    let model = fields.get("model").and_then(|m| m.as_str()).unwrap_or_default().to_string();
    f.used.lock().unwrap().push(model.clone());
    note(&f, json!({"route": "transcriptions", "fields": fields, "samples": samples, "rate": rate, "first": first}));
    if samples < 100 {
        return (StatusCode::NOT_FOUND, Json(json!({"error": {"message": "too short for the engine"}}))).into_response();
    }
    Json(json!({
        "task": "transcribe",
        "text": format!(" heard {samples} samples with {model} "),
        "segments": [
            {"no_speech_prob": 0.3, "avg_logprob": -0.2, "tokens": [1, 2]},
            {"no_speech_prob": 0.1, "avg_logprob": -0.8, "tokens": [3, 4]},
        ],
    }))
    .into_response()
}

async fn speech(State(f): State<Fake>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if !authed(&f, &h) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "no key"}}))).into_response();
    }
    f.used.lock().unwrap().push(body["model"].as_str().unwrap_or_default().to_string());
    note(&f, json!({"route": "speech", "body": body, "encoding": h.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok())}));
    let frames: Vec<Vec<u8>> = (0..3).map(|i| agent_voice::voice::audio::to_pcm16(&vec![0.1 * (i + 1) as f32; FRAME])).collect();
    if body["response_format"] == "pcm" {
        let stream = futures_util::stream::iter(frames.into_iter().map(Ok::<_, std::io::Error>));
        return (
            [(header::CONTENT_TYPE, "audio/L16"), (header::HeaderName::from_static("x-sample-rate"), "24000"), (header::CACHE_CONTROL, "no-store")],
            axum::body::Body::from_stream(stream),
        )
            .into_response();
    }
    let pcm: Vec<f32> = (0..3).flat_map(|i| vec![0.1 * (i + 1) as f32; FRAME]).collect();
    ([(header::CONTENT_TYPE, "audio/wav")], agent_voice::voice::wav_bytes(&pcm, 24_000)).into_response()
}

async fn models(State(f): State<Fake>) -> Json<Value> {
    let used = f.used.lock().unwrap().clone();
    let data: Vec<Value> = f
        .ids
        .iter()
        .zip(&f.kinds)
        .map(|(id, kind)| json!({"id": id, "type": kind, "loaded": used.contains(id)}))
        .collect();
    Json(json!({"data": data}))
}

async fn serve_fake() {
    let config = std::env::var("FAKE_CONFIG").unwrap();
    let text = std::fs::read_to_string(&config).unwrap();
    let port: u16 = config_value(&text, "bind")[0].rsplit(':').next().unwrap().parse().unwrap();
    let paths = config_value(&text, "model");
    let kinds = paths.iter().map(|p| if p.ends_with("phonon") { "speech" } else { "transcription" }.to_string()).collect();
    let f = Fake {
        key: std::env::var("INFERENCE_BENCH_API_KEYS").unwrap(),
        ids: config_value(&text, "id"),
        kinds,
        seen: PathBuf::from(std::env::var("FAKE_SEEN").unwrap()),
        used: Default::default(),
    };
    note(&f, json!({"route": "start", "cuda": std::env::var("CUDA_VISIBLE_DEVICES").ok(), "config": text}));
    let app = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/v1/capabilities", get(|| async { Json(json!({"routes": [{"path": "/v1/audio/transcriptions"}, {"path": "/v1/audio/speech"}]})) }))
        .route("/v1/models", get(models))
        .route("/v1/audio/transcriptions", post(transcriptions))
        .route("/v1/audio/speech", post(speech))
        .route("/v1/audio/voices", get(|| async { Json(json!({"voices": [{"id": "Hadley", "label": "Hadley, American", "installed": true}]})) }))
        .route("/v1/audio/status", get(|| async { Json(json!({"device": "GPU", "reading_on": "GPU", "speaking_on": null})) }))
        .with_state(f);
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    axum::serve(l, app).await.unwrap();
}

#[test]
fn fake_engine_main() {
    if std::env::var_os(FAKE_ENV).is_some() {
        tokio::runtime::Runtime::new().unwrap().block_on(serve_fake());
    }
}

struct Rig {
    url: String,
    token: String,
    seen: PathBuf,
    _dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
}

fn whisper(models: &Path, id: &str) {
    let d = models.join(id);
    std::fs::create_dir_all(&d).unwrap();
    for f in ["config.json", "generation_config.json", "preprocessor_config.json", "tokenizer.json", "model.safetensors"] {
        std::fs::write(d.join(f), "{}").unwrap();
    }
}

fn phonon(models: &Path) {
    let d = models.join("phonon");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("config.json"), r#"{"model_type": "gradium_phonon_tts"}"#).unwrap();
    for f in ["tokenizer.json", "model.safetensors"] {
        std::fs::write(d.join(f), "{}").unwrap();
    }
    std::fs::write(d.join("voices.jsonl"), r#"{"filename": "voices/a.safetensors", "name": "Hadley", "language": "en", "locale": "en-us"}"#).unwrap();
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models");
    whisper(&models, "whisper-base.en");
    whisper(&models, "whisper-large-v3-turbo");
    phonon(&models);
    let seen = dir.path().join("seen.jsonl");
    let program = dir.path().join("inference-bench-server");
    let exe = std::env::current_exe().unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo inference-bench-server 0.0.0-fake; exit 0; fi\nexport {FAKE_ENV}=1 FAKE_SEEN='{}' FAKE_CONFIG=\"$2\"\nexec '{}' --exact fake_engine_main --nocapture --test-threads=1\n",
        seen.display(),
        exe.display()
    );
    std::fs::write(&program, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let store = agentbench_server::store::Store::memory().unwrap();
    let token = agentbench_server::token(&store).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let settings = Settings { program: program.display().to_string(), cpu: true, ..Default::default() };
    let m = models;
    let engine_dir = dir.path().join("engine");
    rt.spawn(async move {
        let _ = agentbench_server::serve_agents_with(store, Vec::new(), None, listener, m, settings, engine_dir, std::future::pending()).await;
    });
    let r = Rig { url, token, seen, _dir: dir, _rt: rt };
    for _ in 0..100 {
        if std::net::TcpStream::connect(r.url.trim_start_matches("http://")).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    r
}

impl Rig {
    fn seen(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.seen).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder().http_status_as_error(false).timeout_global(Some(Duration::from_secs(60))).build().new_agent()
    }

    fn post_form(&self, body: Vec<u8>, boundary: &str, token: &str) -> (u16, Value) {
        let mut r = self
            .agent()
            .post(format!("{}/v1/audio/transcriptions", self.url))
            .header("authorization", &format!("Bearer {token}"))
            .header("content-type", &format!("multipart/form-data; boundary={boundary}"))
            .send(&body[..])
            .unwrap();
        let code = r.status().as_u16();
        (code, r.body_mut().read_json().unwrap_or(Value::Null))
    }

    fn get(&self, path: &str) -> Value {
        let mut r = self.agent().get(format!("{}{path}", self.url)).header("authorization", &format!("Bearer {}", self.token)).call().unwrap();
        r.body_mut().read_json().unwrap()
    }
}

fn pcm16_wav(seconds: f32, rate: u32) -> Vec<u8> {
    let pcm: Vec<f32> = (0..(seconds * rate as f32) as usize).map(|i| 0.25 * (i as f32 * 0.05).sin()).collect();
    agent_voice::voice::wav_bytes(&pcm, rate)
}

#[test]
fn the_app_multipart_body_is_read_by_the_engine_and_scored_as_candle_scored_it() {
    let r = rig();
    let body = agentbench_server::audio::form(&[("model", "whisper-base.en"), ("response_format", "json")], &pcm16_wav(1.0, 48_000), "b1");
    let (code, v) = r.post_form(body, "b1", &r.token);
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["text"], "heard 16000 samples with whisper-base.en", "48 kHz resampled to 16 kHz once, here: {v}");
    assert!((v["no_speech_prob"].as_f64().unwrap() - 0.1).abs() < 1e-6, "{v}");
    assert!((v["avg_logprob"].as_f64().unwrap() + 0.5).abs() < 1e-6, "{v}");
    let seen = r.seen();
    let start = &seen[0];
    assert_eq!(start["route"], "start");
    assert_eq!(start["cuda"], "", "cpu = true hides every card");
    let config = start["config"].as_str().unwrap();
    assert!(config.contains("whisper-base.en") && config.contains("whisper-large-v3-turbo") && config.contains("phonon"), "{config}");
    assert!(!config.contains("preload = true"), "nothing loads before it is asked for: {config}");
    let t = seen.iter().find(|s| s["route"] == "transcriptions").unwrap();
    assert_eq!(t["fields"]["response_format"], "verbose_json", "the scores need segments");
    assert_eq!(t["fields"]["tag"], 3, "float samples, no second rounding to PCM16");
    assert_eq!(t["rate"], 16_000);
}

#[test]
fn an_unknown_model_gets_the_default_and_the_form_fields_pass_through() {
    let r = rig();
    let body = agentbench_server::audio::form(&[("model", "whisper-1"), ("language", "en"), ("prompt", "hello"), ("temperature", "0.2"), ("other", "x")], &pcm16_wav(0.5, 16_000), "b2");
    let (code, v) = r.post_form(body, "b2", &r.token);
    assert_eq!(code, 200, "{v}");
    assert!(v["text"].as_str().unwrap().ends_with("with whisper-large-v3-turbo"), "the saved default: {v}");
    let t = r.seen().into_iter().find(|s| s["route"] == "transcriptions").unwrap();
    assert_eq!((t["fields"]["language"].as_str(), t["fields"]["prompt"].as_str(), t["fields"]["temperature"].as_str()), (Some("en"), Some("hello"), Some("0.2")));
    assert!(t["fields"].get("other").is_none());
}

#[test]
fn a_wrong_token_never_reaches_the_engine() {
    let r = rig();
    let body = agentbench_server::audio::form(&[("model", "whisper-base.en")], &pcm16_wav(0.5, 16_000), "b3");
    let (code, _) = r.post_form(body, "b3", "wrong");
    assert_eq!(code, 401);
    assert!(r.seen().is_empty(), "the engine was not even started");
}

#[test]
fn speech_streams_the_engine_frames_with_the_headers_the_reader_needs() {
    let r = rig();
    let mut res = r
        .agent()
        .post(format!("{}/v1/audio/speech", r.url))
        .header("authorization", &format!("Bearer {}", r.token))
        .header("accept-encoding", "identity")
        .send_json(json!({"model": "kokoro", "input": "Hello there.", "response_format": "pcm", "voice": "zoey"}))
        .unwrap();
    assert_eq!(res.status().as_u16(), 200);
    assert_eq!(res.headers().get("content-type").unwrap(), "audio/L16");
    assert_eq!(res.headers().get("x-sample-rate").unwrap(), "24000");
    assert_eq!(res.headers().get("cache-control").unwrap(), "no-store");
    let bytes = res.body_mut().read_to_vec().unwrap();
    let want: Vec<u8> = (0..3).flat_map(|i| agent_voice::voice::audio::to_pcm16(&vec![0.1 * (i + 1) as f32; FRAME])).collect();
    assert_eq!(bytes, want, "the engine's PCM16 bytes as they came");
    let s = r.seen().into_iter().find(|s| s["route"] == "speech").unwrap();
    assert_eq!(s["body"], json!({"model": "phonon-31m-en", "input": "Hello there.", "voice": "zoey", "response_format": "pcm"}));
    assert_eq!(s["encoding"], "identity");
}

#[test]
fn speech_as_one_wav_for_the_client_that_asks_for_one() {
    let r = rig();
    let mut res = r
        .agent()
        .post(format!("{}/v1/audio/speech", r.url))
        .header("authorization", &format!("Bearer {}", r.token))
        .send_json(json!({"model": "kokoro", "input": "Hello there.", "seed": 1484}))
        .unwrap();
    assert_eq!(res.headers().get("content-type").unwrap(), "audio/wav");
    let (pcm, rate) = agent_voice::voice::read_wav(&res.body_mut().read_to_vec().unwrap()).unwrap();
    assert_eq!((pcm.len(), rate), (3 * FRAME, 24_000));
    let s = r.seen().into_iter().find(|s| s["route"] == "speech").unwrap();
    assert_eq!(s["body"]["seed"], 1484, "a seed is passed on, for a repeatable reading");
    let blank = r.agent().post(format!("{}/v1/audio/speech", r.url)).header("authorization", &format!("Bearer {}", r.token)).send_json(json!({"input": "  "})).unwrap();
    assert_eq!(blank.status().as_u16(), 400, "nothing to say is refused here, as before");
}

#[test]
fn the_lists_and_status_come_from_the_engine_in_the_agentbench_shapes() {
    let r = rig();
    let v = r.get("/v1/audio/voices");
    assert_eq!(v["voices"][0]["label"], "Hadley, American", "from the catalogue on disc before the engine runs: {v}");
    let m = r.get("/v1/audio/models");
    let data = m["data"].as_array().unwrap();
    assert_eq!(data.iter().filter(|d| d["default"] == true).count(), 1);
    let base = data.iter().find(|d| d["id"] == "whisper-base.en").unwrap();
    assert_eq!((base["installed"].as_bool(), base["object"].as_str()), (Some(true), Some("model")));
    assert_eq!(data.iter().find(|d| d["id"] == "whisper-tiny.en").unwrap()["installed"], false);
    let s = r.get("/v1/audio/status");
    assert_eq!(s["device"], "CPU", "cpu = true: {s}");
    assert!(s["reading_on"].is_null());
    let body = agentbench_server::audio::form(&[("model", "whisper-base.en")], &pcm16_wav(0.5, 16_000), "b4");
    assert_eq!(r.post_form(body, "b4", &r.token).0, 200);
    let s = r.get("/v1/audio/status");
    assert_eq!(s["reading_on"], "GPU", "what the engine says once it has read: {s}");
    let v = r.get("/v1/audio/voices");
    assert_eq!(v["voices"].as_array().unwrap().len(), 1);
}

#[test]
fn an_engine_error_comes_back_with_its_words() {
    let r = rig();
    let body = agentbench_server::audio::form(&[("model", "whisper-base.en")], &pcm16_wav(0.002, 16_000), "b5");
    let (code, v) = r.post_form(body, "b5", &r.token);
    assert_eq!(code, 500, "a 404 from the engine is not a missing route here: {v}");
    assert_eq!(v["error"], "the speech engine answered: too short for the engine");
}
