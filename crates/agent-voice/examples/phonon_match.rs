//! Check the candle port against tensors `ptts` wrote for the same text, voice and noise.
//! cargo run --release --features speech --example phonon_match -- /tmp/phonon /tmp/phonon-ref

use candle_core::{DType, Device, Tensor};
use agent_voice::speech::phonon::Model;

struct Ref {
    index: Vec<serde_json::Value>,
    data: Vec<f32>,
}

impl Ref {
    fn open(dir: &std::path::Path) -> Ref {
        let index = serde_json::from_str(&std::fs::read_to_string(dir.join("index.json")).unwrap())
            .unwrap();
        let bytes = std::fs::read(dir.join("tensors.bin")).unwrap();
        let data =
            bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
        Ref { index, data }
    }
    fn get(&self, name: &str) -> Option<(Vec<usize>, &[f32])> {
        let e = self.index.iter().find(|e| e["name"] == name)?;
        let shape = e["shape"].as_array()?.iter().map(|v| v.as_u64().unwrap() as usize).collect();
        let (at, len) = (e["offset"].as_u64()? as usize, e["len"].as_u64()? as usize);
        Some((shape, &self.data[at..at + len]))
    }
}

fn report(name: &str, got: &[f32], want: &[f32]) -> f32 {
    if got.len() != want.len() {
        println!("{name:14} LENGTH {} vs {}", got.len(), want.len());
        return f32::INFINITY;
    }
    let err = got.iter().zip(want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
    let scale = want.iter().map(|v| v.abs()).fold(0.0f32, f32::max).max(1e-12);
    println!("{name:14} max abs err {err:.3e}  (max |ref| {scale:.3e}, rel {:.3e})", err / scale);
    err / scale
}

fn main() -> candle_core::Result<()> {
    let model_dir = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let ref_dir = std::path::PathBuf::from(std::env::args().nth(2).unwrap());
    let r = Ref::open(&ref_dir);
    let text: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(ref_dir.join("text.json")).unwrap()).unwrap();
    let tokens: Vec<u32> =
        text["tokens"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u32).collect();
    let frames_after_eos = text["frames_after_eos"].as_u64().unwrap() as usize;
    let dev = Device::Cpu;
    let model = Model::load(&model_dir, DType::F32, &dev)?;
    let flat = |t: &Tensor| {
        t.flatten_all().unwrap().to_dtype(DType::F32).unwrap().to_vec1::<f32>().unwrap()
    };

    let voice = model.load_voice(&model_dir.join("voices/NbpkqMVS3CJeq2j8.safetensors"), &dev)?;
    let mut worst = 0.0f32;
    worst = worst.max(report("voice_emb", &flat(&voice), r.get("voice_emb").unwrap().1));
    let text_emb = model.flow.embed_tokens(&tokens)?;
    worst = worst.max(report("text_emb", &flat(&text_emb), r.get("text_emb").unwrap().1));
    if let (Some(c), Some((_, want))) = (model.conditions(), r.get("conditions")) {
        worst = worst.max(report("conditions", &flat(c), want));
    }

    let noise = r.get("noise").unwrap().1.to_vec();
    let mut state = model.prime(&voice)?;
    model.prompt_text(&mut state, &tokens)?;
    let mut mimi = model.mimi_state()?;
    let mut prev: Option<Tensor> = None;
    let mut pcm = Vec::new();
    let mut countdown: Option<usize> = None;
    let (mut force_worst, mut force_audio) = (0.0f32, 0.0f32);
    let budget = ((tokens.len() as f64 / 3.0 + 2.0) * 12.5).ceil() as usize;
    for step in 0..budget {
        let n = Tensor::from_vec(noise[step * 32..(step + 1) * 32].to_vec(), (1, 32), &dev)?;
        let (latent, eos, logit) = model.step(&mut state, prev.as_ref(), &n)?;
        let audio = model.decode(&latent, &mut mimi)?;
        if std::env::var("FORCE").is_ok() {
            let (_, want) = r.get(&format!("latent.{step}")).unwrap();
            let e =
                flat(&latent).iter().zip(want).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            let (_, wa) = r.get(&format!("audio.{step}")).unwrap();
            let ea = audio.iter().zip(wa).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
            force_worst = force_worst.max(e);
            force_audio = force_audio.max(ea);
            if step % 8 == 0 {
                println!("forced step {step:3}: latent err {e:.2e}  audio err {ea:.2e}");
            }
            let forced = Tensor::from_vec(want.to_vec(), (1, 1, 32), &dev)?;
            pcm.extend(audio);
            if eos && countdown.is_none() {
                countdown = Some(frames_after_eos);
            }
            match countdown.as_mut() {
                Some(0) => break,
                Some(c) => *c -= 1,
                None => {}
            }
            prev = Some(forced);
            continue;
        }
        if step < 6 && false {
            worst = worst.max(report(
                &format!("latent.{step}"),
                &flat(&latent),
                r.get(&format!("latent.{step}")).unwrap().1,
            ));
            let want_logit = r.get(&format!("eos_logit.{step}")).unwrap().1[0];
            println!("{:14} {logit:.6} vs {want_logit:.6}", format!("eos_logit.{step}"));
            worst = worst.max(report(
                &format!("audio.{step}"),
                &audio,
                r.get(&format!("audio.{step}")).unwrap().1,
            ));
        }
        pcm.extend(audio);
        if eos && countdown.is_none() {
            countdown = Some(frames_after_eos);
        }
        match countdown.as_mut() {
            Some(0) => break,
            Some(c) => *c -= 1,
            None => {}
        }
        prev = Some(latent);
    }
    let want = r.get("pcm").unwrap().1;
    println!("frames: {} vs {}", pcm.len() / 1920, want.len() / 1920);
    for f in (0..want.len() / 1920).step_by(8) {
        let (a, b) = (&pcm[f * 1920..(f + 1) * 1920], &want[f * 1920..(f + 1) * 1920]);
        let err = a.iter().zip(b).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max);
        let rms = (b.iter().map(|v| v * v).sum::<f32>() / 1920.0).sqrt();
        println!("  frame {f:3}: max err {err:.2e}  signal rms {rms:.2e}");
    }
    let wav: Vec<u8> = crate_wav(&pcm, 24000);
    std::fs::write("/tmp/phonon-candle.wav", wav).unwrap();
    let n = pcm.len().min(want.len());
    worst = worst.max(report("pcm (overlap)", &pcm[..n], &want[..n]));
    println!("WORST relative error: {worst:.3e}");
    if std::env::var("FORCE").is_ok() {
        println!(
            "TEACHER FORCED, every step: worst latent err {force_worst:.2e}, worst audio err {force_audio:.2e}"
        );
    }
    Ok(())
}

fn crate_wav(pcm: &[f32], rate: u32) -> Vec<u8> {
    let data = (pcm.len() * 2) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
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
