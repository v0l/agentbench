use agentbench_server::agent::native::system;
use agentbench_server::agent::{Context, Turn};
use agentbench_server::store::Store;
use agentbench_server::{Config, home, home_file, saved_model};
use std::process::Command;

fn main() -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let home = home();
    let config = Config::load(&home_file(&home, "agentbench.toml", "foreman.toml"))?;
    let store = Store::open(&home_file(&home, "agentbench.db", "foreman.db")).map_err(|e| e.0)?;
    let model = saved_model(&store, &config.model);

    let project = store.project_by_path(&cwd.to_string_lossy()).map_err(|e| e.0)?;
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let (project_id, name, notes) = match &project {
        Some(p) => (p.id, p.name.clone(), store.notes(p.id).map_err(|e| e.0)?),
        None => (
            0,
            cwd.file_name().map_or_else(String::new, |n| n.to_string_lossy().into()),
            Vec::new(),
        ),
    };
    let context = Context {
        session: agentbench_core::SessionId(uuid::Uuid::nil()),
        project_id,
        project: name,
        project_path: cwd.to_string_lossy().into(),
        branch,
        home: false,
        issue: None,
        api_url: format!(
            "http://127.0.0.1:{}",
            agentbench_server::DEFAULT_LISTEN.rsplit(':').next().unwrap_or("7420")
        ),
        token: "<token>".into(),
        notes,
    };
    let turn = Turn {
        prompt: "",
        files: &[],
        cwd: &cwd,
        model: None,
        resume: None,
        history: &[],
        context: &context,
    };
    print!("{}", system(&turn, &model));
    Ok(())
}
