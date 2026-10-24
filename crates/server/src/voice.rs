//! Text to speech for windows and phones that have no voice of their own.
//!
//! `POST /v1/audio/speech` takes what OpenAI's endpoint does, a JSON body with `input` and
//! optionally `voice`, behind the same token as the rest of the API, and answers a 16 bit mono
//! WAV, or with `"response_format": "pcm"` raw samples streamed as each frame is made.
//! `GET /v1/audio/voices` lists the voices. Phonon says it (`gradium/phonon-31m-en`, through
//! candle); the voice named is used when it is one of its voices (`Hadley`, say), and anything
//! else, such as OpenAI's `alloy`, gets the default. The model, about 100 MB, and each voice are
//! fetched from Hugging Face the first time they are asked for and stay loaded after, on the
//! GPU when this build has CUDA or Metal and the machine has one.

use egui_bench::speech::Voicer;
use egui_bench::speech::phonon::voices::Voice as Spoken;
use std::sync::{Arc, Mutex};

/// The most text spoken in one request.
pub const INPUT_MAX: usize = 20_000;

/// What a whole reading came out as.
pub struct Said {
    pub samples: Vec<f32>,
    pub rate: u32,
    pub on: String,
}

#[derive(Clone)]
pub struct Voice {
    inner: Arc<Mutex<Voicer>>,
    /// One download at a time, so two requests do not fetch the same file twice.
    fetching: Arc<tokio::sync::Mutex<()>>,
    /// What the voice last ran on, `GPU` or `CPU`, once it has been used.
    on: Arc<Mutex<Option<String>>>,
}

impl Default for Voice {
    fn default() -> Self {
        Voice::new(crate::home().join("models").join("phonon"))
    }
}

impl Voice {
    pub fn new(dir: std::path::PathBuf) -> Voice {
        Voice {
            inner: Arc::new(Mutex::new(Voicer::new(dir))),
            fetching: Arc::new(tokio::sync::Mutex::new(())),
            on: Arc::new(Mutex::new(None)),
        }
    }

    fn voicer(&self) -> std::sync::MutexGuard<'_, Voicer> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What the voice runs on, once it has been used.
    pub fn on(&self) -> Option<String> {
        self.on.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// What a model loaded now would run on.
    pub fn device() -> String {
        static DEVICE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
        DEVICE.get_or_init(|| egui_bench::speech::best_device().1).to_string()
    }

    /// The voice a request for `asked` is spoken in: that one if it is listed, else the default.
    /// `None` until the catalogue has been fetched.
    pub fn pick(&self, asked: &str) -> Option<Spoken> {
        self.voicer().pick(asked)
    }

    /// Fetch the catalogue if it is not here yet: a few kilobytes, so a voice picker has
    /// something to show before the first reading.
    pub async fn catalogue(&self) -> Result<(), String> {
        if !self.voicer().voices().is_empty() {
            return Ok(());
        }
        let _one = self.fetching.lock().await;
        self.blocking(|v| v.fetch_catalogue()).await
    }

    /// Every voice offered: id (its name), label, and whether it is on this machine yet.
    pub fn list(&self) -> Vec<(String, String, bool)> {
        let v = self.voicer();
        let dir = v.dir().to_path_buf();
        v.voices()
            .into_iter()
            .map(|s| {
                let here = egui_bench::speech::phonon::voices::voice_path(&dir, &s).is_file();
                (s.name.clone(), s.label(), here)
            })
            .collect()
    }

    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Voicer) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            work(&mut inner.lock().unwrap_or_else(|e| e.into_inner()))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// Fetch the model, the catalogue and the voice asked for, if they are not here yet, and
    /// load the model. Returns the voice it will speak in.
    pub async fn ready(&self, asked: &str) -> Result<Spoken, String> {
        if !self.voicer().installed() {
            let _one = self.fetching.lock().await;
            if !self.voicer().installed() {
                tracing::info!("fetching the Phonon voice model");
                self.blocking(|v| {
                    v.download(&|p| {
                        tracing::info!("downloading {}: {} of {} MB", p.name, p.got_mb, p.total_mb)
                    })
                })
                .await?;
            }
        }
        let voice = self.pick(asked).ok_or("the voice catalogue lists no English voices")?;
        {
            let _one = self.fetching.lock().await;
            let v = voice.clone();
            self.blocking(move |s| s.fetch_voice(&v, &|_| {}).map(drop)).await?;
        }
        let on = self.blocking(|v| v.load()).await?;
        let shown = if on == "CPU" { "CPU" } else { "GPU" };
        *self.on.lock().unwrap_or_else(|e| e.into_inner()) = Some(shown.to_string());
        Ok(voice)
    }

    /// Say `text` in `voice`, all of it, then return the samples.
    pub async fn say(&self, voice: &str, text: String) -> Result<Said, String> {
        let spoken = self.ready(voice).await?;
        let on = self.on().unwrap_or_default();
        self.blocking(move |v| {
            let mut samples = Vec::new();
            v.say(&text, &spoken, |c| {
                samples.extend_from_slice(c);
                true
            })?;
            Ok(Said { samples, rate: v.sample_rate(), on })
        })
        .await
    }

    /// Say `text` in `voice` a frame at a time: the receiver gets each frame's samples as it is
    /// made. Fetching and loading happen before this returns, so a voice that cannot be had is
    /// an error here and not a stream that ends at once. Dropping the receiver stops the work
    /// after the frame in hand. Returns the sample rate and what it runs on.
    pub async fn stream(
        &self,
        voice: &str,
        text: String,
    ) -> Result<(u32, String, tokio::sync::mpsc::Receiver<Result<Vec<f32>, String>>), String> {
        let spoken = self.ready(voice).await?;
        let rate = self.voicer().sample_rate();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            let mut v = inner.lock().unwrap_or_else(|e| e.into_inner());
            let said = v.say(&text, &spoken, |c| tx.blocking_send(Ok(c.to_vec())).is_ok());
            if let Err(e) = said {
                let _ = tx.blocking_send(Err(e));
            }
        });
        Ok((rate, self.on().unwrap_or_default(), rx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue(dir: &std::path::Path) {
        std::fs::write(
            dir.join("voices.jsonl"),
            r#"{"filename": "voices/a.safetensors", "name": "Hadley", "voice_id": "a", "language": "en", "locale": "en-us", "gender": "female"}
{"filename": "voices/b.safetensors", "name": "Alfie", "voice_id": "b", "language": "en", "locale": "en-gb", "gender": "male"}
"#,
        )
        .unwrap();
    }

    #[test]
    fn a_voice_we_do_not_know_gets_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let v = Voice::new(dir.path().to_path_buf());
        assert!(v.pick("Hadley").is_none(), "no catalogue yet");
        catalogue(dir.path());
        assert_eq!(v.pick("alfie").unwrap().name, "Alfie", "names match whatever the case");
        assert_eq!(v.pick("alloy").unwrap().name, "Hadley", "OpenAI's names are not ours");
        assert_eq!(v.pick("").unwrap().name, "Hadley");
        assert_eq!(v.pick("../../etc/passwd").unwrap().name, "Hadley", "no paths");
    }

    #[test]
    fn the_list_says_which_voices_are_on_disc() {
        let dir = tempfile::tempdir().unwrap();
        catalogue(dir.path());
        std::fs::create_dir_all(dir.path().join("voices")).unwrap();
        std::fs::write(dir.path().join("voices/b.safetensors"), "x").unwrap();
        let list = Voice::new(dir.path().to_path_buf()).list();
        assert_eq!(list[0], ("Hadley".to_string(), "Hadley, American".to_string(), false));
        assert_eq!(list[1], ("Alfie".to_string(), "Alfie, British man".to_string(), true));
    }
}
