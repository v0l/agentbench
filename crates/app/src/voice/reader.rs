//! Reading a reply aloud, with the agentbench server's voice.
//!
//! The text is cut into sentences and each is posted to the server's `/v1/audio/speech`, so the
//! first can be heard while the rest are still being made. What comes back is a WAV; it is
//! resampled to whatever the sound card wants and queued for the output stream. Everything slow
//! happens off the UI thread, and [`Reader::tick`] is called once a frame to tidy up.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// Size of the first piece, in characters. Small, so the wait for the first words is short.
const FIRST: usize = 120;
/// Size of the pieces after it.
const REST: usize = 300;

/// What the worker shares with the sound card's callback.
struct Shared {
    queue: Mutex<VecDeque<f32>>,
    /// The worker has nothing more to add.
    done: AtomicBool,
    /// Somebody pressed stop, or started something else.
    stop: AtomicBool,
}

/// Where to get speech, and in which voice.
#[derive(Clone, Debug, Default)]
pub struct Source {
    pub url: String,
    pub token: String,
    /// A voice id, or empty for the server's default.
    pub voice: String,
}

#[derive(Default)]
pub struct Reader {
    playing: Option<(Arc<Shared>, cpal::Stream)>,
    errors: Option<Receiver<String>>,
}

impl Reader {
    /// Whether anything is being made or played.
    pub fn busy(&self) -> bool {
        self.playing.is_some()
    }

    /// Stop at once and forget what was queued.
    pub fn stop(&mut self) {
        if let Some((shared, _stream)) = self.playing.take() {
            shared.stop.store(true, Ordering::Relaxed);
        }
        self.errors = None;
    }

    /// Read `markdown` aloud, replacing anything being read now.
    pub fn start(
        &mut self,
        markdown: &str,
        from: Source,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), String> {
        self.stop();
        let text = egui_bench::voice::audio::speakable(markdown);
        if text.is_empty() {
            return Err("there is nothing to read in that".into());
        }
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            done: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let (stream, rate) = open(shared.clone())?;
        let (tx, rx) = channel();
        let (worker, wake) = (shared.clone(), Arc::new(wake));
        crate::spawn(async move {
            produce(&text, &from, rate, &worker, &tx, &*wake).await;
            worker.done.store(true, Ordering::Relaxed);
            wake();
        });
        self.playing = Some((shared, stream));
        self.errors = Some(rx);
        Ok(())
    }

    /// Once a frame: drop the stream when everything has been played, and hand back why a piece
    /// could not be made.
    pub fn tick(&mut self) -> Vec<String> {
        let errors: Vec<String> = self.errors.iter().flat_map(|rx| rx.try_iter()).collect();
        let finished = self.playing.as_ref().is_some_and(|(s, _)| {
            s.done.load(Ordering::Relaxed)
                && s.queue.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
        });
        if finished {
            self.stop();
        }
        errors
    }
}

/// Make each piece and queue it. Stops at the first failure, saying why.
async fn produce(
    text: &str,
    from: &Source,
    rate: u32,
    shared: &Shared,
    errors: &Sender<String>,
    wake: &(dyn Fn() + Send + Sync),
) {
    let mut pieces = egui_bench::voice::audio::chunks(text, REST);
    if let Some(first) = pieces.first().filter(|p| p.len() > FIRST * 2).cloned() {
        // Say a short opening on its own, so the wait for the first sound is short.
        let cut = egui_bench::voice::audio::chunks(&first, FIRST);
        pieces.splice(0..1, cut);
    }
    for piece in pieces {
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        match fetch(from, &piece).await {
            Ok((pcm, from_rate)) => {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                let pcm = egui_bench::voice::audio::resample(&pcm, from_rate, rate);
                shared.queue.lock().unwrap_or_else(|e| e.into_inner()).extend(pcm);
                wake();
            }
            Err(e) => {
                let _ = errors.send(e);
                return;
            }
        }
    }
}

/// Ask the server to say `text`. Returns mono samples and their rate.
pub async fn fetch(from: &Source, text: &str) -> Result<(Vec<f32>, u32), String> {
    let url = format!("{}/v1/audio/speech", from.url.trim().trim_end_matches('/'));
    let url = if url.starts_with("http://") || url.starts_with("https://") {
        url
    } else {
        format!("http://{url}")
    };
    let mut body = serde_json::json!({ "model": "kokoro", "input": text });
    if !from.voice.trim().is_empty() {
        body["voice"] = from.voice.trim().into();
    }
    let mut req = crate::client::http().post(&url).json(&body);
    if !from.token.trim().is_empty() {
        req = req.bearer_auth(from.token.trim());
    }
    // The first request after the server was started may be fetching a third of a gigabyte.
    let resp = req
        .timeout(std::time::Duration::from_secs(900))
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| format!("{url}: {e}"))?;
    if !status.is_success() {
        let said = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_string());
        return Err(format!("the server could not read that aloud: {status}: {said}"));
    }
    egui_bench::voice::audio::read_wav(&bytes)
}

/// Open the default sound output and start it pulling from the queue. Returns the stream and the
/// rate it wants.
fn open(shared: Arc<Shared>) -> Result<(cpal::Stream, u32), String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| "no sound output found; check the speakers or headphones".to_string())?;
    let name =
        device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "speakers".into());
    let config = device.default_output_config().map_err(|e| format!("{name}: {e}"))?;
    let (rate, channels) = (config.sample_rate(), config.channels().max(1) as usize);
    let cfg = config.config();
    let err = |e: cpal::Error| eprintln!("agentbench: sound output error: {e}");

    macro_rules! build {
        ($t:ty) => {{
            let shared = shared.clone();
            device.build_output_stream(
                cfg,
                move |data: &mut [$t], _| fill(data, channels, &shared),
                err,
                None,
            )
        }};
    }
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build!(f32),
        cpal::SampleFormat::I16 => build!(i16),
        cpal::SampleFormat::U16 => build!(u16),
        cpal::SampleFormat::I32 => build!(i32),
        other => return Err(format!("{name}: sample format {other:?} is not supported")),
    }
    .map_err(|e| format!("{name}: {e}"))?;
    stream.play().map_err(|e| format!("{name}: {e}"))?;
    Ok((stream, rate))
}

/// One callback's worth of output: the next samples in the queue, the same on every channel, and
/// silence when the queue runs dry.
fn fill<T: cpal::SizedSample + cpal::FromSample<f32>>(
    data: &mut [T],
    channels: usize,
    shared: &Shared,
) {
    let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
    for frame in data.chunks_mut(channels) {
        let v = queue.pop_front().unwrap_or(0.0);
        for out in frame {
            *out = T::from_sample(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A one request HTTP server that answers with `body` and hands back what it was sent.
    fn serve(status: &'static str, body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
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
                "HTTP/1.1 {status}\r\ncontent-type: audio/wav\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            s.write_all(head.as_bytes()).unwrap();
            s.write_all(&body).unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        (url, h)
    }

    #[test]
    fn a_sentence_is_posted_with_the_token_and_the_wav_comes_back() {
        let wav = egui_bench::voice::audio::wav_bytes(&[0.0, 0.25, -0.25, 0.5], 24_000);
        let (url, sent) = serve("200 OK", wav);
        let from = Source { url, token: "secret".into(), voice: "af_bella".into() };
        let (pcm, rate) = crate::runtime().block_on(fetch(&from, "Hello there.")).unwrap();
        assert_eq!(rate, 24_000);
        assert_eq!(pcm.len(), 4);
        let sent = sent.join().unwrap();
        assert!(sent.starts_with("POST /v1/audio/speech"), "{sent:.200}");
        assert!(sent.to_lowercase().contains("authorization: bearer secret"), "{sent:.300}");
        assert!(sent.contains("\"input\":\"Hello there.\""), "{sent}");
        assert!(sent.contains("\"voice\":\"af_bella\""), "{sent}");
    }

    #[test]
    fn a_server_error_is_reported_with_its_words() {
        let (url, _) = serve("500 Internal Server Error", br#"{"error":"no model"}"#.to_vec());
        let from = Source { url, ..Default::default() };
        let e = crate::runtime().block_on(fetch(&from, "Hi.")).unwrap_err();
        assert!(e.contains("no model") && e.contains("500"), "{e}");
    }

    #[test]
    fn the_queue_feeds_every_channel_and_then_goes_quiet() {
        let shared = Shared {
            queue: Mutex::new(VecDeque::from([0.5, -0.5])),
            done: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        };
        let mut out = [9.0f32; 8];
        fill(&mut out, 2, &shared);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5, 0.0, 0.0, 0.0, 0.0]);
    }
}
