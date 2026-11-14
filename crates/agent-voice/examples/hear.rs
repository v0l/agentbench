use agent_voice::speech::Transcriber;

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let wav = args.next().ok_or("give a WAV file")?;
    let models = args.next().map(std::path::PathBuf::from).ok_or("give the models folder")?;
    let id = args.next().unwrap_or_else(|| agent_voice::speech::DEFAULT_MODEL.to_string());

    let bytes = std::fs::read(&wav).map_err(|e| e.to_string())?;
    let (channels, rate) = (
        u16::from_le_bytes([bytes[22], bytes[23]]) as usize,
        u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) as usize,
    );
    let data = bytes.windows(4).position(|w| w == b"data").ok_or("no data chunk")? + 8;
    let samples: Vec<f32> = bytes[data..]
        .chunks_exact(2 * channels)
        .map(|f| i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0)
        .collect();
    let pcm: Vec<f32> =
        (0..samples.len() * 16_000 / rate).map(|i| samples[i * rate / 16_000]).collect();

    let mut t = Transcriber::new(models);
    let start = std::time::Instant::now();
    let on = t.load(&id)?;
    println!("{id} loaded on {on} in {:?}", start.elapsed());
    let start = std::time::Instant::now();
    let text = t.transcribe(&pcm)?;
    println!("{:.1}s of audio read in {:?}", pcm.len() as f64 / 16_000.0, start.elapsed());
    println!("{text}");
    Ok(())
}
