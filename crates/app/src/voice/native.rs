//! The microphone and the two readers of what it heard.
//!
//! Everything slow happens off the UI thread: the model download, loading the weights, running
//! Whisper, and the request to a server. The window asks [`Voice::poll`] once a frame what has
//! finished.

use super::{Endpoint, Heard, MAX_SECONDS, MIN_PEAK, MIN_SECONDS, Mode, RATE, Said, Settings};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use web_time::Instant;

/// What the worker has to say.
pub enum Event {
    /// Progress worth showing while the user waits.
    #[cfg_attr(not(local_speech), allow(dead_code))]
    Status(String),
    /// What was said.
    Text(String),
    /// A reading of take number `.0` while it is still going.
    Partial(u64, String),
    /// Why nothing was.
    Failed(String),
}

enum State {
    Idle,
    Listening(Box<Take>),
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
    /// Decides when the speaker has finished.
    endpoint: Endpoint,
    /// How many samples had been heard when the last partial reading was started.
    partial_at: usize,
    /// A partial reading is under way; only one runs at a time.
    partial_busy: Arc<AtomicBool>,
}

#[cfg(local_speech)]
type Loaded = Arc<Mutex<Option<egui_bench::speech::Transcriber>>>;
#[cfg(not(local_speech))]
type Loaded = ();

pub struct Voice {
    state: State,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    /// The model once loaded, kept for the next take: loading it takes longer than reading with it.
    loaded: Loaded,
    /// The last thing the worker said while a take was being read.
    pub status: String,
    /// Which take is current, so a late partial from an earlier one is dropped.
    take_id: u64,
}

impl Default for Voice {
    fn default() -> Self {
        let (tx, rx) = channel();
        Voice {
            state: State::Idle,
            tx,
            rx,
            loaded: Default::default(),
            status: String::new(),
            take_id: 0,
        }
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
        #[cfg(target_os = "android")]
        if !super::android::microphone_allowed() {
            super::android::ask_for_microphone()?;
            return Err(
                "allow the microphone in the box that just opened, then press speak again".into()
            );
        }
        self.state = State::Listening(Box::new(open()?));
        self.take_id += 1;
        self.status.clear();
        Ok(())
    }

    /// Called once a frame while the microphone is open. Listens for the end of the speech and
    /// closes the take by itself, and every couple of seconds starts a reading of what has been
    /// said so far so the words can appear while you are still talking.
    pub fn tick(&mut self, settings: &Settings, wake: impl Fn() + Send + Sync + Clone + 'static) {
        let State::Listening(take) = &mut self.state else {
            return;
        };
        let (heard, samples) = {
            let heard = take.heard.lock().unwrap_or_else(|e| e.into_inner());
            (take.endpoint.feed(&heard), heard.len())
        };
        match heard {
            Heard::Done => self.finish(settings, wake),
            Heard::GaveUp => {
                self.state = State::Idle;
                let _ = self.tx.send(Event::Failed("no speech was heard".into()));
            }
            Heard::Speaking | Heard::Pause => {
                let due = take.rate as usize * super::PARTIAL_SECONDS;
                if settings.problem().is_none()
                    && !take.partial_busy.load(Ordering::Relaxed)
                    && samples >= take.partial_at + due
                {
                    take.partial_at = samples;
                    take.partial_busy.store(true, Ordering::Relaxed);
                    let pcm = {
                        let h = take.heard.lock().unwrap_or_else(|e| e.into_inner());
                        super::to_whisper_rate(&h, take.rate)
                    };
                    let busy = take.partial_busy.clone();
                    let id = self.take_id;
                    self.read(pcm, settings, Some((id, busy)), wake);
                }
            }
            Heard::Waiting => {}
        }
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
        let mut heard = std::mem::take(&mut *take.heard.lock().unwrap_or_else(|e| e.into_inner()));
        // Keep a little of the silence the take ended on and drop the rest: Whisper invents words
        // for a long quiet tail.
        let tail = ((take.endpoint.quiet_seconds() - 0.3).max(0.0) * rate as f32) as usize;
        heard.truncate(heard.len().saturating_sub(tail));
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
        self.read(pcm, settings, None, wake);
    }

    /// Read `pcm` on a worker thread. A partial reading carries its take's id and busy flag and
    /// reports `Event::Partial`; the last one reports `Event::Text` or `Event::Failed`.
    fn read(
        &self,
        pcm: Vec<f32>,
        settings: &Settings,
        partial: Option<(u64, Arc<AtomicBool>)>,
        wake: impl Fn() + Send + Sync + 'static,
    ) {
        let tx = self.tx.clone();
        let settings = settings.clone();
        let loaded = self.loaded.clone();
        std::thread::Builder::new()
            .name("agentbench-speech".into())
            .spawn(move || {
                let quiet = partial.is_some();
                let said = match settings.mode {
                    Mode::Local => read_locally(&settings, &pcm, &tx, &wake, &loaded, quiet),
                    Mode::Server => read_on_server(&settings, &pcm),
                    // `Settings::resolved` turns this into a server before a take is read.
                    Mode::Agentbench => Err("not connected to an agentbench server".to_string()),
                };
                match partial {
                    Some((id, busy)) => {
                        busy.store(false, Ordering::Relaxed);
                        // A partial that fails says nothing: the last reading reports it.
                        if let Ok(t) = said {
                            let _ = tx.send(Event::Partial(id, t));
                        }
                    }
                    None => {
                        let _ = tx.send(match said {
                            Ok(t) => Event::Text(t),
                            Err(e) => Event::Failed(e),
                        });
                    }
                }
                wake();
            })
            .ok();
    }

    /// Take in what the worker finished: partial readings, then the last one or a problem.
    pub fn poll(&mut self) -> Vec<Said> {
        let mut out = Vec::new();
        for ev in self.rx.try_iter().collect::<Vec<_>>() {
            match ev {
                Event::Status(s) => self.status = s,
                Event::Partial(id, t) => {
                    if id == self.take_id && self.busy() {
                        out.push(Said::Partial(t));
                    }
                }
                Event::Text(t) => {
                    self.state = State::Idle;
                    self.status.clear();
                    out.push(Said::Final(t));
                }
                Event::Failed(e) => {
                    self.state = State::Idle;
                    self.status.clear();
                    out.push(Said::Failed(e));
                }
            }
        }
        out
    }

    /// Whether the files of `model` are already on this machine.
    pub fn installed(model: &str) -> bool {
        #[cfg(local_speech)]
        {
            egui_bench::speech::Transcriber::new(models_dir()).installed(model)
        }
        #[cfg(not(local_speech))]
        {
            let _ = model;
            false
        }
    }
}

/// What this window's own Whisper runs on, `GPU` or `CPU`. Opening the card to find out takes a
/// moment, so it is found out once.
#[cfg(local_speech)]
pub fn local_device() -> String {
    static DEVICE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    DEVICE.get_or_init(|| egui_bench::speech::best_device().1).to_string()
}

#[cfg(not(local_speech))]
pub fn local_device() -> String {
    "not in this build".into()
}

#[cfg(local_speech)]
fn models_dir() -> std::path::PathBuf {
    agentbench_server::home().join("models")
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
    Ok(Take {
        _stream: stream,
        heard,
        peak,
        rate,
        device: name,
        since: Instant::now(),
        endpoint: Endpoint::new(rate),
        partial_at: 0,
        partial_busy: Arc::new(AtomicBool::new(false)),
    })
}

fn tracing_free_warn(e: &str) {
    eprintln!("agentbench: microphone stream error: {e}");
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

#[cfg(local_speech)]
fn read_locally(
    settings: &Settings,
    pcm: &[f32],
    tx: &Sender<Event>,
    wake: &(impl Fn() + Send + Sync + 'static),
    loaded: &Loaded,
    quiet: bool,
) -> Result<String, String> {
    let mut slot = loaded.lock().unwrap_or_else(|e| e.into_inner());
    let t = slot.get_or_insert_with(|| egui_bench::speech::Transcriber::new(models_dir()));
    let id = settings.local_model();
    if !t.installed(id) {
        t.download(id, &|p| {
            let of = if p.total_mb > 0 { format!(" of {} MB", p.total_mb) } else { String::new() };
            let _ = tx.send(Event::Status(format!("downloading {}: {} MB{of}", p.name, p.got_mb)));
            wake();
        })?;
    }
    let _ = tx.send(Event::Status("loading the speech model".into()));
    wake();
    t.load(id)?;
    if !quiet {
        let _ = tx.send(Event::Status("reading".into()));
        wake();
    }
    t.transcribe(pcm)
}

#[cfg(not(local_speech))]
fn read_locally(
    _: &Settings,
    _: &[f32],
    _: &Sender<Event>,
    _: &(impl Fn() + Send + Sync + 'static),
    _: &Loaded,
    _: bool,
) -> Result<String, String> {
    Err("this build has no local speech model; choose a speech server in settings".into())
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

    /// Needs a model on disc: `AGENTBENCH_TEST_MODEL=~/.local/share/waveshark/models/whisper-base.en`
    /// and a recording: `AGENTBENCH_TEST_WAV=said.wav` (16 bit mono). Prints what it read.
    #[cfg(local_speech)]
    #[test]
    #[ignore = "needs a downloaded model and a recording"]
    fn whisper_reads_a_recording() {
        let (Some(dir), Some(wav)) =
            (std::env::var_os("AGENTBENCH_TEST_MODEL"), std::env::var_os("AGENTBENCH_TEST_WAV"))
        else {
            panic!("set AGENTBENCH_TEST_MODEL and AGENTBENCH_TEST_WAV");
        };
        let bytes = std::fs::read(wav).unwrap();
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let pcm: Vec<f32> = bytes[44..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
            .collect();
        let pcm = super::super::to_whisper_rate(&pcm, rate);
        let files = egui_bench::speech::Files::in_dir(std::path::Path::new(&dir)).unwrap();
        let (device, on) = egui_bench::speech::best_device();
        let mut w = egui_bench::speech::Whisper::load(&files, device).unwrap();
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
