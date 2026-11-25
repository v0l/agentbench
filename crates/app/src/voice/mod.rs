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
pub(crate) mod autosend;
pub use settings::{AUTO_SEND_SECS, LOCAL_MODELS, Mode, Settings};

mod converse;
pub use converse::{Converse, Talk, TalkKey};

pub mod output;
pub use output::{Sink, Verdict};

pub mod echo;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{Voice, input_devices, local_device};

#[cfg(not(target_arch = "wasm32"))]
mod reader;
#[cfg(not(target_arch = "wasm32"))]
pub use reader::{Reader, Source};

#[cfg(target_os = "android")]
pub(crate) mod android;

#[cfg(target_arch = "wasm32")]
mod stub;
#[cfg(target_arch = "wasm32")]
pub use stub::{Reader, Source, Voice, input_devices, local_device};

/// What Whisper was trained on, and what a server is sent.
pub const RATE: u32 = agent_voice::voice::audio::SPEECH_RATE;

pub use agent_voice::voice::audio::{read_wav, to_whisper_rate, wav_bytes};

/// Longest take. A forgotten microphone should end, and ten minutes of speech is already more than
/// anyone dictates into a chat box.
pub const MAX_SECONDS: usize = 600;

/// Shortest take worth reading. A double click on the key records a fraction of a second of
/// nothing, and Whisper answers nothing with a confident "Thank you."
pub const MIN_SECONDS: f32 = 0.4;

/// Quietest peak that can be speech. A muted or unplugged microphone records exact silence.
pub const MIN_PEAK: f32 = 0.005;

/// Silence that ends a take. Long enough to stop and think between sentences without the take
/// ending under you.
pub const HANG_SECONDS: f32 = 3.0;

pub const HANG_RANGE: std::ops::RangeInclusive<f32> = 1.0..=15.0;

pub const COUNTDOWN_AFTER: f32 = 1.5;

/// How much new speech is heard between one partial reading starting and the next being allowed.
pub const PARTIAL_SECONDS: usize = 2;

/// A take that never hears speech stops after this long, so a hotkey pressed by mistake does not
/// leave the microphone open.
pub const GIVE_UP_SECONDS: f32 = 10.0;

pub const LEAD_SECONDS: usize = 1;

pub type Mic = std::sync::Arc<std::sync::Mutex<Vec<f32>>>;

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

const RECENT_BLOCKS: usize = 500;

const MIN_BAND: f32 = 0.004;

struct Band {
    hp: f32,
    lp: f32,
    hp_prev_in: f32,
    hp_out: f32,
    lp1: f32,
    lp2: f32,
}

impl Band {
    fn new(rate: u32) -> Self {
        let pole = |hz: f32| (-std::f32::consts::TAU * hz / rate as f32).exp();
        Band {
            hp: pole(300.0),
            lp: 1.0 - pole(3400.0),
            hp_prev_in: 0.0,
            hp_out: 0.0,
            lp1: 0.0,
            lp2: 0.0,
        }
    }

    fn rms(&mut self, chunk: &[f32]) -> f32 {
        let mut sum = 0.0;
        for &x in chunk {
            self.hp_out = self.hp * (self.hp_out + x - self.hp_prev_in);
            self.hp_prev_in = x;
            self.lp1 += self.lp * (self.hp_out - self.lp1);
            self.lp2 += self.lp * (self.lp1 - self.lp2);
            sum += self.lp2 * self.lp2;
        }
        (sum / chunk.len().max(1) as f32).sqrt()
    }
}

/// Decides from the samples of a take when the speaker has finished.
///
/// Only the 300 to 3400 Hz band counts. A block of 20 ms is speech when its band level is well
/// over the 20th percentile of the last ten seconds, which follows a changing room with no fixed
/// cap, and once speech has been heard, over a share of how loud that speech was, so noise that
/// comes and goes cannot pass for it. Three speech blocks in a row count as speaking, which a
/// click does not manage.
pub struct Endpoint {
    hang: f32,
    give_up: f32,
    rate: u32,
    scanned: usize,
    recent: std::collections::VecDeque<f32>,
    band: Band,
    run: u32,
    spoke: bool,
    quiet: f32,
    level: f32,
    listened: f32,
}

impl Endpoint {
    pub fn new(rate: u32) -> Self {
        Self::with_hang(rate, HANG_SECONDS)
    }

    pub fn with_hang(rate: u32, hang: f32) -> Self {
        let rate = rate.max(1);
        Endpoint {
            hang,
            give_up: GIVE_UP_SECONDS,
            rate,
            scanned: 0,
            recent: Default::default(),
            band: Band::new(rate),
            run: 0,
            spoke: false,
            quiet: 0.0,
            level: 0.0,
            listened: 0.0,
        }
    }

    pub fn open(rate: u32) -> Self {
        Endpoint { give_up: f32::INFINITY, ..Self::new(rate) }
    }

    /// How long it has been quiet since the last speech.
    pub fn quiet_seconds(&self) -> f32 {
        self.quiet
    }

    pub fn set_hang(&mut self, hang: f32) {
        self.hang = hang;
    }

    pub fn hang(&self) -> f32 {
        self.hang
    }

    /// Whether speech has been heard yet.
    pub fn spoke(&self) -> bool {
        self.spoke
    }

    pub fn scanned(&self) -> usize {
        self.scanned
    }

    pub fn forget(&mut self, samples: usize) {
        self.scanned = self.scanned.saturating_sub(samples);
    }

    /// `pcm` is the whole take so far; only what is new since the last call is looked at.
    pub fn feed(&mut self, pcm: &[f32]) -> Heard {
        let block = (self.rate / 50).max(1) as usize;
        let step = block as f32 / self.rate as f32;
        while self.scanned + block <= pcm.len() {
            let chunk = &pcm[self.scanned..self.scanned + block];
            self.scanned += block;
            let rms = self.band.rms(chunk);
            if self.recent.len() == RECENT_BLOCKS {
                self.recent.pop_front();
            }
            self.recent.push_back(rms);
            let mut sorted: Vec<f32> = self.recent.iter().copied().collect();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let at = |p: f32| sorted[((sorted.len() - 1) as f32 * p) as usize];
            let floor = (at(0.2) * 3.0).max(MIN_BAND);
            let voice = if self.spoke { (self.level * 0.25).min(floor * 4.0) } else { 0.0 };
            let bar = floor.max(voice) * if self.spoke && self.run > 0 { 0.8 } else { 1.0 };
            let loud = rms > bar;
            self.listened += step;
            if loud {
                self.level = if self.level == 0.0 { rms } else { self.level + 0.02 * (rms - self.level) };
                self.run += 1;
                self.quiet = 0.0;
                self.spoke |= self.run >= 3;
            } else {
                self.run = 0;
                self.quiet += step;
            }
            if self.spoke && self.quiet >= self.hang {
                return Heard::Done;
            }
            if !self.spoke && self.listened >= self.give_up {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    Nothing,
    Finish,
    Cancel,
    TooShort,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Hold {
    held: bool,
    off: bool,
}

impl Hold {
    pub fn held(&self) -> bool {
        self.held
    }

    pub fn off_button(&self) -> bool {
        self.held && self.off
    }

    pub fn press(&mut self) {
        self.held = true;
        self.off = false;
    }

    pub fn drag(&mut self, over_button: bool) {
        if self.held {
            self.off = !over_button;
        }
    }

    pub fn release(&mut self, seconds: f32) -> Release {
        if !std::mem::take(&mut self.held) {
            return Release::Nothing;
        }
        if std::mem::take(&mut self.off) {
            Release::Cancel
        } else if seconds < MIN_SECONDS {
            Release::TooShort
        } else {
            Release::Finish
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
        take.extend(tone(2.5, 0.002));
        assert_eq!(e.feed(&take), Heard::Pause);
        take.extend(tone(0.7, 0.002));
        assert_eq!(e.feed(&take), Heard::Done);
    }

    #[test]
    fn a_gap_between_words_does_not_end_the_take() {
        let mut take = tone(1.0, 0.3);
        take.extend(tone(2.5, 0.002));
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

    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
        }
    }

    fn rms(pcm: &[f32]) -> f32 {
        (pcm.iter().map(|v| v * v).sum::<f32>() / pcm.len().max(1) as f32).sqrt()
    }

    fn scaled(mut pcm: Vec<f32>, target: f32) -> Vec<f32> {
        let gain = target / rms(&pcm).max(1e-6);
        pcm.iter_mut().for_each(|v| *v *= gain);
        pcm
    }

    fn lowpass(pcm: &mut [f32], alpha: f32) {
        let mut y = 0.0;
        for v in pcm.iter_mut() {
            y += alpha * (*v - y);
            *v = y;
        }
    }

    fn fan(n: usize, seed: u64) -> Vec<f32> {
        let mut r = Noise(seed);
        let mut v: Vec<f32> = (0..n).map(|_| r.next()).collect();
        lowpass(&mut v, 0.3);
        for (i, s) in v.iter_mut().enumerate() {
            let t = i as f32 / RATE as f32;
            *s = *s * 0.5 + (t * 120.0 * std::f32::consts::TAU).sin() * 0.5;
        }
        scaled(v, 0.1)
    }

    fn babble(n: usize, seed: u64) -> Vec<f32> {
        let mut out = vec![0.0f32; n];
        for talker in 0..5u64 {
            let mut r = Noise(seed + talker * 7919);
            let mut v: Vec<f32> = (0..n).map(|_| r.next()).collect();
            lowpass(&mut v, 0.35);
            let (rate, phase) = (2.5 + talker as f32 * 0.7, talker as f32 * 1.3);
            for (i, s) in v.iter().enumerate() {
                let t = i as f32 / RATE as f32;
                let env = ((t * rate * std::f32::consts::TAU + phase).sin() * 0.5 + 0.5).powi(2);
                out[i] += s * env;
            }
        }
        scaled(out, 0.06)
    }

    fn street(n: usize, seed: u64) -> Vec<f32> {
        let mut r = Noise(seed);
        let mut v: Vec<f32> = (0..n).map(|_| r.next()).collect();
        lowpass(&mut v, 0.04);
        let mut v = scaled(v, 0.1);
        let mut pass: Vec<f32> = (0..n).map(|_| r.next()).collect();
        lowpass(&mut pass, 0.2);
        for (i, s) in pass.iter().enumerate() {
            let t = i as f32 / RATE as f32;
            let car = (-((t % 2.7) - 1.0).powi(2) * 3.0).exp();
            v[i] += s * car * 0.35;
        }
        v
    }

    fn spoken_clip() -> Vec<f32> {
        let bytes = include_bytes!("../../tests/fixtures/voice/speech.wav");
        let (pcm, rate) = read_wav(bytes).unwrap();
        let pcm = to_whisper_rate(&pcm, rate);
        let end = pcm.iter().rposition(|v| v.abs() > 0.02).unwrap_or(pcm.len());
        let start = pcm.iter().position(|v| v.abs() > 0.02).unwrap_or(0);
        scaled(pcm[start..end].to_vec(), 0.1)
    }

    fn done_after_speech(background: fn(usize, u64) -> Vec<f32>) -> Option<f32> {
        let speech = spoken_clip();
        let lead = RATE as usize / 2;
        let speech_end = lead + speech.len();
        let total = speech_end + RATE as usize * 6;
        let noise = background(total, 12345);
        let mut take = noise.clone();
        for (i, s) in speech.iter().enumerate() {
            take[lead + i] += s;
        }
        let mut e = Endpoint::new(RATE);
        let step = RATE as usize / 10;
        let mut at = step;
        while at <= take.len() {
            match e.feed(&take[..at]) {
                Heard::Done => return Some((at as f32 - speech_end as f32) / RATE as f32),
                Heard::GaveUp => return None,
                _ => {}
            }
            at += step;
        }
        None
    }

    fn speech_at(level: f32, seconds: f32) -> Vec<f32> {
        let clip = scaled(spoken_clip(), level);
        let gap = vec![0.0f32; RATE as usize / 4];
        let mut out = Vec::new();
        while (out.len() as f32) < seconds * RATE as f32 {
            out.extend_from_slice(&clip);
            out.extend_from_slice(&gap);
        }
        out.truncate((seconds * RATE as f32) as usize);
        out
    }

    fn first_done(pcm: &[f32], hang: f32) -> Option<f32> {
        let mut e = Endpoint::with_hang(RATE, hang);
        let step = RATE as usize / 10;
        let mut at = step;
        while at <= pcm.len() {
            if e.feed(&pcm[..at]) == Heard::Done {
                return Some(at as f32 / RATE as f32);
            }
            at += step;
        }
        None
    }

    #[test]
    fn quieter_speech_after_a_loud_stretch_does_not_end_the_take() {
        let mut take = vec![0.0f32; RATE as usize / 2];
        take.extend(speech_at(0.06, 6.0));
        take.extend(speech_at(0.316, 10.0));
        take.extend(speech_at(0.06, 20.0));
        let spoken = take.len() as f32 / RATE as f32;
        take.extend(vec![0.0f32; RATE as usize * 6]);
        let done = first_done(&take, HANG_SECONDS).expect("never ended");
        assert!(done > spoken, "ended at {done}s during speech that lasts until {spoken}s");
    }

    #[test]
    fn five_minutes_of_speech_with_level_changes_never_ends_early() {
        let mut take = vec![0.0f32; RATE as usize / 2];
        let levels = [0.1, 0.05, 0.3, 0.08, 0.2, 0.04, 0.12, 0.35, 0.06, 0.1];
        for level in levels {
            take.extend(speech_at(level, 30.0));
        }
        let spoken = take.len() as f32 / RATE as f32;
        take.extend(vec![0.0f32; RATE as usize * 6]);
        let done = first_done(&take, HANG_SECONDS).expect("never ended");
        assert!(done > spoken, "ended at {done}s during speech that lasts until {spoken}s");
    }

    #[test]
    fn a_longer_hang_waits_longer() {
        let mut take = speech_at(0.1, 3.0);
        let spoken = take.len() as f32 / RATE as f32;
        take.extend(vec![0.0f32; RATE as usize * 12]);
        let short = first_done(&take, 2.0).unwrap();
        let long = first_done(&take, 8.0).unwrap();
        assert!(short < spoken + 2.6, "{short}");
        assert!(long > spoken + 7.0, "{long}");
    }

    #[test]
    fn an_open_microphone_waits_for_speech_as_long_as_it_takes() {
        let mut take = tone(30.0, 0.001);
        let mut e = Endpoint::open(RATE);
        assert_eq!(e.feed(&take), Heard::Waiting);
        let drop = take.len() - RATE as usize;
        take.drain(..drop);
        e.forget(drop);
        assert_eq!(e.scanned(), RATE as usize);
        take.extend(tone(1.0, 0.3));
        assert_eq!(e.feed(&take), Heard::Speaking);
        take.extend(tone(3.2, 0.001));
        assert_eq!(e.feed(&take), Heard::Done);
    }

    #[test]
    fn a_fan_does_not_hold_the_take_open() {
        let after = done_after_speech(fan).expect("the take never ended");
        assert!(after < 3.5, "ended {after}s after the speech");
    }

    #[test]
    fn babble_does_not_hold_the_take_open() {
        let after = done_after_speech(babble).expect("the take never ended");
        assert!(after < 3.5, "ended {after}s after the speech");
    }

    #[test]
    fn street_noise_does_not_hold_the_take_open() {
        let after = done_after_speech(street).expect("the take never ended");
        assert!(after < 3.5, "ended {after}s after the speech");
    }

    #[test]
    fn press_hold_and_release_gives_one_take() {
        let mut h = Hold::default();
        h.press();
        h.drag(true);
        assert!(h.held() && !h.off_button());
        assert_eq!(h.release(2.0), Release::Finish);
        assert_eq!(h.release(2.0), Release::Nothing);
    }

    #[test]
    fn a_short_tap_gives_no_take() {
        let mut h = Hold::default();
        h.press();
        assert_eq!(h.release(MIN_SECONDS - 0.1), Release::TooShort);
    }

    #[test]
    fn sliding_off_and_releasing_cancels() {
        let mut h = Hold::default();
        h.press();
        h.drag(false);
        assert!(h.off_button());
        assert_eq!(h.release(3.0), Release::Cancel);
    }

    #[test]
    fn sliding_back_on_keeps_the_take() {
        let mut h = Hold::default();
        h.press();
        h.drag(false);
        h.drag(true);
        assert_eq!(h.release(3.0), Release::Finish);
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
