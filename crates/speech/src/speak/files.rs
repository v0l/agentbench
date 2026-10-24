//! The four files a voice is, and where they come from.
//!
//! They come from three repositories: the model and its configuration from the publisher, the
//! style tensors from a mirror that publishes them as plain float32 rather than as pickled torch
//! tensors, and the pronunciation dictionary from the front end the model was trained against.
//! About 330 MB altogether, once.

use super::{Error, Result};
use std::path::{Path, PathBuf};

/// Where the model itself is published.
pub const MODEL_REPO: &str = "hexgrad/Kokoro-82M";
/// The style tensors, as raw float32 rather than as torch pickles: 510 by 256 per voice.
pub const VOICE_REPO: &str = "onnx-community/Kokoro-82M-v1.0-ONNX";
/// The pronunciation dictionary the model was trained against (a dataset on the hub).
pub const LEXICON_REPO: &str = "hexgrad/misaki";
/// American English, which is the accent the default voice speaks.
pub const LEXICON_FILE: &str = "us_gold.json";
pub const WEIGHTS: &str = "kokoro-v1_0.pth";
pub const CONFIG: &str = "config.json";

#[derive(Clone, Debug)]
pub struct Files {
    pub config: PathBuf,
    pub weights: PathBuf,
    pub lexicon: PathBuf,
    /// The style tensors for one voice.
    pub voice: PathBuf,
}

/// What a voice needs fetched: the URL of each file and where it goes under the model folder.
pub fn wanted(voice: &str) -> Vec<(String, String)> {
    let hub = "https://huggingface.co";
    vec![
        (format!("{hub}/{MODEL_REPO}/resolve/main/{CONFIG}"), CONFIG.into()),
        (format!("{hub}/{MODEL_REPO}/resolve/main/{WEIGHTS}"), WEIGHTS.into()),
        (format!("{hub}/datasets/{LEXICON_REPO}/resolve/main/{LEXICON_FILE}"), LEXICON_FILE.into()),
        (
            format!("{hub}/{VOICE_REPO}/resolve/main/voices/{voice}.bin"),
            format!("voices/{voice}.bin"),
        ),
    ]
}

impl Files {
    /// The files for `voice` in `dir`, if they are all there.
    pub fn in_dir(dir: impl AsRef<Path>, voice: &str) -> Result<Self> {
        let dir = dir.as_ref();
        let need = |p: PathBuf| -> Result<PathBuf> {
            match p.exists() {
                true => Ok(p),
                false => Err(Error::other(format!("{} is missing", p.display()))),
            }
        };
        Ok(Self {
            config: need(dir.join(CONFIG))?,
            weights: need(dir.join(WEIGHTS))?,
            lexicon: need(dir.join(LEXICON_FILE))?,
            voice: need(dir.join("voices").join(format!("{voice}.bin")))?,
        })
    }
}

/// Voices already fetched into `dir`, in catalogue order, with anything else found beside them
/// after.
pub fn installed(dir: &Path) -> Vec<String> {
    let has = |id: &str| dir.join("voices").join(format!("{id}.bin")).exists();
    let mut out: Vec<String> =
        super::VOICES.iter().filter(|v| has(v.id)).map(|v| v.id.to_string()).collect();
    let Ok(rd) = std::fs::read_dir(dir.join("voices")) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().trim_end_matches(".bin").to_string();
        if !out.contains(&name) && super::voice(&name).is_none() {
            out.push(name);
        }
    }
    out
}
