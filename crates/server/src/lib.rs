pub mod agent;
pub mod api;
pub mod cli;
pub mod git;
pub mod http;
pub mod local;
pub mod projects;
pub mod runner;
pub mod speech;
pub mod store;
pub mod titles;
pub mod tools;
pub mod tui;
pub mod voice;

use agent::{Agent, JsonCommand};
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub listen: Option<SocketAddr>,
    #[serde(default)]
    pub pi: Option<PathBuf>,
    #[serde(default)]
    pub claude: Option<PathBuf>,
    #[serde(default)]
    pub codex: Option<PathBuf>,
    #[serde(default)]
    pub web: Option<PathBuf>,
    #[serde(default)]
    pub model: agent::native::Model,
    #[serde(default)]
    pub title_model: titles::TitleModel,
    /// Language servers behind the `code_*` tools, and the errors put in front of the model.
    #[serde(default)]
    pub code: agent::code::Settings,
    #[serde(default, rename = "agent")]
    pub agents: Vec<JsonCommand>,
}

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7420";

pub fn local_url(listen: SocketAddr) -> String {
    match listen {
        SocketAddr::V4(a) if a.ip().is_unspecified() => format!("http://127.0.0.1:{}", a.port()),
        SocketAddr::V6(a) if a.ip().is_unspecified() => format!("http://[::1]:{}", a.port()),
        a => format!("http://{a}"),
    }
}

/// An environment variable under its current name, else under the name it had when the app was
/// called foreman: `AGENTBENCH_HOME`, else `FOREMAN_HOME`.
pub fn env_var(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("AGENTBENCH_{name}"))
        .or_else(|| std::env::var_os(format!("FOREMAN_{name}")))
}

/// Where the database, token and models live. An install made when the app was called foreman
/// keeps using its `foreman` folder until that is renamed.
pub fn home() -> PathBuf {
    if let Some(dir) = env_var("HOME") {
        return PathBuf::from(dir);
    }
    home_in(&data_dir())
}

/// The user's data folder, where the home folder and the window's settings sit side by side.
pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// The home folder under a data folder. The window keeps its own settings in a folder of the same
/// name, so it is the database that says whether the old folder is still the one in use.
pub fn home_in(data: &Path) -> PathBuf {
    let (new, old) = (data.join("agentbench"), data.join("foreman"));
    let has_db = |d: &Path| d.join("agentbench.db").exists() || d.join("foreman.db").exists();
    if !has_db(&new) && has_db(&old) { old } else { new }
}

/// A file in the home folder: `name`, unless only the `legacy` one from before the rename exists.
pub fn home_file(home: &Path, name: &str, legacy: &str) -> PathBuf {
    let new = home.join(name);
    let old = home.join(legacy);
    if !new.exists() && old.exists() { old } else { new }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn agents(&self) -> Vec<Box<dyn Agent>> {
        self.agents_with(agent::native::Shared::new(self.model.clone()))
    }

    pub fn agents_with(&self, model: agent::native::Shared) -> Vec<Box<dyn Agent>> {
        self.agents_with_titles(model, None)
    }

    /// The agents, with the native agent answering web page prompts using `titles`.
    pub fn agents_with_titles(
        &self,
        model: agent::native::Shared,
        titles: Option<titles::Titles>,
    ) -> Vec<Box<dyn Agent>> {
        self.agents_with_speech(model, titles, None)
    }

    /// The agents, with the native agent reading sound files through `speech` as well.
    pub fn agents_with_speech(
        &self,
        model: agent::native::Shared,
        titles: Option<titles::Titles>,
        speech: Option<speech::Speech>,
    ) -> Vec<Box<dyn Agent>> {
        let mut native = agent::Native::shared(model);
        native.titles = titles;
        native.speech = speech;
        native.code =
            std::sync::Arc::new(agent::code::Code::new(agent::code::unconfined(self.code.clone())));
        let mut all: Vec<Box<dyn Agent>> = vec![
            Box::new(native),
            Box::new(agent::Pi::new(self.pi.clone().unwrap_or_else(|| "pi".into()))),
            Box::new(agent::Claude::new(self.claude.clone().unwrap_or_else(|| "claude".into()))),
            Box::new(agent::Codex::new(self.codex.clone().unwrap_or_else(|| "codex".into()))),
        ];
        for extra in &self.agents {
            all.retain(|a| a.name() != extra.name);
            all.push(Box::new(extra.clone()));
        }
        all
    }
}

pub fn token(store: &store::Store) -> Result<String, store::StoreError> {
    if let Some(t) = store.setting("token")? {
        return Ok(t);
    }
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| store::StoreError(e.to_string()))?;
    let t: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    store.set_setting("token", &t)?;
    Ok(t)
}

pub async fn serve(
    store: store::Store,
    agents: Vec<Box<dyn Agent>>,
    listen: SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    serve_with(store, agents, listen, None, shutdown).await
}

pub fn saved_model(store: &store::Store, fallback: &agent::native::Model) -> agent::native::Model {
    store
        .setting("model")
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| fallback.clone())
}

/// The title model last saved from the window, else the one in `agentbench.toml`.
pub fn saved_title_model(
    store: &store::Store,
    fallback: &titles::TitleModel,
) -> titles::TitleModel {
    store
        .setting("title_model")
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| fallback.clone())
}

pub async fn serve_config(
    store: store::Store,
    config: &Config,
    listen: SocketAddr,
    web: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let model = agent::native::Shared::new(saved_model(&store, &config.model));
    let titles = titles::Titles::new(saved_title_model(&store, &config.title_model), model.clone());
    // One Whisper for the transcription endpoint and the agent's read_file, so it loads once.
    let speech = speech::Speech::new(home().join("models"), &speech_model(&store));
    let agents =
        config.agents_with_speech(model.clone(), Some(titles.clone()), Some(speech.clone()));
    let voice = voice::Voice::default();
    serve_inner(
        store,
        agents,
        Some(model),
        Some(titles),
        Some((speech, voice)),
        listen,
        web,
        shutdown,
    )
    .await
}

/// The model a speech request gets when it names none of ours: set it with
/// AGENTBENCH_SPEECH_MODEL, or it is the one saved, or Whisper large v3 turbo.
fn speech_model(store: &store::Store) -> String {
    env_var("SPEECH_MODEL")
        .and_then(|v| v.into_string().ok())
        .or_else(|| store.setting("speech_model").ok().flatten())
        .unwrap_or_else(|| "whisper-large-v3-turbo".into())
}

pub async fn serve_with(
    store: store::Store,
    agents: Vec<Box<dyn Agent>>,
    listen: SocketAddr,
    web: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    serve_inner(store, agents, None, None, None, listen, web, shutdown).await
}

/// Serve with these agents, keeping downloaded models in `models` rather than the data folder:
/// what a test uses, so it never writes into the user's own.
pub async fn serve_agents(
    store: store::Store,
    agents: Vec<Box<dyn Agent>>,
    model: Option<agent::native::Shared>,
    listen: SocketAddr,
    models: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let speech = speech::Speech::new(models.clone(), &speech_model(&store));
    let voice = voice::Voice::new(models.join("phonon"));
    serve_inner(store, agents, model, None, Some((speech, voice)), listen, None, shutdown).await
}

#[allow(clippy::too_many_arguments)]
async fn serve_inner(
    store: store::Store,
    agents: Vec<Box<dyn Agent>>,
    model: Option<agent::native::Shared>,
    titles: Option<titles::Titles>,
    speech: Option<(speech::Speech, voice::Voice)>,
    listen: SocketAddr,
    web: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let stale = store.reset_running().map_err(|e| e.0)?;
    if stale > 0 {
        tracing::info!("{stale} sessions were running when agentbench last stopped");
    }
    if let Some(dir) = dirs::home_dir() {
        // A home folder that is a project itself is left alone.
        let _ = projects::ensure_home(&store, &dir);
    }
    let token = token(&store).map_err(|e| e.0)?;
    let (speech, voice) = speech.unwrap_or_else(|| {
        (speech::Speech::new(home().join("models"), &speech_model(&store)), voice::Voice::default())
    });
    let mut runner = runner::Runner::new(store, agents);
    let titles = titles.or_else(|| {
        model
            .clone()
            .map(|m| titles::Titles::new(saved_title_model(runner.store(), &Default::default()), m))
    });
    if let Some(titles) = titles {
        runner = runner.with_titles(titles);
    }
    *runner.base.lock().unwrap_or_else(|e| e.into_inner()) =
        runner::Base { url: local_url(listen), token: token.clone() };
    let api = api::Api { runner, token, model, speech, voice };
    api::watch_worktrees(api.clone(), std::time::Duration::from_secs(15));
    let listener =
        tokio::net::TcpListener::bind(listen).await.map_err(|e| format!("{listen}: {e}"))?;
    tracing::info!("listening on http://{}", listener.local_addr().map_err(|e| e.to_string())?);
    let mut app = api::router(api);
    if let Some(dir) = web.filter(|d| d.join("index.html").is_file()) {
        tracing::info!("serving the web app from {}", dir.display());
        app = app.fallback_service(tower_http::services::ServeDir::new(dir));
    }
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_adds_and_overrides_agents() {
        let c: Config = toml::from_str(
            r#"
            listen = "0.0.0.0:9000"
            pi = "/opt/pi"

            [[agent]]
            name = "aider"
            program = "aider"
            args = ["--json"]
            text = "/content"

            [[agent]]
            name = "codex"
            program = "/usr/local/bin/my-codex"
            text = "/msg"
            "#,
        )
        .unwrap();
        assert_eq!(c.listen, Some("0.0.0.0:9000".parse().unwrap()));
        let agents = c.agents();
        let names: Vec<_> = agents.iter().map(|a| a.name()).collect();
        assert_eq!(names, ["native", "pi", "claude", "aider", "codex"]);
    }

    #[test]
    fn an_install_from_before_the_rename_keeps_its_folder_and_files() {
        let data = tempfile::tempdir().unwrap();
        assert_eq!(home_in(data.path()), data.path().join("agentbench"), "a fresh install");
        let old = data.path().join("foreman");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("app.ron"), "x").unwrap();
        assert_eq!(home_in(data.path()), data.path().join("agentbench"), "no database there");
        std::fs::write(old.join("foreman.db"), "x").unwrap();
        std::fs::write(old.join("foreman.toml"), "x").unwrap();
        assert_eq!(home_in(data.path()), old, "the old database is still in use");
        // The window makes the new folder for its settings; the server must not follow it there.
        std::fs::create_dir_all(data.path().join("agentbench")).unwrap();
        std::fs::write(data.path().join("agentbench/app.ron"), "x").unwrap();
        assert_eq!(home_in(data.path()), old);
        assert_eq!(home_file(&old, "agentbench.db", "foreman.db"), old.join("foreman.db"));
        assert_eq!(home_file(&old, "agentbench.toml", "foreman.toml"), old.join("foreman.toml"));
        std::fs::write(old.join("agentbench.db"), "x").unwrap();
        assert_eq!(home_file(&old, "agentbench.db", "foreman.db"), old.join("agentbench.db"));
    }

    #[test]
    fn the_token_is_made_once_and_kept() {
        let s = store::Store::memory().unwrap();
        let t = token(&s).unwrap();
        assert_eq!(t.len(), 48);
        assert_eq!(token(&s).unwrap(), t);
    }
}
