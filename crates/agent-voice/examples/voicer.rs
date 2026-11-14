//! Fetch Phonon into an empty folder and say something: the whole path a server takes.
//! cargo run --release --features speech --example voicer -- <empty dir> "text" [voice]

fn main() -> Result<(), String> {
    let mut a = std::env::args().skip(1);
    let dir = std::path::PathBuf::from(a.next().unwrap());
    let text = a.next().unwrap();
    let asked = a.next().unwrap_or_default();
    let mut v = agent_voice::speech::Voicer::new(dir);
    let t = std::time::Instant::now();
    v.download(&|_| {})?;
    println!("model and catalogue in {:?}, {} voices", t.elapsed(), v.voices().len());
    let voice = v.pick(&asked).ok_or("no voices")?;
    v.fetch_voice(&voice, &|_| {})?;
    let on = v.load()?;
    let t = std::time::Instant::now();
    let mut n = 0usize;
    v.say(&text, &voice, |c| {
        n += c.len();
        true
    })?;
    println!(
        "{} on the {on}: {:.2}s of speech in {:?}",
        voice.label(),
        n as f32 / v.sample_rate() as f32,
        t.elapsed()
    );
    Ok(())
}
