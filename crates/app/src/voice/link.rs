use super::duplex::{FRAME_BYTES, Jitter};
use agent_voice::voice::audio::{DUPLEX_FRAME_SAMPLES, DUPLEX_RATE, Resampler, from_pcm16};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

pub const BACKOFF_START: Duration = Duration::from_millis(250);
pub const BACKOFF_MAX: Duration = Duration::from_secs(8);
const SPEAKING_HOLD_MS: u64 = 400;
const SPEAKING_RMS: f32 = 0.01;
const READY_WAIT: Duration = Duration::from_secs(60);

pub struct Options {
    pub url: String,
    pub key: String,
    pub start: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Ready,
    Text(String),
    Said { text: String, unsaid: u64 },
    Refused(String),
    Lost { why: String, retry_in: Duration },
}

enum Cmd {
    Audio(Vec<u8>),
    Control(Value),
    Stop,
}

struct Shared {
    jitter: Mutex<Jitter>,
    epoch: Instant,
    voiced_at: AtomicU64,
    connected: AtomicBool,
    unmuted: AtomicBool,
    muted: AtomicU64,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    fn pull(&self) -> Vec<f32> {
        let frame = self.jitter.lock().unwrap_or_else(|e| e.into_inner()).pull();
        let rms = (frame.iter().map(|v| v * v).sum::<f32>() / frame.len().max(1) as f32).sqrt();
        if rms > SPEAKING_RMS {
            self.voiced_at.store(self.now_ms(), Ordering::Relaxed);
        }
        frame
    }
}

enum Player {
    #[allow(dead_code)]
    Device(cpal::Stream),
    Manual,
}

pub struct Link {
    cmd: Sender<Cmd>,
    events: Receiver<Event>,
    shared: Arc<Shared>,
    player: Player,
}

pub fn ws_url(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    if b.ends_with("/v1/duplex") && (b.starts_with("ws://") || b.starts_with("wss://")) {
        return b.to_string();
    }
    let b = b.strip_suffix("/v1/duplex").unwrap_or(b);
    let b = b.strip_suffix("/v1").unwrap_or(b);
    let b = b.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let b = if b.starts_with("ws://") || b.starts_with("wss://") { b } else { format!("ws://{b}") };
    format!("{b}/v1/duplex")
}

impl Link {
    pub fn open(options: Options, manual: bool, wake: impl Fn() + Send + Sync + 'static) -> Result<Link, String> {
        let shared = Arc::new(Shared {
            jitter: Mutex::new(Jitter::default()),
            epoch: Instant::now(),
            voiced_at: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            unmuted: AtomicBool::new(false),
            muted: AtomicU64::new(0),
        });
        let player = if manual { Player::Manual } else { Player::Device(play(shared.clone())?) };
        let (cmd, rx) = channel();
        let (tx, events) = channel();
        let worker = shared.clone();
        std::thread::Builder::new()
            .name("agentbench-duplex".into())
            .spawn(move || run(options, rx, tx, worker, wake))
            .map_err(|e| e.to_string())?;
        Ok(Link { cmd, events, shared, player })
    }

    pub fn send_frame(&self, frame: Vec<u8>) {
        debug_assert_eq!(frame.len(), FRAME_BYTES);
        let _ = self.cmd.send(Cmd::Audio(frame));
    }

    pub fn say(&self, text: &str) {
        let _ = self.cmd.send(Cmd::Control(json!({"type": "say", "text": text})));
    }

    pub fn interrupt(&self) {
        let _ = self.cmd.send(Cmd::Control(json!({"type": "interrupt"})));
    }

    pub fn unmute(&self) {
        self.shared.unmuted.store(true, Ordering::Relaxed);
    }

    pub fn mute(&self) {
        self.shared.unmuted.store(false, Ordering::Relaxed);
    }

    pub fn muted_frames(&self) -> u64 {
        self.shared.muted.load(Ordering::Relaxed)
    }

    pub fn connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }

    pub fn speaking(&self) -> bool {
        let at = self.shared.voiced_at.load(Ordering::Relaxed);
        at != 0 && self.shared.now_ms().saturating_sub(at) < SPEAKING_HOLD_MS
    }

    pub fn poll(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }

    pub fn manual(&self) -> bool {
        matches!(self.player, Player::Manual)
    }

    pub fn pull_frame(&self) -> Vec<f32> {
        self.shared.pull()
    }

    pub fn late_frames(&self) -> usize {
        self.shared.jitter.lock().unwrap_or_else(|e| e.into_inner()).late
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Stop);
    }
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

enum Ended {
    Stop,
    Lost(String, bool),
}

fn run(o: Options, rx: Receiver<Cmd>, tx: Sender<Event>, shared: Arc<Shared>, wake: impl Fn()) {
    let mut backoff = BACKOFF_START;
    loop {
        let (why, was_ready) = match session(&o, &rx, &tx, &shared, &wake) {
            Ended::Stop => return,
            Ended::Lost(why, ready) => (why, ready),
        };
        shared.connected.store(false, Ordering::Relaxed);
        shared.jitter.lock().unwrap_or_else(|e| e.into_inner()).clear();
        if was_ready {
            backoff = BACKOFF_START;
        }
        let _ = tx.send(Event::Lost { why, retry_in: backoff });
        wake();
        let until = Instant::now() + backoff;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(Cmd::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break,
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

fn connect(o: &Options) -> Result<Socket, String> {
    use tungstenite::client::IntoClientRequest;
    let mut request = o.url.as_str().into_client_request().map_err(|e| format!("{}: {e}", o.url))?;
    if !o.key.is_empty() {
        let v = format!("Bearer {}", o.key).parse().map_err(|_| "the key is not a valid header".to_string())?;
        request.headers_mut().insert("authorization", v);
    }
    let uri = request.uri();
    let host = uri.host().unwrap_or_default().trim_matches(['[', ']']).to_string();
    let port = uri.port_u16().unwrap_or(if uri.scheme_str() == Some("wss") { 443 } else { 80 });
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} has no address"))?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).map_err(|e| format!("{}: {e}", o.url))?;
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let connector = tungstenite::Connector::Rustls(Arc::new(crate::client::tls_config()));
    let (ws, _) = tungstenite::client_tls_with_config(request, tcp, None, Some(connector))
        .map_err(|e| format!("{}: {e}", o.url))?;
    Ok(ws)
}

fn set_timeout(ws: &Socket, t: Duration) {
    let tcp = match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => s.get_ref(),
        _ => return,
    };
    tcp.set_read_timeout(Some(t)).ok();
}

fn waiting(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
}

fn message_of(v: &Value) -> String {
    v["message"].as_str().or(v["error"]["message"].as_str()).unwrap_or("the duplex server refused").to_string()
}

fn session(o: &Options, rx: &Receiver<Cmd>, tx: &Sender<Event>, shared: &Shared, wake: &impl Fn()) -> Ended {
    let mut ws = match connect(o) {
        Ok(ws) => ws,
        Err(e) => return Ended::Lost(e, false),
    };
    if let Err(e) = ws.send(Message::Text(o.start.to_string().into())) {
        return Ended::Lost(e.to_string(), false);
    }
    set_timeout(&ws, Duration::from_millis(50));
    let deadline = Instant::now() + READY_WAIT;
    loop {
        match drain(rx, None) {
            Some(Ended::Stop) => return Ended::Stop,
            _ if Instant::now() > deadline => return Ended::Lost("no ready from the duplex server".into(), false),
            _ => {}
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(&t).unwrap_or_default();
                match v["type"].as_str() {
                    Some("ready") => break,
                    Some("error") => {
                        let _ = tx.send(Event::Refused(message_of(&v)));
                        wake();
                        return Ended::Lost(message_of(&v), false);
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(_)) => return Ended::Lost("the duplex server closed the session".into(), false),
            Ok(_) => {}
            Err(e) if waiting(&e) => {}
            Err(e) => return Ended::Lost(e.to_string(), false),
        }
    }
    shared.connected.store(true, Ordering::Relaxed);
    let _ = tx.send(Event::Ready);
    wake();
    set_timeout(&ws, Duration::from_millis(5));
    loop {
        if let Some(end) = drain(rx, Some(&mut ws)) {
            return match end {
                Ended::Lost(why, _) => Ended::Lost(why, true),
                stop => stop,
            };
        }
        match ws.read() {
            Ok(Message::Binary(b)) => {
                if shared.unmuted.load(Ordering::Relaxed) {
                    shared.jitter.lock().unwrap_or_else(|e| e.into_inner()).push(from_pcm16(&b));
                } else {
                    shared.muted.fetch_add(1, Ordering::Relaxed);
                }
            }
            Ok(Message::Text(t)) => {
                let v: Value = serde_json::from_str(&t).unwrap_or_default();
                let ev = match v["type"].as_str() {
                    Some("text") => Some(Event::Text(v["text"].as_str().unwrap_or_default().to_string())),
                    Some("said") => Some(Event::Said {
                        text: v["text"].as_str().unwrap_or_default().trim().to_string(),
                        unsaid: v["unsaid_tokens"].as_u64().unwrap_or(0),
                    }),
                    Some("error") => Some(Event::Refused(message_of(&v))),
                    Some("end") => return Ended::Lost(v["reason"].as_str().unwrap_or("the session ended").to_string(), true),
                    _ => None,
                };
                if let Some(ev) = ev {
                    let _ = tx.send(ev);
                    wake();
                }
            }
            Ok(Message::Close(_)) => return Ended::Lost("the duplex server closed the session".into(), true),
            Ok(_) => {}
            Err(e) if waiting(&e) => {}
            Err(e) => return Ended::Lost(e.to_string(), true),
        }
    }
}

fn drain(rx: &Receiver<Cmd>, mut ws: Option<&mut Socket>) -> Option<Ended> {
    loop {
        let cmd = match rx.try_recv() {
            Ok(c) => c,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => return Some(Ended::Stop),
        };
        let Some(ws) = ws.as_deref_mut() else {
            if matches!(cmd, Cmd::Stop) {
                return Some(Ended::Stop);
            }
            continue;
        };
        let sent = match cmd {
            Cmd::Audio(b) => ws.send(Message::Binary(b.into())),
            Cmd::Control(v) => ws.send(Message::Text(v.to_string().into())),
            Cmd::Stop => {
                let _ = ws.send(Message::Text(json!({"type": "stop"}).to_string().into()));
                let _ = ws.close(None);
                let _ = ws.flush();
                return Some(Ended::Stop);
            }
        };
        if let Err(e) = sent {
            return Some(Ended::Lost(e.to_string(), true));
        }
    }
}

fn play(shared: Arc<Shared>) -> Result<cpal::Stream, String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| "no sound output found; check the speakers or headphones".to_string())?;
    let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "speakers".into());
    let config = device.default_output_config().map_err(|e| format!("{name}: {e}"))?;
    let (rate, channels) = (config.sample_rate(), config.channels().max(1) as usize);
    let cfg = config.config();
    let err = |e: cpal::Error| eprintln!("agentbench: duplex output error: {e}");
    macro_rules! build {
        ($t:ty) => {{
            let shared = shared.clone();
            let mut feed = Feed::new(rate);
            device.build_output_stream(
                cfg,
                move |data: &mut [$t], _| {
                    let n = data.len() / channels;
                    let played = feed.take(n, &shared);
                    for (frame, v) in data.chunks_mut(channels).zip(&played) {
                        for out in frame {
                            *out = <$t as cpal::Sample>::from_sample(*v);
                        }
                    }
                    super::echo::PLAYED.push(&played, rate);
                },
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
    Ok(stream)
}

struct Feed {
    resample: Resampler,
    ready: VecDeque<f32>,
    scratch: Vec<f32>,
}

impl Feed {
    fn new(rate: u32) -> Feed {
        Feed { resample: Resampler::new(DUPLEX_RATE, rate), ready: VecDeque::new(), scratch: Vec::new() }
    }

    fn take(&mut self, n: usize, shared: &Shared) -> Vec<f32> {
        while self.ready.len() < n {
            let frame = shared.pull();
            debug_assert_eq!(frame.len(), DUPLEX_FRAME_SAMPLES);
            self.scratch.clear();
            self.resample.push(&frame, &mut self.scratch);
            self.ready.extend(self.scratch.iter().copied());
        }
        self.ready.drain(..n).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn base_urls_become_the_duplex_socket() {
        assert_eq!(ws_url("http://gpu:8080/v1"), "ws://gpu:8080/v1/duplex");
        assert_eq!(ws_url("https://h/"), "wss://h/v1/duplex");
        assert_eq!(ws_url("10.0.0.2:9"), "ws://10.0.0.2:9/v1/duplex");
        assert_eq!(ws_url("ws://h:1/v1/duplex"), "ws://h:1/v1/duplex");
    }

    #[test]
    fn a_dropped_socket_reconnects_with_a_growing_backoff_and_starts_again() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("ws://{}/v1/duplex", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut starts = Vec::new();
            for (i, s) in listener.incoming().take(3).enumerate() {
                let mut ws = tungstenite::accept(s.unwrap()).unwrap();
                let Message::Text(t) = ws.read().unwrap() else { panic!() };
                starts.push((Instant::now(), serde_json::from_str::<Value>(&t).unwrap()));
                if i < 2 {
                    drop(ws);
                    continue;
                }
                ws.send(Message::Text(json!({"type": "ready"}).to_string().into())).unwrap();
                ws.send(Message::Binary(vec![0u8; FRAME_BYTES].into())).unwrap();
                let _ = ws.read();
            }
            starts
        });
        let start = json!({"type": "start", "mode": "relay"});
        let link = Link::open(Options { url, key: String::new(), start: start.clone() }, true, || {}).unwrap();
        let t0 = Instant::now();
        let mut events = Vec::new();
        while !events.contains(&Event::Ready) && t0.elapsed() < Duration::from_secs(10) {
            events.extend(link.poll());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(link.connected());
        let lost: Vec<Duration> = events
            .iter()
            .filter_map(|e| match e {
                Event::Lost { retry_in, .. } => Some(*retry_in),
                _ => None,
            })
            .collect();
        assert_eq!(lost, vec![BACKOFF_START, BACKOFF_START * 2]);
        drop(link);
        let starts = server.join().unwrap();
        assert_eq!(starts.len(), 3);
        assert!(starts.iter().all(|(_, v)| *v == start), "every connection starts the session again");
        assert!(starts[2].0 - starts[1].0 >= BACKOFF_START * 2 - Duration::from_millis(20));
    }
}
