//! The microphone and the two readers of what it heard.
//!
//! Everything slow happens off the UI thread: the model download, loading the weights, running
//! Whisper, and the request to a server. The window asks [`Voice::poll`] once a frame what has
//! finished.

use super::{Endpoint, Heard, MAX_SECONDS, MIN_PEAK, MIN_SECONDS, Mode, RATE, Said, Scores, Settings};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use web_time::Instant;

/// What the worker has to say.
pub enum Event {
    /// Progress worth showing while the user waits.
    #[allow(dead_code)]
    Status(String),
    /// What was said.
    Text(String),
    /// A reading of take number `.0` while it is still going.
    Partial(u64, String),
    Ignored,
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
    _stream: Box<dyn std::any::Any>,
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
    open: bool,
    heard_speech: bool,
    echo: Option<Box<super::echo::Canceller>>,
    cancelled: usize,
    tapped: usize,
}

type Loaded = ();

enum Reason {
    Partial(u64, Arc<AtomicBool>),
    Open(f32),
    Take,
}

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
    shown: Option<(f32, f32)>,
    quiet_stop: Option<f32>,
    speech_started: bool,
    tap: Option<(Vec<f32>, u32)>,
    discard: bool,
    recording: bool,
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
            shown: None,
            quiet_stop: None,
            speech_started: false,
            tap: None,
            discard: false,
            recording: false,
        }
    }
}

impl Voice {
    pub fn listening(&self) -> bool {
        self.shown.is_some() || matches!(self.state, State::Listening(_))
    }

    #[doc(hidden)]
    pub fn show_recording(&mut self, seconds: f32, level: f32) {
        self.shown = Some((seconds, level));
    }

    #[doc(hidden)]
    pub fn stage_take(&mut self) {
        self.show_recording(0.0, 0.0);
    }

    pub fn staged(&self) -> bool {
        self.shown.is_some()
    }

    pub fn reading(&self) -> bool {
        matches!(self.state, State::Reading)
    }

    pub fn busy(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    /// Seconds since the microphone opened.
    pub fn closing_in(&self, settings: &Settings) -> Option<f32> {
        if settings.hold_to_talk {
            return None;
        }
        let State::Listening(t) = &self.state else {
            return None;
        };
        let quiet = t.endpoint.quiet_seconds();
        (t.endpoint.spoke() && quiet > super::COUNTDOWN_AFTER)
            .then(|| (settings.hang_secs - quiet).max(0.0))
    }

    #[doc(hidden)]
    pub fn fake_failure(&mut self, why: &str) {
        let _ = self.tx.send(Event::Failed(why.into()));
    }

    #[doc(hidden)]
    pub fn fake_quiet_stop(&mut self, hang: f32) {
        self.quiet_stop = Some(hang);
    }

    pub fn take_quiet_stop(&mut self) -> Option<f32> {
        self.quiet_stop.take()
    }

    pub fn take_speech_start(&mut self) -> bool {
        std::mem::take(&mut self.speech_started)
    }

    pub fn set_tap(&mut self, on: bool) {
        if on != self.tap.is_some() {
            self.tap = on.then(|| (Vec::new(), RATE));
        }
    }

    pub fn set_discard(&mut self, on: bool) {
        self.discard = on;
    }

    pub fn take_tapped(&mut self) -> (Vec<f32>, u32) {
        match &mut self.tap {
            Some((pcm, rate)) => (std::mem::take(pcm), *rate),
            None => (Vec::new(), RATE),
        }
    }

    pub fn open(&self) -> bool {
        matches!(&self.state, State::Listening(t) if t.open)
    }

    pub fn in_speech(&self) -> bool {
        matches!(&self.state, State::Listening(t) if t.endpoint.spoke())
    }

    pub fn start_open(
        &mut self,
        wake: impl Fn() + Send + Sync + 'static,
        device: &str,
    ) -> Result<(), String> {
        self.start_on(wake, device)?;
        super::echo::PLAYED.clear();
        if let State::Listening(t) = &mut self.state {
            t.open = true;
            t.endpoint = Endpoint::open(t.rate);
        }
        Ok(())
    }

    #[doc(hidden)]
    pub fn open_fake(&mut self) -> super::Mic {
        let heard: super::Mic = Arc::new(Mutex::new(Vec::new()));
        self.shown = None;
        self.state = State::Listening(Box::new(Take {
            _stream: Box::new(()),
            heard: heard.clone(),
            peak: Arc::new(AtomicU32::new(0)),
            rate: RATE,
            device: "test microphone".into(),
            since: Instant::now(),
            endpoint: Endpoint::open(RATE),
            partial_at: 0,
            partial_busy: Arc::new(AtomicBool::new(false)),
            open: true,
            heard_speech: false,
            echo: None,
            cancelled: 0,
            tapped: 0,
        }));
        self.take_id += 1;
        heard
    }

    pub fn seconds(&self) -> f32 {
        if let Some((s, _)) = self.shown {
            return s;
        }
        match &self.state {
            State::Listening(t) => t.since.elapsed().as_secs_f32(),
            _ => 0.0,
        }
    }

    /// Loudest recent sample, 0 to 1, for a meter. Reading it resets it.
    pub fn level(&self) -> f32 {
        if let Some((_, l)) = self.shown {
            return l;
        }
        match &self.state {
            State::Listening(t) => f32::from_bits(t.peak.swap(0, Ordering::Relaxed)),
            _ => 0.0,
        }
    }

    /// What the microphone is, once it is open.
    pub fn device(&self) -> &str {
        if self.shown.is_some() {
            return "test microphone";
        }
        match &self.state {
            State::Listening(t) => &t.device,
            _ => "",
        }
    }

    /// Open the microphone.
    pub fn start(&mut self, wake: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
        self.start_on(wake, "")
    }

    pub fn start_on(
        &mut self,
        wake: impl Fn() + Send + Sync + 'static,
        device: &str,
    ) -> Result<(), String> {
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
        self.state = State::Listening(Box::new(open(device)?));
        self.take_id += 1;
        self.status.clear();
        Ok(())
    }

    pub fn start_recording(
        &mut self,
        wake: impl Fn() + Send + Sync + 'static,
        device: &str,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("the microphone is in use; stop hands free first".into());
        }
        self.start_on(wake, device)?;
        self.recording = true;
        Ok(())
    }

    pub fn recording(&self) -> bool {
        self.recording && self.listening()
    }

    #[doc(hidden)]
    pub fn record_fake(&mut self) -> super::Mic {
        let mic = self.open_fake();
        if let State::Listening(t) = &mut self.state {
            t.open = false;
        }
        self.recording = true;
        mic
    }

    /// Called once a frame while the microphone is open. Listens for the end of the speech and
    /// closes the take by itself, and every couple of seconds starts a reading of what has been
    /// said so far so the words can appear while you are still talking.
    pub fn tick(&mut self, settings: &Settings, wake: impl Fn() + Send + Sync + Clone + 'static) {
        let State::Listening(take) = &mut self.state else {
            return;
        };
        if self.recording {
            return;
        }
        let (heard, samples) = {
            let mut heard = take.heard.lock().unwrap_or_else(|e| e.into_inner());
            if take.open && settings.echo_cancel {
                let rate = take.rate;
                let echo = take.echo.get_or_insert_with(|| Box::new(super::echo::Canceller::new(rate)));
                let from = take.cancelled.min(heard.len());
                let fresh = &mut heard[from..];
                let far = super::echo::PLAYED.take(fresh.len(), rate);
                let mut out = Vec::with_capacity(fresh.len());
                echo.process(fresh, &far, &mut out);
                fresh.copy_from_slice(&out);
                take.cancelled = heard.len();
            } else {
                take.echo = None;
                take.cancelled = heard.len();
            }
            take.endpoint.set_hang(settings.hang_secs);
            if take.open
                && let Some((tap, rate)) = &mut self.tap
            {
                let from = take.tapped.min(heard.len());
                if tap.len() < take.rate as usize * 10 {
                    tap.extend_from_slice(&heard[from..]);
                }
                *rate = take.rate;
            }
            take.tapped = heard.len();
            let said = take.endpoint.feed(&heard);
            if take.open && !take.endpoint.spoke() {
                let keep = take.rate as usize * super::LEAD_SECONDS;
                let drop = take.endpoint.scanned().saturating_sub(keep);
                if drop > keep {
                    heard.drain(..drop);
                    take.endpoint.forget(drop);
                    take.cancelled = take.cancelled.saturating_sub(drop);
                    take.tapped = take.tapped.saturating_sub(drop);
                }
            }
            (said, heard.len())
        };
        if take.open && !take.heard_speech && take.endpoint.spoke() {
            take.heard_speech = true;
            self.speech_started = true;
        }
        match heard {
            Heard::Done | Heard::GaveUp if settings.hold_to_talk && !take.open => {}
            Heard::Done => {
                if !take.open {
                    self.quiet_stop = Some(take.endpoint.hang());
                }
                self.finish(settings, wake)
            }
            Heard::GaveUp => {
                self.state = State::Idle;
                let _ = self.tx.send(Event::Failed("no speech was heard".into()));
            }
            Heard::Speaking | Heard::Pause if take.open => {}
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
                    self.read(pcm, settings, Reason::Partial(id, busy), wake);
                }
            }
            Heard::Waiting => {}
        }
    }

    /// Close the microphone without reading anything.
    pub fn cancel(&mut self) {
        self.recording = false;
        if self.listening() {
            self.state = State::Idle;
        }
    }

    pub fn take_raw(&mut self) -> Option<(Vec<f32>, u32)> {
        self.recording = false;
        let State::Listening(take) = std::mem::replace(&mut self.state, State::Idle) else {
            return None;
        };
        let heard = std::mem::take(&mut *take.heard.lock().unwrap_or_else(|e| e.into_inner()));
        Some((heard, take.rate))
    }

    /// Close the microphone and read what it heard.
    pub fn finish(&mut self, settings: &Settings, wake: impl Fn() + Send + Sync + 'static) {
        if let State::Listening(take) = &mut self.state
            && take.open
        {
            let rate = take.rate;
            let quiet = take.endpoint.quiet_seconds();
            let voiced = take.endpoint.voiced_seconds();
            let heard = std::mem::take(&mut *take.heard.lock().unwrap_or_else(|e| e.into_inner()));
            take.endpoint = Endpoint::open(rate);
            take.heard_speech = false;
            take.cancelled = 0;
            take.tapped = 0;
            if self.discard {
                return;
            }
            if voiced < settings.min_speech_secs {
                let _ = self.tx.send(Event::Ignored);
                return;
            }
            if let Some(pcm) = self.trimmed(heard, rate, quiet, settings, true) {
                self.read(pcm, settings, Reason::Open(voiced), wake);
            }
            return;
        }
        let State::Listening(take) = std::mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        let rate = take.rate;
        let quiet = take.endpoint.quiet_seconds();
        let heard = std::mem::take(&mut *take.heard.lock().unwrap_or_else(|e| e.into_inner()));
        drop(take);
        if let Some(pcm) = self.trimmed(heard, rate, quiet, settings, false) {
            self.state = State::Reading;
            self.status = "reading".into();
            self.read(pcm, settings, Reason::Take, wake);
        }
    }

    fn trimmed(
        &self,
        mut heard: Vec<f32>,
        rate: u32,
        quiet: f32,
        settings: &Settings,
        open: bool,
    ) -> Option<Vec<f32>> {
        // Keep a little of the silence the take ended on and drop the rest: Whisper invents words
        // for a long quiet tail.
        let tail = ((quiet - 0.3).max(0.0) * rate as f32) as usize;
        heard.truncate(heard.len().saturating_sub(tail));

        let seconds = heard.len() as f32 / rate.max(1) as f32;
        let peak = heard.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        let failed = if seconds < MIN_SECONDS {
            Some("too short to read; hold on a little longer")
        } else if peak < MIN_PEAK {
            Some("the microphone heard nothing; check it is not muted")
        } else {
            settings.problem()
        };
        if let Some(why) = failed {
            if !open || settings.problem().is_some() {
                let _ = self.tx.send(Event::Failed(why.into()));
            }
            return None;
        }
        Some(super::to_whisper_rate(&heard, rate))
    }

    pub fn read_take(
        &mut self,
        bytes: &[u8],
        settings: &Settings,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("already reading a take".into());
        }
        let (pcm, rate) = super::read_wav(bytes)?;
        let pcm = super::to_whisper_rate(&pcm, rate);
        if let Some(p) = settings.problem() {
            return Err(p.into());
        }
        self.state = State::Reading;
        self.status = "reading".into();
        self.shown = None;
        self.read(pcm, settings, Reason::Take, wake);
        Ok(())
    }

    /// Read `pcm` on a worker thread. A partial reading carries its take's id and busy flag and
    /// reports `Event::Partial`; the last one reports `Event::Text` or `Event::Failed`.
    fn read(
        &self,
        pcm: Vec<f32>,
        settings: &Settings,
        why: Reason,
        wake: impl Fn() + Send + Sync + 'static,
    ) {
        let tx = self.tx.clone();
        let settings = settings.clone();
        let loaded = self.loaded.clone();
        std::thread::Builder::new()
            .name("agentbench-speech".into())
            .spawn(move || {
                let quiet = matches!(why, Reason::Partial(..));
                let said = match settings.mode {
                    Mode::Server => read_on_server(&settings, &pcm),
                    Mode::Local => {
                        let _ = (&tx, &wake, &loaded, quiet);
                        Err("the agentbench server on this machine has not started yet".to_string())
                    }
                    Mode::Agentbench => Err("not connected to an agentbench server".to_string()),
                };
                match why {
                    Reason::Partial(id, busy) => {
                        busy.store(false, Ordering::Relaxed);
                        // A partial that fails says nothing: the last reading reports it.
                        if let Ok((t, _)) = said {
                            let _ = tx.send(Event::Partial(id, t));
                        }
                    }
                    Reason::Open(voiced) => {
                        let _ = tx.send(match said {
                            Ok((t, scores)) if super::sounds_like_noise(&t, voiced, scores, &settings) => {
                                Event::Ignored
                            }
                            Ok((t, _)) => Event::Text(t),
                            Err(e) => Event::Failed(e),
                        });
                    }
                    Reason::Take => {
                        let _ = tx.send(match said {
                            Ok((t, _)) => Event::Text(t),
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
                    if self.reading() {
                        self.state = State::Idle;
                    }
                    self.status.clear();
                    out.push(Said::Final(t));
                }
                Event::Ignored => out.push(Said::Ignored),
                Event::Failed(e) => {
                    if self.reading() {
                        self.state = State::Idle;
                    }
                    self.status.clear();
                    out.push(Said::Failed(e));
                }
            }
        }
        out
    }

    /// Whether the files of `model` are already on this machine.
    pub fn installed(model: &str) -> bool {
        #[cfg(not(target_os = "android"))]
        return agent_voice::voice::models::installed(&agentbench_server::home().join("models"), model);
        #[cfg(target_os = "android")]
        return {
            let _ = model;
            false
        };
    }
}

pub fn local_device() -> String {
    "the agentbench server on this machine".into()
}

/// Open the default microphone and start collecting mono samples.
fn open(device: &str) -> Result<Take, String> {
    let _ = device;
    #[cfg(target_os = "android")]
    {
        let heard = Arc::new(Mutex::new(Vec::with_capacity(RATE as usize * 10)));
        let peak = Arc::new(AtomicU32::new(0));
        let stream = super::android::open_voice_stream(
            RATE as usize * MAX_SECONDS,
            heard.clone(),
            peak.clone(),
        )?;
        let rate = stream.rate;
        Ok(Take {
            _stream: Box::new(stream),
            heard,
            peak,
            rate,
            device: "microphone (voice recognition)".into(),
            since: Instant::now(),
            endpoint: Endpoint::new(rate),
            partial_at: 0,
            partial_busy: Arc::new(AtomicBool::new(false)),
            open: false,
            heard_speech: false,
            echo: None,
            cancelled: 0,
            tapped: 0,
        })
    }
    #[cfg(not(target_os = "android"))]
    open_cpal(device)
}

pub fn input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|d| {
            d.filter_map(|d| d.description().ok().map(|d| d.name().to_string())).collect()
        })
        .unwrap_or_default()
}

#[cfg_attr(target_os = "android", allow(dead_code))]
fn open_cpal(wanted: &str) -> Result<Take, String> {
    let host = cpal::default_host();
    let named = (!wanted.is_empty())
        .then(|| {
            host.input_devices().ok()?.find(|d| {
                d.description().is_ok_and(|d| d.name() == wanted)
            })
        })
        .flatten();
    let device = named
        .or_else(|| host.default_input_device())
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
        _stream: Box::new(stream),
        heard,
        peak,
        rate,
        device: name,
        since: Instant::now(),
        endpoint: Endpoint::new(rate),
        partial_at: 0,
        partial_busy: Arc::new(AtomicBool::new(false)),
        open: false,
        heard_speech: false,
        echo: None,
        cancelled: 0,
        tapped: 0,
    })
}

fn tracing_free_warn(e: &str) {
    eprintln!("agentbench: microphone stream error: {e}");
}

/// One callback's worth of samples, mixed to mono and appended until the take is as long as allowed.
pub(super) fn push<T: cpal::SizedSample>(
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

/// Post the take as a WAV to an OpenAI compatible transcription endpoint.
fn read_on_server(settings: &Settings, pcm: &[f32]) -> Result<(String, Option<Scores>), String> {
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
            Ok(v) => (
                v.get("text").and_then(|t| t.as_str()).unwrap_or_default().trim().to_string(),
                scores_of(&v),
            ),
            Err(_) => (body.trim().to_string(), None),
        })
    })
}

fn scores_of(v: &serde_json::Value) -> Option<Scores> {
    let num = |k: &str| v.get(k).and_then(|n| n.as_f64()).map(|n| n as f32);
    Some(Scores { no_speech: num("no_speech_prob")?, avg_logprob: num("avg_logprob")? })
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
        assert_eq!(said, ("hello there".to_string(), None));
        let sent = seen.join().unwrap();
        assert!(sent.starts_with("POST /v1/audio/transcriptions"), "{sent:.200}");
        assert!(sent.to_lowercase().contains("authorization: bearer sekret"));
        assert!(sent.contains("multipart/form-data"));
        assert!(sent.contains("name=\"model\""));
        assert!(sent.contains("RIFF"));
    }

    #[test]
    fn a_server_that_scores_the_reading_hands_the_scores_on() {
        let (url, _) = serve(r#"{"text":"you","no_speech_prob":0.9,"avg_logprob":-1.8}"#);
        let settings = Settings { mode: Mode::Server, url, ..Default::default() };
        let (text, scores) = read_on_server(&settings, &vec![0.1f32; 16_000]).unwrap();
        assert_eq!(text, "you");
        assert_eq!(scores, Some(Scores { no_speech: 0.9, avg_logprob: -1.8 }));
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

    fn take_with(pcm: Vec<f32>) -> Voice {
        Voice {
            state: State::Listening(Box::new(Take {
                _stream: Box::new(()),
                heard: Arc::new(Mutex::new(pcm)),
                peak: Arc::new(AtomicU32::new(0)),
                rate: RATE,
                device: "test".into(),
                since: Instant::now(),
                endpoint: Endpoint::new(RATE),
                partial_at: 0,
                partial_busy: Arc::new(AtomicBool::new(false)),
                open: false,
                heard_speech: false,
            echo: None,
            cancelled: 0,
            tapped: 0,
            })),
            ..Voice::default()
        }
    }

    fn speech_then_quiet(quiet: f32) -> Vec<f32> {
        let mut pcm = vec![0.001f32; RATE as usize];
        pcm.extend((0..RATE as usize * 2).map(|i| 0.3 * (i as f32 * 0.1).sin()));
        pcm.extend(vec![0.001f32; (RATE as f32 * quiet) as usize]);
        pcm
    }

    #[test]
    fn an_open_microphone_reads_each_take_and_stays_open() {
        let mut v = Voice::default();
        let mic = v.open_fake();
        let settings = Settings {
            mode: Mode::Server,
            url: "http://127.0.0.1:9/v1".into(),
            hang_secs: 3.0,
            ..Default::default()
        };
        mic.lock().unwrap().extend(vec![0.001f32; RATE as usize * 20]);
        v.tick(&settings, || {});
        assert!(v.open() && !v.in_speech() && !v.take_speech_start());
        assert!(mic.lock().unwrap().len() <= RATE as usize * 2, "room before speech is dropped");
        mic.lock().unwrap().extend(speech_then_quiet(0.5));
        v.tick(&settings, || {});
        assert!(v.in_speech());
        assert!(v.take_speech_start(), "speech starting is reported once");
        assert!(!v.take_speech_start());
        mic.lock().unwrap().extend(vec![0.001f32; RATE as usize * 4]);
        v.tick(&settings, || {});
        assert!(v.open() && !v.in_speech(), "the take ended and the microphone stays open");
        assert!(mic.lock().unwrap().is_empty());
        assert!(v.take_quiet_stop().is_none());
        let mut said = Vec::new();
        for _ in 0..100 {
            said.extend(v.poll());
            if !said.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(matches!(said.as_slice(), [Said::Failed(_)]), "{said:?}");
        assert!(v.open(), "a failed reading does not close it");
        v.cancel();
        assert!(!v.open() && !v.listening());
    }

    #[test]
    fn the_tap_hands_on_every_open_sample_once_even_when_room_is_dropped() {
        let mut v = Voice::default();
        let mic = v.open_fake();
        let settings = Settings { mode: Mode::Server, url: "http://127.0.0.1:9/v1".into(), ..Default::default() };
        v.tick(&settings, || {});
        assert!(v.take_tapped().0.is_empty(), "nothing is kept until the tap is on");
        v.set_tap(true);
        let mut fed = 0;
        let mut got = 0;
        for _ in 0..30 {
            mic.lock().unwrap().extend(vec![0.001f32; RATE as usize]);
            fed += RATE as usize;
            v.tick(&settings, || {});
            let (pcm, rate) = v.take_tapped();
            assert_eq!(rate, RATE);
            got += pcm.len();
        }
        assert!(mic.lock().unwrap().len() < fed, "room before speech was dropped");
        assert_eq!(got, fed);
    }

    #[test]
    fn with_echo_cancelling_on_the_reply_heard_through_speakers_is_not_speech() {
        let (pcm, rate) = super::super::read_wav(include_bytes!("../../tests/fixtures/voice/speech.wav")).unwrap();
        let reply: Vec<f32> = super::super::to_whisper_rate(&pcm, rate).iter().map(|v| v * 0.5).collect();
        let delay = RATE as usize / 40;
        let run = |cancel: bool| {
            super::super::echo::PLAYED.clear();
            let mut v = Voice::default();
            let mic = v.open_fake();
            let settings = Settings { mode: Mode::Server, url: "http://127.0.0.1:9/v1".into(), echo_cancel: cancel, ..Default::default() };
            let mut spoke = false;
            let block = 320;
            let mut warm = vec![0.0f32; RATE as usize * 6];
            warm.extend(reply.iter().chain(&reply).chain(&reply));
            for (i, chunk) in warm.chunks(block).enumerate() {
                super::super::echo::PLAYED.push(chunk, RATE);
                let at = i * block;
                let heard: Vec<f32> = (at..at + chunk.len())
                    .map(|n| 0.0005 + if n >= delay { 0.4 * warm[n - delay] } else { 0.0 })
                    .collect();
                mic.lock().unwrap().extend(heard);
                v.tick(&settings, || {});
                spoke |= v.in_speech() && at > RATE as usize * 6;
            }
            spoke
        };
        assert!(run(false), "without cancelling the echo passes for speech");
        assert!(!run(true), "with it the echo is taken out");
    }

    #[test]
    fn a_held_take_stays_open_through_twenty_seconds_of_silence() {
        let mut v = take_with(speech_then_quiet(20.0));
        let held = Settings { mode: Mode::Server, hold_to_talk: true, ..Default::default() };
        v.tick(&held, || {});
        assert!(v.listening());
        assert!(v.take_quiet_stop().is_none());
        assert!(v.closing_in(&held).is_none());
    }

    #[test]
    fn a_toggle_take_ends_on_quiet_and_says_so() {
        let mut v = take_with(speech_then_quiet(4.0));
        let toggle = Settings { mode: Mode::Server, hold_to_talk: false, hang_secs: 3.0, ..Default::default() };
        v.tick(&toggle, || {});
        assert!(!v.listening());
        assert_eq!(v.take_quiet_stop(), Some(3.0));
    }

    #[test]
    fn the_countdown_shows_once_quiet_passes_a_second_and_a_half() {
        let toggle = Settings { mode: Mode::Server, hold_to_talk: false, hang_secs: 3.0, ..Default::default() };
        let mut v = take_with(speech_then_quiet(1.0));
        v.tick(&toggle, || {});
        assert!(v.closing_in(&toggle).is_none());
        let mut v = take_with(speech_then_quiet(2.0));
        v.tick(&toggle, || {});
        let left = v.closing_in(&toggle).unwrap();
        assert!((left - 1.0).abs() < 0.2, "{left}");
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
}
