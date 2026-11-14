//! Kokoro on disc, fetched once and kept loaded between sentences.

use super::download::Progress;
use super::speak::{DEFAULT_VOICE, DeviceChoice, Engine, Files, installed, label_of, wanted};
use std::path::PathBuf;

/// What a reading aloud came out as.
pub struct Said {
    pub samples: Vec<f32>,
    pub rate: u32,
    /// The device it ran on.
    pub on: String,
}

/// The longest stretch of speech made in one go. A reply longer than this is cut short.
pub const MAX_SECONDS: f64 = 600.0;

/// Loads the voice it is asked for and speaks with it. The model is a third of a gigabyte, so it
/// stays in memory and only the voice's style tensors are swapped.
pub struct Speaker {
    dir: PathBuf,
    loaded: Option<(Engine, String, String)>,
}

impl Speaker {
    /// `dir` is the folder the model, the dictionary and the voices live in.
    pub fn new(dir: PathBuf) -> Speaker {
        Speaker { dir, loaded: None }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// The voice to use for a name somebody sent: itself when it is a plain voice name, else the
    /// default. Names become file names, so nothing with a path in it gets through.
    pub fn pick(asked: &str) -> String {
        let asked = asked.trim();
        let plain = !asked.is_empty()
            && asked.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if plain { asked.to_string() } else { DEFAULT_VOICE.to_string() }
    }

    /// Whether everything `voice` needs is on this machine.
    pub fn installed(&self, voice: &str) -> bool {
        Files::in_dir(&self.dir, voice).is_ok()
    }

    /// The voices already fetched.
    pub fn voices(&self) -> Vec<String> {
        installed(&self.dir)
    }

    /// Fetch what `voice` needs, once. `progress` is called as bytes arrive. Blocks until it is
    /// all there.
    pub fn download(&self, voice: &str, progress: &dyn Fn(Progress<'_>)) -> Result<(), String> {
        for (url, name) in wanted(voice) {
            super::download::get(&url, &self.dir.join(&name), &name, progress)?;
        }
        Ok(())
    }

    /// Load `voice`, keeping the model if it is loaded already. The files must be there.
    pub fn load(&mut self, voice: &str) -> Result<String, String> {
        let files = Files::in_dir(&self.dir, voice).map_err(|e| e.to_string())?;
        if let Some((engine, have, on)) = &mut self.loaded {
            if have != voice {
                engine.set_voice(&files.voice).map_err(|e| e.to_string())?;
                *have = voice.to_string();
            }
            return Ok(on.clone());
        }
        let (engine, on, note) =
            Engine::load_on(&files, DeviceChoice::Auto).map_err(|e| e.to_string())?;
        if !note.is_empty() {
            tracing::warn!("{note}");
        }
        self.loaded = Some((engine, voice.to_string(), on.clone()));
        Ok(on)
    }

    /// Say `text` with the loaded voice. Call `load` first.
    pub fn say(&mut self, text: &str) -> Result<Said, String> {
        let (engine, voice, on) = self.loaded.as_mut().ok_or("no voice is loaded")?;
        let samples = engine.say(text, MAX_SECONDS).map_err(|e| e.to_string())?;
        tracing::debug!("said {} samples as {}", samples.len(), label_of(voice));
        Ok(Said { samples, rate: engine.rate() as u32, on: on.clone() })
    }
}
