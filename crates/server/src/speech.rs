//! Speech to text for windows and phones that have no model of their own.
//!
//! `POST /v1/audio/transcriptions` takes what OpenAI's endpoint does, a multipart form with a
//! WAV `file` and a `model`, behind the same token as the rest of the API, so anything that can
//! use a speech server (agentbench's own window and app included) can use this one. The model named
//! in the form is used when it is one of ours (`whisper-large-v3-turbo`, say); anything else, such
//! as OpenAI's `whisper-1`, gets the server's default. A model is fetched from Hugging Face the
//! first time it is asked for and stays loaded after, on the GPU when this build has CUDA and the
//! machine has a card.

use egui_bench::speech::{DEFAULT_MODEL, Transcriber, model};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Speech {
    inner: Arc<Mutex<Transcriber>>,
    /// Used when a request names no model of ours.
    default: Arc<Mutex<String>>,
    /// Models being fetched, so two requests for one do not download it twice.
    fetching: Arc<tokio::sync::Mutex<()>>,
    /// What the model last used ran on, `GPU` or `CPU`.
    on: Arc<Mutex<Option<String>>>,
}

impl Default for Speech {
    fn default() -> Self {
        Speech::new(crate::home().join("models"), DEFAULT_MODEL)
    }
}

impl Speech {
    pub fn new(models: std::path::PathBuf, default: &str) -> Speech {
        let default = if model(default).is_some() { default } else { DEFAULT_MODEL };
        Speech {
            inner: Arc::new(Mutex::new(Transcriber::new(models))),
            default: Arc::new(Mutex::new(default.to_string())),
            fetching: Arc::new(tokio::sync::Mutex::new(())),
            on: Arc::new(Mutex::new(None)),
        }
    }

    /// What the model runs on, once it has been used.
    pub fn on(&self) -> Option<String> {
        self.on.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_default(&self, id: &str) {
        if model(id).is_some() {
            *self.default.lock().unwrap_or_else(|e| e.into_inner()) = id.to_string();
        }
    }

    /// Every model this server can read with: id, label, download size in MB, whether it is on
    /// this machine yet, and whether it is the default.
    pub fn list(&self) -> Vec<(String, String, u32, bool, bool)> {
        let default = self.default.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let t = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        egui_bench::speech::LOCAL_MODELS
            .iter()
            .map(|m| (m.0.to_string(), m.2.to_string(), m.3, t.installed(m.0), m.0 == default))
            .collect()
    }

    /// The model a request for `asked` runs on.
    pub fn pick(&self, asked: &str) -> String {
        let asked = asked.trim();
        if model(asked).is_some() {
            asked.to_string()
        } else {
            self.default.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
    }

    /// Read 16 kHz mono samples, fetching and loading `id` first if need be. Returns the text and
    /// the device it ran on.
    pub async fn read(&self, id: &str, pcm: Vec<f32>) -> Result<(String, &'static str), String> {
        let have = self.inner.lock().unwrap_or_else(|e| e.into_inner()).installed(id);
        if !have {
            let _one_at_a_time = self.fetching.lock().await;
            let t = self.inner.clone();
            let has = t.lock().unwrap_or_else(|e| e.into_inner()).installed(id);
            if !has {
                // The download holds no lock, so a read on a model already loaded is not held up.
                let dir = t.lock().unwrap_or_else(|e| e.into_inner()).dir(id);
                let models =
                    dir.and_then(|d| d.parent().map(|p| p.to_path_buf())).unwrap_or_default();
                tracing::info!("fetching the speech model {id}");
                let id = id.to_string();
                tokio::task::spawn_blocking(move || {
                    Transcriber::new(models).download(&id, &|p| {
                        tracing::info!("downloading {}: {} of {} MB", p.name, p.got_mb, p.total_mb)
                    })
                })
                .await
                .map_err(|e| e.to_string())??;
            }
        }
        let inner = self.inner.clone();
        let seen = self.on.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut t = inner.lock().unwrap_or_else(|e| e.into_inner());
            let on = t.load(&id)?;
            *seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(on.to_string());
            t.transcribe(&pcm).map(|text| (text, on))
        })
        .await
        .map_err(|e| e.to_string())?
    }
}
