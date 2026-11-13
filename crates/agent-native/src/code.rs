use agentbench_core::{Lsp, LspHealth};
pub use agent_core::code::{Code, Report, ServerSpec, Settings, render, specs};
use agent_core::code::{Health, Status};
use std::path::Path;

pub const TOOLS: [&str; 3] = ["code_outline", "code_find", "code_diagnostics"];

pub fn health(h: Health) -> LspHealth {
    match h {
        Health::Idle => LspHealth::Idle,
        Health::Starting => LspHealth::Starting,
        Health::Indexing => LspHealth::Indexing,
        Health::Ready => LspHealth::Ready,
        Health::Missing => LspHealth::Missing,
        Health::Failed => LspHealth::Failed,
    }
}

fn lsp(s: Status) -> Lsp {
    Lsp { name: s.name, dir: s.dir, health: health(s.health), detail: s.detail }
}

pub fn status(code: &Code, root: &Path) -> Vec<Lsp> {
    code.status(root).into_iter().map(lsp).collect()
}

pub fn unconfined(mut settings: Settings) -> Settings {
    settings.confine = agent_core::spawn::Confine::Open { env: Vec::new() };
    settings.configured_in = "[[code.lsp]] in agentbench.toml".into();
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_health_maps_to_the_wire_type() {
        for (h, want) in [
            (Health::Idle, LspHealth::Idle),
            (Health::Starting, LspHealth::Starting),
            (Health::Indexing, LspHealth::Indexing),
            (Health::Ready, LspHealth::Ready),
            (Health::Missing, LspHealth::Missing),
            (Health::Failed, LspHealth::Failed),
        ] {
            assert_eq!(health(h), want);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_server_points_at_agentbench_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.nosuchlang"), "x").unwrap();
        let ws = agent_core::workspace::Workspace::open(dir.path()).unwrap();
        let code = std::sync::Arc::new(Code::new(unconfined(Settings::default())));
        let stop = tokio_util::sync::CancellationToken::new();
        let err = code
            .run("code_outline", &serde_json::json!({"path": "a.nosuchlang"}), &ws, &stop)
            .await
            .unwrap_err();
        assert!(err.contains("agentbench.toml"), "{err}");
    }
}
