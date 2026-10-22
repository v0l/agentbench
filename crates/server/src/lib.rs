pub mod agent;
pub mod api;
pub mod git;
pub mod projects;
pub mod runner;
pub mod store;

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
    #[serde(default, rename = "agent")]
    pub agents: Vec<JsonCommand>,
}

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7420";

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
        let mut all: Vec<Box<dyn Agent>> = vec![
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

pub async fn serve_with(
    store: store::Store,
    agents: Vec<Box<dyn Agent>>,
    listen: SocketAddr,
    web: Option<PathBuf>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    let stale = store.reset_running().map_err(|e| e.0)?;
    if stale > 0 {
        tracing::info!("{stale} sessions were running when foreman last stopped");
    }
    let token = token(&store).map_err(|e| e.0)?;
    let api = api::Api { runner: runner::Runner::new(store, agents), token };
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
        assert_eq!(names, ["pi", "claude", "aider", "codex"]);
        assert_eq!(agents[0].program(), Path::new("/opt/pi"));
        assert_eq!(agents[3].program(), Path::new("/usr/local/bin/my-codex"));
    }

    #[test]
    fn the_token_is_made_once_and_kept() {
        let s = store::Store::memory().unwrap();
        let t = token(&s).unwrap();
        assert_eq!(t.len(), 48);
        assert_eq!(token(&s).unwrap(), t);
    }
}
