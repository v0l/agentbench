//! Speech from a server that speaks OpenAI's audio endpoints, in both directions.
//!
//! `transcribe` posts a take to `/audio/transcriptions` and gets words back; `speak` posts a
//! sentence to `/audio/speech` and gets samples back. Any server that does these will do: OpenAI,
//! Groq, faster-whisper-server, whisper.cpp, or an app that runs the models itself (see the
//! `speech` feature). Both block, so call them off the UI thread.

use super::audio::{read_wav, wav_bytes};
use std::time::Duration;

/// A speech server: its address up to the `/v1`, and the key it wants, if any.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Server {
    pub url: String,
    pub key: String,
}

fn agent(patience: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(patience))
        .http_status_as_error(false)
        .build()
        .new_agent()
}

impl Server {
    /// `url` with the endpoint's path on the end, whether or not `url` already has it.
    pub fn endpoint(&self, path: &str) -> String {
        let base = self.url.trim().trim_end_matches('/');
        let base = if base.starts_with("http://") || base.starts_with("https://") {
            base.to_string()
        } else {
            format!("http://{base}")
        };
        if base.ends_with(path) { base } else { format!("{base}{path}") }
    }

    /// The words in a take of 16 kHz mono samples. `model` is sent as the model name; empty sends
    /// `whisper-1`.
    pub fn transcribe(&self, pcm: &[f32], model: &str) -> Result<String, String> {
        let url = self.endpoint("/audio/transcriptions");
        let model = if model.trim().is_empty() { "whisper-1" } else { model.trim() };
        let boundary = "----agent-voice-speech";
        let mut body = Vec::new();
        let mut field = |name: &str, value: &str| {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        };
        field("model", model);
        // Words only: a server that cannot do verbose_json still answers this.
        field("response_format", "json");
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&wav_bytes(pcm, super::audio::SPEECH_RATE));
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let mut req = agent(Duration::from_secs(120))
            .post(&url)
            .header("Content-Type", &format!("multipart/form-data; boundary={boundary}"));
        if !self.key.trim().is_empty() {
            req = req.header("Authorization", &format!("Bearer {}", self.key.trim()));
        }
        let mut res = req.send(&body[..]).map_err(|e| format!("{url}: {e}"))?;
        let status = res.status();
        let text = res.body_mut().read_to_string().map_err(|e| format!("{url}: {e}"))?;
        if !status.is_success() {
            return Err(format!("{url}: {status}: {}", text.trim()));
        }
        // `{"text": "..."}`, or a plain body from a server that ignored the format.
        Ok(match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => v.get("text").and_then(|t| t.as_str()).unwrap_or_default().trim().to_string(),
            Err(_) => text.trim().to_string(),
        })
    }

    /// `text` said aloud, as mono samples and their rate. `voice` empty lets the server choose.
    /// The first request to a server that fetches its model may wait a long time.
    pub fn speak(&self, text: &str, voice: &str) -> Result<(Vec<f32>, u32), String> {
        let url = self.endpoint("/audio/speech");
        let mut body = serde_json::json!({ "model": "kokoro", "input": text });
        if !voice.trim().is_empty() {
            body["voice"] = voice.trim().into();
        }
        let mut req =
            agent(Duration::from_secs(900)).post(&url).header("content-type", "application/json");
        if !self.key.trim().is_empty() {
            req = req.header("Authorization", &format!("Bearer {}", self.key.trim()));
        }
        let mut res = req.send(body.to_string()).map_err(|e| format!("{url}: {e}"))?;
        let status = res.status();
        let bytes = res
            .body_mut()
            .with_config()
            .limit(256 << 20)
            .read_to_vec()
            .map_err(|e| format!("{url}: {e}"))?;
        if !status.is_success() {
            let said = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_string());
            return Err(format!("the server could not read that aloud: {status}: {said}"));
        }
        read_wav(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A one request HTTP server that answers with `body`, and hands back what it was sent.
    fn serve(status: &'static str, body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if rest.len() >= want {
                        break;
                    }
                }
            }
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            s.write_all(head.as_bytes()).unwrap();
            s.write_all(&body).unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        (url, h)
    }

    #[test]
    fn the_endpoint_is_built_from_a_base_url() {
        let s = |u: &str| Server { url: u.into(), key: String::new() };
        assert_eq!(
            s("localhost:8000/v1/").endpoint("/audio/speech"),
            "http://localhost:8000/v1/audio/speech"
        );
        assert_eq!(
            s("https://x.org/v1/audio/speech").endpoint("/audio/speech"),
            "https://x.org/v1/audio/speech"
        );
    }

    #[test]
    fn a_take_is_posted_as_a_wav_and_the_text_comes_back() {
        let (url, sent) = serve("200 OK", br#"{"text":" hello there "}"#.to_vec());
        let server = Server { url, key: "secret".into() };
        assert_eq!(server.transcribe(&[0.0; 1600], "").unwrap(), "hello there");
        let sent = sent.join().unwrap();
        assert!(sent.starts_with("POST /v1/audio/transcriptions"), "{sent:.200}");
        assert!(sent.to_lowercase().contains("authorization: bearer secret"));
        assert!(sent.contains("whisper-1") && sent.contains("RIFF"), "model and wav are sent");
    }

    #[test]
    fn a_sentence_is_posted_and_the_wav_comes_back() {
        let wav = wav_bytes(&[0.0, 0.25, -0.25, 0.5], 24_000);
        let (url, sent) = serve("200 OK", wav);
        let server = Server { url, key: String::new() };
        let (pcm, rate) = server.speak("Hello there.", "af_bella").unwrap();
        assert_eq!((pcm.len(), rate), (4, 24_000));
        let sent = sent.join().unwrap();
        assert!(sent.starts_with("POST /v1/audio/speech"), "{sent:.200}");
        assert!(sent.contains("\"input\":\"Hello there.\""), "{sent}");
        assert!(sent.contains("\"voice\":\"af_bella\""), "{sent}");
    }

    #[test]
    fn a_server_error_is_reported_with_its_words() {
        let (url, _) = serve("500 Internal Server Error", br#"{"error":"no model"}"#.to_vec());
        let e = Server { url, key: String::new() }.speak("Hi.", "").unwrap_err();
        assert!(e.contains("no model") && e.contains("500"), "{e}");
        let (url, _) = serve("401 Unauthorized", b"bad key".to_vec());
        let e = Server { url, key: String::new() }.transcribe(&[0.0; 10], "").unwrap_err();
        assert!(e.contains("401") && e.contains("bad key"), "{e}");
    }
}
