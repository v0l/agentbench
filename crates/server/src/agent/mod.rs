mod claude;
mod codex;
mod command;
mod pi;

pub use claude::Claude;
pub use codex::Codex;
pub use command::JsonCommand;
pub use pi::Pi;

use foreman_core::{AgentInfo, Usage};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct Turn<'a> {
    pub prompt: &'a str,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    pub resume: Option<&'a str>,
}

pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Started { agent_session: String },
    Text { delta: String },
    Thinking { text: String },
    ToolStart { call_id: String, name: String, input: Value },
    ToolEnd { call_id: String, name: String, output: String, error: bool },
    Usage(Usage),
    Notice(String),
    Error(String),
    Done,
}

pub trait Decoder: Send {
    fn line(&mut self, line: &str) -> Vec<Event>;
}

pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn program(&self) -> &Path;
    fn resumable(&self) -> bool;
    fn launch(&self, turn: &Turn) -> Launch;
    fn decoder(&self) -> Box<dyn Decoder>;

    fn available(&self) -> bool {
        resolve(self.program()).is_some()
    }

    fn info(&self) -> AgentInfo {
        AgentInfo {
            name: self.name().to_string(),
            label: self.label().to_string(),
            available: self.available(),
            resumable: self.resumable(),
        }
    }
}

pub fn resolve(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(program)).find(|p| p.is_file())
}

pub fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

pub fn u64_at(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

pub fn parse(line: &str) -> Option<Value> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    serde_json::from_str(line).ok()
}

pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                b.as_str().map(str::to_string).or_else(|| str_at(b, "text").map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn defaults() -> Vec<Box<dyn Agent>> {
    vec![Box::new(Pi::new("pi")), Box::new(Claude::new("claude")), Box::new(Codex::new("codex"))]
}

#[cfg(test)]
pub fn decode_all(agent: &dyn Agent, lines: &[&str]) -> Vec<Event> {
    let mut d = agent.decoder();
    lines.iter().flat_map(|l| d.line(l)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_text_reads_strings_and_blocks() {
        assert_eq!(content_text(&serde_json::json!("hi")), "hi");
        assert_eq!(
            content_text(
                &serde_json::json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])
            ),
            "a\nb"
        );
        assert_eq!(content_text(&Value::Null), "");
    }

    #[test]
    fn parse_skips_non_json_lines() {
        assert!(parse("Reading additional input from stdin...").is_none());
        assert!(parse("  {\"a\":1}  ").is_some());
        assert!(parse("{broken").is_none());
    }

    #[test]
    fn resolve_finds_programs_on_path_and_by_path() {
        assert!(resolve(Path::new("sh")).is_some());
        assert!(resolve(Path::new("/bin/sh")).is_some());
        assert!(resolve(Path::new("definitely-not-a-real-binary-xyz")).is_none());
    }

    #[test]
    fn every_default_agent_has_a_distinct_name() {
        let agents = defaults();
        let mut names: Vec<_> = agents.iter().map(|a| a.name().to_string()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), agents.len());
    }
}
