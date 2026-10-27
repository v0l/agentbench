use agentbench_core::{Lsp, LspHealth};
pub use egui_bench::agent::code::{Code, Report, ServerSpec, Settings, render, specs};
use egui_bench::agent::code::{Health, Status};
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
    settings.confine = egui_bench::agent::spawn::Confine::Open { env: Vec::new() };
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

    #[test]
    fn the_config_reads_from_toml_and_runs_servers_as_the_user() {
        let c: crate::Config = toml::from_str(
            "[code]\nrole = \"developer\"\ndiagnostics = false\n\n[[code.lsp]]\nname = \"zig\"\ncommand = \"zls\"\nextensions = [\"zig\"]\nmarkers = [\"build.zig\"]\n[code.lsp.ids]\nzon = \"zon\"\n",
        )
        .unwrap();
        assert_eq!(c.code.role, "developer");
        assert!(!c.code.diagnostics);
        assert_eq!(c.code.lsp[0].extensions, ["zig"]);
        assert_eq!(c.code.lsp[0].ids["zon"], "zon");
        let d: crate::Config = toml::from_str("").unwrap();
        assert!(d.code.diagnostics && d.code.role == "user" && d.code.lsp.is_empty());
        let open = unconfined(d.code);
        assert!(!open.confine.sandboxed());
        assert!(open.configured_in.contains("agentbench.toml"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_server_points_at_agentbench_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.nosuchlang"), "x").unwrap();
        let ws = egui_bench::agent::workspace::Workspace::open(dir.path()).unwrap();
        let code = std::sync::Arc::new(Code::new(unconfined(Settings::default())));
        let stop = tokio_util::sync::CancellationToken::new();
        let err = code
            .run("code_outline", &serde_json::json!({"path": "a.nosuchlang"}), &ws, &stop)
            .await
            .unwrap_err();
        assert!(err.contains("agentbench.toml"), "{err}");
    }
}
