//! A Whisper model on disc, fetched once and kept loaded between takes.

use crate::whisper::{Files, Whisper, best_device};
use foreman_core::speech_models::model;
use std::path::{Path, PathBuf};

/// Where a download has got to, for a status line.
pub struct Progress<'a> {
    pub name: &'a str,
    pub got_mb: u64,
    pub total_mb: u64,
}

const FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// Loads the model it is asked for and reads with it. Loading takes longer than reading, so the
/// model stays in memory until a different one is asked for.
pub struct Transcriber {
    models: PathBuf,
    loaded: Option<(String, Whisper, &'static str)>,
}

impl Transcriber {
    /// `models` is the folder each model gets a directory in.
    pub fn new(models: PathBuf) -> Transcriber {
        Transcriber { models, loaded: None }
    }

    pub fn dir(&self, id: &str) -> Option<PathBuf> {
        model(id).map(|m| self.models.join(m.0))
    }

    /// Whether the files of `id` are already on this machine.
    pub fn installed(&self, id: &str) -> bool {
        self.dir(id).is_some_and(|d| has_files(&d))
    }

    /// Fetch the files of `id` from Hugging Face, once. `progress` is called as bytes arrive.
    pub async fn download(
        &self,
        http: &reqwest::Client,
        id: &str,
        progress: impl Fn(Progress<'_>),
    ) -> Result<(), String> {
        let info = model(id).ok_or("unknown speech model")?;
        let dir = self.dir(id).ok_or("unknown speech model")?;
        fetch(http, info.1, &dir, progress).await
    }

    /// Load `id` if it is not the model already held. The files must be there.
    pub fn load(&mut self, id: &str) -> Result<&'static str, String> {
        if let Some((have, _, on)) = &self.loaded
            && have == id
        {
            return Ok(on);
        }
        self.loaded = None;
        let dir = self.dir(id).ok_or("unknown speech model")?;
        let files = Files::in_dir(&dir)
            .ok_or_else(|| format!("{} is missing model files", dir.display()))?;
        let (device, on) = best_device();
        let (model, on) = match Whisper::load(&files, device) {
            Ok(m) => (m, on),
            // A card that took the weights and then failed falls back to the CPU.
            Err(_) => (Whisper::load(&files, candle_core::Device::Cpu)?, "CPU"),
        };
        self.loaded = Some((id.to_string(), model, on));
        Ok(on)
    }

    /// Read 16 kHz mono samples with the loaded model. Call `load` first.
    pub fn transcribe(&mut self, pcm: &[f32]) -> Result<String, String> {
        let (_, model, _) = self.loaded.as_mut().ok_or("no speech model is loaded")?;
        model.transcribe(pcm)
    }
}

fn has_files(dir: &Path) -> bool {
    FILES.iter().all(|f| dir.join(f).exists())
}

async fn fetch(
    http: &reqwest::Client,
    repo: &str,
    dir: &Path,
    progress: impl Fn(Progress<'_>),
) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let name = repo.rsplit('/').next().unwrap_or(repo);
    for file in FILES {
        let target = dir.join(file);
        if target.exists() {
            continue;
        }
        let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");
        let mut resp = http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?
            .error_for_status()
            .map_err(|e| format!("{url}: {e}"))?;
        let total = resp.content_length().unwrap_or(0);
        // Written beside the target and renamed, so a download that stops half way is not
        // taken for a model.
        let part = dir.join(format!("{file}.part"));
        let mut out =
            std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        let (mut got, mut shown) = (0u64, 0u64);
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{url}: {e}"))? {
            std::io::Write::write_all(&mut out, &chunk)
                .map_err(|e| format!("{}: {e}", part.display()))?;
            got += chunk.len() as u64;
            if got - shown > 1 << 20 || got == total {
                shown = got;
                progress(Progress { name, got_mb: got >> 20, total_mb: total >> 20 });
            }
        }
        drop(out);
        std::fs::rename(&part, &target).map_err(|e| format!("{}: {e}", target.display()))?;
    }
    Ok(())
}
