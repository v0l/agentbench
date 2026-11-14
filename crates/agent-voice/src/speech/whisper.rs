//! Whisper through candle: mel filterbank, encoder, greedy decode.
//!
//! Ported down from waveshark's `stt` crate to what dictation needs: one take of 16 kHz PCM in,
//! one string out. Greedy decoding only, so the same audio reads the same twice, and audio longer
//! than Whisper's 30 second window is read a window at a time and joined.

use candle_core::{Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_nn::ops::softmax;
use candle_transformers::models::whisper::{self as m, Config, audio};
use std::path::Path;
use tokenizers::Tokenizer;

type Result<T> = std::result::Result<T, String>;

/// 30 seconds at 16 kHz: the window the encoder was trained on. Shorter audio is zero padded to it
/// rather than run short, because a truncated window shifts the positional embeddings and the
/// model starts inventing endings for sentences that were not cut off.
const WINDOW: usize = 30 * m::SAMPLE_RATE;

/// The files of one downloaded model.
pub struct Files {
    pub config: std::path::PathBuf,
    pub tokenizer: std::path::PathBuf,
    pub weights: std::path::PathBuf,
}

impl Files {
    pub fn in_dir(dir: &Path) -> Option<Files> {
        let f = Files {
            config: dir.join("config.json"),
            tokenizer: dir.join("tokenizer.json"),
            weights: dir.join("model.safetensors"),
        };
        (f.config.exists() && f.tokenizer.exists() && f.weights.exists()).then_some(f)
    }
}

pub struct Whisper {
    model: m::model::Whisper,
    tokenizer: Tokenizer,
    device: Device,
    mel_filters: Vec<f32>,
    suppress: Tensor,
    sot: u32,
    eot: u32,
    transcribe: u32,
    no_timestamps: u32,
    /// `<|en|>` for a multilingual model, none for an English only one, which has no such token.
    language: Option<u32>,
}

/// The fastest device candle was built for and can run on. A card that opens and then cannot run
/// a kernel (a driver older than the toolkit) is not worth stopping for: the CPU is slower and
/// not nothing.
pub fn best_device() -> (Device, &'static str) {
    #[cfg(feature = "speech-cuda")]
    if let Ok(d) = Device::new_cuda(0) {
        let ok = || -> candle_core::Result<f32> {
            let a = Tensor::new(&[[1.0f32, 2.0], [3.0, 4.0]], &d)?;
            a.matmul(&a)?.sum_all()?.to_scalar::<f32>()
        };
        if ok().is_ok() {
            return (d, "GPU");
        }
    }
    #[cfg(feature = "speech-metal")]
    if let Ok(d) = Device::new_metal(0) {
        let ok = || -> candle_core::Result<f32> {
            let a = Tensor::new(&[[1.0f32, 2.0], [3.0, 4.0]], &d)?;
            a.matmul(&a)?.sum_all()?.to_scalar::<f32>()
        };
        if ok().is_ok() {
            return (d, "GPU");
        }
    }
    (Device::Cpu, "CPU")
}

impl Whisper {
    pub fn load(files: &Files, device: Device) -> Result<Self> {
        let read =
            |p: &Path| std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()));
        let config: Config = serde_json::from_str(&read(&files.config)?)
            .map_err(|e| format!("whisper config: {e}"))?;
        let tokenizer = Tokenizer::from_file(&files.tokenizer)
            .map_err(|e| format!("whisper tokenizer: {e}"))?;

        let mel_bytes: &[u8] = match config.num_mel_bins {
            80 => include_bytes!("melfilters.bytes").as_slice(),
            // large v3 and the distilled large v3 listen with 128 bins instead of 80.
            128 => include_bytes!("melfilters128.bytes").as_slice(),
            n => return Err(format!("unsupported whisper model: {n} mel bins")),
        };
        let mut mel_filters = vec![0f32; mel_bytes.len() / 4];
        <byteorder::LittleEndian as byteorder::ByteOrder>::read_f32_into(
            mel_bytes,
            &mut mel_filters,
        );

        let suppress: Vec<f32> = (0..config.vocab_size as u32)
            .map(|i| if config.suppress_tokens.contains(&i) { f32::NEG_INFINITY } else { 0f32 })
            .collect();
        let suppress = Tensor::new(suppress.as_slice(), &device).map_err(candle)?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[&files.weights], m::DTYPE, &device)
                .map_err(candle)?
        };
        // English only models have one token fewer than 51865: no language tokens at all.
        let multilingual = config.vocab_size > 51_864;
        let model = m::model::Whisper::load(&vb, config).map_err(candle)?;
        let language = if multilingual { token(&tokenizer, "<|en|>").ok() } else { None };

        Ok(Self {
            sot: token(&tokenizer, m::SOT_TOKEN)?,
            eot: token(&tokenizer, m::EOT_TOKEN)?,
            transcribe: token(&tokenizer, m::TRANSCRIBE_TOKEN)?,
            no_timestamps: token(&tokenizer, m::NO_TIMESTAMPS_TOKEN)?,
            model,
            tokenizer,
            device,
            mel_filters,
            suppress,
            language,
        })
    }

    /// 16 kHz mono PCM to text.
    pub fn transcribe(&mut self, pcm: &[f32]) -> Result<String> {
        let mut out = String::new();
        for chunk in pcm.chunks(WINDOW) {
            let mut window = chunk.to_vec();
            window.resize(WINDOW, 0.0);
            let text = self.window(&window)?;
            let text = text.trim();
            if !text.is_empty() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(text);
            }
        }
        Ok(out)
    }

    fn window(&mut self, pcm: &[f32]) -> Result<String> {
        let bins = self.model.config.num_mel_bins;
        let mel = audio::pcm_to_mel(&self.model.config, pcm, &self.mel_filters);
        let frames = mel.len() / bins;
        let mel = Tensor::from_vec(mel, (1, bins, frames), &self.device).map_err(candle)?;
        // `pcm_to_mel` appends padding frames of its own, so a window that is exactly 30 seconds
        // comes back longer than the encoder's context.
        let mel = mel.narrow(2, 0, frames.min(m::N_FRAMES)).map_err(candle)?;
        self.decode(&mel)
    }

    fn decode(&mut self, mel: &Tensor) -> Result<String> {
        let features = self.model.encoder.forward(mel, true).map_err(candle)?;
        let max_positions = self.model.config.max_target_positions;
        let sample_len = max_positions / 2;

        let mut tokens = vec![self.sot];
        tokens.extend(self.language);
        tokens.push(self.transcribe);
        tokens.push(self.no_timestamps);

        for i in 0..sample_len {
            let t = Tensor::new(tokens.as_slice(), &self.device)
                .map_err(candle)?
                .unsqueeze(0)
                .map_err(candle)?;
            let ys = self.model.decoder.forward(&t, &features, i == 0).map_err(candle)?;
            let (_, seq_len, _) = ys.dims3().map_err(candle)?;
            let logits = self
                .model
                .decoder
                .final_linear(&ys.i((..1, seq_len - 1..)).map_err(candle)?)
                .map_err(candle)?
                .i(0)
                .map_err(candle)?
                .i(0)
                .map_err(candle)?
                .broadcast_add(&self.suppress)
                .map_err(candle)?;
            let logits = softmax(&logits, candle_core::D::Minus1).map_err(candle)?;
            let p: Vec<f32> = logits.to_vec1().map_err(candle)?;
            let next = p
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(i, _)| i as u32)
                .unwrap_or(self.eot);
            if next == self.eot || tokens.len() > max_positions {
                break;
            }
            tokens.push(next);
        }
        self.tokenizer.decode(&tokens, true).map_err(|e| format!("whisper detokenise: {e}"))
    }
}

fn token(tokenizer: &Tokenizer, t: &str) -> Result<u32> {
    tokenizer.token_to_id(t).ok_or_else(|| format!("no token id for {t}"))
}

fn candle(e: candle_core::Error) -> String {
    format!("candle: {e}")
}
