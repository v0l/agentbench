pub mod browser;
mod claude;
pub mod code;
mod codex;
mod command;
pub mod files;
pub mod media;
pub mod native;
mod pi;
mod process;
pub mod screenshot;
pub mod search;

pub use async_trait::async_trait;
pub use claude::Claude;
pub use codex::Codex;
pub use command::JsonCommand;
pub use native::Native;
pub use pi::Pi;
pub use process::{Cli, Launch};
pub use tokio_util::sync::CancellationToken;

use agentbench_core::{AgentInfo, Entry, Question, SessionId, Usage};
use serde_json::Value;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct Context {
    pub session: SessionId,
    pub project_id: agentbench_core::Id,
    pub project: String,
    pub project_path: String,
    pub branch: Option<String>,
    /// A chat in the user's home folder, which belongs to no git repository.
    pub home: bool,
    pub issue: Option<agentbench_core::Id>,
    pub api_url: String,
    pub token: String,
    /// What was remembered about the project with `remember`: topic and note.
    pub notes: Vec<(String, String)>,
}

/// A file the user sent with a message, written out where an agent can read it.
#[derive(Clone, Debug, PartialEq)]
pub struct Attached {
    pub name: String,
    pub mime: String,
    pub path: PathBuf,
}

/// A message sent while the agent works, and anything sent with it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Steer {
    pub text: String,
    pub files: Vec<Attached>,
}

impl From<&str> for Steer {
    fn from(text: &str) -> Steer {
        Steer { text: text.to_string(), files: Vec::new() }
    }
}

/// The prompt with a line naming each attached file, for agents that read files themselves.
pub fn with_files(prompt: &str, files: &[Attached]) -> String {
    if files.is_empty() {
        return prompt.to_string();
    }
    let mut out = prompt.to_string();
    out.push_str("\n\nAttached files, read them from these paths:\n");
    for f in files {
        out.push_str(&format!("- {} ({}): {}\n", f.name, f.mime, f.path.display()));
    }
    out
}

pub struct Turn<'a> {
    pub prompt: &'a str,
    pub files: &'a [Attached],
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub resume: Option<&'a str>,
    pub history: &'a [Entry],
    pub context: &'a Context,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Started { agent_session: String },
    Text { delta: String },
    Thinking { text: String },
    ToolStart { call_id: String, name: String, input: Value },
    ToolEnd { call_id: String, name: String, output: String, error: bool },
    Steered { text: String },
    Usage(Usage),
    Notice(String),
    Diagnostics { text: String, broken: Vec<String> },
    Sent(Vec<agentbench_core::Sent>),
    Error(String),
    Done,
}

pub trait Decoder: Send {
    fn line(&mut self, line: &str) -> Vec<Event>;
}

#[async_trait]
pub trait Io: Send + Sync {
    fn emit(&self, event: Event);
    /// Cancelled when the user stops the turn; anything waiting should race it.
    fn stop(&self) -> &CancellationToken;
    fn stopped(&self) -> bool {
        self.stop().is_cancelled()
    }
    fn steer(&self) -> Option<Steer>;
    async fn ask(&self, question: Question) -> Result<String, String>;
    /// Put several questions at once; one answer back for each, in order.
    async fn ask_many(&self, questions: Vec<Question>) -> Result<Vec<String>, String>;
    fn api(&self) -> &dyn Api;
}

pub trait Api: Send + Sync {
    fn call(&self, tool: &str, args: &Value, session: SessionId) -> Result<Value, String>;
}

#[async_trait]
pub trait Agent: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn available(&self) -> bool;
    fn about(&self) -> String;
    fn resumable(&self) -> bool;
    fn steerable(&self) -> bool {
        false
    }
    fn writes_log(&self) -> bool {
        false
    }
    /// Once a chat's context is this many percent full, agentbench summarises it and starts the
    /// agent afresh. 0 for agents that manage their own context.
    fn compact_at(&self) -> u64 {
        0
    }
    async fn run(&self, turn: &Turn<'_>, io: &dyn Io) -> Result<(), String>;

    fn end(&self, _session: SessionId) {}

    fn jobs(&self, _session: SessionId) -> Vec<agentbench_core::Job> {
        Vec::new()
    }

    async fn kill_job(&self, _session: SessionId, _handle: &str) -> Result<(), String> {
        Err("this agent has no background commands".into())
    }

    /// The models this agent can be told to use. Empty when it cannot say, and then the
    /// window offers a plain text field instead of a dropdown.
    async fn models(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    fn info(&self) -> AgentInfo {
        AgentInfo {
            name: self.name().to_string(),
            label: self.label().to_string(),
            available: self.available(),
            resumable: self.resumable(),
            about: self.about(),
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

pub fn brief(c: &Context) -> String {
    if c.home {
        return home_brief(c);
    }
    let mut out = format!(
        "You are running inside agentbench, a harness that runs coding agents in git worktrees and keeps an issue tracker beside them. \
         This is session {} in the project {} ({}).",
        c.session, c.project, c.project_path
    );
    if let Some(b) = &c.branch {
        out.push_str(&format!(" You are working on the branch {b}."));
    }
    if let Some(i) = c.issue {
        out.push_str(&format!(" You were started on issue #{i}."));
    }
    out.push_str(&format!(
        "\n\nThe agentbench REST API is at {url} and needs the header `Authorization: Bearer $AGENTBENCH_TOKEN` \
         (also in the environment as AGENTBENCH_URL, AGENTBENCH_TOKEN, AGENTBENCH_SESSION and AGENTBENCH_PROJECT). \
         Use it to keep the tracker true:\n\
         - GET {url}/api/issues lists issues; filter by project_id {pid}.\n\
         - POST {url}/api/issues with {{\"project_id\": {pid}, \"title\", \"body\"}} files one when you find work that is out of scope.\n\
         - PATCH {url}/api/issues/ID with {{\"state\": \"review\"}} when you finish one, or \"done\" when it is merged.\n\
         - POST {url}/api/issues/ID/comments with {{\"body\"}} to leave a note for the user.\n\
         Other agents may be working in other worktrees of this project. Stay in your own worktree. \
         When you need another worktree, create it with git worktree add beside the main checkout and agentbench will pick it up.",
        url = c.api_url.trim_end_matches('/'),
        pid = c.project_id,
    ));
    out
}

/// The brief for a chat in the home folder: no repository, branch or worktrees, just a place to
/// do whatever the user asks.
fn home_brief(c: &Context) -> String {
    format!(
        "You are running inside agentbench, a harness that runs agents and keeps an issue tracker beside them. \
         This is session {session}, a general chat in the user's home folder ({path}). It is not tied to a git repository: \
         expect odd jobs such as parsing or converting files, running skills, looking things up or small scripts. \
         Work wherever the task is, and keep files you make out of the way unless the user wants them.\n\n\
         The agentbench REST API is at {url} and needs the header `Authorization: Bearer $AGENTBENCH_TOKEN` \
         (also in the environment as AGENTBENCH_URL, AGENTBENCH_TOKEN and AGENTBENCH_SESSION). \
         It holds the user's projects and issues; use it only if asked to.",
        session = c.session,
        path = c.project_path,
        url = c.api_url.trim_end_matches('/'),
    )
}

impl Context {
    pub fn env(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("URL", self.api_url.clone()),
            ("TOKEN", self.token.clone()),
            ("SESSION", self.session.to_string()),
            ("PROJECT", self.project_id.to_string()),
        ];
        if let Some(i) = self.issue {
            pairs.push(("ISSUE", i.to_string()));
        }
        pairs.into_iter().map(|(k, v)| (format!("AGENTBENCH_{k}"), v)).collect()
    }
}

pub fn defaults() -> Vec<Box<dyn Agent>> {
    vec![
        Box::new(Native::default()),
        Box::new(Pi::new("pi")),
        Box::new(Claude::new("claude")),
        Box::new(Codex::new("codex")),
    ]
}

#[cfg(test)]
pub struct TestIo {
    pub events: Mutex<Vec<Event>>,
    pub stop: CancellationToken,
    pub inbox: Mutex<Vec<Steer>>,
    pub answers: Mutex<Vec<String>>,
    pub api: Arc<dyn Api>,
}

#[cfg(test)]
impl TestIo {
    pub fn new(api: Arc<dyn Api>) -> TestIo {
        TestIo {
            events: Mutex::new(Vec::new()),
            stop: CancellationToken::new(),
            inbox: Mutex::new(Vec::new()),
            answers: Mutex::new(Vec::new()),
            api,
        }
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

#[cfg(test)]
#[async_trait]
impl Io for TestIo {
    fn emit(&self, event: Event) {
        self.events.lock().unwrap().push(event);
    }
    fn stop(&self) -> &CancellationToken {
        &self.stop
    }
    fn steer(&self) -> Option<Steer> {
        let mut inbox = self.inbox.lock().unwrap();
        (!inbox.is_empty()).then(|| inbox.remove(0))
    }
    async fn ask(&self, _question: Question) -> Result<String, String> {
        let mut a = self.answers.lock().unwrap();
        if a.is_empty() { Err("no answer".into()) } else { Ok(a.remove(0)) }
    }
    async fn ask_many(&self, questions: Vec<Question>) -> Result<Vec<String>, String> {
        let mut a = self.answers.lock().unwrap();
        if a.len() < questions.len() {
            return Err("no answer".into());
        }
        Ok(a.drain(..questions.len()).collect())
    }
    fn api(&self) -> &dyn Api {
        self.api.as_ref()
    }
}

#[cfg(test)]
pub struct NoApi;

#[cfg(test)]
impl Api for NoApi {
    fn call(&self, tool: &str, _args: &Value, _session: SessionId) -> Result<Value, String> {
        Err(format!("no api for {tool}"))
    }
}

#[cfg(test)]
pub fn context() -> Context {
    Context {
        session: test_session(),
        project_id: 4,
        project: "demo".into(),
        project_path: "/tmp/demo".into(),
        branch: Some("main".into()),
        home: false,
        issue: None,
        api_url: "http://127.0.0.1:7420".into(),
        token: "t".into(),
        notes: Vec::new(),
    }
}

#[cfg(test)]
pub fn test_session() -> SessionId {
    "00000000-0000-4000-8000-000000000001".parse().unwrap()
}

#[cfg(test)]
pub fn decode_all(agent: &dyn Cli, lines: &[&str]) -> Vec<Event> {
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

    #[test]
    fn the_brief_names_the_session_and_the_api() {
        let b = brief(&context());
        assert!(b.contains("inside agentbench"));
        assert!(b.contains(&format!("session {} in the project demo", test_session())));
        assert!(b.contains("http://127.0.0.1:7420/api/issues"));
        assert!(b.contains("branch main"));
        assert!(b.contains("\"project_id\": 4"));
    }

    #[test]
    fn the_brief_for_the_home_folder_has_no_repository_in_it() {
        let b = brief(&Context { home: true, project_path: "/home/me".into(), ..context() });
        assert!(b.contains("general chat in the user's home folder (/home/me)"), "{b}");
        assert!(b.contains(&test_session().to_string()));
        assert!(!b.contains("worktree") && !b.contains("branch"), "{b}");
    }

    #[test]
    fn attached_files_are_named_with_their_paths_for_cli_agents() {
        let files = [Attached {
            name: "shot.png".into(),
            mime: "image/png".into(),
            path: PathBuf::from("/tmp/agentbench/x/1.png"),
        }];
        let p = with_files("look", &files);
        assert!(p.starts_with("look\n\nAttached files"), "{p}");
        assert!(p.contains("- shot.png (image/png): /tmp/agentbench/x/1.png"), "{p}");
        assert_eq!(with_files("look", &[]), "look");
    }
}
