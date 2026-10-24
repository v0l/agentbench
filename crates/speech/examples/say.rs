//! Say something to a WAV file, from a model folder already on disc.
//!
//! `cargo run --release -p agentbench-speech --example say -- "radio check" ~/.local/share/waveshark/models/kokoro`

use agentbench_speech::Speaker;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let text = args.next().unwrap_or_else(|| "Hello from agentbench.".to_string());
    let dir = args.next().map(std::path::PathBuf::from).ok_or("give the model folder")?;
    let voice = std::env::var("VOICE").unwrap_or_else(|_| "af_heart".into());
    let mut speaker = Speaker::new(dir);
    let start = std::time::Instant::now();
    let on = speaker.load(&voice)?;
    println!("loaded on {on} in {:?}", start.elapsed());
    let start = std::time::Instant::now();
    let said = speaker.say(&text)?;
    let secs = said.samples.len() as f64 / said.rate as f64;
    println!("{secs:.2}s of speech in {:?}", start.elapsed());
    let pcm: Vec<i16> =
        said.samples.iter().map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect();
    let data = (pcm.len() * 2) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&said.rate.to_le_bytes());
    out.extend_from_slice(&(said.rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write("/tmp/said.wav", out).map_err(|e| e.to_string())?;
    println!("wrote /tmp/said.wav");
    Ok(())
}
