pub mod pairing;
pub mod prompts;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Id = i64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoteVersion {
    pub id: Id,
    pub topic: String,
    pub body: String,
    pub hash: String,
    pub author: String,
    pub session: Option<SessionId>,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoteHistory {
    pub topic: String,
    pub current: Option<String>,
    pub versions: Vec<NoteVersion>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryCompactRequest {
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub project: Option<Id>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryScopeReport {
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub before: usize,
    #[serde(default)]
    pub after: usize,
    #[serde(default)]
    pub skipped: Option<String>,
    #[serde(default)]
    pub rewritten: Vec<String>,
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub merged: Vec<String>,
    #[serde(default)]
    pub dropped: Vec<String>,
    #[serde(default)]
    pub redacted: Vec<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MemoryCompaction {
    #[serde(default)]
    pub scopes: Vec<MemoryScopeReport>,
}

impl MemoryCompaction {
    pub fn summary(&self) -> String {
        let lines: Vec<String> = self
            .scopes
            .iter()
            .map(|s| match &s.skipped {
                Some(why) => format!("{}: left alone ({why})", s.scope),
                None => format!(
                    "{}: {} to {} bytes, {} merged, {} dropped",
                    s.scope,
                    s.before,
                    s.after,
                    s.merged.len(),
                    s.dropped.len()
                ),
            })
            .collect();
        if lines.is_empty() { "no notes to compact".into() } else { lines.join("; ") }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySettings {
    #[serde(default = "default_note_budget")]
    pub note_budget: usize,
    #[serde(default = "default_scope_budget")]
    pub scope_budget: usize,
    #[serde(default = "default_auto_compact")]
    pub auto: bool,
    #[serde(default)]
    pub model: String,
}

fn default_note_budget() -> usize {
    DEFAULT_NOTE_BUDGET
}

fn default_scope_budget() -> usize {
    DEFAULT_SCOPE_BUDGET
}

fn default_auto_compact() -> bool {
    true
}

pub const DEFAULT_NOTE_BUDGET: usize = 4096;
pub const DEFAULT_SCOPE_BUDGET: usize = 12288;

impl Default for MemorySettings {
    fn default() -> Self {
        MemorySettings { note_budget: DEFAULT_NOTE_BUDGET, scope_budget: DEFAULT_SCOPE_BUDGET, auto: true, model: String::new() }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    #[default]
    Active,
    Done,
    Dropped,
}

impl GoalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            GoalStatus::Active => "active",
            GoalStatus::Done => "done",
            GoalStatus::Dropped => "dropped",
        }
    }

    pub fn parse(s: &str) -> Option<GoalStatus> {
        match s.trim().to_ascii_lowercase().as_str() {
            "active" => Some(GoalStatus::Active),
            "done" => Some(GoalStatus::Done),
            "dropped" => Some(GoalStatus::Dropped),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub text: String,
    #[serde(default)]
    pub status: GoalStatus,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VisionBody {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub goals: Vec<Goal>,
    #[serde(default)]
    pub non_goals: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Vision {
    #[serde(default)]
    pub id: Id,
    #[serde(default)]
    pub project_id: Id,
    #[serde(flatten)]
    pub body: VisionBody,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub updated_at: i64,
}

impl Vision {
    pub fn active_goals(&self) -> impl Iterator<Item = &Goal> {
        self.body.goals.iter().filter(|g| g.status == GoalStatus::Active)
    }

    pub fn goal(&self, id: &str) -> Option<&Goal> {
        self.body.goals.iter().find(|g| g.id == id)
    }

    pub fn brief(&self) -> String {
        let mut out = format!("Vision: {}\n", self.body.summary.trim());
        for g in self.active_goals() {
            out.push_str(&format!("- goal {}: {}\n", g.id, g.text.trim()));
        }
        if !self.body.non_goals.is_empty() {
            out.push_str(&format!("Non-goals: {}\n", self.body.non_goals.join("; ")));
        }
        out
    }
}

/// A chat's id. Never reused, so nothing cached under one id can surface in another chat.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SessionId(pub uuid::Uuid);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for SessionId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        uuid::Uuid::parse_str(s.trim()).map(SessionId)
    }
}

#[cfg(feature = "rusqlite")]
impl rusqlite::ToSql for SessionId {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.0.to_string()))
    }
}

#[cfg(feature = "rusqlite")]
impl rusqlite::types::FromSql for SessionId {
    fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        let text = v.as_str()?;
        text.parse().map_err(|e| rusqlite::types::FromSqlError::Other(Box::new(e)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Backlog,
    Todo,
    Doing,
    Review,
    Done,
    #[serde(other)]
    Unknown,
}

impl IssueState {
    pub const ALL: [IssueState; 5] = [
        IssueState::Backlog,
        IssueState::Todo,
        IssueState::Doing,
        IssueState::Review,
        IssueState::Done,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            IssueState::Backlog => "backlog",
            IssueState::Todo => "todo",
            IssueState::Doing => "doing",
            IssueState::Review => "review",
            IssueState::Done => "done",
            IssueState::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<IssueState> {
        IssueState::ALL.into_iter().find(|state| state.as_str() == s)
    }

    pub fn is_open(self) -> bool {
        self != IssueState::Done
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: Id,
    pub name: String,
    pub path: String,
    pub branch: Option<String>,
    pub remote: Option<String>,
    pub created_at: i64,
    /// The built-in "home" project: the user's home folder, for chats that belong to no git
    /// repository. It has one fixed checkout and is never scanned for worktrees.
    #[serde(default)]
    pub home: bool,
    /// The built-in supervisor chat's project: one plain folder, no repository, whose session
    /// sees every project, issue and session.
    #[serde(default)]
    pub supervisor: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub automation: Option<Automation>,
    #[serde(default)]
    pub risky: Vec<Risky>,
    #[serde(default)]
    pub deploy: Option<String>,
    #[serde(default)]
    pub tracker: Tracker,
    #[serde(default)]
    pub post_mode: PostMode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Tracker {
    Github {
        #[serde(default)]
        repo: String,
    },
    #[default]
    #[serde(other)]
    Internal,
}

impl Tracker {
    pub fn is_github(&self) -> bool {
        matches!(self, Tracker::Github { .. })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostMode {
    Auto,
    Off,
    #[default]
    #[serde(other)]
    Draft,
}

impl PostMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PostMode::Draft => "draft",
            PostMode::Auto => "auto",
            PostMode::Off => "off",
        }
    }

    pub fn parse(s: &str) -> Option<PostMode> {
        [PostMode::Draft, PostMode::Auto, PostMode::Off].into_iter().find(|m| m.as_str() == s)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueSource {
    Github,
    #[default]
    #[serde(other)]
    Internal,
}

impl IssueSource {
    pub fn as_str(self) -> &'static str {
        match self {
            IssueSource::Internal => "internal",
            IssueSource::Github => "github",
        }
    }

    pub fn parse(s: &str) -> IssueSource {
        if s == "github" { IssueSource::Github } else { IssueSource::Internal }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GithubStatus {
    #[serde(default)]
    pub tracker: Tracker,
    #[serde(default)]
    pub post_mode: PostMode,
    #[serde(default)]
    pub installed: bool,
    #[serde(default)]
    pub authenticated: bool,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub write_access: Option<bool>,
    #[serde(default)]
    pub rate_limit_remaining: Option<i64>,
    #[serde(default)]
    pub last_sync: Option<i64>,
    #[serde(default)]
    pub problem: Option<String>,
}

impl Project {
    pub fn builtin(&self) -> bool {
        self.home || self.supervisor
    }

    pub fn level(&self, default: Automation) -> Automation {
        self.automation.unwrap_or(default)
    }

    pub fn allows(&self, default: Automation, action: Risky) -> bool {
        self.level(default) == Automation::FullAuto && self.risky.contains(&action)
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Automation {
    Manual,
    Assist,
    #[default]
    Delegate,
    Autowork,
    Ship,
    FullAuto,
}

impl Automation {
    pub const ALL: [Automation; 6] = [
        Automation::Manual,
        Automation::Assist,
        Automation::Delegate,
        Automation::Autowork,
        Automation::Ship,
        Automation::FullAuto,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Automation::Manual => "manual",
            Automation::Assist => "assist",
            Automation::Delegate => "delegate",
            Automation::Autowork => "autowork",
            Automation::Ship => "ship",
            Automation::FullAuto => "full_auto",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Automation::FullAuto => "full auto",
            other => other.as_str(),
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Automation::Manual => "only tracks and reports; starts, sends, merges and closes nothing unasked",
            Automation::Assist => "files issues, comments, keeps states true and proposes next work",
            Automation::Delegate => "starts workers on named issues, sends sessions back, closes finished ones",
            Automation::Autowork => "the autopilot picks todo issues by priority and keeps them moving",
            Automation::Ship => "autowork, and merges concluded branches into the default branch and pushes",
            Automation::FullAuto => "ship, and the risky actions switched on for the project",
        }
    }

    pub fn rank(self) -> u8 {
        self as u8
    }

    pub fn from_rank(rank: u8) -> Automation {
        Automation::ALL[usize::from(rank).min(Automation::ALL.len() - 1)]
    }

    pub fn parse(s: &str) -> Option<Automation> {
        let t = s.trim().to_lowercase().replace([' ', '-'], "_");
        if let Ok(n) = t.parse::<u8>() {
            return (usize::from(n) < Automation::ALL.len()).then(|| Automation::from_rank(n));
        }
        let t = if t == "fullauto" || t == "full" { "full_auto".to_string() } else { t };
        Automation::ALL.into_iter().find(|a| a.as_str() == t)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risky {
    Deploy,
    ProdData,
    ExternalMessages,
    Publish,
    SpendMoney,
}

impl Risky {
    pub const ALL: [Risky; 5] =
        [Risky::Deploy, Risky::ProdData, Risky::ExternalMessages, Risky::Publish, Risky::SpendMoney];

    pub fn as_str(self) -> &'static str {
        match self {
            Risky::Deploy => "deploy",
            Risky::ProdData => "prod_data",
            Risky::ExternalMessages => "external_messages",
            Risky::Publish => "publish",
            Risky::SpendMoney => "spend_money",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Risky::Deploy => "deploy to production",
            Risky::ProdData => "prod data changes",
            Risky::ExternalMessages => "external messages",
            Risky::Publish => "publish releases",
            Risky::SpendMoney => "spend money",
        }
    }

    pub fn parse(s: &str) -> Option<Risky> {
        let t = s.trim().to_lowercase().replace([' ', '-'], "_");
        Risky::ALL.into_iter().find(|r| r.as_str() == t)
    }
}

fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Worktree {
    pub id: Id,
    pub project_id: Id,
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub main: bool,
    pub missing: bool,
    pub owned: bool,
    pub created_at: i64,
}

impl Worktree {
    pub fn label(&self) -> String {
        match &self.branch {
            Some(b) => b.clone(),
            None => {
                self.head.as_deref().map(|h| h.chars().take(8).collect()).unwrap_or("folder".into())
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub id: Id,
    pub project_id: Id,
    pub title: String,
    pub body: String,
    pub state: IssueState,
    pub priority: i64,
    pub labels: Vec<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub off_vision: bool,
    #[serde(default)]
    pub depends_on: Vec<Id>,
    #[serde(default)]
    pub waiting_on: Vec<Id>,
    #[serde(default)]
    pub blocks: Vec<Id>,
    #[serde(default)]
    pub source: IssueSource,
    #[serde(default)]
    pub external_number: Option<i64>,
    #[serde(default)]
    pub external_url: Option<String>,
    #[serde(default)]
    pub external_author: Option<String>,
    #[serde(default)]
    pub external_updated_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comment {
    pub id: Id,
    pub issue_id: Id,
    pub author: String,
    pub body: String,
    pub created_at: i64,
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub external_author: Option<String>,
    #[serde(default)]
    pub external_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    Waiting,
    Failed,
    Stopped,
    #[serde(other)]
    Unknown,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Idle => "idle",
            SessionStatus::Running => "running",
            SessionStatus::Waiting => "waiting",
            SessionStatus::Failed => "failed",
            SessionStatus::Stopped => "stopped",
            SessionStatus::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<SessionStatus> {
        [
            SessionStatus::Idle,
            SessionStatus::Running,
            SessionStatus::Waiting,
            SessionStatus::Failed,
            SessionStatus::Stopped,
        ]
        .into_iter()
        .find(|status| status.as_str() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub project_id: Id,
    pub worktree_id: Id,
    pub agent: String,
    pub title: String,
    pub cwd: String,
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub issue_id: Option<Id>,
    #[serde(default)]
    pub parent: Option<SessionId>,
    pub status: SessionStatus,
    pub agent_session: Option<String>,
    pub usage: Usage,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_plan_reads: Option<u32>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub deferred: usize,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub queue_system: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_until: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub released: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stuck: bool,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
    pub turns: u64,
    /// Tokens the conversation takes up as of the agent's latest request. Unlike the
    /// counts above this is not a total: a newer reading replaces it. 0 when unknown.
    #[serde(default)]
    pub context: u64,
    /// Tokens the model can hold at once. 0 when the agent cannot say.
    #[serde(default)]
    pub window: u64,
    /// The model the agent last used, as the server resolved it. Empty when unknown.
    #[serde(default)]
    pub model: String,
    /// The host of the server that model runs on. Empty for agents that keep that to themselves.
    #[serde(default)]
    pub host: String,
    /// How hard the model was asked to think. Empty when the server decides.
    #[serde(default)]
    pub effort: String,
    /// The most tokens one reply may have, when the user set one. 0 leaves it to the server.
    #[serde(default)]
    pub max_tokens: u64,
    /// The language servers behind the agent's code tools, as they were last seen. Like `context`
    /// this is a reading, not a total: a newer one replaces it.
    #[serde(default)]
    pub lsp: Vec<Lsp>,
    /// Bash calls that wrote or edited a file, which the file tools do better. A total.
    #[serde(default)]
    pub bash_edits: u64,
    /// Bash calls refused for sleeping to wait on work. A total.
    #[serde(default)]
    pub bash_sleeps: u64,
    /// Calls to write_file, edit_file and replace_in_files. A total.
    #[serde(default)]
    pub file_edits: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Quota>,
}

/// How far a language server has got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspHealth {
    /// The project uses the language and the server is installed, but nothing has needed it yet.
    #[default]
    Idle,
    Starting,
    /// Running, and still reading the project; answers may be incomplete.
    Indexing,
    Ready,
    /// The project uses the language and the server is not installed.
    Missing,
    /// It would not start, or it stopped.
    Failed,
}

/// One language server of a project.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Lsp {
    /// The language: `rust`, `go`, `typescript`.
    pub name: String,
    /// The folder it runs in, relative to the project; `.` for the top.
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub health: LspHealth,
    /// Why it is missing or failed, or how to install it.
    #[serde(default)]
    pub detail: String,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cost_usd += other.cost_usd;
        self.turns += other.turns;
        self.bash_edits += other.bash_edits;
        self.bash_sleeps += other.bash_sleeps;
        self.file_edits += other.file_edits;
        if other.context > 0 {
            self.context = other.context;
        }
        if other.window > 0 {
            self.window = other.window;
        }
        if !other.model.is_empty() {
            self.model = other.model.clone();
            self.host = other.host.clone();
            self.effort = other.effort.clone();
            self.max_tokens = other.max_tokens;
        }
        if !other.lsp.is_empty() {
            self.lsp = other.lsp.clone();
        }
        if other.quota.is_some() {
            self.quota = other.quota.clone();
        }
    }

    /// A reading of the model in use, to merge in with `add`.
    pub fn serving(model: &str, host: &str, window: u64, effort: &str) -> Usage {
        Usage {
            model: model.into(),
            host: host.into(),
            window,
            effort: effort.into(),
            ..Usage::default()
        }
    }

    pub fn with_max_tokens(mut self, max_tokens: u64) -> Usage {
        self.max_tokens = max_tokens;
        self
    }

    /// How full the context is, 0.0 to 1.0 and a little beyond when it has overflowed.
    /// None until both the size and the window are known.
    pub fn fill(&self) -> Option<f64> {
        (self.context > 0 && self.window > 0).then(|| self.context as f64 / self.window as f64)
    }

    /// The share of the input that came from the prompt cache, 0.0 to 1.0.
    pub fn cache_hit(&self) -> Option<f64> {
        let all = self.input_tokens + self.cache_read_tokens;
        (all > 0 && self.cache_read_tokens > 0).then(|| self.cache_read_tokens as f64 / all as f64)
    }

    /// Whether the context has reached `percent` of the window. Never when `percent` is 0
    /// or the window is unknown.
    pub fn past(&self, percent: u64) -> bool {
        percent > 0
            && self.window > 0
            && self.context.saturating_mul(100) >= self.window.saturating_mul(percent)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Screen {
    pub rows: Vec<String>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub cursor_shown: bool,
    pub height: u16,
    pub width: u16,
    pub alive: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TerminalInput {
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cols: u16,
}

pub const SECRET_SHOWN: &str = "(secret given)";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
    #[serde(default)]
    pub options: Vec<String>,
    /// More than one option may be picked.
    #[serde(default)]
    pub multiple: bool,
    /// The answer is a secret: typed masked, never kept in the transcript.
    #[serde(default)]
    pub secret: bool,
}

/// A file kept with a chat message: what the transcript shows, not the bytes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Id,
    pub name: String,
    pub mime: String,
    pub size: u64,
}

/// What a client sends with a message: the file, base64 encoded.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Upload {
    pub name: String,
    pub mime: String,
    pub data: String,
}

/// Most attachments one message can carry.
pub const ATTACHMENTS_MAX: usize = 8;
/// Largest single attachment, in bytes before encoding.
pub const ATTACHMENT_MAX: usize = 20 << 20;

/// What kind of media a MIME type is, if it is one a message can carry.
pub fn media_kind(mime: &str) -> Option<&'static str> {
    let top = mime.split('/').next()?.trim().to_ascii_lowercase();
    ["image", "audio", "video"].into_iter().find(|k| *k == top)
}

/// The MIME type for a file name, for the media types that matter here.
pub fn mime_for(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "pdf" => "application/pdf",
        "txt" | "log" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "html" | "htm" => "text/html",
        "svg" => "image/svg+xml",
        "zip" => "application/zip",
        "rs" | "py" | "js" | "ts" | "toml" | "yaml" | "yml" | "sh" | "c" | "h" | "cpp" | "go"
        | "java" | "kt" | "css" | "xml" | "sql" | "diff" | "patch" => "text/plain",
        _ => return None,
    })
}

impl Attachment {
    pub fn is_text(&self) -> bool {
        self.mime.starts_with("text/") || self.mime == "application/json"
    }

    pub fn is_markdown(&self) -> bool {
        self.mime == "text/markdown"
    }

    pub fn is_image(&self) -> bool {
        media_kind(&self.mime) == Some("image")
    }

    /// The name the bytes are kept under on the server's disk, unique and safe.
    pub fn file_name(&self) -> String {
        let ext: String = self
            .name
            .rsplit_once('.')
            .map(|(_, e)| e)
            .filter(|e| {
                !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric())
            })
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "bin".into());
        format!("{}.{ext}", self.id)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        from: Vec<String>,
    },
    Question {
        id: String,
        question: Question,
        answer: Option<String>,
    },
    /// Several questions put at once, answered together, one answer each.
    Questions {
        id: String,
        questions: Vec<Question>,
        answers: Option<Vec<String>>,
    },
    Summary {
        text: String,
    },
    /// The oldest `covers` messages of the replay, folded into a summary while the rest stay as
    /// they were. Sections stack in order and are never summarised again.
    Section {
        text: String,
        covers: usize,
        #[serde(default)]
        topic: String,
    },
    /// A file an agent sent the user, kept on the server to download.
    File {
        #[serde(default)]
        text: String,
        file: Attachment,
    },
    Assistant {
        text: String,
    },
    Thinking {
        text: String,
    },
    Tool {
        call_id: String,
        name: String,
        input: Value,
        output: Option<String>,
        error: bool,
    },
    Error {
        text: String,
    },
    Notice {
        text: String,
    },
    Warning {
        text: String,
    },
    Diagnostics {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        broken: Vec<String>,
    },
    Wire {
        messages: Vec<Sent>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sent {
    pub message: Value,
    #[serde(default)]
    pub said: Said,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "as", rename_all = "snake_case")]
pub enum Said {
    #[default]
    Plain,
    Typed {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        from: Vec<String>,
    },
    Diagnostics {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        broken: Vec<String>,
    },
    /// A message kept word for word when the conversation before it was folded into a summary.
    /// It is replayed to the model but not shown again.
    Kept,
}

pub const PICTURE_REMOVED: &str = "(picture removed: the model server refused it)";

pub fn remove_pictures(message: &mut Value) -> bool {
    let mut removed = false;
    let Some(parts) = message["content"].as_array_mut() else { return false };
    for part in parts.iter_mut().filter(|p| p["type"] == "image_url") {
        *part = serde_json::json!({ "type": "text", "text": PICTURE_REMOVED });
        removed = true;
    }
    removed
}

pub fn remove_pictures_from(entry: &mut Entry) -> bool {
    let Entry::Wire { messages } = entry else { return false };
    messages.iter_mut().fold(false, |any, s| remove_pictures(&mut s.message) | any)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cut", rename_all = "snake_case")]
pub enum Trim {
    Picture { url: String, text: String },
    ToolResult { call_id: String, length: usize, content: String },
}

pub fn apply_trims(message: &mut Value, trims: &[Trim]) -> bool {
    let mut changed = false;
    for trim in trims {
        match trim {
            Trim::Picture { url, text } => {
                let Some(parts) = message["content"].as_array_mut() else { continue };
                for part in parts.iter_mut().filter(|p| p["image_url"]["url"] == url.as_str()) {
                    *part = serde_json::json!({ "type": "text", "text": text });
                    changed = true;
                }
            }
            Trim::ToolResult { call_id, length, content } => {
                if message["role"] == "tool"
                    && message["tool_call_id"] == call_id.as_str()
                    && message["content"].as_str().is_some_and(|c| c.len() == *length)
                {
                    message["content"] = Value::String(content.clone());
                    changed = true;
                }
            }
        }
    }
    changed
}

pub fn apply_trims_to(entry: &mut Entry, trims: &[Trim]) -> bool {
    let Entry::Wire { messages } = entry else { return false };
    messages.iter_mut().fold(false, |any, s| apply_trims(&mut s.message, trims) | any)
}

pub const SUMMARY_PREAMBLE: &str =
    "Here is a summary of our conversation so far. Carry on from it.";

pub const SYSTEM_PROMPT: &str = "system prompt";

pub fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn tool_input(arguments: &Value) -> Value {
    match arguments.as_str() {
        Some(s) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string())),
        None => arguments.clone(),
    }
}

fn tool_output(content: &Value) -> (String, bool) {
    let text = content_text(content);
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(o)) if o.len() == 1 && o.get("error").is_some_and(Value::is_string) => {
            (o["error"].as_str().unwrap_or_default().to_string(), true)
        }
        _ => (text, false),
    }
}

pub const SHOWN_PER_ROW: i64 = 256;

pub fn shown(rows: &[Message]) -> Vec<Message> {
    let mut results: std::collections::HashMap<&str, (String, bool)> = Default::default();
    for row in rows {
        if let Entry::Wire { messages } = &row.entry {
            for Sent { message, .. } in messages {
                if message["role"] == "tool"
                    && let Some(id) = message["tool_call_id"].as_str()
                {
                    results.insert(id, tool_output(&message["content"]));
                }
            }
        }
    }
    let mut out = Vec::new();
    for row in rows {
        let mut entries = Vec::new();
        match &row.entry {
            Entry::Wire { messages } => {
                for Sent { message, said } in messages {
                    let text = content_text(&message["content"]);
                    match (message["role"].as_str().unwrap_or_default(), said) {
                        (_, Said::Kept) => {}
                        ("system", _) => entries.push(Entry::Tool {
                            call_id: String::new(),
                            name: SYSTEM_PROMPT.into(),
                            input: Value::Null,
                            output: Some(text),
                            error: false,
                        }),
                        ("assistant", _) => {
                            if !text.trim().is_empty() {
                                entries
                                    .push(Entry::Assistant { text: text.trim_start().to_string() });
                            }
                            for call in message["tool_calls"].as_array().into_iter().flatten() {
                                let call_id = call["id"].as_str().unwrap_or_default().to_string();
                                let (output, error) = match results.get(call_id.as_str()) {
                                    Some((o, e)) => (Some(o.clone()), *e),
                                    None => (None, false),
                                };
                                entries.push(Entry::Tool {
                                    name: call["function"]["name"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .to_string(),
                                    input: tool_input(&call["function"]["arguments"]),
                                    call_id,
                                    output,
                                    error,
                                });
                            }
                        }
                        ("tool", _) => {}
                        (_, Said::Typed { attachments, from }) => entries.push(Entry::User {
                            text,
                            attachments: attachments.clone(),
                            from: from.clone(),
                        }),
                        (_, Said::Diagnostics { broken }) => {
                            entries.push(Entry::Diagnostics { text, broken: broken.clone() })
                        }
                        (_, Said::Plain | Said::Kept) => {}
                    }
                }
            }
            other => entries.push(other.clone()),
        }
        for (i, entry) in entries.into_iter().enumerate() {
            out.push(Message { id: row.id * SHOWN_PER_ROW + i as i64, entry, ..row.clone() });
        }
    }
    out
}

impl Entry {
    pub fn user(text: impl Into<String>) -> Entry {
        Entry::User { text: text.into(), attachments: Vec::new(), from: Vec::new() }
    }

    pub fn text(&self) -> &str {
        match self {
            Entry::User { text, .. }
            | Entry::Assistant { text }
            | Entry::Thinking { text }
            | Entry::Error { text }
            | Entry::Notice { text }
            | Entry::Warning { text }
            | Entry::Diagnostics { text, .. }
            | Entry::File { text, .. } => text,
            Entry::Tool { name, .. } => name,
            Entry::Question { question, .. } => &question.text,
            Entry::Questions { questions, .. } => {
                questions.first().map(|q| q.text.as_str()).unwrap_or_default()
            }
            Entry::Summary { text } | Entry::Section { text, .. } => text,
            Entry::Wire { messages } => {
                messages.iter().find_map(|s| s.message["content"].as_str()).unwrap_or_default()
            }
            Entry::Unknown => "",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub session_id: SessionId,
    pub entry: Entry,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub name: String,
    pub label: String,
    pub available: bool,
    pub resumable: bool,
    #[serde(default)]
    pub about: String,
}

/// Where the server's speech models run, for the settings page. Devices are "GPU" or "CPU".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioStatus {
    /// What a model loaded now would run on.
    pub device: String,
    /// What the speech to text model is running on, once it has been used.
    #[serde(default)]
    pub reading_on: Option<String>,
    /// What the text to speech model is running on, once it has been used.
    #[serde(default)]
    pub speaking_on: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelSettings {
    pub url: String,
    pub model: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub has_key: bool,
    #[serde(default)]
    pub steps: usize,
    /// Compact a chat once its context is this many percent full. 0 turns it off.
    #[serde(default = "default_compact_at")]
    pub compact_at: u64,
    /// Seconds a chat sits idle before its oldest sections are folded into summaries, under
    /// the provider's prompt cache lifetime. 0 turns it off.
    #[serde(default = "default_fold_idle_secs")]
    pub fold_idle_secs: u64,
    /// Fold while idle once the context is this many percent full.
    #[serde(default = "default_fold_at")]
    pub fold_at: u64,
    /// The share of the conversation, in percent, that a fold leaves as it was.
    #[serde(default = "default_fold_keep")]
    pub fold_keep: u64,
    /// The models the agent and the user can switch a chat to. Anything left
    /// empty in one uses the native agent's own setting.
    #[serde(default)]
    pub low: LevelSettings,
    #[serde(default)]
    pub medium: LevelSettings,
    #[serde(default)]
    pub high: LevelSettings,
    /// How hard the model thinks. Empty leaves it to the server.
    #[serde(default)]
    pub effort: String,
    /// The most tokens one reply may have. 0 leaves it to the server.
    #[serde(default)]
    pub max_tokens: u64,
}

/// One rung of the model ladder: a cheap model for mechanical work, a stronger
/// one for the hard parts. The key is never read back, only whether one is set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LevelSettings {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub has_key: bool,
    /// Empty uses the native agent's own effort.
    #[serde(default)]
    pub effort: String,
}

pub const EFFORTS: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

/// The host part of a server address: `http://user@gpu.lan:11434/v1` is `gpu.lan:11434`.
pub fn host_of(url: &str) -> String {
    let rest = url.trim().split_once("://").map_or(url.trim(), |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority.rsplit('@').next().unwrap_or_default().to_string()
}

/// The window assumed for a model whose server does not say.
pub const DEFAULT_CONTEXT: u64 = 1_000_000;
pub const DEFAULT_COMPACT_AT: u64 = 80;
pub const DEFAULT_FOLD_IDLE_SECS: u64 = 240;
pub const DEFAULT_FOLD_AT: u64 = 50;
pub const DEFAULT_FOLD_KEEP: u64 = 40;

fn default_fold_idle_secs() -> u64 {
    DEFAULT_FOLD_IDLE_SECS
}

fn default_fold_at() -> u64 {
    DEFAULT_FOLD_AT
}

fn default_fold_keep() -> u64 {
    DEFAULT_FOLD_KEEP
}

fn default_compact_at() -> u64 {
    DEFAULT_COMPACT_AT
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    #[default]
    ClaudeSubscription,
    CodexSubscription,
    AnthropicKey,
    OpenaiKey,
    Endpoint,
    Local,
}

impl ProviderKind {
    pub const ALL: [ProviderKind; 6] = [
        ProviderKind::ClaudeSubscription,
        ProviderKind::CodexSubscription,
        ProviderKind::AnthropicKey,
        ProviderKind::OpenaiKey,
        ProviderKind::Endpoint,
        ProviderKind::Local,
    ];

    pub fn id(self) -> &'static str {
        match self {
            ProviderKind::ClaudeSubscription => "claude_subscription",
            ProviderKind::CodexSubscription => "codex_subscription",
            ProviderKind::AnthropicKey => "anthropic_key",
            ProviderKind::OpenaiKey => "openai_key",
            ProviderKind::Endpoint => "endpoint",
            ProviderKind::Local => "local",
        }
    }

    pub fn from_id(id: &str) -> Option<ProviderKind> {
        ProviderKind::ALL.into_iter().find(|k| k.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::ClaudeSubscription => "Claude subscription",
            ProviderKind::CodexSubscription => "ChatGPT subscription",
            ProviderKind::AnthropicKey => "Anthropic API key",
            ProviderKind::OpenaiKey => "OpenAI API key",
            ProviderKind::Endpoint => "OpenAI compatible endpoint",
            ProviderKind::Local => "Local model",
        }
    }

    pub fn subscription(self) -> bool {
        matches!(self, ProviderKind::ClaudeSubscription | ProviderKind::CodexSubscription)
    }

    pub fn needs_key(self) -> bool {
        matches!(self, ProviderKind::AnthropicKey | ProviderKind::OpenaiKey)
    }

    pub fn default_url(self) -> &'static str {
        match self {
            ProviderKind::AnthropicKey => "https://api.anthropic.com/v1",
            ProviderKind::OpenaiKey => "https://api.openai.com/v1",
            ProviderKind::Local => "http://127.0.0.1:11434/v1",
            _ => "",
        }
    }

    /// The agent a chat runs with when this provider is the default.
    pub fn agent(self) -> &'static str {
        match self {
            ProviderKind::ClaudeSubscription => "claude",
            ProviderKind::CodexSubscription => "codex",
            _ => "native",
        }
    }
}

/// A way to reach a model. Subscriptions use the vendor's own CLI login and hold no secret
/// here; the rest hold a server address and maybe a key, which is never read back.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    pub kind: ProviderKind,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub has_key: bool,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub low: String,
    #[serde(default)]
    pub medium: String,
    #[serde(default)]
    pub high: String,
    #[serde(default)]
    pub supervisor: String,
}

/// What one health check found: the same call whether it runs from the setup or later.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderCheck {
    pub ok: bool,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub kind: ProviderKind,
    pub ok: bool,
    pub message: String,
}

/// The login CLI the setup runs in a terminal pane.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoginStart {
    pub cli: String,
}

/// A vendor CLI on the server's machine and whether it holds a login.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CliLogin {
    pub installed: bool,
    #[serde(default)]
    pub path: String,
    pub logged_in: bool,
    #[serde(default)]
    pub detail: String,
    pub install: String,
    pub login: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Onboarding {
    /// The window opens the setup by itself: nothing is set up and it was neither finished nor skipped.
    pub needed: bool,
    /// "new", "open", "skipped" or "done".
    pub status: String,
    #[serde(default)]
    pub step: String,
    pub providers: Vec<Provider>,
    pub claude: CliLogin,
    pub codex: CliLogin,
    /// The agent a new chat starts with, from the default provider.
    #[serde(default)]
    pub default_agent: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OnboardingPatch {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub step: Option<String>,
}

/// The names a chat's model can take besides a plain model id.
pub const LEVELS: [&str; 3] = ["low", "medium", "high"];

/// The small model that names chats. Anything left empty uses the agentbench
/// agent's own setting, and the key is never read back, only whether one is set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TitleSettings {
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub has_key: bool,
}

pub struct Slash {
    pub name: &'static str,
    pub args: &'static str,
    pub about: &'static str,
}

pub const SLASH: [Slash; 8] = [
    Slash { name: "new", args: "", about: "start a fresh chat in this worktree with this agent" },
    Slash { name: "close", args: "", about: "stop the agent and delete this chat" },
    Slash { name: "stop", args: "", about: "interrupt what the agent is doing" },
    Slash {
        name: "compact",
        args: "",
        about: "summarise the conversation and carry on from the summary",
    },
    Slash {
        name: "clear",
        args: "",
        about: "forget the conversation so far; the transcript stays",
    },
    Slash {
        name: "model",
        args: "<low|medium|high|id>",
        about: "switch this chat to another model, or another server by level; empty goes back to the default",
    },
    Slash {
        name: "thinking",
        args: "<none|minimal|low|medium|high|xhigh>",
        about: "set how hard the model thinks in this chat; empty goes back to the setting",
    },
    Slash {
        name: "title",
        args: "[text]",
        about: "rename this chat, or have the title model name it again",
    },
];

pub fn parse_slash(text: &str) -> Option<(&'static Slash, &str)> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    SLASH.iter().find(|c| c.name == name).map(|c| (c, arg.trim()))
}

pub fn since_summary(entries: &[Entry]) -> &[Entry] {
    match entries.iter().rposition(|e| matches!(e, Entry::Summary { .. })) {
        Some(i) => &entries[i..],
        None => entries,
    }
}

pub fn diagnostics_findings(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with('<') && !l.starts_with("Checked by the language server"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn short_title(text: &str, max: usize) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let heading = first.trim_start_matches('#');
    let first = if heading.starts_with(' ') { heading.trim() } else { first };
    if first.chars().count() <= max {
        return first.to_string();
    }
    let cut: String = first.chars().take(max).collect();
    let word = match cut.rfind(' ') {
        Some(i) if i > max / 2 => &cut[..i],
        _ => &cut,
    };
    format!("{}…", word.trim_end_matches([',', '.', ';', ':', ' ']))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NewProject {
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Make the folder when it is missing, and `git init` it when it is not a repo yet.
    #[serde(default)]
    pub create: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub automation: Option<Option<Automation>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risky: Option<Vec<Risky>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deploy: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker: Option<Tracker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_mode: Option<PostMode>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_system: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NewIssue {
    pub project_id: Id,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub state: Option<IssueState>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub off_vision: bool,
    #[serde(default)]
    pub depends_on: Vec<Id>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IssuePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<IssueState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<Id>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NewComment {
    #[serde(default)]
    pub author: Option<String>,
    pub body: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Place {
    #[default]
    Main,
    Existing {
        worktree_id: Id,
    },
    New {
        branch: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NewSession {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
    pub project_id: Id,
    pub agent: String,
    #[serde(default)]
    pub place: Place,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub issue_id: Option<Id>,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Sent with the prompt.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Upload>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NewWorktree {
    pub branch: String,
    #[serde(default)]
    pub base: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SendMessage {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Upload>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    /// The id of the question entry being answered.
    pub question: String,
    /// The answer to a single question. Several picks are joined with a comma.
    #[serde(default)]
    pub answer: String,
    /// One answer for each question of a set of questions, in order.
    #[serde(default)]
    pub answers: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Interrupted {
    pub interrupted: bool,
    #[serde(default)]
    pub queued: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub handle: String,
    pub command: String,
    pub running: bool,
    #[serde(default)]
    pub exit: Option<i32>,
    pub seconds: u64,
    #[serde(default)]
    pub dropped: usize,
    pub text: String,
}

pub fn restore_queued(draft: &str, queued: &[String]) -> String {
    queued
        .iter()
        .map(|q| q.trim())
        .chain([draft.trim()])
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

impl SessionStatus {
    pub fn busy(self) -> bool {
        matches!(self, SessionStatus::Running | SessionStatus::Waiting)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub model: String,
    pub window: String,
    pub used_pct: f64,
    pub resets_at: i64,
    #[serde(default)]
    pub next_available_at: Option<i64>,
    #[serde(default)]
    pub updated_at: i64,
}

pub const HINT_AT: f64 = 75.0;
pub const HINT_AGAIN_AT: f64 = 90.0;

impl Quota {
    pub fn expired(&self, now: i64) -> bool {
        self.resets_at > 0 && now >= self.resets_at
    }

    pub fn exhausted_until(&self, now: i64) -> Option<i64> {
        if let Some(t) = self.next_available_at.filter(|t| *t > now) {
            return Some(t);
        }
        (self.used_pct >= 100.0 && self.resets_at > now).then_some(self.resets_at)
    }

    pub fn level(&self, now: i64) -> u8 {
        if self.expired(now) {
            0
        } else if self.used_pct >= HINT_AGAIN_AT {
            2
        } else if self.used_pct >= HINT_AT {
            1
        } else {
            0
        }
    }

    pub fn applies_to(&self, model: &str) -> bool {
        model.is_empty() || self.model.is_empty() || self.model == model
    }

    pub fn label(&self, now: i64) -> String {
        let used = if self.model.is_empty() {
            format!("{} {:.0}%", self.window, self.used_pct)
        } else {
            format!("{}: {} {:.0}%", self.model, self.window, self.used_pct)
        };
        match self.exhausted_until(now) {
            Some(t) => format!("{used}, exhausted until {}", clock_hm(t)),
            None if self.resets_at > now => format!("{used}, resets {}", clock_hm(self.resets_at)),
            None => used,
        }
    }

    pub fn hint(&self, now: i64) -> String {
        let named = if self.model.is_empty() { String::new() } else { format!("{} ", self.model) };
        let used = format!("Quota {named}{} at {:.0}%", self.window, self.used_pct);
        match self.exhausted_until(now).or((self.resets_at > now).then_some(self.resets_at)) {
            Some(t) => format!("{used}, resets at {} (in {})", clock_hm(t), span(t - now)),
            None => used,
        }
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn clock_hm(ms: i64) -> String {
    let secs = ms.div_euclid(1000).rem_euclid(86_400);
    format!("{:02}:{:02} UTC", secs / 3600, secs / 60 % 60)
}

pub fn span(ms: i64) -> String {
    let minutes = (ms.max(0) + 59_999) / 60_000;
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectSpend {
    pub project_id: Id,
    pub today_usd: f64,
    pub total_usd: f64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Spend {
    pub today_usd: f64,
    pub total_usd: f64,
    #[serde(default)]
    pub projects: Vec<ProjectSpend>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageReport {
    pub spend: Spend,
    #[serde(default)]
    pub quotas: Vec<Quota>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    pub version: String,
    #[serde(default)]
    pub api_version: u32,
    #[serde(default)]
    pub min_client_api: u32,
    pub agents: Vec<AgentInfo>,
    pub projects: Vec<Project>,
    pub worktrees: Vec<Worktree>,
    pub issues: Vec<Issue>,
    pub sessions: Vec<Session>,
    /// The standing supervisor chat, once it has been opened. It belongs to no listed project.
    #[serde(default)]
    pub supervisor: Option<SessionId>,
    #[serde(default)]
    pub autopilot: Option<AutopilotState>,
    #[serde(default)]
    pub triage: Option<TriageState>,
    #[serde(default)]
    pub visions: Vec<Vision>,
    #[serde(default)]
    pub usage: Option<UsageReport>,
    #[serde(default)]
    pub host: Option<Host>,
    pub cursor: Id,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Host {
    pub mem_total: u64,
    pub mem_used: u64,
    pub disk_total: u64,
    pub disk_free: u64,
    pub load: f32,
    pub cores: u32,
}

impl Host {
    pub fn mem_percent(&self) -> u32 {
        percent(self.mem_used, self.mem_total)
    }

    pub fn disk_percent(&self) -> u32 {
        percent(self.disk_total.saturating_sub(self.disk_free), self.disk_total)
    }

    pub fn cpu_percent(&self) -> u32 {
        ((self.load / self.cores.max(1) as f32) * 100.0).round().max(0.0) as u32
    }
}

fn percent(part: u64, whole: u64) -> u32 {
    if whole == 0 { 0 } else { (part as f64 / whole as f64 * 100.0).round() as u32 }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub id: Id,
    #[serde(default)]
    pub project_id: Option<Id>,
    #[serde(default)]
    pub issue_id: Option<Id>,
    pub text: String,
    pub at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AutopilotState {
    pub enabled: bool,
    pub cap: u32,
    pub project_cap: u32,
    pub stuck_minutes: u64,
    pub max_nudges: u32,
    #[serde(default)]
    pub default_level: Automation,
    pub idle: bool,
    pub workers: u32,
    #[serde(default)]
    pub auto_close: bool,
    #[serde(default)]
    pub check_in_minutes: u64,
    #[serde(default)]
    pub check_in_all: bool,
    #[serde(default)]
    pub auto_escalate: bool,
    #[serde(default)]
    pub groom: bool,
    #[serde(default)]
    pub groom_hours: u64,
    #[serde(default)]
    pub stuck_failed_runs: u32,
    #[serde(default)]
    pub stuck_compactions: u32,
    #[serde(default)]
    pub stuck_no_commit_minutes: u64,
    #[serde(default)]
    pub activity: Vec<Activity>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AutopilotPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_cap: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_nudges: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_level: Option<Automation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_close: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_in_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_in_all: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_escalate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groom: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub groom_hours: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_failed_runs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_compactions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck_no_commit_minutes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Duplicate,
    Stale,
    Labels,
    CrossCutting,
    AddProject,
    GlobalNote,
}

impl ProposalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProposalKind::Duplicate => "duplicate",
            ProposalKind::Stale => "stale",
            ProposalKind::Labels => "labels",
            ProposalKind::CrossCutting => "cross_cutting",
            ProposalKind::AddProject => "add_project",
            ProposalKind::GlobalNote => "global_note",
        }
    }

    pub fn parse(s: &str) -> Option<ProposalKind> {
        match s.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "duplicate" | "duplicates" => Some(ProposalKind::Duplicate),
            "stale" => Some(ProposalKind::Stale),
            "labels" | "label" | "priority" | "labels_priority" => Some(ProposalKind::Labels),
            "cross_cutting" | "crosscutting" => Some(ProposalKind::CrossCutting),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Change {
    Update { issue_id: Id, patch: IssuePatch },
    Comment { issue_id: Id, body: String },
    Create { issue: NewIssue },
    AddProject { request: AddProject },
    GlobalNote { topic: String, note: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddMode {
    Import,
    Create,
    Clone,
}

impl AddMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AddMode::Import => "import",
            AddMode::Create => "create",
            AddMode::Clone => "clone",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AddProject {
    pub mode: AddMode,
    pub path: String,
    pub name: String,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
    #[serde(default)]
    pub free_bytes: Option<u64>,
    #[serde(default)]
    pub requester: Option<SessionId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Pending,
    Accepted,
    Rejected,
}

impl ProposalState {
    pub fn as_str(self) -> &'static str {
        match self {
            ProposalState::Pending => "pending",
            ProposalState::Accepted => "accepted",
            ProposalState::Rejected => "rejected",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: Id,
    pub kind: ProposalKind,
    pub summary: String,
    pub issues: Vec<Id>,
    pub sessions: Vec<String>,
    pub projects: Vec<Id>,
    pub changes: Vec<Change>,
    pub state: ProposalState,
    pub created_at: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriageState {
    pub proposals: Vec<Proposal>,
    pub running: bool,
    pub budget_tokens: u64,
    pub every_hours: u64,
    #[serde(default)]
    pub last_run: Option<i64>,
    #[serde(default)]
    pub last_status: String,
    #[serde(default)]
    pub notes: Vec<(Id, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TriagePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_hours: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_project: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

pub const API_VERSION: u32 = 7;
pub const API_GITHUB_FLOW: u32 = 7;
pub const API_GLOBAL_NOTE: u32 = 6;
pub const API_PROMPTS: u32 = 5;
pub const API_ISSUE_DEPS: u32 = 2;
pub const API_ATTENTION_FEED: u32 = 3;
pub const API_MEMORY_COMPACT: u32 = 4;
pub const MIN_CLIENT_API: u32 = 1;
pub const API_RULE: &str = "API changes are additive: new fields get serde defaults, never rename or remove a field or enum variant the app decodes, never change a field's type, and add no required request field. A break bumps API_VERSION and MIN_CLIENT_API on purpose and the PR says so. Details in AGENTS.md.";
pub const API_HEADER: &str = "x-agentbench-api";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compat {
    Ok,
    AppOlder { app: u32, server: u32 },
    ServerOlder { app: u32, server: u32 },
}

impl Compat {
    pub fn notice(self) -> Option<String> {
        match self {
            Compat::Ok => None,
            Compat::AppOlder { app, server } => Some(format!(
                "This app is older than the server (app v{app}, server v{server}). Update the app."
            )),
            Compat::ServerOlder { app, server } => Some(format!(
                "Server v{server} is older than this app (app v{app}). Update the server."
            )),
        }
    }
}

pub fn server_has(server_api: u32, needs: u32) -> bool {
    server_api.max(1) >= needs
}

pub fn compat(app_api: u32, app_min_server: u32, server_api: u32, server_min_client: u32) -> Compat {
    let server = server_api.max(1);
    if server_min_client > app_api {
        Compat::AppOlder { app: app_api, server }
    } else if server < app_min_server {
        Compat::ServerOlder { app: app_api, server }
    } else {
        Compat::Ok
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangesFeed {
    pub cursor: Id,
    #[serde(default)]
    pub changes: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
}

pub fn issue_prompt(issue: &Issue, comments: &[Comment]) -> String {
    let mut prompt = format!("Work on issue #{}: {}\n", issue.id, issue.title);
    if !issue.body.trim().is_empty() {
        prompt.push('\n');
        prompt.push_str(issue.body.trim());
        prompt.push('\n');
    }
    if !comments.is_empty() {
        prompt.push_str("\nDiscussion so far:\n");
        for c in comments {
            prompt.push_str(&format!("- {}: {}\n", c.author, c.body.trim()));
        }
    }
    prompt
}

pub fn branch_slug(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    slug.trim_end_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trim_rewrites_only_the_picture_and_tool_result_it_names() {
        let mut entry = Entry::Wire {
            messages: vec![
                Sent {
                    message: serde_json::json!({"role": "user", "content": [
                        {"type": "text", "text": "a.png:"},
                        {"type": "image_url", "image_url": {"url": "data:image/png;base64,OLD"}},
                        {"type": "image_url", "image_url": {"url": "data:image/png;base64,NEW"}},
                    ]}),
                    said: Said::Plain,
                },
                Sent {
                    message: serde_json::json!({"role": "tool", "tool_call_id": "c1", "content": "long"}),
                    said: Said::Plain,
                },
                Sent {
                    message: serde_json::json!({"role": "tool", "tool_call_id": "c2", "content": "kept"}),
                    said: Said::Plain,
                },
                Sent {
                    message: serde_json::json!({"role": "tool", "tool_call_id": "c1", "content": "other answer"}),
                    said: Said::Plain,
                },
            ],
        };
        let trims = [
            Trim::Picture { url: "data:image/png;base64,OLD".into(), text: "(gone)".into() },
            Trim::ToolResult { call_id: "c1".into(), length: 4, content: "short".into() },
        ];
        assert!(apply_trims_to(&mut entry, &trims));
        assert!(!apply_trims_to(&mut entry, &trims), "a second pass changes nothing");
        let Entry::Wire { messages } = entry else { unreachable!() };
        assert_eq!(messages[0].message["content"][1], serde_json::json!({"type": "text", "text": "(gone)"}));
        assert_eq!(messages[0].message["content"][2]["type"], "image_url");
        assert_eq!(messages[1].message["content"], "short");
        assert_eq!(messages[2].message["content"], "kept");
        assert_eq!(messages[3].message["content"], "other answer");
    }
    #[test]
    fn queued_messages_return_to_the_draft_one_per_line() {
        let q = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(restore_queued("", &q(&["first", "second"])), "first\nsecond");
        assert_eq!(restore_queued("typing", &q(&["first", ""])), "first\ntyping");
        assert_eq!(restore_queued("typing", &[]), "typing");
        let old: Interrupted = serde_json::from_str(r#"{"interrupted":true}"#).unwrap();
        assert!(old.queued.is_empty());
    }

    #[test]
    fn issue_states_round_trip_their_names() {
        for state in IssueState::ALL {
            assert_eq!(IssueState::parse(state.as_str()), Some(state));
            let json = serde_json::to_string(&state).unwrap();
            assert_eq!(json, format!("\"{}\"", state.as_str()));
        }
        assert_eq!(IssueState::parse("nope"), None);
    }

    #[test]
    fn entries_are_tagged_by_kind() {
        let e = Entry::Tool {
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
            output: Some("a".into()),
            error: false,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "tool");
        assert_eq!(serde_json::from_value::<Entry>(v).unwrap(), e);
    }

    #[test]
    fn a_patch_only_serializes_what_it_changes() {
        let patch = IssuePatch { state: Some(IssueState::Done), ..Default::default() };
        assert_eq!(serde_json::to_value(&patch).unwrap(), serde_json::json!({"state": "done"}));
    }

    #[test]
    fn a_session_lands_in_main_unless_told_otherwise() {
        let s: NewSession = serde_json::from_str(r#"{"project_id": 1, "agent": "pi"}"#).unwrap();
        assert_eq!(s.place, Place::Main);
        let s: NewSession = serde_json::from_str(
            r#"{"project_id": 1, "agent": "pi", "place": {"kind": "new", "branch": "fix-login"}}"#,
        )
        .unwrap();
        assert_eq!(s.place, Place::New { branch: "fix-login".into() });
    }

    #[test]
    fn slash_commands_parse_with_their_argument() {
        assert_eq!(parse_slash("/compact").map(|(c, a)| (c.name, a)), Some(("compact", "")));
        assert_eq!(
            parse_slash(" /model  qwen3 ").map(|(c, a)| (c.name, a)),
            Some(("model", "qwen3"))
        );
        assert!(parse_slash("/nope").is_none());
        assert!(parse_slash("not a command").is_none());
    }

    #[test]
    fn context_is_the_latest_reading_not_a_total() {
        let mut u = Usage::default();
        u.add(&Usage { input_tokens: 10, context: 1000, window: 4000, ..Default::default() });
        u.add(&Usage { input_tokens: 10, context: 2500, ..Default::default() });
        assert_eq!((u.input_tokens, u.context, u.window), (20, 2500, 4000));
        // A turn that cannot say keeps the last reading.
        u.add(&Usage { input_tokens: 5, ..Default::default() });
        assert_eq!(u.context, 2500);
        assert_eq!(u.fill(), Some(0.625));
        assert!(u.past(60) && !u.past(70) && !u.past(0));
        assert!(!Usage { context: 9, ..Default::default() }.past(1), "no window, no verdict");
    }

    #[test]
    fn the_host_is_what_the_address_names() {
        assert_eq!(host_of("http://gpu.lan:11434/v1"), "gpu.lan:11434");
        assert_eq!(host_of(" https://u:p@api.example.com/v1/chat/completions "), "api.example.com");
        assert_eq!(host_of("localhost:8080/v1"), "localhost:8080");
        assert_eq!(host_of(""), "");
    }

    #[test]
    fn the_cache_hit_is_its_share_of_the_input() {
        let u = Usage { input_tokens: 25, cache_read_tokens: 75, ..Default::default() };
        assert_eq!(u.cache_hit(), Some(0.75));
        assert_eq!(Usage { input_tokens: 25, ..Default::default() }.cache_hit(), None);
        // Saved before these fields existed.
        let old: Usage = serde_json::from_str(r#"{"input_tokens":1,"output_tokens":2,"cache_read_tokens":0,"cost_usd":0.0,"turns":1}"#).unwrap();
        assert_eq!((old.context, old.window), (0, 0));
        assert_eq!(old.model, "");
    }

    #[test]
    fn history_starts_at_the_last_summary() {
        let e = vec![Entry::user("a"), Entry::Summary { text: "s1".into() }, Entry::user("b")];
        assert_eq!(since_summary(&e).len(), 2);
        assert_eq!(since_summary(&e[..1]).len(), 1);
    }

    #[test]
    fn an_entry_of_a_kind_this_build_does_not_know_still_decodes() {
        let m: Vec<Message> = serde_json::from_str(
            r#"[{"id":1,"session_id":"00000000-0000-4000-8000-000000000001","entry":{"kind":"from_the_future","text":"x"},"created_at":0},
                {"id":2,"session_id":"00000000-0000-4000-8000-000000000001","entry":{"kind":"user","text":"hi"},"created_at":0}]"#,
        )
        .unwrap();
        assert_eq!(m[0].entry, Entry::Unknown);
        assert_eq!(m[1].entry, Entry::user("hi"));
    }

    #[test]
    fn diagnostics_are_shown_without_the_wrapper_meant_for_the_model() {
        let note = "<language-server-diagnostics>\nChecked by the language server after your edit; this is the harness speaking, not the user. Fix what is yours:\nsrc/lib.rs: 1 error (rust)\n  src/lib.rs:2:5  error[E0308] mismatched types\n</language-server-diagnostics>";
        assert_eq!(
            diagnostics_findings(note),
            "src/lib.rs: 1 error (rust)\n  src/lib.rs:2:5  error[E0308] mismatched types"
        );
    }

    #[test]
    fn titles_stop_at_a_word() {
        assert_eq!(short_title("fix it", 40), "fix it");
        assert_eq!(
            short_title("The current credit system isnt very good, there isnt a way for users", 40),
            "The current credit system isnt very…"
        );
        assert_eq!(short_title("\n\n  ## Plan\nmore", 40), "Plan");
        assert_eq!(short_title("#12 Fix login", 40), "#12 Fix login");
        assert_eq!(short_title(&"x".repeat(50), 10), format!("{}…", "x".repeat(10)));
    }

    #[test]
    fn branch_slugs_are_git_safe() {
        assert_eq!(branch_slug("Fix: the login loop!!"), "fix-the-login-loop");
        assert_eq!(branch_slug("  #12 Ünïcode / paths  "), "12-n-code-paths");
        assert!(branch_slug(&"a".repeat(100)).len() <= 40);
    }

    #[test]
    fn worktree_labels_fall_back_to_the_head() {
        let mut w = Worktree {
            id: 1,
            project_id: 1,
            path: "/x".into(),
            branch: Some("feat".into()),
            head: Some("0123456789abcdef".into()),
            main: false,
            missing: false,
            owned: false,
            created_at: 0,
        };
        assert_eq!(w.label(), "feat");
        w.branch = None;
        assert_eq!(w.label(), "01234567");
    }

    #[test]
    fn an_issue_from_before_dependencies_still_decodes() {
        let old = r#"{"id":1,"project_id":2,"title":"t","body":"","state":"todo","priority":0,"labels":[],"created_at":0,"updated_at":0}"#;
        let i: Issue = serde_json::from_str(old).unwrap();
        assert!(i.depends_on.is_empty() && i.waiting_on.is_empty() && i.blocks.is_empty());
        let patch: IssuePatch = serde_json::from_str("{}").unwrap();
        assert_eq!(patch.depends_on, None);
        let new: NewIssue = serde_json::from_str(r#"{"project_id":1,"title":"t"}"#).unwrap();
        assert!(new.depends_on.is_empty());
    }

    #[test]
    fn the_issue_prompt_carries_title_body_and_discussion() {
        let issue = Issue {
            id: 7,
            project_id: 1,
            title: "Fix login".into(),
            body: "It loops.".into(),
            state: IssueState::Todo,
            priority: 0,
            labels: vec![],
            goal: None,
            off_vision: false,
            depends_on: vec![],
            waiting_on: vec![],
            blocks: vec![],
            source: Default::default(),
            external_number: None,
            external_url: None,
            external_author: None,
            external_updated_at: None,
            created_at: 0,
            updated_at: 0,
        };
        let comments = vec![Comment {
            id: 1,
            issue_id: 7,
            author: "kieran".into(),
            body: "only on firefox".into(),
            created_at: 0,
            external_id: None,
            external_author: None,
            external_url: None,
        }];
        let p = issue_prompt(&issue, &comments);
        assert!(p.starts_with("Work on issue #7: Fix login\n"));
        assert!(p.contains("It loops."));
        assert!(p.contains("- kieran: only on firefox"));
    }
}
