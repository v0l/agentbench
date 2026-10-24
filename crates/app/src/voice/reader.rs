//! Reading a reply aloud, with the agentbench server's voice.
//!
//! The whole reply is posted to the server's `/v1/audio/speech` as one request for a stream of
//! raw samples. The server makes it a sentence at a time and sends each as soon as it is made, so
//! the first words play while the rest are still being made. The samples are resampled to whatever
//! the sound card wants and queued for the output stream as they arrive. Everything slow happens
//! off the UI thread, and [`Reader::tick`] is called once a frame to tidy up.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// How much is queued before playing starts, so a slow second sentence does not make the first
/// stutter. A quarter of a second.
const PREBUFFER_DIVISOR: usize = 4;

/// What the worker shares with the sound card's callback.
struct Shared {
    queue: Mutex<VecDeque<f32>>,
    /// The worker has nothing more to add.
    done: AtomicBool,
    /// Somebody pressed stop, or started something else.
    stop: AtomicBool,
    /// Samples to queue before the first one is played.
    prebuffer: AtomicUsize,
    /// Playing has begun, so a gap later is a gap and not the wait for the start.
    started: AtomicBool,
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
            prebuffer: AtomicUsize::new(usize::MAX),
            started: AtomicBool::new(false),
        });
        let (stream, rate) = open(shared.clone())?;
        shared.prebuffer.store(rate as usize / PREBUFFER_DIVISOR, Ordering::Relaxed);
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

/// Linear resampling of a stream, with the last sample and the fractional position carried from
/// one chunk to the next so the joins are the same as if it had come in one piece.
struct Resample {
    step: f64,
    pos: f64,
    carry: Vec<f32>,
}

impl Resample {
    fn new(from: u32, to: u32) -> Resample {
        Resample { step: from as f64 / to.max(1) as f64, pos: 0.0, carry: Vec::new() }
    }

    fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }
        let mut c = std::mem::take(&mut self.carry);
        c.extend_from_slice(input);
        while self.pos + 1.0 < c.len() as f64 {
            let lo = self.pos as usize;
            let t = (self.pos - lo as f64) as f32;
            out.push(c[lo] * (1.0 - t) + c[lo + 1] * t);
            self.pos += self.step;
        }
        let used = (self.pos as usize).min(c.len() - 1);
        self.carry = c.split_off(used);
        self.pos -= used as f64;
    }
}

/// Little endian 16 bit samples from a stream that may split a sample between two chunks.
#[derive(Default)]
struct Pcm {
    odd: Option<u8>,
}

impl Pcm {
    fn push(&mut self, bytes: &[u8], out: &mut Vec<f32>) {
        let mut bytes = bytes;
        if let Some(low) = self.odd.take() {
            let Some((&high, rest)) = bytes.split_first() else {
                self.odd = Some(low);
                return;
            };
            out.push(i16::from_le_bytes([low, high]) as f32 / 32768.0);
            bytes = rest;
        }
        let pairs = bytes.chunks_exact(2);
        self.odd = pairs.remainder().first().copied();
        out.extend(pairs.map(|p| i16::from_le_bytes([p[0], p[1]]) as f32 / 32768.0));
    }
}

/// Ask for the speech and queue it as it arrives. Says why on the first failure.
async fn produce(
    text: &str,
    from: &Source,
    rate: u32,
    shared: &Shared,
    errors: &Sender<String>,
    wake: &(dyn Fn() + Send + Sync),
) {
    let result: Result<(), String> = async {
        let mut resp = request(from, text).await?;
        // The server says how it is sending. A server that sends a whole WAV anyway (an older
        // one, or somebody else's) is played when all of it has come.
        let streamed = resp.headers().get("x-sample-rate").and_then(|v| v.to_str().ok());
        let in_rate = streamed.and_then(|v| v.trim().parse::<u32>().ok());
        let Some(in_rate) = in_rate else {
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            let (pcm, from_rate) = egui_bench::voice::audio::read_wav(&bytes)?;
            let pcm = egui_bench::voice::audio::resample(&pcm, from_rate, rate);
            shared.queue.lock().unwrap_or_else(|e| e.into_inner()).extend(pcm);
            return Ok(());
        };
        let (mut pcm, mut resample) = (Pcm::default(), Resample::new(in_rate, rate));
        let (mut samples, mut out) = (Vec::new(), Vec::new());
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("the voice stopped: {e}"))? {
            if shared.stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            samples.clear();
            out.clear();
            pcm.push(&chunk, &mut samples);
            resample.push(&samples, &mut out);
            shared.queue.lock().unwrap_or_else(|e| e.into_inner()).extend(out.iter().copied());
            wake();
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        let _ = errors.send(e);
    }
}

/// Post the text and wait for the server to start answering: the headers come once the first
/// sentence has been made, or once it is known the voice cannot be had.
async fn request(from: &Source, text: &str) -> Result<reqwest::Response, String> {
    let url = format!("{}/v1/audio/speech", from.url.trim().trim_end_matches('/'));
    let url = if url.starts_with("http://") || url.starts_with("https://") {
        url
    } else {
        format!("http://{url}")
    };
    let mut body =
        serde_json::json!({ "model": "kokoro", "input": text, "response_format": "pcm" });
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
        .map_err(|e| format!("could not reach the server at {url}: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    if matches!(status.as_u16(), 404 | 405) {
        return Err("this agentbench server cannot read aloud yet: update and restart it".into());
    }
    if status.as_u16() == 401 {
        return Err("the server refused the token, so it cannot read aloud".into());
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    let said = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_string());
    Err(format!("the server could not read that aloud: {status}: {said}"))
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
    if !shared.started.load(Ordering::Relaxed) {
        if queue.len() < shared.prebuffer.load(Ordering::Relaxed)
            && !shared.done.load(Ordering::Relaxed)
        {
            data.fill(T::from_sample(0.0));
            return;
        }
        shared.started.store(true, Ordering::Relaxed);
    }
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

    /// A one request HTTP server. It answers with `chunks`, each sent on its own with a pause
    /// between so they arrive apart, and hands back what it was sent.
    fn serve(
        status: &'static str,
        headers: &'static str,
        chunks: Vec<Vec<u8>>,
    ) -> (String, std::thread::JoinHandle<String>) {
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
                "HTTP/1.1 {status}\r\n{headers}transfer-encoding: chunked\r\nconnection: close\r\n\r\n"
            );
            s.write_all(head.as_bytes()).unwrap();
            for c in chunks {
                s.write_all(format!("{:x}\r\n", c.len()).as_bytes()).unwrap();
                s.write_all(&c).unwrap();
                s.write_all(b"\r\n").unwrap();
                s.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            s.write_all(b"0\r\n\r\n").unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        (url, h)
    }

    fn shared() -> Shared {
        Shared {
            queue: Mutex::new(VecDeque::new()),
            done: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            prebuffer: AtomicUsize::new(0),
            started: AtomicBool::new(false),
        }
    }

    fn run(from: &Source, shared: &Shared, rate: u32) -> Vec<String> {
        let (tx, rx) = channel();
        crate::runtime().block_on(produce("Hello there.", from, rate, shared, &tx, &|| {}));
        rx.try_iter().collect()
    }

    fn le(samples: &[i16]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    #[test]
    fn the_text_is_posted_for_a_stream_with_the_token_and_voice() {
        let (url, sent) = serve("200 OK", "x-sample-rate: 24000\r\n", vec![le(&[0, 100])]);
        let from = Source { url, token: "secret".into(), voice: "af_bella".into() };
        let s = shared();
        assert!(run(&from, &s, 24_000).is_empty());
        let sent = sent.join().unwrap();
        assert!(sent.starts_with("POST /v1/audio/speech"), "{sent:.200}");
        assert!(sent.to_lowercase().contains("authorization: bearer secret"), "{sent:.300}");
        assert!(sent.contains("\"input\":\"Hello there.\""), "{sent}");
        assert!(sent.contains("\"voice\":\"af_bella\""), "{sent}");
        assert!(sent.contains("\"response_format\":\"pcm\""), "asks for the stream: {sent}");
    }

    #[test]
    fn samples_are_queued_as_each_chunk_arrives_even_when_one_splits_a_sample() {
        // 4 samples sent as 3 chunks, the second cutting the second sample in half.
        let all = le(&[1000, -1000, 2000, -2000]);
        let chunks = vec![all[..3].to_vec(), all[3..6].to_vec(), all[6..].to_vec()];
        let (url, _) = serve("200 OK", "x-sample-rate: 24000\r\n", chunks);
        let s = shared();
        let errors = run(&Source { url, ..Default::default() }, &s, 24_000);
        assert!(errors.is_empty(), "{errors:?}");
        let got: Vec<f32> = s.queue.lock().unwrap().iter().copied().collect();
        // The resampler holds back the last sample until it has a neighbour to lean on.
        assert!(got.len() >= 3, "{got:?}");
        assert!((got[0] - 1000.0 / 32768.0).abs() < 1e-4, "{got:?}");
        assert!((got[1] + 1000.0 / 32768.0).abs() < 1e-4, "{got:?}");
        assert!((got[2] - 2000.0 / 32768.0).abs() < 1e-4, "{got:?}");
    }

    #[test]
    fn a_server_that_sends_a_whole_wav_is_still_played() {
        let wav = egui_bench::voice::audio::wav_bytes(&[0.0, 0.25, -0.25, 0.5], 24_000);
        let (url, _) = serve("200 OK", "content-type: audio/wav\r\n", vec![wav]);
        let s = shared();
        let errors = run(&Source { url, ..Default::default() }, &s, 24_000);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(s.queue.lock().unwrap().len(), 4);
    }

    #[test]
    fn a_server_error_is_reported_with_its_words() {
        let (url, _) =
            serve("500 Internal Server Error", "", vec![br#"{"error":"no model"}"#.to_vec()]);
        let errors = run(&Source { url, ..Default::default() }, &shared(), 24_000);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("no model") && errors[0].contains("500"), "{errors:?}");
    }

    #[test]
    fn a_server_without_the_endpoint_is_told_to_update() {
        let (url, _) = serve("404 Not Found", "", vec![Vec::new()]);
        let errors = run(&Source { url, ..Default::default() }, &shared(), 24_000);
        assert!(errors[0].contains("update and restart"), "{errors:?}");
        let (url, _) = serve("401 Unauthorized", "", vec![Vec::new()]);
        let errors = run(&Source { url, ..Default::default() }, &shared(), 24_000);
        assert!(errors[0].contains("token"), "{errors:?}");
    }

    #[test]
    fn a_stream_cut_into_odd_pieces_gives_the_same_samples() {
        let all = le(&[5, -5, 300, -300, 12000]);
        let (mut whole, mut parts) = (Vec::new(), Vec::new());
        Pcm::default().push(&all, &mut whole);
        let mut p = Pcm::default();
        for cut in all.chunks(3) {
            p.push(cut, &mut parts);
        }
        assert_eq!(whole, parts);
        assert_eq!(whole.len(), 5);
        // An empty chunk in the middle of a split sample loses nothing.
        let mut p = Pcm::default();
        let mut out = Vec::new();
        p.push(&all[..1], &mut out);
        p.push(&[], &mut out);
        p.push(&all[1..], &mut out);
        assert_eq!(out, whole);
    }

    #[test]
    fn resampling_in_chunks_matches_resampling_whole() {
        let tone: Vec<f32> = (0..2400).map(|i| (i as f32 * 0.05).sin()).collect();
        let whole = egui_bench::voice::audio::resample(&tone, 24_000, 48_000);
        let (mut r, mut out) = (Resample::new(24_000, 48_000), Vec::new());
        for c in tone.chunks(333) {
            r.push(c, &mut out);
        }
        // Chunked output holds back the last sample, and matches the whole run up to there.
        assert!(
            out.len() + 3 >= whole.len() && out.len() <= whole.len(),
            "{} {}",
            out.len(),
            whole.len()
        );
        for (a, b) in out.iter().zip(&whole) {
            assert!((a - b).abs() < 1e-4, "{a} {b}");
        }
        // Down as well as up.
        let (mut r, mut out) = (Resample::new(48_000, 44_100), Vec::new());
        for c in whole.chunks(500) {
            r.push(c, &mut out);
        }
        assert!(out.len() > 2000);
    }

    #[test]
    fn the_queue_feeds_every_channel_and_then_goes_quiet() {
        let s = shared();
        s.queue.lock().unwrap().extend([0.5, -0.5]);
        let mut out = [9.0f32; 8];
        fill(&mut out, 2, &s);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn playing_waits_for_a_little_audio_so_the_start_does_not_stutter() {
        let s = shared();
        s.prebuffer.store(4, Ordering::Relaxed);
        s.queue.lock().unwrap().extend([0.5, 0.5]);
        let mut out = [9.0f32; 2];
        fill(&mut out, 1, &s);
        assert_eq!(out, [0.0, 0.0], "two samples is not enough yet");
        assert_eq!(s.queue.lock().unwrap().len(), 2, "and nothing is used up");
        s.queue.lock().unwrap().extend([0.5, 0.5]);
        fill(&mut out, 1, &s);
        assert_eq!(out, [0.5, 0.5], "four is enough");
        // And a reply shorter than the buffer is not held back for ever.
        let s = shared();
        s.prebuffer.store(1000, Ordering::Relaxed);
        s.queue.lock().unwrap().extend([0.25]);
        s.done.store(true, Ordering::Relaxed);
        let mut out = [9.0f32; 1];
        fill(&mut out, 1, &s);
        assert_eq!(out, [0.25]);
    }
}
