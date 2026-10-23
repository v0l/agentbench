//! Audio helpers shared by the window, the Android app and the server: what speech is sent as
//! and what it is read from.

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
