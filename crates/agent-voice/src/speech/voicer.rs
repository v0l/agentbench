//! Phonon on disc, fetched once and kept loaded: the model, its catalogue of voices, each voice
//! fetched the first time it is asked for.

use super::download::{Progress, get};
use super::phonon::Phonon;
use super::phonon::voices::{self, CATALOG, DEFAULT_VOICE, MODEL_FILES, Voice};
use std::path::PathBuf;

/// Loads Phonon the first time it is needed and speaks with whichever voice is asked for. The
/// model is a tenth of a gigabyte and a voice a quarter of a megabyte, so both stay in memory.
pub struct Voicer {
    dir: PathBuf,
    loaded: Option<(Phonon, &'static str)>,
}

impl Voicer {
    /// `dir` is the folder the model, the catalogue and the voices live in.
    pub fn new(dir: PathBuf) -> Voicer {
        Voicer { dir, loaded: None }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Whether the model and the catalogue are on this machine.
    pub fn installed(&self) -> bool {
        MODEL_FILES.iter().chain([&CATALOG]).all(|f| self.dir.join(f).is_file())
    }

    /// The voices the catalogue lists, empty until it has been fetched.
    pub fn voices(&self) -> Vec<Voice> {
        voices::read_catalog(&self.dir.join(CATALOG)).unwrap_or_default()
    }

    /// The voice called `asked`, else the default, else the first listed.
    pub fn pick(&self, asked: &str) -> Option<Voice> {
        let all = self.voices();
        let find = |n: &str| all.iter().find(|v| v.name.eq_ignore_ascii_case(n.trim())).cloned();
        find(asked).or_else(|| find(DEFAULT_VOICE)).or_else(|| all.first().cloned())
    }

    /// Fetch the model and the catalogue, once. Blocks until they are there.
    pub fn download(&self, progress: &dyn Fn(Progress<'_>)) -> Result<(), String> {
        for f in MODEL_FILES {
            get(&voices::model_url(f), &self.dir.join(f), f, progress)?;
        }
        get(&voices::voice_url(CATALOG), &self.dir.join(CATALOG), CATALOG, progress)
    }

    /// Fetch the catalogue alone, so the voices can be listed before the model is wanted.
    pub fn fetch_catalogue(&self) -> Result<(), String> {
        get(&voices::voice_url(CATALOG), &self.dir.join(CATALOG), CATALOG, &|_| {})
    }

    /// Fetch one voice, if it is not here yet.
    pub fn fetch_voice(
        &self,
        v: &Voice,
        progress: &dyn Fn(Progress<'_>),
    ) -> Result<PathBuf, String> {
        let path = voices::voice_path(&self.dir, v);
        get(&voices::voice_url(&v.filename), &path, &v.name, progress)?;
        Ok(path)
    }

    /// Load the model, on the fastest device this build has. Returns what it runs on.
    pub fn load(&mut self) -> Result<&'static str, String> {
        if let Some((_, on)) = &self.loaded {
            return Ok(on);
        }
        let (device, on) = super::best_device();
        let tts = match Phonon::load(&self.dir, &device) {
            Ok(t) => (t, on),
            Err(e) if !device.is_cpu() => {
                tracing::warn!("Phonon could not load on the {on} ({e}); using the CPU");
                (
                    Phonon::load(&self.dir, &candle_core::Device::Cpu)
                        .map_err(|e| e.to_string())?,
                    "CPU",
                )
            }
            Err(e) => return Err(e.to_string()),
        };
        self.loaded = Some(tts);
        Ok(self.loaded.as_ref().map(|(_, on)| *on).unwrap_or("CPU"))
    }

    pub fn sample_rate(&self) -> u32 {
        self.loaded.as_ref().map_or(24_000, |(t, _)| t.sample_rate())
    }

    /// Say `text` in `voice` (already fetched), handing each frame's samples to `out` as it is
    /// made; `out` returning false stops it. Call `load` first.
    pub fn say(
        &mut self,
        text: &str,
        voice: &Voice,
        out: impl FnMut(&[f32]) -> bool,
    ) -> Result<usize, String> {
        let path = voices::voice_path(&self.dir, voice);
        let (tts, _) = self.loaded.as_mut().ok_or("the voice model is not loaded")?;
        tts.say(text, &path, out).map_err(|e| e.to_string())
    }
}
