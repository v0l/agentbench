//! Say a text with the candle port and write /tmp/phonon-say.wav.
//! cargo run --release --features speech --example phonon_say -- <model dir> <voice file> "text"

use candle_core::Device;
use agent_voice::speech::phonon::Phonon;

fn main() -> candle_core::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(args.next().unwrap());
    let voice = std::path::PathBuf::from(args.next().unwrap());
    let text = args.next().unwrap();
    let device = match std::env::var("DEVICE").as_deref() {
        Ok("cuda") => Device::new_cuda(0)?,
        Ok("metal") => Device::new_metal(0)?,
        _ => Device::Cpu,
    };
    let t = std::time::Instant::now();
    let mut tts = Phonon::load(&dir, &device)?;
    println!("loaded on {device:?} in {:?}", t.elapsed());
    for run in 0..2 {
        let t = std::time::Instant::now();
        let mut first = None;
        let mut pcm = Vec::new();
        tts.say(&text, &voice, |chunk| {
            first.get_or_insert(t.elapsed());
            pcm.extend_from_slice(chunk);
            true
        })?;
        println!(
            "run {run}: {:.2}s of speech in {:?}, first audio after {:?}",
            pcm.len() as f32 / 24000.0,
            t.elapsed(),
            first.unwrap()
        );
        if run == 1 {
            let mut out = Vec::new();
            let data = (pcm.len() * 2) as u32;
            out.extend_from_slice(b"RIFF");
            out.extend_from_slice(&(36 + data).to_le_bytes());
            out.extend_from_slice(b"WAVEfmt ");
            out.extend_from_slice(&[16, 0, 0, 0, 1, 0, 1, 0]);
            out.extend_from_slice(&24000u32.to_le_bytes());
            out.extend_from_slice(&48000u32.to_le_bytes());
            out.extend_from_slice(&[2, 0, 16, 0]);
            out.extend_from_slice(b"data");
            out.extend_from_slice(&data.to_le_bytes());
            for s in &pcm {
                out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
            }
            std::fs::write("/tmp/phonon-say.wav", out).unwrap();
            let raw: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
            std::fs::write("/tmp/phonon-say.f32", raw).unwrap();
        }
    }
    Ok(())
}
