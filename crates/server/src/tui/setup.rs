use crate::Config;
use crate::agent::native::{Model, Native, Scope, Shared};
use crate::memory::{Memory, Scope as Notes};
use agent_native::Context;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flags {
    pub memory: bool,
    pub skills: bool,
    pub mcp: bool,
    pub save: bool,
}

impl Default for Flags {
    fn default() -> Self {
        Flags { memory: true, skills: true, mcp: true, save: false }
    }
}

impl Flags {
    pub fn bare() -> Flags {
        Flags { memory: false, skills: false, mcp: false, save: false }
    }

    pub fn scope(self) -> Scope {
        Scope { tracked: false, memory: self.memory, skills: self.skills, mcp: self.mcp }
    }
}

pub struct Loaded {
    pub native: Native,
    pub context: Context,
}

fn database(home: &Path) -> Option<rusqlite::Connection> {
    let path = home.join("agentbench.db");
    if !path.is_file() {
        return None;
    }
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

fn saved_model(db: Option<&rusqlite::Connection>, fallback: &Model) -> Model {
    db.and_then(|db| {
        db.query_row("SELECT value FROM settings WHERE key = 'model'", [], |r| r.get::<_, String>(0))
            .ok()
    })
    .and_then(|text| serde_json::from_str(&text).ok())
    .unwrap_or_else(|| fallback.clone())
}

fn project_of(db: &rusqlite::Connection, cwd: &Path) -> Option<(i64, String)> {
    cwd.ancestors().find_map(|dir| {
        db.query_row(
            "SELECT id, name FROM projects WHERE path = ?1",
            [dir.to_string_lossy().as_ref()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
    })
}

pub fn load(home: &Path, cwd: &Path, model: Option<&str>, flags: Flags) -> Result<Loaded, String> {
    let config = Config::load(&home.join("agentbench.toml"))?;
    let db = database(home);
    let base = saved_model(db.as_ref(), &config.model);
    if base.choose(model.unwrap_or_default()).model.trim().is_empty() {
        return Err("no model is set: pick one in the agentbench window, or put [model] in agentbench.toml".into());
    }
    let shared = Shared::new(base);
    let mut native = Native::shared(shared.clone());
    native.scope = flags.scope();
    native.pace.prices = config.prices.clone();
    native.code = Arc::new(crate::agent::code::Code::new(crate::agent::code::unconfined(
        config.code.clone(),
    )));
    native.reader = Some(Arc::new(crate::titles::Titles::new(config.title_model.clone(), shared)));
    let found = db.as_ref().and_then(|db| project_of(db, cwd));
    let name = found.as_ref().map(|(_, n)| n.clone()).unwrap_or_else(|| {
        cwd.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    });
    let (mut notes, mut global_notes) = (Vec::new(), Vec::new());
    if flags.memory {
        let memory = Memory::open(crate::memory::default_root());
        global_notes = memory.list(Notes::Global);
        if let Some((id, _)) = &found {
            notes = memory.list(Notes::Project(*id));
        }
    }
    let context = Context {
        session: agentbench_core::SessionId(uuid::Uuid::new_v4()),
        project_id: 0,
        project: name,
        project_path: cwd.to_string_lossy().into_owned(),
        branch: None,
        home: false,
        supervisor: false,
        issue: None,
        api_url: String::new(),
        token: String::new(),
        notes,
        global_notes,
        automation: String::new(),
        quota: String::new(),
        vision: String::new(),
    };
    Ok(Loaded { native, context })
}

pub fn transcripts(home: &Path) -> PathBuf {
    home.join("chats")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_reads_the_config_and_creates_nothing_in_the_data_dir() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("agentbench.toml"), "[model]\nmodel = \"fake\"\nurl = \"http://127.0.0.1:1/v1\"\n").unwrap();
        let before: Vec<_> = std::fs::read_dir(home.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        let loaded = load(home.path(), cwd.path(), None, Flags::bare()).unwrap();
        assert_eq!(loaded.native.scope, Flags::bare().scope());
        assert!(!loaded.native.scope.tracked);
        assert!(loaded.context.api_url.is_empty() && loaded.context.notes.is_empty());
        let after: Vec<_> = std::fs::read_dir(home.path()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(before, after);
        assert!(!home.path().join("agentbench.db").exists());
    }

    #[test]
    fn no_model_is_an_error_that_says_where_to_set_one() {
        let home = tempfile::tempdir().unwrap();
        let err = load(home.path(), home.path(), None, Flags::default()).err().unwrap();
        assert!(err.contains("no model is set"), "{err}");
    }

    #[test]
    fn the_flags_map_to_the_scope() {
        let scope = Flags { memory: false, ..Flags::default() }.scope();
        assert!(!scope.memory && scope.skills && scope.mcp && !scope.tracked);
    }
}
