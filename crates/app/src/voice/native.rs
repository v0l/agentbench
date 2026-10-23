//! The microphone and the two readers of what it heard.
//!
//! Everything slow happens off the UI thread: the model download, loading the weights, running
//! Whisper, and the request to a server. The window asks [`Voice::poll`] once a frame what has
//! finished.

use super::{MAX_SECONDS, MIN_PEAK, MIN_SECONDS, Mode, RATE, Settings};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use web_time::Instant;

/// What the worker has to say.
pub enum Event {
    /// Progress worth showing while the user waits.
    #[cfg_attr(not(feature = "local-speech"), allow(dead_code))]
    Status(String),
    /// What was said.
    Text(String),
    /// Why nothing was.
    Failed(String),
}

enum State {
    Idle,
    Listening(Take),
    /// A take is being read; `Event::Text` or `Event::Failed` ends it.
    Reading,
}

/// An open microphone and what it has heard so far.
struct Take {
    _stream: cpal::Stream,
    heard: Arc<Mutex<Vec<f32>>>,
    peak: Arc<AtomicU32>,
    rate: u32,
    device: String,
    since: Instant,
}

#[cfg(feature = "local-speech")]
type Loaded = Arc<Mutex<Option<(String, super::whisper::Whisper)>>>;
#[cfg(not(feature = "local-speech"))]
type Loaded = ();

pub struct Voice {
    state: State,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    /// The model once loaded, kept for the next take: loading it takes longer than reading with it.
    loaded: Loaded,
    /// The last thing the worker said while a take was being read.
    pub status: String,
}

impl Default for Voice {
    fn default() -> Self {
        let (tx, rx) = channel();
        Voice { state: State::Idle, tx, rx, loaded: Default::default(), status: String::new() }
    }
}

impl Voice {
    pub fn listening(&self) -> bool {
        matches!(self.state, State::Listening(_))
    }

    pub fn reading(&self) -> bool {
        matches!(self.state, State::Reading)
    }

    pub fn busy(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    /// Seconds since the microphone opened.
    pub fn seconds(&self) -> f32 {
        match &self.state {
            State::Listening(t) => t.since.elapsed().as_secs_f32(),
            _ => 0.0,
        }
    }

    /// Loudest recent sample, 0 to 1, for a meter. Reading it resets it.
    pub fn level(&self) -> f32 {
        match &self.state {
            State::Listening(t) => f32::from_bits(t.peak.swap(0, Ordering::Relaxed)),
            _ => 0.0,
        }
    }

    /// What the microphone is, once it is open.
    pub fn device(&self) -> &str {
        match &self.state {
            State::Listening(t) => &t.device,
            _ => "",
        }
    }

    /// Open the microphone.
    pub fn start(&mut self, wake: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
        if self.busy() {
            return Ok(());
        }
        let _ = wake;
        self.state = State::Listening(open()?);
        self.status.clear();
        Ok(())
    }

    /// Close the microphone without reading anything.
    pub fn cancel(&mut self) {
        if self.listening() {
            self.state = State::Idle;
        }
    }

    /// Close the microphone and read what it heard.
    pub fn finish(&mut self, settings: &Settings, wake: impl Fn() + Send + Sync + 'static) {
        let State::Listening(take) = std::mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        let rate = take.rate;
        let heard = std::mem::take(&mut *take.heard.lock().unwrap_or_else(|e| e.into_inner()));
        drop(take);

        let seconds = heard.len() as f32 / rate.max(1) as f32;
        let peak = heard.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        if seconds < MIN_SECONDS {
            let _ =
                self.tx.send(Event::Failed("too short to read; hold on a little longer".into()));
            return;
        }
        if peak < MIN_PEAK {
            let _ = self
                .tx
                .send(Event::Failed("the microphone heard nothing; check it is not muted".into()));
            return;
        }
        if let Some(p) = settings.problem() {
            let _ = self.tx.send(Event::Failed(p.into()));
            return;
        }

        self.state = State::Reading;
        self.status = "reading".into();
        let pcm = super::to_whisper_rate(&heard, rate);
        let tx = self.tx.clone();
        let settings = settings.clone();
        let loaded = self.loaded.clone();
        std::thread::Builder::new()
            .name("foreman-speech".into())
            .spawn(move || {
                let said = match settings.mode {
                    Mode::Local => read_locally(&settings, &pcm, &tx, &wake, &loaded),
                    Mode::Server => read_on_server(&settings, &pcm),
                };
                let _ = tx.send(match said {
                    Ok(t) => Event::Text(t),
                    Err(e) => Event::Failed(e),
                });
                wake();
            })
            .ok();
    }

    /// Take in what the worker finished. `Ok` carries words to add to the draft, `Err` a problem to
    /// show.
    pub fn poll(&mut self) -> Vec<Result<String, String>> {
        let mut out = Vec::new();
        for ev in self.rx.try_iter().collect::<Vec<_>>() {
            match ev {
                Event::Status(s) => self.status = s,
                Event::Text(t) => {
                    self.state = State::Idle;
                    self.status.clear();
                    out.push(Ok(t));
                }
                Event::Failed(e) => {
                    self.state = State::Idle;
                    self.status.clear();
                    out.push(Err(e));
                }
            }
        }
        out
    }

    /// Whether the files of `model` are already on this machine.
    pub fn installed(model: &str) -> bool {
        model_dir(model).is_some_and(|d| has_files(&d))
    }
}

fn model_dir(model: &str) -> Option<std::path::PathBuf> {
    super::settings::model(model).map(|m| foreman_server::home().join("models").join(m.0))
}

fn has_files(dir: &std::path::Path) -> bool {
    ["config.json", "tokenizer.json", "model.safetensors"].iter().all(|f| dir.join(f).exists())
}

/// Open the default microphone and start collecting mono samples.
fn open() -> Result<Take, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| "no microphone found; plug one in or check its permissions".to_string())?;
    let name =
        device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "microphone".into());
    let config = device.default_input_config().map_err(|e| format!("{name}: {e}"))?;
    let (rate, channels) = (config.sample_rate(), config.channels() as usize);
    let heard = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 10)));
    let peak = Arc::new(AtomicU32::new(0));
    let limit = rate as usize * MAX_SECONDS;
    let cfg = config.config();
    let err = |e: cpal::Error| tracing_free_warn(&e.to_string());

    macro_rules! build {
        ($t:ty) => {{
            let (heard, peak) = (heard.clone(), peak.clone());
            device.build_input_stream(
                cfg,
                move |data: &[$t], _| push(data, channels, limit, &heard, &peak),
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
    Ok(Take { _stream: stream, heard, peak, rate, device: name, since: Instant::now() })
}

fn tracing_free_warn(e: &str) {
    eprintln!("foreman: microphone stream error: {e}");
}

/// One callback's worth of samples, mixed to mono and appended until the take is as long as allowed.
fn push<T: cpal::SizedSample>(
    data: &[T],
    channels: usize,
    limit: usize,
    heard: &Mutex<Vec<f32>>,
    peak: &AtomicU32,
) where
    f32: cpal::FromSample<T>,
{
    let Ok(mut heard) = heard.lock() else { return };
    let mut loudest = 0.0f32;
    for frame in data.chunks(channels.max(1)) {
        // Averaged: a headset with a dead channel would otherwise record at half level.
        let v = frame.iter().map(|s| cpal::Sample::to_sample::<f32>(*s)).sum::<f32>()
            / frame.len() as f32;
        loudest = loudest.max(v.abs());
        if heard.len() < limit {
            heard.push(v);
        }
    }
    // The meter keeps the loudest thing since it was last read.
    let mut cur = peak.load(Ordering::Relaxed);
    while f32::from_bits(cur) < loudest {
        match peak.compare_exchange_weak(
            cur,
            loudest.to_bits(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(now) => cur = now,
        }
    }
}

#[cfg(feature = "local-speech")]
fn read_locally(
    settings: &Settings,
    pcm: &[f32],
    tx: &Sender<Event>,
    wake: &(impl Fn() + Send + Sync + 'static),
    loaded: &Loaded,
) -> Result<String, String> {
    let dir = model_dir(&settings.model).ok_or("unknown speech model")?;
    let mut slot = loaded.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_none_or(|(id, _)| *id != settings.model) {
        *slot = None;
        if !has_files(&dir) {
            let repo = super::settings::model(&settings.model).map(|m| m.1).unwrap_or_default();
            download(repo, &dir, tx, wake)?;
        }
        let _ = tx.send(Event::Status("loading the speech model".into()));
        wake();
        let files = super::whisper::Files::in_dir(&dir)
            .ok_or_else(|| format!("{} is missing model files", dir.display()))?;
        let (device, _) = super::whisper::best_device();
        let model = match super::whisper::Whisper::load(&files, device) {
            Ok(m) => m,
            // A card that took the weights and then failed falls back to the CPU.
            Err(_) => super::whisper::Whisper::load(&files, candle_core::Device::Cpu)?,
        };
        *slot = Some((settings.model.clone(), model));
    }
    let _ = tx.send(Event::Status("reading".into()));
    wake();
    let (_, model) = slot.as_mut().expect("model loaded above");
    model.transcribe(pcm)
}

#[cfg(not(feature = "local-speech"))]
fn read_locally(
    _: &Settings,
    _: &[f32],
    _: &Sender<Event>,
    _: &(impl Fn() + Send + Sync + 'static),
    _: &Loaded,
) -> Result<String, String> {
    Err("this build has no local speech model; choose a speech server in settings".into())
}

/// Fetch the three files of a model from Hugging Face into `dir`, once.
#[cfg(feature = "local-speech")]
fn download(
    repo: &str,
    dir: &std::path::Path,
    tx: &Sender<Event>,
    wake: &(impl Fn() + Send + Sync + 'static),
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = repo.rsplit('/').next().unwrap_or(repo);
    crate::runtime().block_on(async {
        let http = crate::client::http();
        for file in ["config.json", "tokenizer.json", "model.safetensors"] {
            let target = dir.join(file);
            if target.exists() {
                continue;
            }
            let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");
            let mut resp = http
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("{url}: {e}"))?
                .error_for_status()
                .map_err(|e| format!("{url}: {e}"))?;
            let total = resp.content_length().unwrap_or(0);
            // Written beside the target and renamed, so a download that stops half way is not
            // taken for a model.
            let part = dir.join(format!("{file}.part"));
            let mut out =
                std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
            let (mut got, mut shown) = (0u64, 0u64);
            while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{url}: {e}"))? {
                std::io::Write::write_all(&mut out, &chunk)
                    .map_err(|e| format!("{}: {e}", part.display()))?;
                got += chunk.len() as u64;
                if got - shown > 1 << 20 || got == total {
                    shown = got;
                    let of =
                        if total > 0 { format!(" of {} MB", total >> 20) } else { String::new() };
                    let _ =
                        tx.send(Event::Status(format!("downloading {name}: {} MB{of}", got >> 20)));
                    wake();
                }
            }
            drop(out);
            std::fs::rename(&part, &target).map_err(|e| format!("{}: {e}", target.display()))?;
        }
        Ok::<(), String>(())
    })
}

/// Post the take as a WAV to an OpenAI compatible transcription endpoint.
fn read_on_server(settings: &Settings, pcm: &[f32]) -> Result<String, String> {
    let url = settings.endpoint();
    let wav = super::wav_bytes(pcm, RATE);
    let model = if settings.server_model.trim().is_empty() {
        "whisper-1".to_string()
    } else {
        settings.server_model.trim().to_string()
    };
    let key = settings.key.trim().to_string();
    crate::runtime().block_on(async {
        let file = reqwest::multipart::Part::bytes(wav)
            .file_name("speech.wav")
            .mime_str("audio/wav")
            .map_err(|e| e.to_string())?;
        let form = reqwest::multipart::Form::new()
            .part("file", file)
            .text("model", model)
            // Words only: a server that cannot do verbose_json still answers this.
            .text("response_format", "json");
        let mut req = crate::client::http().post(&url).multipart(form);
        if !key.is_empty() {
            req = req.bearer_auth(key);
        }
        let resp = req
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| format!("{url}: {e}"))?;
        if !status.is_success() {
            return Err(format!("{url}: {status}: {}", body.trim()));
        }
        // `{"text": "..."}`, or a plain body from a server that ignored the format.
        Ok(match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(v) => v.get("text").and_then(|t| t.as_str()).unwrap_or_default().trim().to_string(),
            Err(_) => body.trim().to_string(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A one request HTTP server that answers with `body` and hands back what it was sent.
    fn serve(body: &'static str) -> (String, std::thread::JoinHandle<String>) {
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
                // Headers, then as many body bytes as Content-Length promised.
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let want = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if got.len() >= head.len() + 4 + want
                        || (want == 0 && rest.is_empty() && n == 0)
                    {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        (url, h)
    }

    #[test]
    fn a_take_is_posted_as_a_wav_and_the_text_comes_back() {
        let (url, seen) = serve(r#"{"text":" hello there "}"#);
        let settings = Settings {
            mode: Mode::Server,
            url,
            key: "sekret".into(),
            server_model: "tiny".into(),
            ..Default::default()
        };
        let said = read_on_server(&settings, &vec![0.1f32; 16_000]).unwrap();
        assert_eq!(said, "hello there");
        let sent = seen.join().unwrap();
        assert!(sent.starts_with("POST /v1/audio/transcriptions"), "{sent:.200}");
        assert!(sent.to_lowercase().contains("authorization: bearer sekret"));
        assert!(sent.contains("multipart/form-data"));
        assert!(sent.contains("name=\"model\""));
        assert!(sent.contains("RIFF"));
    }

    #[test]
    fn a_server_error_is_reported_with_its_words() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 65536];
            let _ = s.read(&mut buf);
            let body = "no such model";
            let _ = s.write_all(
                format!("HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
                    .as_bytes(),
            );
        });
        let settings = Settings { mode: Mode::Server, url, ..Default::default() };
        let e = read_on_server(&settings, &vec![0.1f32; 16_000]).unwrap_err();
        assert!(e.contains("404") && e.contains("no such model"), "{e}");
    }

    #[test]
    fn samples_are_mixed_to_mono_and_the_meter_keeps_the_peak() {
        let heard = Mutex::new(Vec::new());
        let peak = AtomicU32::new(0);
        push(&[0.5f32, -0.5, 1.0, 0.0], 2, 100, &heard, &peak);
        assert_eq!(*heard.lock().unwrap(), vec![0.0, 0.5]);
        assert_eq!(f32::from_bits(peak.load(Ordering::Relaxed)), 0.5);
        // The limit stops a forgotten microphone growing without end.
        push(&[0.1f32; 10], 1, 3, &heard, &peak);
        assert_eq!(heard.lock().unwrap().len(), 3);
    }

    /// Needs a model on disc: `FOREMAN_TEST_MODEL=~/.local/share/waveshark/models/whisper-base.en`
    /// and a recording: `FOREMAN_TEST_WAV=said.wav` (16 bit mono). Prints what it read.
    #[cfg(feature = "local-speech")]
    #[test]
    #[ignore = "needs a downloaded model and a recording"]
    fn whisper_reads_a_recording() {
        let (Some(dir), Some(wav)) =
            (std::env::var_os("FOREMAN_TEST_MODEL"), std::env::var_os("FOREMAN_TEST_WAV"))
        else {
            panic!("set FOREMAN_TEST_MODEL and FOREMAN_TEST_WAV");
        };
        let bytes = std::fs::read(wav).unwrap();
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let pcm: Vec<f32> = bytes[44..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect();
        let pcm = super::super::to_whisper_rate(&pcm, rate);
        let files = super::super::whisper::Files::in_dir(std::path::Path::new(&dir)).unwrap();
        let (device, on) = super::super::whisper::best_device();
        let mut w = super::super::whisper::Whisper::load(&files, device).unwrap();
        let t = std::time::Instant::now();
        let said = w.transcribe(&pcm).unwrap();
        println!(
            "on {on}: {:.1}s audio in {:.1}s: {said}",
            pcm.len() as f32 / 16_000.0,
            t.elapsed().as_secs_f32()
        );
        assert!(!said.trim().is_empty());
    }
}
