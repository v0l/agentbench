//! Text to speech for windows and phones that have no voice of their own.
//!
//! `POST /v1/audio/speech` takes what OpenAI's endpoint does, a JSON body with `input` and
//! optionally `voice`, `model` and `speed`, behind the same token as the rest of the API, and
//! answers a 16 bit mono WAV. `GET /v1/audio/voices` lists the voices. The voice named is used
//! when it is a Kokoro voice (`af_heart`, say); anything else, such as OpenAI's `alloy`, gets
//! the default. The model and a voice are fetched from Hugging Face the first time they are
//! asked for and stay loaded after, on the GPU when this build has CUDA and the machine has one.

use egui_bench::speech::speak::{DEFAULT_VOICE, VOICES};
use egui_bench::speech::{Said, Speaker};
use std::sync::{Arc, Mutex};

/// The most text spoken in one request. A reply that long is read in pieces by the window.
pub const INPUT_MAX: usize = 20_000;

#[derive(Clone)]
pub struct Voice {
    inner: Arc<Mutex<Speaker>>,
    /// One download at a time, so two requests for a voice do not fetch it twice.
    fetching: Arc<tokio::sync::Mutex<()>>,
    /// What the voice last ran on, `GPU` or `CPU`, once it has been used.
    on: Arc<Mutex<Option<String>>>,
}

/// `CUDA` and `Metal` are both a graphics card to somebody reading a settings page.
fn plain(device: &str) -> String {
    if device.eq_ignore_ascii_case("cpu") { "CPU".into() } else { "GPU".into() }
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
            on: Arc::new(Mutex::new(None)),
        }
    }

    /// What the voice runs on, once it has been used.
    pub fn on(&self) -> Option<String> {
        self.on.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// What a voice loaded now would run on: the card when this build has CUDA and the machine
    /// has one that works, else the processor.
    pub fn device() -> String {
        static DEVICE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        DEVICE
            .get_or_init(|| {
                plain(&egui_bench::speech::speak::device_label(
                    &egui_bench::speech::speak::best_device(),
                ))
            })
            .clone()
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

    /// Fetch what `voice` needs if it is not on this machine yet.
    async fn ensure(&self, voice: &str) -> Result<(), String> {
        if self.speaker().installed(voice) {
            return Ok(());
        }
        let _one_at_a_time = self.fetching.lock().await;
        if self.speaker().installed(voice) {
            return Ok(());
        }
        tracing::info!("fetching the voice {voice}");
        let dir = self.speaker().dir().to_path_buf();
        let name = voice.to_string();
        tokio::task::spawn_blocking(move || {
            Speaker::new(dir).download(&name, &|p| {
                tracing::info!("downloading {}: {} of {} MB", p.name, p.got_mb, p.total_mb)
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Say `text` in `voice`, fetching and loading it first if need be.
    pub async fn say(&self, voice: &str, text: String) -> Result<Said, String> {
        self.ensure(voice).await?;
        let (inner, seen) = (self.inner.clone(), self.on.clone());
        let voice = voice.to_string();
        tokio::task::spawn_blocking(move || {
            let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
            let on = plain(&s.load(&voice)?);
            *seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(on);
            s.say(&text)
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Say `text` a sentence at a time. The first sentence is made before this returns, so a
    /// voice that cannot be fetched or loaded is an error here and not a stream that ends at
    /// once; the rest arrive on the receiver as they are made. Dropping the receiver stops the
    /// work after the sentence in hand.
    pub async fn stream(
        &self,
        voice: &str,
        text: String,
    ) -> Result<(Said, tokio::sync::mpsc::Receiver<Result<Said, String>>), String> {
        self.ensure(voice).await?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let (inner, seen) = (self.inner.clone(), self.on.clone());
        let voice = voice.to_string();
        tokio::task::spawn_blocking(move || {
            let mut s = inner.lock().unwrap_or_else(|e| e.into_inner());
            match s.load(&voice) {
                Ok(on) => *seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(plain(&on)),
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    return;
                }
            }
            // One sentence to a piece, so the first sound does not wait on the rest.
            for piece in egui_bench::voice::audio::chunks(&text, 1) {
                let said = s.say(&piece);
                let failed = said.is_err();
                if tx.blocking_send(said).is_err() || failed {
                    return;
                }
            }
        });
        match rx.recv().await {
            Some(Ok(first)) => Ok((first, rx)),
            Some(Err(e)) => Err(e),
            None => Err("there was nothing to say".into()),
        }
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
