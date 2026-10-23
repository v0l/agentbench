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
//! microphone to open (the browser) get `stub`, which says so.

mod settings;
pub use settings::{LOCAL_MODELS, Mode, Settings};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::Voice;

#[cfg(target_os = "android")]
mod android;

#[cfg(target_arch = "wasm32")]
mod stub;
#[cfg(target_arch = "wasm32")]
pub use stub::Voice;

/// What Whisper was trained on, and what a server is sent.
pub const RATE: u32 = foreman_core::audio::SPEECH_RATE;

pub use foreman_core::audio::{read_wav, to_whisper_rate, wav_bytes};

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
