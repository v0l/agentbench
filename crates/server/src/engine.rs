use agent_voice::engine::{Engine, Host, ModelSpec, Plan, Want};
use agent_voice::voice::{catalogue, models};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const WHISPER_MIB: u64 = 6 * 1024;
pub const SPEECH_MIB: u64 = 4 * 1024;
const READY_WAIT: Duration = agent_voice::engine::LOAD_WAIT;
const POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Engine,
    Candle,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub backend: Backend,
    pub program: String,
    pub gpu: Option<u32>,
    pub cpu: bool,
    pub idle_secs: Option<u64>,
}

impl Settings {
    pub fn idle(&self) -> Duration {
        Duration::from_secs(self.idle_secs.unwrap_or(600))
    }
}

pub struct Child {
    pub url: String,
    pub key: String,
    pub model: String,
}

struct State {
    engine: Engine,
    plan: Option<Plan>,
    last: Instant,
}

#[derive(Clone)]
pub struct Engines {
    pub settings: Settings,
    pub models: PathBuf,
    state: Arc<Mutex<State>>,
    fetching: Arc<tokio::sync::Mutex<()>>,
    default: Arc<Mutex<String>>,
}

fn phonon_dir(models: &std::path::Path) -> PathBuf {
    models.join("phonon")
}

pub const PHONON: &str = "phonon-31m-en";

impl Engines {
    pub fn new(settings: Settings, models: PathBuf, default: &str, dir: PathBuf) -> Engines {
        let default = if models::model(default).is_some() { default } else { models::DEFAULT_MODEL };
        let engines = Engines {
            settings,
            models,
            state: Arc::new(Mutex::new(State { engine: Engine::in_dir(dir), plan: None, last: Instant::now() })),
            fetching: Arc::new(tokio::sync::Mutex::new(())),
            default: Arc::new(Mutex::new(default.to_string())),
        };
        engines.spawn_idle();
        engines
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn default_model(&self) -> String {
        self.default.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_default(&self, id: &str) {
        if models::model(id).is_some() {
            *self.default.lock().unwrap_or_else(|e| e.into_inner()) = id.to_string();
        }
    }

    pub fn pick(&self, asked: &str) -> String {
        let asked = asked.trim();
        if models::model(asked).is_some() { asked.to_string() } else { self.default_model() }
    }

    pub fn phonon(&self) -> PathBuf {
        phonon_dir(&self.models)
    }

    pub fn whisper_installed(&self) -> Vec<String> {
        models::LOCAL_MODELS.iter().filter(|m| models::installed(&self.models, m.0)).map(|m| m.0.to_string()).collect()
    }

    fn phonon_ready(&self) -> bool {
        let dir = self.phonon();
        catalogue::MODEL_FILES.iter().chain([&catalogue::CATALOG]).all(|f| dir.join(f).is_file())
            && !catalogue::needs_new_config(&dir)
    }

    fn plan(&self, whisper: &[String], speech: bool) -> Plan {
        let mut models: Vec<ModelSpec> = whisper
            .iter()
            .filter_map(|id| models::model(id))
            .map(|m| ModelSpec { id: m.0.into(), path: self.models.join(m.0), kind: "transcription".into(), preload: false })
            .collect();
        if speech {
            models.push(ModelSpec { id: PHONON.into(), path: self.phonon(), kind: "speech".into(), preload: false });
        }
        let need = if speech { SPEECH_MIB } else { 0 } + if whisper.is_empty() { 0 } else { WHISPER_MIB };
        Plan {
            models,
            gpu: self.settings.gpu,
            program: self.settings.program.clone(),
            need_mib: need,
            host: if self.settings.cpu { Host::Always } else { Host::Fallback },
        }
    }

    fn wanted(&self, need: Need) -> Plan {
        let mut whisper = self.whisper_installed();
        if let Need::Whisper(id) = &need
            && !whisper.contains(id)
        {
            whisper.push(id.clone());
        }
        let speech = matches!(need, Need::Speech) || self.phonon_ready();
        let mut plan = self.plan(&whisper, speech);
        if let Some(old) = &self.state().plan {
            for m in &old.models {
                if !plan.models.iter().any(|n| n.id == m.id) {
                    plan.models.push(m.clone());
                }
            }
        }
        plan
    }

    async fn fetch(&self, need: &Need) -> Result<(), String> {
        let _one = self.fetching.lock().await;
        let models = self.models.clone();
        match need.clone() {
            Need::Whisper(id) => {
                let fresh = !models::installed(&models, &id);
                if fresh {
                    tracing::info!("fetching the speech model {id}");
                }
                tokio::task::spawn_blocking(move || {
                    models::fetch(&models, &id, &|p| {
                        if fresh {
                            tracing::info!("downloading {}: {} of {} MB", p.name, p.got_mb, p.total_mb)
                        }
                    })
                    .map(drop)
                })
                .await
                .map_err(|e| e.to_string())?
            }
            Need::Speech if !self.phonon_ready() => {
                tracing::info!("fetching the Phonon voice model");
                let dir = self.phonon();
                tokio::task::spawn_blocking(move || {
                    catalogue::fetch_model(&dir, &|p| tracing::info!("downloading {}: {} of {} MB", p.name, p.got_mb, p.total_mb))
                })
                .await
                .map_err(|e| e.to_string())?
            }
            Need::Speech => Ok(()),
        }
    }

    pub async fn child(&self, need: Need) -> Result<Child, String> {
        self.fetch(&need).await?;
        let plan = self.wanted(need.clone());
        let model = match &need {
            Need::Whisper(id) => id.clone(),
            Need::Speech => PHONON.to_string(),
        };
        let started = Instant::now();
        loop {
            {
                let mut s = self.state();
                s.last = Instant::now();
                s.plan = Some(plan.clone());
                let idle = self.settings.idle();
                if let Some(n) = s.engine.sync(Want::Use(plan.clone()), idle, Instant::now()) {
                    tracing::warn!("speech engine: {n}");
                }
                if let Some((url, key)) = s.engine.endpoint() {
                    return Ok(Child { url, key, model });
                }
                if let Some(why) = s.engine.failed() {
                    return Err(why.to_string());
                }
            }
            if started.elapsed() > READY_WAIT {
                return Err(format!("the speech engine did not load in {} s", READY_WAIT.as_secs()));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    pub fn running(&self) -> Option<Child> {
        let s = self.state();
        let (url, key) = s.engine.endpoint()?;
        Some(Child { url, key, model: String::new() })
    }

    pub fn device(&self) -> Option<String> {
        let s = self.state();
        s.engine.endpoint()?;
        Some(if s.engine.on_host() { "CPU".into() } else { "GPU".into() })
    }

    pub fn pid(&self) -> Option<u32> {
        self.state().engine.pid()
    }

    pub fn gpu(&self) -> Option<u32> {
        self.state().engine.gpu().map(|g| g.index)
    }

    fn spawn_idle(&self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let me = self.clone();
        rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let mut s = me.state();
                let Some(plan) = s.plan.clone() else { continue };
                let idle = me.settings.idle();
                if let Some(n) = s.engine.sync(Want::Idle(plan), idle, Instant::now()) {
                    tracing::warn!("speech engine: {n}");
                }
            }
        });
    }

    pub fn stop(&self) {
        let mut s = self.state();
        s.plan = None;
        s.engine.stop_now();
    }

    #[doc(hidden)]
    pub fn set_test_gpus(&self, gpus: Vec<agent_voice::engine::Gpu>) {
        let mut s = self.state();
        let dir = s.engine.dir();
        s.engine.set_test(dir, gpus);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Need {
    Whisper(String),
    Speech,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whisper(dir: &std::path::Path, id: &str) {
        let d = dir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        for f in ["config.json", "tokenizer.json", "model.safetensors", "generation_config.json", "preprocessor_config.json"] {
            std::fs::write(d.join(f), "{}").unwrap();
        }
    }

    #[tokio::test]
    async fn the_plan_serves_every_whisper_on_disc_and_phonon_once_it_is_there() {
        let dir = tempfile::tempdir().unwrap();
        whisper(dir.path(), "whisper-base.en");
        whisper(dir.path(), "whisper-large-v3-turbo");
        let e = Engines::new(Settings::default(), dir.path().into(), "whisper-base.en", dir.path().join("engine"));
        let p = e.wanted(Need::Whisper("whisper-base.en".into()));
        let ids: Vec<_> = p.models.iter().map(|m| (m.id.as_str(), m.kind.as_str())).collect();
        assert_eq!(ids, [("whisper-base.en", "transcription"), ("whisper-large-v3-turbo", "transcription")]);
        assert_eq!(p.need_mib, WHISPER_MIB);
        assert_eq!(p.host, Host::Fallback);
        let p = e.wanted(Need::Speech);
        assert_eq!(p.models.last().unwrap().id, PHONON);
        assert_eq!(p.need_mib, WHISPER_MIB + SPEECH_MIB);
        assert!(p.models.iter().all(|m| !m.preload), "each model loads on its first request");
    }

    #[test]
    fn the_speech_backend_reads_from_the_config_with_the_engine_as_default() {
        #[derive(Deserialize)]
        struct C {
            #[serde(default)]
            speech: Settings,
        }
        let c: C = toml::from_str("").unwrap();
        assert_eq!(c.speech.backend, Backend::Engine);
        let c: C = toml::from_str("[speech]\nbackend = \"candle\"\ngpu = 1\nprogram = \"/x/inference-bench-server\"\n").unwrap();
        assert_eq!((c.speech.backend, c.speech.gpu, c.speech.program.as_str()), (Backend::Candle, Some(1), "/x/inference-bench-server"));
        assert!(toml::from_str::<C>("[speech]\nbackend = \"kokoro\"\n").is_err());
    }

    #[test]
    fn a_request_for_a_model_we_do_not_know_gets_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let e = Engines::new(Settings::default(), dir.path().into(), "whisper-large-v3-turbo", dir.path().join("engine"));
        assert_eq!(e.pick("whisper-1"), "whisper-large-v3-turbo");
        assert_eq!(e.pick("whisper-base.en"), "whisper-base.en");
        e.set_default("nonsense");
        assert_eq!(e.default_model(), "whisper-large-v3-turbo");
    }
}
