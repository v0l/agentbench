//! Speaking into the composer instead of typing.
//!
//! The microphone is opened by the speak key or its hotkey and closed when you go quiet (or press
//! the key again), so the window never listens to the room in between. While you talk the take
//! so far is read every couple of seconds and the words appear in the draft; when you stop, the
//! whole take is read once more and replaces them. What was said is turned into text either
//! by Whisper running on this machine (candle, off the UI thread, fetched once from Hugging
//! Face) or by any server that speaks OpenAI's `/v1/audio/transcriptions`. The words land in the
//! draft for you to read and send; nothing is sent on its own.
//!
//! This module holds what is the same on every platform: the settings, the audio helpers and the
//! shape of a result. The microphone and the two readers live in `native`, and platforms with no
//! microphone to open (the browser, Android) get `stub`, which says so.

mod settings;
pub use settings::{LOCAL_MODELS, Mode, Settings};

#[cfg(all(feature = "local-speech", not(any(target_arch = "wasm32", target_os = "android"))))]
mod whisper;

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
mod native;
#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
pub use native::Voice;

#[cfg(any(target_arch = "wasm32", target_os = "android"))]
mod stub;
#[cfg(any(target_arch = "wasm32", target_os = "android"))]
pub use stub::Voice;

/// What Whisper was trained on, and what a server is sent.
pub const RATE: u32 = 16_000;

/// Longest take. A forgotten microphone should end, and ten minutes of speech is already more than
/// anyone dictates into a chat box.
pub const MAX_SECONDS: usize = 600;

/// Shortest take worth reading. A double click on the key records a fraction of a second of
/// nothing, and Whisper answers nothing with a confident "Thank you."
pub const MIN_SECONDS: f32 = 0.4;

/// Quietest peak that can be speech. A muted or unplugged microphone records exact silence.
pub const MIN_PEAK: f32 = 0.005;

/// Silence that ends a take. Long enough for the pause between two sentences, short enough that
/// dictation does not wait on you.
pub const HANG_SECONDS: f32 = 1.0;

/// How much new speech is heard between one partial reading starting and the next being allowed.
pub const PARTIAL_SECONDS: usize = 2;

/// A take that never hears speech stops after this long, so a hotkey pressed by mistake does not
/// leave the microphone open.
pub const GIVE_UP_SECONDS: f32 = 10.0;

/// What the take sounds like right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Heard {
    /// Nothing but room so far.
    Waiting,
    Speaking,
    /// Speech, then a gap that has not run out yet.
    Pause,
    /// Speech, then silence for [`HANG_SECONDS`]: the take is over.
    Done,
    /// No speech in [`GIVE_UP_SECONDS`].
    GaveUp,
}

/// Decides from the samples of a take when the speaker has finished.
///
/// A block of 20 ms is speech when its RMS is well over the quietest block of the last three
/// seconds, so a fan or a noisy microphone raises the bar instead of keeping the take open for
/// ever. Three speech blocks in a row count as speaking, which a click does not manage.
pub struct Endpoint {
    rate: u32,
    scanned: usize,
    recent: std::collections::VecDeque<f32>,
    run: u32,
    spoke: bool,
    quiet: f32,
    listened: f32,
}

impl Endpoint {
    pub fn new(rate: u32) -> Self {
        Endpoint {
            rate: rate.max(1),
            scanned: 0,
            recent: Default::default(),
            run: 0,
            spoke: false,
            quiet: 0.0,
            listened: 0.0,
        }
    }

    /// How long it has been quiet since the last speech.
    pub fn quiet_seconds(&self) -> f32 {
        self.quiet
    }

    /// Whether speech has been heard yet.
    pub fn spoke(&self) -> bool {
        self.spoke
    }

    /// `pcm` is the whole take so far; only what is new since the last call is looked at.
    pub fn feed(&mut self, pcm: &[f32]) -> Heard {
        let block = (self.rate / 50).max(1) as usize;
        let step = block as f32 / self.rate as f32;
        while self.scanned + block <= pcm.len() {
            let chunk = &pcm[self.scanned..self.scanned + block];
            self.scanned += block;
            let rms = (chunk.iter().map(|v| v * v).sum::<f32>() / block as f32).sqrt();
            if self.recent.len() == 150 {
                self.recent.pop_front();
            }
            self.recent.push_back(rms);
            let floor = self.recent.iter().copied().fold(f32::INFINITY, f32::min);
            let loud = rms > (floor * 3.0).clamp(0.01, 0.08);
            self.listened += step;
            if loud {
                self.run += 1;
                self.quiet = 0.0;
                self.spoke |= self.run >= 3;
            } else {
                self.run = 0;
                self.quiet += step;
            }
            if self.spoke && self.quiet >= HANG_SECONDS {
                return Heard::Done;
            }
            if !self.spoke && self.listened >= GIVE_UP_SECONDS {
                return Heard::GaveUp;
            }
        }
        match (self.spoke, self.quiet > 0.0) {
            (false, _) => Heard::Waiting,
            (true, false) => Heard::Speaking,
            (true, true) => Heard::Pause,
        }
    }
}

/// What the voice has to hand to the window.
#[derive(Clone, Debug, PartialEq)]
pub enum Said {
    /// A reading of the take so far, still going. Replaces the last one.
    Partial(String),
    /// The reading of the whole take. Replaces the partials; nothing follows it.
    Final(String),
    /// Why there is no text, ending the take.
    Failed(String),
}

/// Mono samples at `from` Hz to `RATE`.
///
/// Averages the source samples that fall under each output sample and then interpolates between
/// those, which keeps frequencies above the new Nyquist from folding back into the speech band.
/// It is not a studio resampler; Whisper does not need one.
pub fn to_whisper_rate(pcm: &[f32], from: u32) -> Vec<f32> {
    if from == RATE || pcm.is_empty() || from == 0 {
        return pcm.to_vec();
    }
    let ratio = from as f64 / RATE as f64;
    let out_len = (pcm.len() as f64 / ratio).floor() as usize;
    let half = (ratio / 2.0).max(0.5);
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let centre = (i as f64 + 0.5) * ratio;
        let lo = ((centre - half).floor().max(0.0)) as usize;
        let hi = (((centre + half).ceil()) as usize).min(pcm.len());
        let hi = hi.max(lo + 1).min(pcm.len());
        let sum: f32 = pcm[lo..hi].iter().sum();
        out.push(sum / (hi - lo) as f32);
    }
    out
}

/// 16 bit mono WAV, which every transcription server takes.
pub fn wav_bytes(pcm: &[f32], rate: u32) -> Vec<u8> {
    let data = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    out
}

/// `draft` with `said` after it, a space between if the draft does not already end in whitespace.
pub fn append(draft: &mut String, said: &str) {
    let said = said.trim();
    if said.is_empty() {
        return;
    }
    if !draft.is_empty() && !draft.ends_with(char::is_whitespace) {
        draft.push(' ');
    }
    draft.push_str(said);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(seconds: f32, amp: f32) -> Vec<f32> {
        (0..(seconds * RATE as f32) as usize).map(|i| amp * (i as f32 * 0.1).sin()).collect()
    }

    #[test]
    fn a_take_ends_after_a_pause_that_follows_speech() {
        let mut take = tone(0.5, 0.002);
        let mut e = Endpoint::new(RATE);
        assert_eq!(e.feed(&take), Heard::Waiting);
        take.extend(tone(1.0, 0.3));
        assert_eq!(e.feed(&take), Heard::Speaking);
        take.extend(tone(0.5, 0.002));
        assert_eq!(e.feed(&take), Heard::Pause);
        take.extend(tone(0.7, 0.002));
        assert_eq!(e.feed(&take), Heard::Done);
    }

    #[test]
    fn a_gap_between_words_does_not_end_the_take() {
        let mut take = tone(1.0, 0.3);
        take.extend(tone(0.6, 0.002));
        take.extend(tone(1.0, 0.3));
        let mut e = Endpoint::new(RATE);
        assert_eq!(e.feed(&take), Heard::Speaking);
    }

    #[test]
    fn a_click_is_not_speech_and_a_silent_take_gives_up() {
        let mut take = tone(0.02, 0.5);
        take.extend(tone(5.0, 0.001));
        let mut e = Endpoint::new(RATE);
        assert_eq!(e.feed(&take), Heard::Waiting);
        assert!(!e.spoke());
        take.extend(tone(6.0, 0.001));
        assert_eq!(e.feed(&take), Heard::GaveUp);
    }

    #[test]
    fn steady_noise_raises_the_bar_instead_of_counting_as_speech() {
        let take = tone(4.0, 0.02);
        let mut e = Endpoint::new(RATE);
        assert_eq!(e.feed(&take), Heard::Waiting);
    }

    #[test]
    fn resampling_changes_length_by_the_ratio() {
        let one_second = vec![0.1f32; 48_000];
        assert_eq!(to_whisper_rate(&one_second, 48_000).len(), 16_000);
        let cd = vec![0.1f32; 44_100];
        let out = to_whisper_rate(&cd, 44_100);
        assert!((out.len() as i64 - 16_000).abs() <= 1, "{}", out.len());
        assert_eq!(to_whisper_rate(&cd[..100], RATE), cd[..100].to_vec());
    }

    #[test]
    fn resampling_keeps_a_tone_below_the_new_nyquist() {
        let rate = 48_000.0f32;
        let tone: Vec<f32> = (0..48_000)
            .map(|n| (2.0 * std::f32::consts::PI * 440.0 * n as f32 / rate).sin())
            .collect();
        let out = to_whisper_rate(&tone, 48_000);
        let peak = out.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak > 0.9, "a 440 Hz tone lost its level: {peak}");
    }

    #[test]
    fn wav_header_describes_the_samples() {
        let wav = wav_bytes(&[0.0, 0.5, -0.5, 1.0], 16_000);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 8);
        assert_eq!(wav.len(), 44 + 8);
        assert_eq!(i16::from_le_bytes(wav[46..48].try_into().unwrap()), 16383);
    }

    #[test]
    fn speech_joins_the_draft_with_one_space() {
        let mut d = String::new();
        append(&mut d, "  hello ");
        assert_eq!(d, "hello");
        append(&mut d, "world");
        assert_eq!(d, "hello world");
        d.push('\n');
        append(&mut d, "next");
        assert_eq!(d, "hello world\nnext");
        append(&mut d, "   ");
        assert_eq!(d, "hello world\nnext");
    }
}
