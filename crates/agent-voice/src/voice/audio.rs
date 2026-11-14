//! Audio helpers for speech both ways: what speech is sent as and read from, a reply turned into
//! something that can be said, and cut into pieces small enough to start playing early.

/// What Whisper was trained on, and what a speech server is sent.
pub const SPEECH_RATE: u32 = 16_000;

/// Mono samples at `from` Hz to `SPEECH_RATE`.
///
/// Averages the source samples that fall under each output sample and then interpolates between
/// those, which keeps frequencies above the new Nyquist from folding back into the speech band.
/// It is not a studio resampler; Whisper does not need one.
pub fn to_whisper_rate(pcm: &[f32], from: u32) -> Vec<f32> {
    if from == SPEECH_RATE || pcm.is_empty() || from == 0 {
        return pcm.to_vec();
    }
    let ratio = from as f64 / SPEECH_RATE as f64;
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

/// Mono samples at `from` Hz to `to` Hz, by linear interpolation. For playing speech on a sound
/// card that wants another rate, where the other way round (`to_whisper_rate`) is for reading it.
pub fn resample(pcm: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || pcm.is_empty() || from == 0 || to == 0 {
        return pcm.to_vec();
    }
    let out_len = (pcm.len() as u64 * to as u64 / from as u64) as usize;
    let step = from as f64 / to as f64;
    (0..out_len)
        .map(|i| {
            let at = i as f64 * step;
            let lo = (at as usize).min(pcm.len() - 1);
            let hi = (lo + 1).min(pcm.len() - 1);
            let t = (at - lo as f64) as f32;
            pcm[lo] * (1.0 - t) + pcm[hi] * t
        })
        .collect()
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

/// Read a WAV file into mono samples and the rate they are at. Takes 8, 16, 24 and 32 bit
/// integer PCM and 32 bit float, in any number of channels, which are averaged to one.
pub fn read_wav(bytes: &[u8]) -> Result<(Vec<f32>, u32), String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".into());
    }
    let (mut fmt, mut data) = (None, None);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let size = u32::from_le_bytes([bytes[at + 4], bytes[at + 5], bytes[at + 6], bytes[at + 7]])
            as usize;
        let body = at + 8;
        // A streamed WAV leaves the data size at its maximum; take what is there.
        let end = body.saturating_add(size).min(bytes.len());
        match &bytes[at..at + 4] {
            b"fmt " if end - body >= 16 => fmt = Some(&bytes[body..end]),
            b"data" => {
                data = Some(&bytes[body..end]);
                break;
            }
            _ => {}
        }
        at = end + (size & 1);
    }
    let (Some(fmt), Some(data)) = (fmt, data) else {
        return Err("the WAV file has no format or no samples".into());
    };
    let tag = u16::from_le_bytes([fmt[0], fmt[1]]);
    let channels = u16::from_le_bytes([fmt[2], fmt[3]]).max(1) as usize;
    let rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
    let bits = u16::from_le_bytes([fmt[14], fmt[15]]);
    // 0xFFFE is WAVE_FORMAT_EXTENSIBLE, which carries the same two layouts.
    let float = tag == 3 || (tag == 0xFFFE && bits == 32 && fmt.len() >= 26 && fmt[24] == 3);
    let width = (bits / 8) as usize;
    if rate == 0 || !(1..=4).contains(&width) || (float && width != 4) {
        return Err(format!("unsupported WAV: {bits} bit, tag {tag}"));
    }
    let sample = |b: &[u8]| -> f32 {
        match (float, width) {
            (true, _) => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            (_, 1) => (b[0] as f32 - 128.0) / 128.0,
            (_, 2) => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
            (_, 3) => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8_388_608.0,
            _ => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
        }
    };
    let frame = width * channels;
    let pcm = data
        .chunks_exact(frame)
        .map(|f| f.chunks_exact(width).map(sample).sum::<f32>() / channels as f32)
        .collect();
    Ok((pcm, rate))
}

#[cfg(test)]
mod tests {
    #[test]
    fn resampling_keeps_the_length_in_seconds_and_the_shape() {
        let one_second: Vec<f32> = (0..24_000).map(|i| (i as f32 / 24_000.0).sin()).collect();
        let up = resample(&one_second, 24_000, 48_000);
        assert_eq!(up.len(), 48_000);
        assert!((up[24_000] - one_second[12_000]).abs() < 1e-3);
        assert_eq!(resample(&one_second, 24_000, 24_000), one_second);
        assert!(resample(&[], 24_000, 48_000).is_empty());
        assert_eq!(resample(&[0.5], 24_000, 48_000).len(), 2, "one sample does not panic");
    }

    use super::*;

    #[test]
    fn resampling_changes_length_by_the_ratio() {
        let one_second = vec![0.1f32; 48_000];
        assert_eq!(to_whisper_rate(&one_second, 48_000).len(), 16_000);
        let cd = vec![0.1f32; 44_100];
        let out = to_whisper_rate(&cd, 44_100);
        assert!((out.len() as i64 - 16_000).abs() <= 1, "{}", out.len());
        assert_eq!(to_whisper_rate(&cd[..100], SPEECH_RATE), cd[..100].to_vec());
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
    fn a_wav_we_wrote_reads_back() {
        let pcm = vec![0.0, 0.25, -0.25, 0.5];
        let (got, rate) = read_wav(&wav_bytes(&pcm, 22_050)).unwrap();
        assert_eq!(rate, 22_050);
        assert_eq!(got.len(), 4);
        for (a, b) in pcm.iter().zip(&got) {
            assert!((a - b).abs() < 1e-3, "{a} {b}");
        }
    }

    #[test]
    fn stereo_is_averaged_and_float_is_read() {
        // Two frames of stereo 16 bit: (1000, 3000) and (-2000, 0).
        let mut wav = wav_bytes(&[0.0; 4], 8_000);
        wav[22] = 2; // channels
        for (i, v) in [1000i16, 3000, -2000, 0].iter().enumerate() {
            wav[44 + i * 2..46 + i * 2].copy_from_slice(&v.to_le_bytes());
        }
        let (got, _) = read_wav(&wav).unwrap();
        assert_eq!(got.len(), 2);
        assert!((got[0] - 2000.0 / 32768.0).abs() < 1e-6);
        assert!((got[1] + 1000.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn something_that_is_not_a_wav_is_refused() {
        assert!(read_wav(b"hello there, I am not audio").is_err());
        assert!(read_wav(b"RIFF\0\0\0\0WAVE").is_err());
    }
}

/// A chat reply as it should be read aloud: markdown marks dropped, links read as their words,
/// code blocks and URLs left out, since spelling those out is noise.
pub fn speakable(markdown: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for line in markdown.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_code = !in_code;
            if in_code {
                out.push_str("Code omitted.\n");
            }
            continue;
        }
        if in_code
            || line.is_empty()
            || line.chars().all(|c| matches!(c, '-' | '=' | '*' | '_' | '|' | ' '))
        {
            continue;
        }
        let line = line.trim_start_matches('#').trim_start_matches('>').trim();
        let line = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
            .unwrap_or(line);
        let mut text = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '`' | '*' | '_' | '~' | '|' => {}
                '[' => {
                    // [words](address): keep the words.
                    let mut words = String::new();
                    for w in chars.by_ref() {
                        if w == ']' {
                            break;
                        }
                        words.push(w);
                    }
                    if chars.peek() == Some(&'(') {
                        for w in chars.by_ref() {
                            if w == ')' {
                                break;
                            }
                        }
                    }
                    text.push_str(&words);
                }
                c => text.push(c),
            }
        }
        let text: Vec<&str> = text
            .split_whitespace()
            .filter(|w| !(w.starts_with("http://") || w.starts_with("https://")))
            .collect();
        if text.is_empty() {
            continue;
        }
        out.push_str(&text.join(" "));
        // A line is a pause: give it the full stop a sentence would have.
        if !out.ends_with(['.', '!', '?', ':']) {
            out.push('.');
        }
        out.push('\n');
    }
    out.trim().to_string()
}

/// `text` in pieces of about `target` characters, split where a person would breathe, so the first
/// piece can be spoken while the rest are still being made.
pub fn chunks(text: &str, target: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut sentence = String::new();
    for c in text.chars() {
        sentence.push(c);
        if matches!(c, '.' | '!' | '?' | '\n') && sentence.trim().len() > 1 {
            if !current.is_empty() && current.len() + sentence.len() > target {
                out.push(std::mem::take(&mut current).trim().to_string());
            }
            current.push_str(&sentence);
            sentence.clear();
        }
    }
    current.push_str(&sentence);
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

#[cfg(test)]
mod speech_tests {
    use super::*;

    #[test]
    fn markdown_is_read_as_words() {
        let md = "# Done\n\nI fixed **three** things:\n\n- the `config` loader\n- see [the docs](https://x.org/a)\n\n```rust\nlet x = 1;\n```\n\nOpen https://example.com now";
        let said = speakable(md);
        assert_eq!(
            said,
            "Done.\nI fixed three things:\nthe config loader.\nsee the docs.\nCode omitted.\nOpen now."
        );
        assert_eq!(speakable("---\n\n"), "");
    }

    #[test]
    fn long_text_is_cut_at_sentences_and_nothing_is_lost() {
        let text = "One two three. Four five six. Seven eight nine. Ten.";
        let parts = chunks(text, 20);
        assert_eq!(parts, ["One two three.", "Four five six.", "Seven eight nine.", "Ten."]);
        assert_eq!(chunks(text, 1000), [text]);
        assert_eq!(chunks("no stop", 10), ["no stop"]);
        assert!(chunks("  ", 10).is_empty());
        let one_long = "word ".repeat(100);
        assert_eq!(chunks(&one_long, 20).len(), 1, "a sentence is never cut in the middle");
    }
}
