//! Text to speech for windows and phones that have no voice of their own.
//!
//! `POST /v1/audio/speech` takes what OpenAI's endpoint does, a JSON body with `input` and
//! optionally `voice`, `model` and `speed`, behind the same token as the rest of the API, and
//! answers a 16 bit mono WAV. `GET /v1/audio/voices` lists the voices. The voice named is used
//! when it is a Kokoro voice (`af_heart`, say); anything else, such as OpenAI's `alloy`, gets
//! the default. The model and a voice are fetched from Hugging Face the first time they are
//! asked for and stay loaded after, on the GPU when this build has CUDA and the machine has one.

use agentbench_speech::speak::{DEFAULT_VOICE, VOICES};
use agentbench_speech::{Said, Speaker};
use std::sync::{Arc, Mutex};

/// The most text spoken in one request. A reply that long is read in pieces by the window.
pub const INPUT_MAX: usize = 20_000;

#[derive(Clone)]
pub struct Voice {
    inner: Arc<Mutex<Speaker>>,
    /// One download at a time, so two requests for a voice do not fetch it twice.
    fetching: Arc<tokio::sync::Mutex<()>>,
}

impl Default for Voice {
    fn default() -> Self {
        Voice::new(crate::home().join("models").join("kokoro"))
    }
}

impl Voice {
    pub fn new(dir: std::path::PathBuf) -> Voice {
        Voice {
            inner: Arc::new(Mutex::new(Speaker::new(dir))),
            fetching: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn speaker(&self) -> std::sync::MutexGuard<'_, Speaker> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The voice a request for `asked` is spoken in: a voice we know, else the default.
    pub fn pick(&self, asked: &str) -> String {
        let asked = Speaker::pick(asked);
        let known =
            VOICES.iter().any(|v| v.id == asked) || self.speaker().voices().contains(&asked);
        if known { asked } else { DEFAULT_VOICE.to_string() }
    }

    /// Every voice offered, and whether it is on this machine yet.
    pub fn list(&self) -> Vec<(String, String, bool)> {
        let have = self.speaker().voices();
        let mut out: Vec<(String, String, bool)> = VOICES
            .iter()
            .map(|v| (v.id.to_string(), v.label.to_string(), have.contains(&v.id.to_string())))
            .collect();
        for id in have {
            if !out.iter().any(|(i, _, _)| *i == id) {
                out.push((id.clone(), id, true));
            }
        }
        out
    }

    /// Say `text` in `voice`, fetching and loading it first if need be.
    pub async fn say(&self, voice: &str, text: String) -> Result<Said, String> {
        if !self.speaker().installed(voice) {
            let _one_at_a_time = self.fetching.lock().await;
            if !self.speaker().installed(voice) {
                tracing::info!("fetching the voice {voice}");
                let dir = self.speaker().dir().to_path_buf();
                Speaker::new(dir)
                    .download(crate::http::client(), voice, |p| {
                        tracing::info!("downloading {}: {} of {} MB", p.name, p.got_mb, p.total_mb)
                    })
                    .await?;
            }
        }
        let inner = self.inner.clone();
        let voice = voice.to_string();
        tokio::task::spawn_blocking(move || {
            let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
            s.load(&voice)?;
            s.say(&text)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_voice_we_do_not_know_gets_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let v = Voice::new(dir.path().to_path_buf());
        assert_eq!(v.pick("af_bella"), "af_bella");
        assert_eq!(v.pick("alloy"), DEFAULT_VOICE, "OpenAI's names are not ours");
        assert_eq!(v.pick(""), DEFAULT_VOICE);
        assert_eq!(v.pick("../../etc/passwd"), DEFAULT_VOICE, "no paths");
        std::fs::create_dir_all(dir.path().join("voices")).unwrap();
        std::fs::write(dir.path().join("voices/bf_custom.bin"), "x").unwrap();
        assert_eq!(v.pick("bf_custom"), "bf_custom", "a voice put there by hand");
    }

    #[test]
    fn the_list_says_which_voices_are_on_disc() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("voices")).unwrap();
        std::fs::write(dir.path().join("voices/af_bella.bin"), "x").unwrap();
        let list = Voice::new(dir.path().to_path_buf()).list();
        assert_eq!(list[0].0, DEFAULT_VOICE);
        assert!(!list[0].2);
        assert!(list.iter().any(|(id, _, have)| id == "af_bella" && *have));
    }
}
