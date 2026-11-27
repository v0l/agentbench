use crate::engine::{Child, Engines, Need};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::time::Duration;

pub type Failure = (StatusCode, String);

fn internal(e: impl std::fmt::Display) -> Failure {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub fn float_wav(pcm: &[f32], rate: u32) -> Vec<u8> {
    let data = (pcm.len() * 4) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 4);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

pub fn form(fields: &[(&str, &str)], wav: &[u8], boundary: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn message(status: StatusCode, body: &str) -> Failure {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let text = v["error"]["message"].as_str().or(v["error"].as_str()).map(str::to_string).unwrap_or_else(|| body.trim().to_string());
    let status = match status.as_u16() {
        400 | 413 | 429 => status,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, format!("the speech engine answered: {text}"))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Heard {
    pub text: String,
    pub no_speech: Option<f32>,
    pub avg_logprob: Option<f32>,
}

pub fn heard(v: &Value) -> Heard {
    let segments = v["segments"].as_array().cloned().unwrap_or_default();
    let no_speech = segments.iter().filter_map(|s| s["no_speech_prob"].as_f64()).fold(None, |m: Option<f64>, p| Some(m.map_or(p, |m| m.min(p))));
    let (mut sum, mut n) = (0.0f64, 0usize);
    for s in &segments {
        let tokens = s["tokens"].as_array().map_or(1, |t| t.len().max(1));
        if let Some(lp) = s["avg_logprob"].as_f64() {
            sum += lp * tokens as f64;
            n += tokens;
        }
    }
    Heard {
        text: v["text"].as_str().unwrap_or_default().trim().to_string(),
        no_speech: no_speech.map(|p| p as f32),
        avg_logprob: (n > 0).then(|| (sum / n as f64) as f32),
    }
}

pub async fn read(engines: &Engines, id: &str, pcm: &[f32], extra: &[(String, String)]) -> Result<(Heard, Child), Failure> {
    let child = engines.child(Need::Whisper(id.to_string())).await.map_err(internal)?;
    let boundary = format!("agentbench-{}", uuid::Uuid::new_v4().simple());
    let mut fields: Vec<(&str, &str)> = vec![("model", child.model.as_str()), ("response_format", "verbose_json")];
    fields.extend(extra.iter().filter(|(k, _)| matches!(k.as_str(), "language" | "prompt" | "temperature")).map(|(k, v)| (k.as_str(), v.as_str())));
    let body = form(&fields, &float_wav(pcm, agent_voice::voice::SPEECH_RATE), &boundary);
    let res = crate::http::client()
        .post(format!("{}/v1/audio/transcriptions", child.url))
        .bearer_auth(&child.key)
        .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
        .body(body)
        .timeout(Duration::from_secs(3600))
        .send()
        .await
        .map_err(|e| internal(format!("the speech engine: {e}")))?;
    let status = res.status();
    let text = res.text().await.map_err(internal)?;
    if !status.is_success() {
        return Err(message(status, &text));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| internal(format!("the speech engine answered {e}: {text:.200}")))?;
    Ok((heard(&v), child))
}

pub async fn speak(engines: &Engines, input: &str, voice: &str, stream: bool) -> Result<Response, Failure> {
    let child = engines.child(Need::Speech).await.map_err(internal)?;
    let mut body = json!({"model": child.model, "input": input, "voice": voice});
    if stream {
        body["response_format"] = json!("pcm");
    }
    let res = crate::http::client()
        .post(format!("{}/v1/audio/speech", child.url))
        .bearer_auth(&child.key)
        .header(header::ACCEPT_ENCODING, "identity")
        .json(&body)
        .timeout(Duration::from_secs(3600))
        .send()
        .await
        .map_err(|e| internal(format!("the speech engine: {e}")))?;
    let status = res.status();
    if !status.is_success() {
        let text = res.text().await.unwrap_or_default();
        return Err(message(status, &text));
    }
    let mut headers = HeaderMap::new();
    for name in [header::CONTENT_TYPE, HeaderName::from_static("x-sample-rate"), header::CACHE_CONTROL] {
        if let Some(v) = res.headers().get(&name) {
            headers.insert(name, v.clone());
        }
    }
    Ok((headers, axum::body::Body::from_stream(res.bytes_stream())).into_response())
}

async fn get_json(child: &Child, path: &str) -> Option<Value> {
    let res = crate::http::client().get(format!("{}{path}", child.url)).bearer_auth(&child.key).timeout(Duration::from_secs(10)).send().await.ok()?;
    res.status().is_success().then_some(())?;
    res.json().await.ok()
}

pub async fn voices(engines: &Engines) -> Value {
    if let Some(child) = engines.running()
        && let Some(v) = get_json(&child, "/v1/audio/voices").await
        && v["voices"].as_array().is_some_and(|l| !l.is_empty())
    {
        return v;
    }
    let dir = engines.phonon();
    let list = tokio::task::spawn_blocking(move || {
        use agent_voice::voice::catalogue;
        let path = dir.join(catalogue::CATALOG);
        if !path.is_file()
            && let Err(e) = catalogue::fetch_catalogue(&dir)
        {
            tracing::warn!("could not fetch the voice catalogue: {e}");
        }
        catalogue::read_catalog(&path)
            .unwrap_or_default()
            .into_iter()
            .map(|v| json!({"id": v.name, "label": v.label(), "installed": catalogue::voice_path(&dir, &v).is_file()}))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    json!({"voices": list})
}

pub async fn loaded(engines: &Engines) -> Vec<String> {
    let Some(child) = engines.running() else { return Vec::new() };
    let Some(v) = get_json(&child, "/v1/models").await else { return Vec::new() };
    v["data"].as_array().into_iter().flatten().filter(|m| m["loaded"] == true).filter_map(|m| m["id"].as_str().map(str::to_string)).collect()
}

pub async fn status(engines: &Engines) -> agentbench_core::AudioStatus {
    let mut s = agentbench_core::AudioStatus { device: device(engines).await, reading_on: None, speaking_on: None };
    if let Some(child) = engines.running()
        && let Some(v) = get_json(&child, "/v1/audio/status").await
    {
        s.reading_on = v["reading_on"].as_str().map(str::to_string);
        s.speaking_on = v["speaking_on"].as_str().map(str::to_string);
    }
    s
}

async fn device(engines: &Engines) -> String {
    if let Some(d) = engines.device() {
        return d;
    }
    if engines.settings.cpu {
        return "CPU".into();
    }
    let pick = engines.settings.gpu;
    tokio::task::spawn_blocking(move || {
        let gpus = agent_voice::engine::smi_gpus().unwrap_or_default();
        let need = crate::engine::WHISPER_MIB + crate::engine::SPEECH_MIB;
        if agent_voice::engine::choose_gpu(&gpus, pick, need).is_ok() { "GPU" } else { "CPU" }.to_string()
    })
    .await
    .unwrap_or_else(|_| "CPU".into())
}

#[derive(Clone)]
pub struct Ears(pub Engines);

#[agent_core::async_trait]
impl agent_native::native::Ears for Ears {
    fn pick(&self, asked: &str) -> String {
        self.0.pick(asked)
    }

    async fn read(&self, id: &str, pcm: Vec<f32>) -> Result<(String, &'static str), String> {
        let (h, _) = read(&self.0, id, &pcm, &[]).await.map_err(|e| e.1)?;
        let on = if self.0.device().as_deref() == Some("CPU") { "CPU" } else { "GPU" };
        Ok((h.text, on))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_float_wav_reads_back_as_the_same_samples() {
        let pcm = [0.0f32, 0.25, -0.5, 0.999];
        let (back, rate) = agent_voice::voice::read_wav(&float_wav(&pcm, 16_000)).unwrap();
        assert_eq!((back.as_slice(), rate), (&pcm[..], 16_000));
    }

    #[test]
    fn the_scores_come_from_the_segments_as_the_candle_reader_made_them() {
        let v = json!({"text": " Hello there. ", "segments": [
            {"no_speech_prob": 0.2, "avg_logprob": -0.5, "tokens": [1, 2, 3]},
            {"no_speech_prob": 0.05, "avg_logprob": -1.0, "tokens": [4]},
        ]});
        let h = heard(&v);
        assert_eq!(h.text, "Hello there.");
        assert_eq!(h.no_speech, Some(0.05), "the least likely window, as candle took");
        assert!((h.avg_logprob.unwrap() + 0.625).abs() < 1e-6, "weighted by tokens: {:?}", h.avg_logprob);
        assert_eq!(heard(&json!({"text": "x"})), Heard { text: "x".into(), no_speech: None, avg_logprob: None });
    }

    #[test]
    fn an_engine_error_keeps_its_words_and_only_client_codes_pass_through() {
        let body = r#"{"error": {"message": "input is empty", "type": "invalid_request_error"}}"#;
        assert_eq!(message(StatusCode::BAD_REQUEST, body), (StatusCode::BAD_REQUEST, "the speech engine answered: input is empty".into()));
        assert_eq!(message(StatusCode::NOT_FOUND, "gone").0, StatusCode::INTERNAL_SERVER_ERROR, "a 404 would read as cannot read aloud");
        assert_eq!(message(StatusCode::TOO_MANY_REQUESTS, "{}").0, StatusCode::TOO_MANY_REQUESTS);
    }
}
