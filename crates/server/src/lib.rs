pub mod agent;
pub mod api;
pub mod cli;
pub mod git;
pub mod http;
pub mod local;
pub mod projects;
pub mod runner;
pub mod store;
pub mod titles;
pub mod tools;

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

pub fn home() -> PathBuf {
    std::env::var_os("FOREMAN_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("foreman"))
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

    /// The agents, with the foreman agent answering web page prompts using `titles`.
    pub fn agents_with_titles(
        &self,
        model: agent::native::Shared,
        titles: Option<titles::Titles>,
    ) -> Vec<Box<dyn Agent>> {
        let mut native = agent::Native::shared(model);
        native.titles = titles;
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

/// The title model last saved from the window, else the one in `foreman.toml`.
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
    let agents = config.agents_with_titles(model.clone(), Some(titles.clone()));
    serve_inner(store, agents, Some(model), Some(titles), listen, web, shutdown).await
}

pub async fn serve_with(
    store: store::Store,
    agents: Vec<Box<dyn Agent>>,
    listen: SocketAddr,
    web: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    serve_inner(store, agents, None, None, listen, web, shutdown).await
}

pub async fn serve_agents(
    store: store::Store,
    agents: Vec<Box<dyn Agent>>,
    model: Option<agent::native::Shared>,
    listen: SocketAddr,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    serve_inner(store, agents, model, None, listen, None, shutdown).await
}

async fn serve_inner(
    store: store::Store,
    agents: Vec<Box<dyn Agent>>,
    model: Option<agent::native::Shared>,
    titles: Option<titles::Titles>,
    listen: SocketAddr,
    web: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let stale = store.reset_running().map_err(|e| e.0)?;
    if stale > 0 {
        tracing::info!("{stale} sessions were running when foreman last stopped");
    }
    let token = token(&store).map_err(|e| e.0)?;
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
    let api = api::Api { runner, token, model };
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
        assert_eq!(names, ["foreman", "pi", "claude", "aider", "codex"]);
    }

    #[test]
    fn the_token_is_made_once_and_kept() {
        let s = store::Store::memory().unwrap();
        let t = token(&s).unwrap();
        assert_eq!(t.len(), 48);
        assert_eq!(token(&s).unwrap(), t);
    }
}
