//! Speaking into the composer instead of typing.
//!
//! The microphone is opened when the speak key is pressed and closed when it is pressed again,
//! so the window never listens to the room in between. What was said is turned into text either
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
