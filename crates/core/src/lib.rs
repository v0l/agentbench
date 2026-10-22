use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Id = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Backlog,
    Todo,
    Doing,
    Review,
    Done,
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
    pub created_by_foreman: bool,
    pub created_at: i64,
}

impl Worktree {
    pub fn label(&self) -> String {
        match &self.branch {
            Some(b) => b.clone(),
            None => self
                .head
                .as_deref()
                .map(|h| h.chars().take(8).collect())
                .unwrap_or("detached".into()),
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    Waiting,
    Failed,
    Stopped,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Idle => "idle",
            SessionStatus::Running => "running",
            SessionStatus::Waiting => "waiting",
            SessionStatus::Failed => "failed",
            SessionStatus::Stopped => "stopped",
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
    pub id: Id,
    pub project_id: Id,
    pub worktree_id: Id,
    pub agent: String,
    pub title: String,
    pub cwd: String,
    pub model: Option<String>,
    pub issue_id: Option<Id>,
    pub status: SessionStatus,
    pub agent_session: Option<String>,
    pub usage: Usage,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cost_usd: f64,
    pub turns: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cost_usd += other.cost_usd;
        self.turns += other.turns;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    User { text: String },
    Question { id: String, question: Question, answer: Option<String> },
    Assistant { text: String },
    Thinking { text: String },
    Tool { call_id: String, name: String, input: Value, output: Option<String>, error: bool },
    Error { text: String },
    Notice { text: String },
}

impl Entry {
    pub fn text(&self) -> &str {
        match self {
            Entry::User { text }
            | Entry::Assistant { text }
            | Entry::Thinking { text }
            | Entry::Error { text }
            | Entry::Notice { text } => text,
            Entry::Tool { name, .. } => name,
            Entry::Question { question, .. } => &question.text,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: Id,
    pub session_id: Id,
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelSettings {
    pub url: String,
    pub model: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub has_key: bool,
}

pub fn short_title(text: &str, max: usize) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let first = first.trim_start_matches('#').trim();
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
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct IssuePatch {
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
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Answer {
    pub question: String,
    pub answer: String,
}

impl SessionStatus {
    pub fn busy(self) -> bool {
        matches!(self, SessionStatus::Running | SessionStatus::Waiting)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    pub version: String,
    pub agents: Vec<AgentInfo>,
    pub projects: Vec<Project>,
    pub worktrees: Vec<Worktree>,
    pub issues: Vec<Issue>,
    pub sessions: Vec<Session>,
    pub cursor: Id,
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
    fn titles_stop_at_a_word() {
        assert_eq!(short_title("fix it", 40), "fix it");
        assert_eq!(
            short_title("The current credit system isnt very good, there isnt a way for users", 40),
            "The current credit system isnt very…"
        );
        assert_eq!(short_title("\n\n  ## Plan\nmore", 40), "Plan");
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
            created_by_foreman: false,
            created_at: 0,
        };
        assert_eq!(w.label(), "feat");
        w.branch = None;
        assert_eq!(w.label(), "01234567");
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
            created_at: 0,
            updated_at: 0,
        };
        let comments = vec![Comment {
            id: 1,
            issue_id: 7,
            author: "kieran".into(),
            body: "only on firefox".into(),
            created_at: 0,
        }];
        let p = issue_prompt(&issue, &comments);
        assert!(p.starts_with("Work on issue #7: Fix login\n"));
        assert!(p.contains("It loops."));
        assert!(p.contains("- kieran: only on firefox"));
    }
}
