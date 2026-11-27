use agentbench_core::{
    AutoMerge, Automation, Comment, Entry, Id, Issue, IssuePatch, IssueSource, IssueState, Message, MergeMethod, MergeMode, PostMode, PrCheck, PullRequest, Tracker, Trust, NewComment, NewIssue,
    Project, ProjectPatch, Risky, Session, SessionId, SessionStatus, Usage, Vision, VisionBody, Worktree,
};
use crate::memory::{Memory, Scope};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA: &[&str] = &[
    r#"
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE projects (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    branch TEXT,
    remote TEXT,
    created_at INTEGER NOT NULL
);
CREATE TABLE worktrees (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path TEXT NOT NULL UNIQUE,
    branch TEXT,
    head TEXT,
    main INTEGER NOT NULL DEFAULT 0,
    missing INTEGER NOT NULL DEFAULT 0,
    owned INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE issues (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 0,
    labels TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE comments (
    id INTEGER PRIMARY KEY,
    issue_id INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author TEXT NOT NULL,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE RESTRICT,
    agent TEXT NOT NULL,
    title TEXT NOT NULL,
    model TEXT,
    issue_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,
    status TEXT NOT NULL,
    agent_session TEXT,
    usage TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    entry TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX messages_session ON messages(session_id, id);
CREATE TABLE changes (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    ref_id INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
"#,
    "ALTER TABLE sessions ADD COLUMN title_auto INTEGER NOT NULL DEFAULT 0;",
    r#"
CREATE TABLE attachments (
    id INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    mime TEXT NOT NULL,
    data BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX attachments_session ON attachments(session_id);
"#,
    r#"
CREATE TEMP TABLE session_ids AS
    SELECT id AS old,
        lower(hex(randomblob(4))) || '-' || lower(hex(randomblob(2))) || '-4' ||
        substr(lower(hex(randomblob(2))), 2) || '-' ||
        substr('89ab', 1 + (abs(random()) % 4), 1) || substr(lower(hex(randomblob(2))), 2) || '-' ||
        lower(hex(randomblob(6))) AS new
    FROM sessions;
CREATE TABLE sessions_new (
    id TEXT PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    worktree_id INTEGER NOT NULL REFERENCES worktrees(id) ON DELETE RESTRICT,
    agent TEXT NOT NULL,
    title TEXT NOT NULL,
    model TEXT,
    issue_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,
    status TEXT NOT NULL,
    agent_session TEXT,
    usage TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    title_auto INTEGER NOT NULL DEFAULT 0
);
INSERT INTO sessions_new
    SELECT m.new, s.project_id, s.worktree_id, s.agent, s.title, s.model, s.issue_id, s.status,
        s.agent_session, s.usage, s.created_at, s.updated_at, s.title_auto
    FROM sessions s JOIN session_ids m ON m.old = s.id;
CREATE TABLE messages_new (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    entry TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO messages_new
    SELECT x.id, m.new, x.entry, x.created_at FROM messages x JOIN session_ids m ON m.old = x.session_id;
CREATE TABLE attachments_new (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    mime TEXT NOT NULL,
    data BLOB NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO attachments_new
    SELECT x.id, m.new, x.name, x.mime, x.data, x.created_at FROM attachments x JOIN session_ids m ON m.old = x.session_id;
CREATE TABLE changes_new (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    ref_id TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
INSERT INTO changes_new SELECT id, kind, CAST(ref_id AS TEXT), created_at FROM changes;
DROP TABLE attachments;
DROP TABLE messages;
DROP TABLE sessions;
DROP TABLE changes;
ALTER TABLE sessions_new RENAME TO sessions;
ALTER TABLE messages_new RENAME TO messages;
ALTER TABLE attachments_new RENAME TO attachments;
ALTER TABLE changes_new RENAME TO changes;
CREATE INDEX messages_session ON messages(session_id, id);
CREATE INDEX attachments_session ON attachments(session_id);
DROP TABLE session_ids;
"#,
    r#"
CREATE TABLE notes (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    topic TEXT NOT NULL,
    body TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (project_id, topic)
);
"#,
    "ALTER TABLE projects ADD COLUMN home INTEGER NOT NULL DEFAULT 0;",
    "ALTER TABLE sessions ADD COLUMN effort TEXT;",
    "ALTER TABLE projects ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE projects ADD COLUMN tags TEXT NOT NULL DEFAULT '[]';",
    "ALTER TABLE sessions ADD COLUMN title_turns INTEGER NOT NULL DEFAULT 0;",
    r#"
CREATE TABLE watches (
    watched TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    watcher TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    PRIMARY KEY (watched, watcher)
);
"#,
    r#"
CREATE TABLE autopilot_workers (
    session TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
    issue INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    project INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'working',
    nudges INTEGER NOT NULL DEFAULT 0,
    reworks INTEGER NOT NULL DEFAULT 0,
    green INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE autopilot_log (
    id INTEGER PRIMARY KEY,
    project_id INTEGER,
    issue_id INTEGER,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#,
    "ALTER TABLE projects ADD COLUMN automation TEXT;
     ALTER TABLE projects ADD COLUMN risky TEXT NOT NULL DEFAULT '[]';
     ALTER TABLE projects ADD COLUMN deploy TEXT;",
    r#"
CREATE TABLE triage_proposals (
    id INTEGER PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    body TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    created_at INTEGER NOT NULL,
    decided_at INTEGER
);
CREATE INDEX triage_fingerprint ON triage_proposals(fingerprint);
"#,
    r#"
CREATE TABLE note_versions (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    topic TEXT NOT NULL,
    body TEXT NOT NULL,
    hash TEXT NOT NULL,
    author TEXT NOT NULL,
    session TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX note_versions_topic ON note_versions(project_id, topic, id);
"#,
    r#"
CREATE TABLE global_notes (
    topic TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
"#,
    "ALTER TABLE sessions ADD COLUMN parent TEXT;",
    r#"
ALTER TABLE autopilot_workers ADD COLUMN asked_at INTEGER NOT NULL DEFAULT 0;
CREATE TABLE verdicts (
    id INTEGER PRIMARY KEY,
    session TEXT NOT NULL,
    issue INTEGER,
    head TEXT,
    verdict TEXT NOT NULL,
    reason TEXT NOT NULL,
    followup TEXT,
    reply TEXT NOT NULL,
    judge TEXT NOT NULL,
    acted INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE INDEX verdicts_session ON verdicts(session, id);
"#,
    r#"
CREATE TABLE deferred (
    id INTEGER PRIMARY KEY,
    session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    worker TEXT,
    urgent INTEGER NOT NULL DEFAULT 0,
    text TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX deferred_session ON deferred(session, id);
"#,
    r#"
CREATE TABLE vision_versions (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    author TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX vision_versions_project ON vision_versions(project_id, id);
"#,
    r#"
CREATE TABLE spend (
    day INTEGER NOT NULL,
    project_id INTEGER NOT NULL,
    usd REAL NOT NULL,
    PRIMARY KEY (day, project_id)
);
"#,
    r#"
CREATE TABLE issue_deps (
    issue_id INTEGER NOT NULL,
    depends_on INTEGER NOT NULL,
    PRIMARY KEY (issue_id, depends_on)
);
CREATE INDEX issue_deps_on ON issue_deps(depends_on);
"#,
    "DROP TABLE IF EXISTS notes; DROP TABLE IF EXISTS note_versions; DROP TABLE IF EXISTS global_notes;",
    r#"
CREATE TABLE open_items (
    id INTEGER PRIMARY KEY,
    session TEXT NOT NULL,
    kind TEXT NOT NULL,
    target TEXT,
    head TEXT,
    text TEXT NOT NULL,
    delivered_at INTEGER NOT NULL
);
CREATE INDEX open_items_session ON open_items(session, id);
"#,
    r#"
CREATE TABLE supervisor_log (
    id INTEGER PRIMARY KEY,
    supervisor TEXT,
    kind TEXT NOT NULL,
    asker TEXT NOT NULL DEFAULT '',
    asker_title TEXT NOT NULL DEFAULT '',
    session TEXT,
    project_id INTEGER,
    issue_id INTEGER,
    question TEXT NOT NULL DEFAULT '',
    arrived_at INTEGER NOT NULL,
    needs_answer INTEGER NOT NULL DEFAULT 1,
    closed_at INTEGER,
    action TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    outcome TEXT NOT NULL DEFAULT '',
    reply TEXT NOT NULL DEFAULT '',
    merge TEXT NOT NULL DEFAULT ''
);
CREATE INDEX supervisor_log_arrived ON supervisor_log(arrived_at);
CREATE INDEX supervisor_log_session ON supervisor_log(session, id);
"#,
];

const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("sessions", "title_auto", "INTEGER NOT NULL DEFAULT 0"),
    ("projects", "home", "INTEGER NOT NULL DEFAULT 0"),
    ("projects", "supervisor", "INTEGER NOT NULL DEFAULT 0"),
    ("sessions", "effort", "TEXT"),
    ("projects", "pinned", "INTEGER NOT NULL DEFAULT 0"),
    ("projects", "tags", "TEXT NOT NULL DEFAULT '[]'"),
    ("sessions", "title_turns", "INTEGER NOT NULL DEFAULT 0"),
    ("projects", "automation", "TEXT"),
    ("projects", "risky", "TEXT NOT NULL DEFAULT '[]'"),
    ("projects", "deploy", "TEXT"),
    ("sessions", "queue_system", "INTEGER NOT NULL DEFAULT 0"),
    ("issues", "goal", "TEXT"),
    ("issues", "off_vision", "INTEGER NOT NULL DEFAULT 0"),
    ("sessions", "released", "INTEGER NOT NULL DEFAULT 0"),
    ("projects", "tracker", "TEXT NOT NULL DEFAULT 'internal'"),
    ("projects", "github_repo", "TEXT"),
    ("projects", "post_mode", "TEXT NOT NULL DEFAULT 'draft'"),
    ("projects", "merge_mode", "TEXT NOT NULL DEFAULT 'pr'"),
    ("issues", "source", "TEXT NOT NULL DEFAULT 'internal'"),
    ("issues", "external_number", "INTEGER"),
    ("issues", "external_url", "TEXT"),
    ("issues", "external_author", "TEXT"),
    ("issues", "external_updated_at", "INTEGER"),
    ("comments", "external_id", "TEXT"),
    ("comments", "external_author", "TEXT"),
    ("comments", "external_url", "TEXT"),
    ("issues", "trust", "TEXT NOT NULL DEFAULT 'trusted'"),
    ("issues", "ready_by", "TEXT"),
    ("issues", "held", "TEXT"),
    ("issues", "model_level", "TEXT"),
    ("projects", "model_low", "TEXT NOT NULL DEFAULT ''"),
    ("projects", "model_med", "TEXT NOT NULL DEFAULT ''"),
    ("projects", "model_high", "TEXT NOT NULL DEFAULT ''"),
    ("issues", "ready_status", "TEXT"),
    ("issues", "ready_hash", "TEXT"),
    ("issues", "ready_after", "INTEGER NOT NULL DEFAULT 0"),
    ("comments", "trust", "TEXT NOT NULL DEFAULT 'trusted'"),
    ("projects", "automerge", "TEXT NOT NULL DEFAULT 'off'"),
    ("projects", "merge_method", "TEXT NOT NULL DEFAULT 'merge'"),
    ("projects", "judge_unset_goals", "INTEGER"),
    ("issues", "judge_state", "TEXT NOT NULL DEFAULT ''"),
    ("issues", "judge_at", "INTEGER NOT NULL DEFAULT 0"),
];

const LATE_COLUMNS: &[(&str, &str, &str)] = &[
    ("outbox", "asked_by", "TEXT NOT NULL DEFAULT ''"),
    ("outbox", "trusted", "INTEGER NOT NULL DEFAULT 1"),
    ("outbox", "number", "INTEGER NOT NULL DEFAULT 0"),
    ("outbox", "text", "TEXT NOT NULL DEFAULT ''"),
    ("outbox", "attempts", "INTEGER NOT NULL DEFAULT 0"),
    ("outbox", "next_at", "INTEGER NOT NULL DEFAULT 0"),
    ("outbox", "external_id", "TEXT"),
    ("outbox", "error", "TEXT"),
    ("outbox", "updated_at", "INTEGER NOT NULL DEFAULT 0"),
    ("outbox", "approved_by", "TEXT NOT NULL DEFAULT ''"),
    ("pull_requests", "checks_sha", "TEXT"),
    ("pull_requests", "review_verdict", "TEXT"),
    ("pull_requests", "review_row", "INTEGER"),
    ("pull_requests", "review_asked", "INTEGER NOT NULL DEFAULT 0"),
    ("pull_requests", "review_answered", "INTEGER NOT NULL DEFAULT 0"),
    ("pull_requests", "review_seen", "INTEGER NOT NULL DEFAULT 0"),
    ("pull_requests", "by_agentbench", "INTEGER NOT NULL DEFAULT 0"),
    ("pull_requests", "ci_reworks", "INTEGER NOT NULL DEFAULT 0"),
    ("pull_requests", "ci_sent_sha", "TEXT"),
    ("open_items", "log_id", "INTEGER"),
];

const AFTER_COLUMNS: &str = r#"
CREATE UNIQUE INDEX IF NOT EXISTS issues_external ON issues(project_id, external_number) WHERE source = 'github';
CREATE UNIQUE INDEX IF NOT EXISTS comments_external ON comments(issue_id, external_id) WHERE external_id IS NOT NULL;
CREATE TABLE IF NOT EXISTS pull_requests (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    number INTEGER NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    author TEXT NOT NULL DEFAULT '',
    head_ref TEXT NOT NULL DEFAULT '',
    base_ref TEXT NOT NULL DEFAULT '',
    head_sha TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'open',
    draft INTEGER NOT NULL DEFAULT 0,
    mergeable TEXT,
    checks TEXT NOT NULL DEFAULT '[]',
    review_decision TEXT,
    url TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0,
    issue_id INTEGER REFERENCES issues(id) ON DELETE SET NULL,
    reviewed_sha TEXT,
    trust TEXT NOT NULL DEFAULT 'trusted',
    UNIQUE (project_id, number)
);
CREATE TABLE IF NOT EXISTS outbox (
    id INTEGER PRIMARY KEY,
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'pending',
    created_at INTEGER NOT NULL DEFAULT 0
);
"#;

const WIRE_TRANSCRIPTS: &str = "native_transcripts_are_wire";

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError(e.to_string())
    }
}

pub use agentbench_core::OutboxItem;

pub struct NewOutbox<'a> {
    pub project_id: Id,
    pub kind: &'a str,
    pub payload: &'a str,
    pub status: &'a str,
    pub asked_by: &'a str,
    pub trusted: bool,
    pub number: i64,
    pub text: &'a str,
}

fn outbox_row(r: &Row) -> rusqlite::Result<OutboxItem> {
    Ok(OutboxItem {
        id: r.get("id")?,
        project_id: r.get("project_id")?,
        kind: r.get("kind")?,
        payload: r.get("payload")?,
        status: r.get("status")?,
        created_at: r.get("created_at")?,
        asked_by: r.get("asked_by")?,
        trusted: r.get("trusted")?,
        number: r.get("number")?,
        text: r.get("text")?,
        attempts: r.get("attempts")?,
        next_at: r.get("next_at")?,
        external_id: r.get("external_id")?,
        error: r.get("error")?,
        updated_at: r.get("updated_at")?,
        approved_by: r.get("approved_by")?,
        url: None,
    })
}

fn pull_request_row(r: &Row) -> rusqlite::Result<PullRequest> {
    Ok(PullRequest {
        project_id: r.get("project_id")?,
        number: r.get("number")?,
        title: r.get("title")?,
        body: r.get("body")?,
        author: r.get("author")?,
        head_ref: r.get("head_ref")?,
        base_ref: r.get("base_ref")?,
        head_sha: r.get("head_sha")?,
        state: r.get("state")?,
        draft: r.get("draft")?,
        url: r.get("url")?,
        checks: serde_json::from_str(&r.get::<_, String>("checks")?).unwrap_or_default(),
        checks_sha: r.get("checks_sha")?,
        issue_id: r.get("issue_id")?,
        reviewed_sha: r.get("reviewed_sha")?,
        review_verdict: r.get("review_verdict")?,
        review_row: r.get("review_row")?,
        review_asked: r.get("review_asked")?,
        review_answered: r.get("review_answered")?,
        trust: Trust::parse(&r.get::<_, String>("trust")?),
        by_agentbench: r.get("by_agentbench")?,
        ci_reworks: r.get("ci_reworks")?,
        ci_sent_sha: r.get("ci_sent_sha")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}

#[derive(Clone, Debug)]
pub struct MirrorIssue {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub state: IssueState,
    pub priority: i64,
    pub labels: Vec<String>,
    pub author: String,
    pub url: String,
    pub updated_at: i64,
    pub trust: Trust,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReadyRow {
    pub status: Option<String>,
    pub hash: Option<String>,
    pub after: i64,
}

#[derive(Clone, Debug)]
pub struct MirrorPr {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub association: String,
    pub head_ref: String,
    pub base_ref: String,
    pub head_sha: String,
    pub state: String,
    pub draft: bool,
    pub url: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub trust: Trust,
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Clone, Debug, PartialEq)]
pub struct Worker {
    pub session: SessionId,
    pub issue: Id,
    pub project: Id,
    pub state: String,
    pub nudges: u32,
    pub reworks: u32,
    pub green: bool,
    pub asked_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Judged {
    pub id: Id,
    pub session: SessionId,
    pub issue: Option<Id>,
    pub head: Option<String>,
    pub verdict: String,
    pub reason: String,
    pub followup: Option<String>,
    pub reply: String,
    pub judge: SessionId,
    pub acted: bool,
    pub created_at: i64,
}

fn judged_row(r: &Row) -> rusqlite::Result<Judged> {
    Ok(Judged {
        id: r.get("id")?,
        session: r.get("session")?,
        issue: r.get("issue")?,
        head: r.get("head")?,
        verdict: r.get("verdict")?,
        reason: r.get("reason")?,
        followup: r.get("followup")?,
        reply: r.get("reply")?,
        judge: r.get("judge")?,
        acted: r.get::<_, i64>("acted")? != 0,
        created_at: r.get("created_at")?,
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Deferred {
    pub id: Id,
    pub source: String,
    pub worker: Option<SessionId>,
    pub urgent: bool,
    pub text: String,
    pub created_at: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenItem {
    pub id: Id,
    pub kind: String,
    pub target: Option<String>,
    pub head: Option<String>,
    pub text: String,
    pub delivered_at: i64,
    pub log_id: Option<Id>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NewLogItem<'a> {
    pub supervisor: Option<SessionId>,
    pub kind: &'a str,
    pub asker: &'a str,
    pub asker_title: &'a str,
    pub session: Option<SessionId>,
    pub project_id: Option<Id>,
    pub issue_id: Option<Id>,
    pub question: &'a str,
    pub needs_answer: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Closing {
    pub action: String,
    pub detail: String,
    pub outcome: String,
    pub reply: String,
    pub issue_id: Option<Id>,
}

impl Closing {
    pub fn new(action: &str) -> Closing {
        Closing { action: action.into(), ..Default::default() }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Closing {
        self.detail = detail.into();
        self
    }

    pub fn outcome(mut self, outcome: impl Into<String>) -> Closing {
        self.outcome = outcome.into();
        self
    }
}

fn log_row(r: &Row) -> rusqlite::Result<agentbench_core::LogItem> {
    let state: Option<String> = r.get("issue_state")?;
    Ok(agentbench_core::LogItem {
        id: r.get("id")?,
        kind: r.get("kind")?,
        asker: r.get("asker")?,
        asker_title: r.get("asker_title")?,
        session: r.get::<_, Option<String>>("session")?.and_then(|s| s.parse().ok()),
        supervisor: r.get::<_, Option<String>>("supervisor")?.and_then(|s| s.parse().ok()),
        project_id: r.get("project_id")?,
        project: r.get::<_, Option<String>>("project")?.unwrap_or_default(),
        issue_id: r.get("issue_id")?,
        issue_state: state.as_deref().and_then(IssueState::parse),
        question: r.get("question")?,
        arrived_at: r.get("arrived_at")?,
        needs_answer: r.get::<_, i64>("needs_answer")? != 0,
        closed_at: r.get("closed_at")?,
        action: r.get("action")?,
        detail: r.get("detail")?,
        outcome: r.get("outcome")?,
        reply: r.get("reply")?,
        merge: r.get("merge")?,
        clipped: false,
    })
}

const LOG_SELECT: &str = "SELECT l.*, p.name AS project, i.state AS issue_state FROM supervisor_log l
     LEFT JOIN projects p ON p.id = l.project_id LEFT JOIN issues i ON i.id = l.issue_id";

#[derive(Clone, Debug, PartialEq)]
pub struct SeenTree {
    pub path: String,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub main: bool,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    memory: Arc<Memory>,
}

pub use crate::memory::hash_note;

const MS_PER_DAY: i64 = 86_400_000;

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn proposal_row(row: &Row) -> rusqlite::Result<Option<agentbench_core::Proposal>> {
    let body: String = row.get(1)?;
    let state: String = row.get(2)?;
    let Ok(mut p) = serde_json::from_str::<agentbench_core::Proposal>(&body) else {
        return Ok(None);
    };
    p.id = row.get(0)?;
    p.created_at = row.get(3)?;
    p.state = match state.as_str() {
        "accepted" => agentbench_core::ProposalState::Accepted,
        "rejected" => agentbench_core::ProposalState::Rejected,
        _ => agentbench_core::ProposalState::Pending,
    };
    Ok(Some(p))
}

fn issue_row(row: &Row) -> rusqlite::Result<Issue> {
    let state: String = row.get("state")?;
    let labels: String = row.get("labels")?;
    Ok(Issue {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        title: row.get("title")?,
        body: row.get("body")?,
        state: IssueState::parse(&state).unwrap_or(IssueState::Backlog),
        priority: row.get("priority")?,
        labels: serde_json::from_str(&labels).unwrap_or_default(),
        goal: row.get("goal")?,
        off_vision: row.get::<_, i64>("off_vision")? != 0,
        depends_on: Vec::new(),
        waiting_on: Vec::new(),
        blocks: Vec::new(),
        source: IssueSource::parse(&row.get::<_, String>("source")?),
        external_number: row.get("external_number")?,
        external_url: row.get("external_url")?,
        external_author: row.get("external_author")?,
        external_updated_at: row.get("external_updated_at")?,
        trust: Trust::parse(&row.get::<_, String>("trust")?),
        ready_by: row.get("ready_by")?,
        held: row.get("held")?,
        model_level: row
            .get::<_, Option<String>>("model_level")?
            .as_deref()
            .and_then(agentbench_core::ModelLevel::parse),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

const SESSION_SELECT: &str =
    "SELECT s.*, w.path AS cwd FROM sessions s JOIN worktrees w ON w.id = s.worktree_id";

fn session_row(row: &Row) -> rusqlite::Result<Session> {
    let status: String = row.get("status")?;
    let usage: String = row.get("usage")?;
    Ok(Session {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        worktree_id: row.get("worktree_id")?,
        agent: row.get("agent")?,
        title: row.get("title")?,
        cwd: row.get("cwd")?,
        model: row.get("model")?,
        effort: row.get("effort")?,
        issue_id: row.get("issue_id")?,
        parent: row.get("parent")?,
        status: SessionStatus::parse(&status).unwrap_or(SessionStatus::Idle),
        agent_session: row.get("agent_session")?,
        usage: serde_json::from_str(&usage).unwrap_or_default(),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        queued: Vec::new(),
        no_plan_reads: None,
        deferred: 0,
        queue_system: row.get::<_, i64>("queue_system")? != 0,
        paused_until: None,
            stuck: false,
        released: row.get::<_, i64>("released")? != 0,
    })
}

#[cfg(unix)]
fn restrict_database_files(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let name = path.as_os_str().to_owned();
    for suffix in ["", "-wal", "-shm"] {
        let mut file = name.clone();
        file.push(suffix);
        let file = std::path::PathBuf::from(file);
        if file.is_file() {
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        }
    }
}

fn worktree_row(row: &Row) -> rusqlite::Result<Worktree> {
    Ok(Worktree {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        path: row.get("path")?,
        branch: row.get("branch")?,
        head: row.get("head")?,
        main: row.get("main")?,
        missing: row.get("missing")?,
        owned: row.get("owned")?,
        created_at: row.get("created_at")?,
    })
}

pub const TITLE_STAND_IN: i64 = 1;
pub const TITLE_GENERATED: i64 = 2;

fn clean_tags(tags: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tag in tags {
        let tag = tag.trim().to_lowercase();
        if !tag.is_empty() && !out.contains(&tag) {
            out.push(tag);
        }
    }
    out
}

fn project_row(row: &Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get("id")?,
        name: row.get("name")?,
        path: row.get("path")?,
        branch: row.get("branch")?,
        remote: row.get("remote")?,
        created_at: row.get("created_at")?,
        home: row.get("home")?,
        supervisor: row.get("supervisor")?,
        pinned: row.get("pinned")?,
        tags: serde_json::from_str(&row.get::<_, String>("tags")?).unwrap_or_default(),
        automation: row
            .get::<_, Option<String>>("automation")?
            .as_deref()
            .and_then(Automation::parse),
        risky: serde_json::from_str(&row.get::<_, String>("risky")?).unwrap_or_default(),
        deploy: row.get("deploy")?,
        tracker: match row.get::<_, String>("tracker")?.as_str() {
            "github" => Tracker::Github { repo: row.get::<_, Option<String>>("github_repo")?.unwrap_or_default() },
            _ => Tracker::Internal,
        },
        post_mode: PostMode::parse(&row.get::<_, String>("post_mode")?).unwrap_or_default(),
        merge_mode: MergeMode::parse(&row.get::<_, String>("merge_mode")?).unwrap_or_default(),
        automerge: AutoMerge::parse(&row.get::<_, String>("automerge")?).unwrap_or_default(),
        merge_method: MergeMethod::parse(&row.get::<_, String>("merge_method")?).unwrap_or_default(),
        judge_unset_goals: row.get("judge_unset_goals")?,
        model_levels: agentbench_core::ModelLevels {
            low: row.get("model_low")?,
            med: row.get("model_med")?,
            high: row.get("model_high")?,
        },
    })
}

fn deps_of(conn: &Connection, id: Id) -> rusqlite::Result<Vec<Id>> {
    let mut stmt = conn.prepare("SELECT depends_on FROM issue_deps WHERE issue_id = ?1 ORDER BY depends_on")?;
    let rows = stmt.query_map([id], |r| r.get(0))?;
    rows.collect()
}

fn dep_path(conn: &Connection, from: Id, to: Id) -> rusqlite::Result<Option<Vec<Id>>> {
    let mut seen = vec![from];
    let mut stack = vec![vec![from]];
    while let Some(path) = stack.pop() {
        let last = *path.last().unwrap();
        if last == to {
            return Ok(Some(path));
        }
        for next in deps_of(conn, last)? {
            if !seen.contains(&next) {
                seen.push(next);
                let mut p = path.clone();
                p.push(next);
                stack.push(p);
            }
        }
    }
    Ok(None)
}

fn fill_deps(conn: &Connection, issues: &mut [Issue]) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT d.issue_id, d.depends_on, COALESCE(i.state, 'done') FROM issue_deps d LEFT JOIN issues i ON i.id = d.depends_on ORDER BY d.depends_on",
    )?;
    let edges: Vec<(Id, Id, String)> =
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    for issue in issues.iter_mut() {
        for (from, on, state) in &edges {
            if *from == issue.id {
                issue.depends_on.push(*on);
                if state != "done" {
                    issue.waiting_on.push(*on);
                }
            }
            if *on == issue.id {
                issue.blocks.push(*from);
            }
        }
        issue.blocks.sort_unstable();
    }
    Ok(())
}

fn comment_row(row: &Row) -> rusqlite::Result<Comment> {
    Ok(Comment {
        id: row.get("id")?,
        issue_id: row.get("issue_id")?,
        author: row.get("author")?,
        body: row.get("body")?,
        created_at: row.get("created_at")?,
        external_id: row.get("external_id")?,
        external_author: row.get("external_author")?,
        external_url: row.get("external_url")?,
        trust: Trust::parse(&row.get::<_, String>("trust")?),
    })
}

fn message_row(row: &Row) -> rusqlite::Result<Message> {
    let entry: String = row.get("entry")?;
    Ok(Message {
        id: row.get("id")?,
        session_id: row.get("session_id")?,
        entry: serde_json::from_str(&entry)
            .unwrap_or(Entry::Error { text: format!("unreadable entry: {entry}") }),
        created_at: row.get("created_at")?,
    })
}

fn not_found(what: &str, id: impl std::fmt::Display) -> StoreError {
    StoreError(format!("{what} {id} not found"))
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        let memory = path.parent().unwrap_or(Path::new(".")).join("memory");
        Store::open_with_memory(path, memory)
    }

    pub fn open_with_memory(path: &Path, memory: std::path::PathBuf) -> Result<Store> {
        if let Some(dir) = path.parent() {
            let existed = dir.is_dir();
            std::fs::create_dir_all(dir).map_err(|e| StoreError(e.to_string()))?;
            #[cfg(unix)]
            if !existed {
                // The store holds the bearer token and model API keys; keep the
                // directory to the owner only instead of the umask default.
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
            }
        }
        let memory = Memory::open(memory);
        let store = Store::setup(Connection::open(path)?, memory, path.parent())?;
        #[cfg(unix)]
        restrict_database_files(path);
        Ok(store)
    }

    pub fn memory() -> Result<Store> {
        Store::setup(Connection::open_in_memory()?, Memory::temporary(), None)
    }

    fn archive_legacy_notes(conn: &Connection, dir: Option<&Path>) -> Result<()> {
        let has = |table: &str| -> Result<bool> {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![table],
                |r| r.get::<_, i64>(0),
            )? > 0)
        };
        let (notes, versions, globals) = (has("notes")?, has("note_versions")?, has("global_notes")?);
        let mut out = serde_json::Map::new();
        if notes {
            let mut stmt = conn.prepare(
                "SELECT n.project_id, p.name, n.topic, n.body, n.updated_at FROM notes n
                 LEFT JOIN projects p ON p.id = n.project_id ORDER BY n.project_id, n.topic",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(serde_json::json!({
                        "project_id": r.get::<_, Id>(0)?,
                        "project": r.get::<_, Option<String>>(1)?,
                        "topic": r.get::<_, String>(2)?,
                        "body": r.get::<_, String>(3)?,
                        "updated_at": r.get::<_, i64>(4)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.insert("notes".into(), rows.into());
        }
        if versions {
            let mut stmt = conn.prepare(
                "SELECT v.id, v.project_id, p.name, v.topic, v.body, v.hash, v.author, v.session, v.created_at
                 FROM note_versions v LEFT JOIN projects p ON p.id = v.project_id ORDER BY v.id",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(serde_json::json!({
                        "id": r.get::<_, Id>(0)?,
                        "project_id": r.get::<_, Id>(1)?,
                        "project": r.get::<_, Option<String>>(2)?,
                        "topic": r.get::<_, String>(3)?,
                        "body": r.get::<_, String>(4)?,
                        "hash": r.get::<_, String>(5)?,
                        "author": r.get::<_, String>(6)?,
                        "session": r.get::<_, Option<String>>(7)?,
                        "created_at": r.get::<_, i64>(8)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.insert("note_versions".into(), rows.into());
        }
        if globals {
            let mut stmt = conn.prepare("SELECT topic, body, updated_at FROM global_notes ORDER BY topic")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(serde_json::json!({
                        "topic": r.get::<_, String>(0)?,
                        "body": r.get::<_, String>(1)?,
                        "updated_at": r.get::<_, i64>(2)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.insert("global_notes".into(), rows.into());
        }
        let any = out.values().any(|v| v.as_array().is_some_and(|a| !a.is_empty()));
        let Some(dir) = dir.filter(|_| any) else {
            return Ok(());
        };
        let secs = now() / 1000;
        let path = dir.join(format!("legacy-notes-{secs}.json"));
        let text = serde_json::to_string_pretty(&serde_json::Value::Object(out))
            .map_err(|e| StoreError(e.to_string()))?;
        std::fs::write(&path, text).map_err(|e| StoreError(e.to_string()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    fn setup(conn: Connection, memory: Memory, dir: Option<&Path>) -> Result<Store> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if (version.max(0) as usize) < SCHEMA.len() {
            Store::archive_legacy_notes(&conn, dir)?;
        }
        for (i, step) in SCHEMA.iter().enumerate().skip(version.max(0) as usize) {
            conn.execute_batch(&format!(
                "BEGIN; {step}; PRAGMA user_version = {}; COMMIT;",
                i + 1
            ))?;
        }
        for (table, column, definition) in ADDED_COLUMNS {
            let present = conn
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |r| r.get::<_, String>(1))?
                .any(|name| name.map(|n| n == *column).unwrap_or(false));
            if !present {
                conn.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN {column} {definition};"
                ))?;
            }
        }
        conn.execute_batch(AFTER_COLUMNS)?;
        for (table, column, definition) in LATE_COLUMNS {
            let present = conn
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |r| r.get::<_, String>(1))?
                .any(|name| name.map(|n| n == *column).unwrap_or(false));
            if !present {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition};"))?;
            }
        }
        let pr_trust = conn
            .prepare("PRAGMA table_info(pull_requests)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .any(|name| name.map(|n| n == "trust").unwrap_or(false));
        if !pr_trust {
            conn.execute_batch("ALTER TABLE pull_requests ADD COLUMN trust TEXT NOT NULL DEFAULT 'trusted';")?;
        }
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Store { conn: Arc::new(Mutex::new(conn)), memory: Arc::new(memory) };
        if store.setting(WIRE_TRANSCRIPTS)?.is_none() {
            store.convert_native_transcripts()?;
        }
        Ok(store)
    }

    fn convert_native_transcripts(&self) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let rows: Vec<(Id, String)> = {
            let mut stmt = tx.prepare(
                "SELECT m.id, m.entry FROM messages m JOIN sessions s ON s.id = m.session_id \
                 WHERE s.agent = 'native'",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
        };
        for (id, text) in rows {
            let Ok(entry) = serde_json::from_str::<Entry>(&text) else { continue };
            let messages = crate::agent::native::wire_of(&entry, "user");
            if messages.is_empty() {
                continue;
            }
            let wire = serde_json::to_string(&Entry::Wire { messages }).unwrap_or_default();
            tx.execute("UPDATE messages SET entry = ?1 WHERE id = ?2", params![wire, id])?;
        }
        tx.execute("INSERT INTO settings (key, value) VALUES (?1, '1')", [WIRE_TRANSCRIPTS])?;
        tx.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn db_for_test(&self) -> MutexGuard<'_, Connection> {
        self.db()
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn changed(conn: &Connection, kind: &str, ref_id: impl std::fmt::Display) -> Result<()> {
        conn.execute(
            "INSERT INTO changes (kind, ref_id, created_at) VALUES (?1, ?2, ?3)",
            params![kind, ref_id.to_string(), now()],
        )?;
        Ok(())
    }

    pub fn cursor(&self) -> Result<Id> {
        Ok(self.db().query_row("SELECT COALESCE(MAX(id), 0) FROM changes", [], |r| r.get(0))?)
    }

    pub fn attention_rows(&self) -> Result<Vec<crate::attention::Row>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT s.id, s.status, s.title, COALESCE(p.name, ''),
                    (s.parent IS NOT NULL AND EXISTS (SELECT 1 FROM sessions b WHERE b.id = s.parent))
                    OR EXISTS (SELECT 1 FROM autopilot_workers w WHERE w.session = s.id)
             FROM sessions s
             LEFT JOIN projects p ON p.id = s.project_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::attention::Row {
                id: r.get(0)?,
                status: r.get(1)?,
                title: r.get(2)?,
                project: r.get(3)?,
                managed: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn changes_since(&self, cursor: Id) -> Result<Vec<(Id, String, String)>> {
        let db = self.db();
        let mut stmt = db
            .prepare("SELECT id, kind, ref_id FROM changes WHERE id > ?1 ORDER BY id LIMIT 1000")?;
        let rows = stmt.query_map([cursor], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn delete_setting(&self, key: &str) -> Result<()> {
        self.db().execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let db = self.db();
        let mut stmt =
            db.prepare("SELECT * FROM projects ORDER BY home DESC, supervisor DESC, name")?;
        let rows = stmt.query_map([], project_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn project(&self, id: Id) -> Result<Project> {
        self.db()
            .query_row("SELECT * FROM projects WHERE id = ?1", [id], project_row)
            .optional()?
            .ok_or_else(|| not_found("project", id))
    }

    pub fn project_by_path(&self, path: &str) -> Result<Option<Project>> {
        Ok(self
            .db()
            .query_row("SELECT * FROM projects WHERE path = ?1", [path], project_row)
            .optional()?)
    }

    pub fn add_project(
        &self,
        name: &str,
        path: &str,
        branch: Option<&str>,
        remote: Option<&str>,
    ) -> Result<Project> {
        if name.trim().is_empty() || path.trim().is_empty() {
            return Err(StoreError("a project needs a name and a path".into()));
        }
        if self.project_by_path(path)?.is_some() {
            return Err(StoreError(format!("{path} is already a project")));
        }
        let id = {
            let db = self.db();
            db.execute(
                "INSERT INTO projects (name, path, branch, remote, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![name.trim(), path, branch, remote, now()],
            )?;
            let id = db.last_insert_rowid();
            Store::changed(&db, "project", id)?;
            id
        };
        self.project(id)
    }

    /// The built-in home project, if it has been made.
    pub fn home_project(&self) -> Result<Option<Project>> {
        Ok(self
            .db()
            .query_row("SELECT * FROM projects WHERE home = 1", [], project_row)
            .optional()?)
    }

    pub fn supervisor_project(&self) -> Result<Option<Project>> {
        Ok(self
            .db()
            .query_row("SELECT * FROM projects WHERE supervisor = 1", [], project_row)
            .optional()?)
    }

    /// Make the home project: `path` as a project and its one, fixed checkout.
    pub fn add_home(&self, name: &str, path: &str) -> Result<Project> {
        self.add_builtin(name, path, "home")
    }

    pub fn add_supervisor(&self, name: &str, path: &str) -> Result<Project> {
        self.add_builtin(name, path, "supervisor")
    }

    fn add_builtin(&self, name: &str, path: &str, kind: &str) -> Result<Project> {
        if self.project_by_path(path)?.is_some() {
            return Err(StoreError(format!("{path} is already a project")));
        }
        let id = {
            let db = self.db();
            let t = now();
            db.execute(
                &format!("INSERT INTO projects (name, path, branch, remote, created_at, {kind}) VALUES (?1, ?2, NULL, NULL, ?3, 1)"),
                params![name, path, t],
            )?;
            let id = db.last_insert_rowid();
            db.execute(
                "INSERT INTO worktrees (project_id, path, branch, head, main, missing, owned, created_at)
                 VALUES (?1, ?2, NULL, NULL, 1, 0, 0, ?3)",
                params![id, path, t],
            )?;
            Store::changed(&db, "project", id)?;
            id
        };
        self.project(id)
    }

    pub fn update_project(&self, id: Id, patch: &ProjectPatch) -> Result<Project> {
        let mut project = self.project(id)?;
        if let Some(name) = &patch.name {
            let name = name.trim();
            if name.is_empty() {
                return Err(StoreError("a project needs a name".into()));
            }
            project.name = name.to_string();
        }
        if let Some(pinned) = patch.pinned {
            project.pinned = pinned;
        }
        if let Some(tags) = &patch.tags {
            project.tags = clean_tags(tags);
        }
        if let Some(level) = patch.automation {
            project.automation = level;
        }
        if let Some(risky) = &patch.risky {
            let mut kept: Vec<Risky> = Vec::new();
            for r in risky {
                if !kept.contains(r) {
                    kept.push(*r);
                }
            }
            project.risky = kept;
        }
        if let Some(deploy) = &patch.deploy {
            project.deploy = Some(deploy.trim().to_string()).filter(|d| !d.is_empty());
        }
        if let Some(tracker) = &patch.tracker {
            project.tracker = match tracker {
                Tracker::Github { repo } => Tracker::Github { repo: repo.trim().to_string() },
                other => other.clone(),
            };
        }
        if let Some(mode) = patch.post_mode {
            project.post_mode = mode;
        }
        if let Some(mode) = patch.merge_mode {
            project.merge_mode = mode;
        }
        if let Some(mode) = patch.automerge {
            project.automerge = mode;
        }
        if let Some(method) = patch.merge_method {
            project.merge_method = method;
        }
        if let Some(judge) = patch.judge_unset_goals {
            project.judge_unset_goals = judge;
        }
        if let Some(levels) = &patch.model_levels {
            project.model_levels = agentbench_core::ModelLevels {
                low: levels.low.trim().to_string(),
                med: levels.med.trim().to_string(),
                high: levels.high.trim().to_string(),
            };
        }
        {
            let db = self.db();
            db.execute(
                "UPDATE projects SET name = ?1, pinned = ?2, tags = ?3, automation = ?4, risky = ?5, deploy = ?6, tracker = ?8, github_repo = ?9, post_mode = ?10, model_low = ?11, model_med = ?12, model_high = ?13, merge_mode = ?14, automerge = ?15, merge_method = ?16, judge_unset_goals = ?17 WHERE id = ?7",
                params![
                    project.name,
                    project.pinned,
                    serde_json::to_string(&project.tags).unwrap_or_else(|_| "[]".into()),
                    project.automation.map(Automation::as_str),
                    serde_json::to_string(&project.risky).unwrap_or_else(|_| "[]".into()),
                    project.deploy,
                    id,
                    if project.tracker.is_github() { "github" } else { "internal" },
                    match &project.tracker {
                        Tracker::Github { repo } => Some(repo.clone()),
                        _ => None,
                    },
                    project.post_mode.as_str(),
                    project.model_levels.low,
                    project.model_levels.med,
                    project.model_levels.high,
                    project.merge_mode.as_str(),
                    project.automerge.as_str(),
                    project.merge_method.as_str(),
                    project.judge_unset_goals
                ],
            )?;
            Store::changed(&db, "project", id)?;
        }
        self.project(id)
    }

    pub fn delete_project(&self, id: Id) -> Result<()> {
        if self.project(id)?.builtin() {
            return Err(StoreError("that project is built in and cannot be removed".into()));
        }
        let db = self.db();
        db.execute("DELETE FROM sessions WHERE project_id = ?1", [id])?;
        if db.execute("DELETE FROM projects WHERE id = ?1", [id])? == 0 {
            return Err(not_found("project", id));
        }
        Store::changed(&db, "project", id)
    }

    pub fn worktrees(&self, project_id: Option<Id>) -> Result<Vec<Worktree>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT * FROM worktrees WHERE (?1 IS NULL OR project_id = ?1)
             ORDER BY project_id, main DESC, branch, path",
        )?;
        let rows = stmt.query_map([project_id], worktree_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn worktree(&self, id: Id) -> Result<Worktree> {
        self.db()
            .query_row("SELECT * FROM worktrees WHERE id = ?1", [id], worktree_row)
            .optional()?
            .ok_or_else(|| not_found("worktree", id))
    }

    pub fn main_worktree(&self, project_id: Id) -> Result<Worktree> {
        self.db()
            .query_row(
                "SELECT * FROM worktrees WHERE project_id = ?1 AND main = 1",
                [project_id],
                worktree_row,
            )
            .optional()?
            .ok_or_else(|| StoreError(format!("project {project_id} has no main worktree")))
    }

    pub fn sync_worktrees(
        &self,
        project_id: Id,
        seen: &[SeenTree],
        created: Option<&str>,
    ) -> Result<bool> {
        let before = self.worktrees(Some(project_id))?;
        let db = self.db();
        let t = now();
        let mut changed = false;
        for tree in seen {
            let known = before.iter().find(|w| w.path == tree.path);
            let fresh = known.is_none_or(|w| {
                w.branch != tree.branch || w.head != tree.head || w.main != tree.main || w.missing
            });
            if !fresh {
                continue;
            }
            changed = true;
            db.execute(
                "INSERT INTO worktrees (project_id, path, branch, head, main, missing, owned, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)
                 ON CONFLICT(path) DO UPDATE SET branch = excluded.branch, head = excluded.head,
                     main = excluded.main, missing = 0",
                params![
                    project_id,
                    tree.path,
                    tree.branch,
                    tree.head,
                    tree.main,
                    created == Some(tree.path.as_str()),
                    t
                ],
            )?;
        }
        for gone in before.iter().filter(|w| !seen.iter().any(|t| t.path == w.path)) {
            let in_use: i64 = db.query_row(
                "SELECT COUNT(*) FROM sessions WHERE worktree_id = ?1",
                [gone.id],
                |r| r.get(0),
            )?;
            if in_use > 0 {
                if !gone.missing {
                    changed = true;
                    db.execute("UPDATE worktrees SET missing = 1 WHERE id = ?1", [gone.id])?;
                }
            } else {
                // Nothing left to read: a worktree already marked missing is dropped
                // once its last session is deleted, instead of lingering as "gone".
                changed = true;
                db.execute("DELETE FROM worktrees WHERE id = ?1", [gone.id])?;
            }
        }
        if let Some(main) = seen.iter().find(|t| t.main) {
            db.execute(
                "UPDATE projects SET branch = ?1 WHERE id = ?2",
                params![main.branch, project_id],
            )?;
        }
        if changed {
            Store::changed(&db, "project", project_id)?;
        }
        Ok(changed)
    }

    pub fn forget_worktree(&self, id: Id) -> Result<()> {
        let w = self.worktree(id)?;
        let db = self.db();
        db.execute("DELETE FROM sessions WHERE worktree_id = ?1", [id])?;
        db.execute("DELETE FROM worktrees WHERE id = ?1", [id])?;
        Store::changed(&db, "project", w.project_id)
    }

    pub fn issues(&self) -> Result<Vec<Issue>> {
        let db = self.db();
        let mut stmt =
            db.prepare("SELECT * FROM issues ORDER BY priority DESC, updated_at DESC")?;
        let rows = stmt.query_map([], issue_row)?;
        let mut issues: Vec<Issue> = rows.collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        fill_deps(&db, &mut issues)?;
        Ok(issues)
    }

    pub fn issue(&self, id: Id) -> Result<Issue> {
        let db = self.db();
        let issue = db
            .query_row("SELECT * FROM issues WHERE id = ?1", [id], issue_row)
            .optional()?
            .ok_or_else(|| not_found("issue", id))?;
        let mut issues = [issue];
        fill_deps(&db, &mut issues)?;
        let [issue] = issues;
        Ok(issue)
    }

    pub fn set_deps(&self, id: Id, deps: &[Id]) -> Result<()> {
        let mut wanted: Vec<Id> = Vec::new();
        for d in deps {
            if !wanted.contains(d) {
                wanted.push(*d);
            }
        }
        let mut db = self.db();
        let tx = db.transaction()?;
        let exists = |i: Id| -> rusqlite::Result<bool> {
            tx.query_row("SELECT 1 FROM issues WHERE id = ?1", [i], |_| Ok(())).optional().map(|r| r.is_some())
        };
        if !exists(id)? {
            return Err(not_found("issue", id));
        }
        for d in &wanted {
            if !exists(*d)? {
                return Err(not_found("issue", d));
            }
        }
        tx.execute("DELETE FROM issue_deps WHERE issue_id = ?1", [id])?;
        for d in &wanted {
            tx.execute(
                "INSERT INTO issue_deps (issue_id, depends_on) VALUES (?1, ?2)",
                params![id, d],
            )?;
        }
        for d in &wanted {
            if let Some(path) = dep_path(&tx, *d, id)? {
                let mut chain = vec![id];
                chain.extend(path);
                let chain: Vec<String> = chain.iter().map(|i| format!("#{i}")).collect();
                return Err(StoreError(format!(
                    "a dependency cycle: {} depends on itself ({})",
                    chain[0],
                    chain.join(" -> ")
                )));
            }
        }
        tx.commit()?;
        Store::changed(&db, "issue", id)
    }

    pub fn unblock_dependents(&self, done: Id) -> Result<()> {
        let dependents: Vec<Id> = {
            let db = self.db();
            let mut stmt = db.prepare("SELECT issue_id FROM issue_deps WHERE depends_on = ?1 ORDER BY issue_id")?;
            let rows = stmt.query_map([done], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for d in dependents {
            let issue = self.issue(d)?;
            if issue.state == IssueState::Done || !issue.waiting_on.is_empty() {
                continue;
            }
            let mut note = format!("unblocked by #{done}");
            if issue.state == IssueState::Backlog && issue.goal.is_some() {
                self.update_issue(
                    d,
                    &IssuePatch { state: Some(IssueState::Todo), ..Default::default() },
                )?;
                note.push_str("; moved to todo");
            }
            self.add_comment_local(d, &NewComment { author: Some("agentbench".into()), body: note })?;
            self.log_autopilot(
                Some(issue.project_id),
                Some(d),
                &format!("unblocked #{d} (all dependencies done)"),
            )?;
        }
        Ok(())
    }

    pub fn add_issue(&self, new: &NewIssue) -> Result<Issue> {
        if new.title.trim().is_empty() {
            return Err(StoreError("an issue needs a title".into()));
        }
        self.project(new.project_id)?;
        let goal = self.checked_goal(new.project_id, new.goal.as_deref())?;
        let goal = goal.as_deref();
        let t = now();
        let id = {
            let db = self.db();
            db.execute(
                "INSERT INTO issues (project_id, title, body, state, priority, labels, goal, off_vision, model_level, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?8, ?9, ?10, ?7, ?7)",
                params![
                    new.project_id,
                    new.title.trim(),
                    new.body,
                    new.state.unwrap_or(IssueState::Todo).as_str(),
                    new.priority.unwrap_or(0),
                    serde_json::to_string(&new.labels).unwrap_or_else(|_| "[]".into()),
                    t,
                    goal,
                    new.off_vision && goal.is_none(),
                    new.model_level.filter(|l| *l != agentbench_core::ModelLevel::Unknown).map(agentbench_core::ModelLevel::as_str)
                ],
            )?;
            let id = db.last_insert_rowid();
            Store::changed(&db, "issue", id)?;
            id
        };
        if !new.depends_on.is_empty()
            && let Err(e) = self.set_deps(id, &new.depends_on)
        {
            let _ = self.delete_issue(id);
            return Err(e);
        }
        self.issue(id)
    }

    pub fn update_issue(&self, id: Id, patch: &IssuePatch) -> Result<Issue> {
        let mut issue = self.issue(id)?;
        let before = issue.clone();
        let was_done = issue.state == IssueState::Done;
        if let Some(project) = patch.project.filter(|p| *p != issue.project_id) {
            self.project(project)?;
            let db = self.db();
            db.execute(
                "UPDATE issues SET project_id = ?1, goal = NULL WHERE id = ?2",
                params![project, id],
            )?;
            drop(db);
            issue.project_id = project;
            issue.goal = None;
        }
        if let Some(deps) = &patch.depends_on {
            self.set_deps(id, deps)?;
        }
        if let Some(title) = &patch.title {
            if title.trim().is_empty() {
                return Err(StoreError("an issue needs a title".into()));
            }
            issue.title = title.trim().to_string();
        }
        if let Some(body) = &patch.body {
            issue.body = body.clone();
        }
        if let Some(state) = patch.state {
            issue.state = state;
        }
        if let Some(priority) = patch.priority {
            issue.priority = priority;
        }
        if let Some(labels) = &patch.labels {
            issue.labels = labels.clone();
        }
        if let Some(goal) = &patch.goal {
            issue.goal = self.checked_goal(issue.project_id, Some(goal))?;
            if issue.goal.is_some() {
                issue.off_vision = false;
            }
        }
        if let Some(level) = patch.model_level {
            issue.model_level = level.filter(|l| *l != agentbench_core::ModelLevel::Unknown);
        }
        if let Some(off) = patch.off_vision {
            issue.off_vision = off;
            if off {
                issue.goal = None;
            }
        }
        {
            let db = self.db();
            db.execute(
                "UPDATE issues SET title = ?1, body = ?2, state = ?3, priority = ?4,
                 labels = ?5, updated_at = ?6, goal = ?8, off_vision = ?9, model_level = ?10 WHERE id = ?7",
                params![
                    issue.title,
                    issue.body,
                    issue.state.as_str(),
                    issue.priority,
                    serde_json::to_string(&issue.labels).unwrap_or_else(|_| "[]".into()),
                    now(),
                    id,
                    issue.goal,
                    issue.off_vision,
                    issue.model_level.map(agentbench_core::ModelLevel::as_str)
                ],
            )?;
            Store::changed(&db, "issue", id)?;
        }
        if issue.state == IssueState::Done && !was_done {
            self.unblock_dependents(id)?;
        }
        let waiting = |i: &Issue| matches!(i.state, IssueState::Backlog | IssueState::Todo) && i.goal.is_none() && !i.off_vision;
        if waiting(&issue) && !waiting(&before) || issue.project_id != before.project_id {
            self.set_judge_mark(id, "", 0)?;
        }
        let issue = self.issue(id)?;
        if issue.source == IssueSource::Github {
            crate::github::flow::mirror_update(self, &before, &issue);
        }
        Ok(issue)
    }

    pub fn open_pr_numbers(&self, project_id: Id) -> Result<Vec<(i64, Option<Id>)>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT number, issue_id FROM pull_requests WHERE project_id = ?1 AND state = 'open' ORDER BY number")?;
        let rows = stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn pr_state(&self, project_id: Id, number: i64) -> Result<Option<String>> {
        Ok(self
            .db()
            .query_row("SELECT state FROM pull_requests WHERE project_id = ?1 AND number = ?2", params![project_id, number], |r| r.get(0))
            .optional()?)
    }

    pub fn set_pr_state(&self, project_id: Id, number: i64, state: &str) -> Result<()> {
        self.db().execute(
            "UPDATE pull_requests SET state = ?3 WHERE project_id = ?1 AND number = ?2",
            params![project_id, number, state],
        )?;
        Ok(())
    }

    pub fn link_external(&self, issue_id: Id, number: i64, url: &str, author: &str) -> Result<()> {
        let issue = self.issue(issue_id)?;
        if let Some(other) = self.mirrored_issue(issue.project_id, number)?
            && other.id != issue_id
        {
            self.delete_issue(issue_id)?;
            return Ok(());
        }
        let db = self.db();
        db.execute(
            "UPDATE issues SET source = 'github', external_number = ?2, external_url = ?3, external_author = ?4 WHERE id = ?1",
            params![issue_id, number, url, author],
        )?;
        Store::changed(&db, "issue", issue_id)
    }

    pub fn outbox(&self, project_id: Option<Id>) -> Result<Vec<OutboxItem>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT * FROM outbox WHERE (?1 IS NULL OR project_id = ?1) ORDER BY id")?;
        let rows = stmt.query_map([project_id], outbox_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn outbox_item(&self, id: Id) -> Result<OutboxItem> {
        self.db()
            .query_row("SELECT * FROM outbox WHERE id = ?1", [id], outbox_row)
            .optional()?
            .ok_or_else(|| not_found("outbox row", id))
    }

    pub fn outbox_insert(&self, new: &NewOutbox) -> Result<Id> {
        let db = self.db();
        let t = now();
        db.execute(
            "INSERT INTO outbox (project_id, kind, payload, status, created_at, asked_by, trusted, number, text, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?5)",
            params![new.project_id, new.kind, new.payload, new.status, t, new.asked_by, new.trusted, new.number, new.text],
        )?;
        Ok(db.last_insert_rowid())
    }

    pub fn outbox_transition(&self, id: Id, from: &[&str], to: &str, text: Option<&str>) -> Result<bool> {
        let db = self.db();
        let current: Option<String> =
            db.query_row("SELECT status FROM outbox WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        let Some(current) = current else { return Err(not_found("outbox row", id)) };
        if !from.contains(&current.as_str()) {
            return Ok(false);
        }
        db.execute(
            "UPDATE outbox SET status = ?2, text = COALESCE(?3, text), updated_at = ?4 WHERE id = ?1 AND status = ?5",
            params![id, to, text, now(), current],
        )?;
        Ok(true)
    }

    pub fn outbox_result(
        &self,
        id: Id,
        status: &str,
        external_id: Option<&str>,
        error: Option<&str>,
        attempts: i64,
        next_at: i64,
    ) -> Result<()> {
        self.db().execute(
            "UPDATE outbox SET status = ?2, external_id = COALESCE(?3, external_id), error = ?4, attempts = ?5, next_at = ?6, updated_at = ?7 WHERE id = ?1",
            params![id, status, external_id, error, attempts, next_at, now()],
        )?;
        Ok(())
    }

    pub fn outbox_set_approver(&self, id: Id, by: &str) -> Result<()> {
        self.db().execute("UPDATE outbox SET approved_by = ?2 WHERE id = ?1", params![id, by])?;
        Ok(())
    }

    pub fn target_trust(&self, project_id: Id, number: i64) -> Result<Trust> {
        if let Some(t) = self.pr_trust(project_id, number)? {
            return Ok(t);
        }
        Ok(self.mirrored_issue(project_id, number)?.map_or(Trust::Trusted, |i| i.trust))
    }

    pub fn outbox_ready(&self, at: i64) -> Result<Vec<OutboxItem>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT * FROM outbox WHERE status = 'approved' AND next_at <= ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([at], outbox_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mirror_issue(&self, project_id: Id, m: &MirrorIssue) -> Result<Id> {
        let db = self.db();
        let labels = serde_json::to_string(&m.labels).unwrap_or_else(|_| "[]".into());
        let existing: Option<Issue> = db
            .query_row(
                "SELECT * FROM issues WHERE project_id = ?1 AND source = 'github' AND external_number = ?2",
                params![project_id, m.number],
                issue_row,
            )
            .optional()?;
        let Some(old) = existing else {
            let t = now();
            db.execute(
                "INSERT INTO issues (project_id, title, body, state, priority, labels, source, external_number, external_url, external_author, external_updated_at, created_at, updated_at, trust)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'github', ?7, ?8, ?9, ?10, ?11, ?11, ?12)",
                params![project_id, m.title, m.body, m.state.as_str(), m.priority, labels, m.number, m.url, m.author, m.updated_at, t, m.trust.as_str()],
            )?;
            let id = db.last_insert_rowid();
            Store::changed(&db, "issue", id)?;
            return Ok(id);
        };
        let same = old.external_updated_at == Some(m.updated_at)
            && old.external_url.as_deref() == Some(m.url.as_str())
            && old.external_author.as_deref() == Some(m.author.as_str())
            && old.trust == m.trust;
        if !same {
            db.execute(
                "UPDATE issues SET title = ?1, body = ?2, state = ?3, priority = ?4, labels = ?5, external_url = ?6, external_author = ?7, external_updated_at = ?8, updated_at = ?9, trust = ?11 WHERE id = ?10",
                params![m.title, m.body, m.state.as_str(), m.priority, labels, m.url, m.author, m.updated_at, now(), old.id, m.trust.as_str()],
            )?;
            Store::changed(&db, "issue", old.id)?;
        }
        Ok(old.id)
    }

    pub fn ready_row(&self, id: Id) -> Result<ReadyRow> {
        Ok(self.db().query_row(
            "SELECT ready_status, ready_hash, ready_after FROM issues WHERE id = ?1",
            [id],
            |r| Ok(ReadyRow { status: r.get(0)?, hash: r.get(1)?, after: r.get(2)? }),
        )?)
    }

    pub fn set_ready(&self, id: Id, status: Option<&str>, by: Option<&str>, hash: Option<&str>, after: i64) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE issues SET ready_status = ?1, ready_by = ?2, ready_hash = ?3, ready_after = ?4 WHERE id = ?5",
            params![status, by, hash, after, id],
        )?;
        Store::changed(&db, "issue", id)
    }

    pub fn set_held(&self, id: Id, held: Option<&str>) -> Result<()> {
        let db = self.db();
        db.execute("UPDATE issues SET held = ?1 WHERE id = ?2", params![held, id])?;
        Store::changed(&db, "issue", id)
    }

    pub fn set_mirror_state(&self, id: Id, state: IssueState) -> Result<()> {
        let db = self.db();
        db.execute("UPDATE issues SET state = ?1 WHERE id = ?2", params![state.as_str(), id])?;
        Store::changed(&db, "issue", id)
    }

    pub fn mirrored_issues(&self, project_id: Id) -> Result<Vec<Issue>> {
        Ok(self.issues()?.into_iter().filter(|i| i.project_id == project_id && i.source == IssueSource::Github).collect())
    }

    pub fn mirrored_issue(&self, project_id: Id, number: i64) -> Result<Option<Issue>> {
        let id: Option<Id> = self
            .db()
            .query_row(
                "SELECT id FROM issues WHERE project_id = ?1 AND source = 'github' AND external_number = ?2",
                params![project_id, number],
                |r| r.get(0),
            )
            .optional()?;
        id.map(|id| self.issue(id)).transpose()
    }

    pub fn open_mirrored_numbers(&self, project_id: Id) -> Result<Vec<i64>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT external_number FROM issues WHERE project_id = ?1 AND source = 'github' AND state != 'done' ORDER BY external_number",
        )?;
        let rows = stmt.query_map([project_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mirror_comment(
        &self,
        issue_id: Id,
        external_id: &str,
        author: &str,
        body: &str,
        url: &str,
        created_at: i64,
        trust: Trust,
    ) -> Result<bool> {
        let db = self.db();
        let old: Option<(Id, String)> = db
            .query_row(
                "SELECT id, body FROM comments WHERE issue_id = ?1 AND external_id = ?2",
                params![issue_id, external_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match old {
            Some((_, b)) if b == body => Ok(false),
            Some((id, _)) => {
                db.execute("UPDATE comments SET body = ?1 WHERE id = ?2", params![body, id])?;
                Store::changed(&db, "issue", issue_id)?;
                Ok(true)
            }
            None => {
                db.execute(
                    "INSERT INTO comments (issue_id, author, body, created_at, external_id, external_author, external_url, trust) VALUES (?1, ?2, ?3, ?4, ?5, ?2, ?6, ?7)",
                    params![issue_id, author, body, created_at, external_id, url, trust.as_str()],
                )?;
                Store::changed(&db, "issue", issue_id)?;
                Ok(true)
            }
        }
    }

    pub fn mirror_pr(&self, project_id: Id, m: &MirrorPr) -> Result<()> {
        let db = self.db();
        db.execute(
            "INSERT INTO pull_requests (project_id, number, title, body, author, head_ref, base_ref, head_sha, state, draft, url, created_at, updated_at, trust)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(project_id, number) DO UPDATE SET title = excluded.title, body = excluded.body, author = excluded.author,
               head_ref = excluded.head_ref, base_ref = excluded.base_ref, head_sha = excluded.head_sha, state = excluded.state,
               draft = excluded.draft, url = excluded.url, updated_at = excluded.updated_at, trust = excluded.trust",
            params![project_id, m.number, m.title, m.body, m.author, m.head_ref, m.base_ref, m.head_sha, m.state, m.draft, m.url, m.created_at, m.updated_at, m.trust.as_str()],
        )?;
        Ok(())
    }

    pub fn close_prs_not_in(&self, project_id: Id, open: &[i64]) -> Result<()> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT number FROM pull_requests WHERE project_id = ?1 AND state = 'open'")?;
        let numbers: Vec<i64> =
            stmt.query_map([project_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        for n in numbers.into_iter().filter(|n| !open.contains(n)) {
            db.execute(
                "UPDATE pull_requests SET state = 'closed' WHERE project_id = ?1 AND number = ?2",
                params![project_id, n],
            )?;
        }
        Ok(())
    }

    pub fn open_pr_texts(&self, project_id: Id) -> Result<Vec<(i64, String, String)>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT number, body, head_ref FROM pull_requests WHERE project_id = ?1 AND state = 'open' ORDER BY number",
        )?;
        let rows = stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn link_pr(&self, project_id: Id, pr_number: i64, issue_number: Option<i64>) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE pull_requests SET issue_id = (SELECT id FROM issues WHERE project_id = ?1 AND source = 'github' AND external_number = ?3)
             WHERE project_id = ?1 AND number = ?2",
            params![project_id, pr_number, issue_number],
        )?;
        Ok(())
    }

    pub fn pr_issue(&self, project_id: Id, pr_number: i64) -> Result<Option<Id>> {
        Ok(self
            .db()
            .query_row(
                "SELECT issue_id FROM pull_requests WHERE project_id = ?1 AND number = ?2",
                params![project_id, pr_number],
                |r| r.get::<_, Option<Id>>(0),
            )
            .optional()?
            .flatten())
    }

    pub fn pr_trust(&self, project_id: Id, number: i64) -> Result<Option<Trust>> {
        Ok(self
            .db()
            .query_row(
                "SELECT trust FROM pull_requests WHERE project_id = ?1 AND number = ?2",
                params![project_id, number],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .map(|t| Trust::parse(&t)))
    }

    pub fn pr_count(&self, project_id: Id, state: &str) -> Result<i64> {
        Ok(self.db().query_row(
            "SELECT count(*) FROM pull_requests WHERE project_id = ?1 AND state = ?2",
            params![project_id, state],
            |r| r.get(0),
        )?)
    }

    pub fn pull_requests(&self, project_id: Id) -> Result<Vec<PullRequest>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT * FROM pull_requests WHERE project_id = ?1 ORDER BY number")?;
        let rows = stmt.query_map([project_id], pull_request_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn pull_request(&self, project_id: Id, number: i64) -> Result<Option<PullRequest>> {
        Ok(self
            .db()
            .query_row("SELECT * FROM pull_requests WHERE project_id = ?1 AND number = ?2", params![project_id, number], pull_request_row)
            .optional()?)
    }

    fn pr_update(&self, project_id: Id, number: i64, set: &str, values: &[&dyn rusqlite::ToSql]) -> Result<()> {
        let db = self.db();
        let mut all: Vec<&dyn rusqlite::ToSql> = vec![&project_id, &number];
        all.extend_from_slice(values);
        db.execute(&format!("UPDATE pull_requests SET {set} WHERE project_id = ?1 AND number = ?2"), all.as_slice())?;
        Store::changed(&db, "pull_request", number)
    }

    pub fn set_pr_checks(&self, project_id: Id, number: i64, sha: &str, checks: &[PrCheck]) -> Result<()> {
        let json = serde_json::to_string(checks).unwrap_or_else(|_| "[]".into());
        self.pr_update(project_id, number, "checks = ?3, checks_sha = ?4", &[&json, &sha])
    }

    pub fn set_pr_review(&self, project_id: Id, number: i64, sha: &str, verdict: &str, row: Id, answered: i64) -> Result<()> {
        self.pr_update(
            project_id,
            number,
            "reviewed_sha = ?3, review_verdict = ?4, review_row = ?5, review_answered = ?6",
            &[&sha, &verdict, &row, &answered],
        )
    }

    pub fn ask_pr_review(&self, project_id: Id, number: i64) -> Result<()> {
        self.pr_update(project_id, number, "review_asked = review_asked + 1", &[])
    }

    pub fn pr_review_command(&self, project_id: Id, number: i64, comment_id: i64) -> Result<bool> {
        let db = self.db();
        let n = db.execute(
            "UPDATE pull_requests SET review_seen = ?3, review_asked = review_asked + 1 WHERE project_id = ?1 AND number = ?2 AND review_seen < ?3",
            params![project_id, number, comment_id],
        )?;
        if n > 0 {
            Store::changed(&db, "pull_request", number)?;
        }
        Ok(n > 0)
    }

    pub fn set_pr_by_agentbench(&self, project_id: Id, number: i64) -> Result<()> {
        self.pr_update(project_id, number, "by_agentbench = 1", &[])
    }

    pub fn set_pr_ci_sent(&self, project_id: Id, number: i64, sha: &str) -> Result<()> {
        self.pr_update(project_id, number, "ci_reworks = ci_reworks + 1, ci_sent_sha = ?3", &[&sha])
    }

    pub fn delete_issue(&self, id: Id) -> Result<()> {
        let db = self.db();
        if db.execute("DELETE FROM issues WHERE id = ?1", [id])? == 0 {
            return Err(not_found("issue", id));
        }
        db.execute("DELETE FROM issue_deps WHERE issue_id = ?1 OR depends_on = ?1", [id])?;
        Store::changed(&db, "issue", id)
    }

    pub fn comments(&self, issue_id: Id) -> Result<Vec<Comment>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT * FROM comments WHERE issue_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([issue_id], comment_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_comment(&self, issue_id: Id, new: &NewComment) -> Result<Comment> {
        let comment = self.add_comment_local(issue_id, new)?;
        let issue = self.issue(issue_id)?;
        let by_user = new.author.as_deref().is_none_or(|a| a.trim().is_empty());
        if issue.source == IssueSource::Github && by_user {
            if let Some(number) = issue.external_number {
                let trusted = crate::github::flow::trusted_target(self, &issue);
                crate::github::flow::queue(self, issue.project_id, "comment", number, &comment.body, serde_json::json!({"issue_id": issue_id}), trusted);
            }
        }
        Ok(comment)
    }

    pub(crate) fn add_comment_local(&self, issue_id: Id, new: &NewComment) -> Result<Comment> {
        if new.body.trim().is_empty() {
            return Err(StoreError("a comment needs a body".into()));
        }
        self.issue(issue_id)?;
        let db = self.db();
        let t = now();
        let author = new.author.clone().filter(|a| !a.trim().is_empty()).unwrap_or("you".into());
        db.execute(
            "INSERT INTO comments (issue_id, author, body, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![issue_id, author, new.body.trim(), t],
        )?;
        let id = db.last_insert_rowid();
        db.execute("UPDATE issues SET updated_at = ?1 WHERE id = ?2", params![t, issue_id])?;
        Store::changed(&db, "issue", issue_id)?;
        Ok(db.query_row("SELECT * FROM comments WHERE id = ?1", [id], comment_row)?)
    }

    pub fn sessions(&self) -> Result<Vec<Session>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!("{SESSION_SELECT} ORDER BY s.updated_at DESC"))?;
        let rows = stmt.query_map([], session_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn session(&self, id: SessionId) -> Result<Session> {
        self.db()
            .query_row(&format!("{SESSION_SELECT} WHERE s.id = ?1"), [id], session_row)
            .optional()?
            .ok_or_else(|| not_found("session", id))
    }

    pub fn set_parent(&self, id: SessionId, parent: Option<SessionId>) -> Result<()> {
        let db = self.db();
        db.execute("UPDATE sessions SET parent = ?1 WHERE id = ?2", params![parent, id])?;
        Store::changed(&db, "session", id)?;
        Ok(())
    }

    pub fn set_released(&self, id: SessionId, released: bool) -> Result<()> {
        let db = self.db();
        db.execute("UPDATE sessions SET released = ?1 WHERE id = ?2", params![released as i64, id])?;
        Store::changed(&db, "session", id)?;
        Ok(())
    }

    pub fn remove_watch(&self, watched: SessionId, watcher: SessionId) -> Result<bool> {
        let n = self
            .db()
            .execute("DELETE FROM watches WHERE watched = ?1 AND watcher = ?2", params![watched, watcher])?;
        Ok(n > 0)
    }

    pub fn add_session(
        &self,
        worktree_id: Id,
        agent: &str,
        title: &str,
        model: Option<&str>,
        issue_id: Option<Id>,
    ) -> Result<Session> {
        let worktree = self.worktree(worktree_id)?;
        if worktree.missing {
            return Err(StoreError(format!("{} no longer exists", worktree.path)));
        }
        let t = now();
        let id = SessionId(uuid::Uuid::new_v4());
        {
            let db = self.db();
            db.execute(
                "INSERT INTO sessions (id, project_id, worktree_id, agent, title, model, issue_id, status, created_at, updated_at)
                 VALUES (?9, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                params![
                    worktree.project_id,
                    worktree_id,
                    agent,
                    title,
                    model,
                    issue_id,
                    SessionStatus::Idle.as_str(),
                    t,
                    id
                ],
            )?;
            Store::changed(&db, "session", id)?;
        }
        self.session(id)
    }

    pub fn set_status(&self, id: SessionId, status: SessionStatus) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.as_str(), now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    pub fn touch_session(&self, id: SessionId) -> Result<()> {
        let db = self.db();
        db.execute("UPDATE sessions SET updated_at = ?1 WHERE id = ?2", params![now(), id])?;
        Store::changed(&db, "session", id)
    }

    pub fn set_context(&self, id: SessionId, context: u64) -> Result<()> {
        let mut usage = self.session(id)?.usage;
        usage.context = context;
        let db = self.db();
        db.execute(
            "UPDATE sessions SET usage = ?1, updated_at = ?2 WHERE id = ?3",
            params![serde_json::to_string(&usage).unwrap_or_default(), now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    /// Forget the agent's own session, so it starts afresh. The context it held is gone
    /// with it, so the reading of how full it was goes too.
    pub fn clear_agent_session(&self, id: SessionId) -> Result<()> {
        let mut usage = self.session(id)?.usage;
        usage.context = 0;
        let db = self.db();
        db.execute(
            "UPDATE sessions SET agent_session = NULL, usage = ?1, updated_at = ?2 WHERE id = ?3",
            params![serde_json::to_string(&usage).unwrap_or_default(), now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    /// Mark a session's title as a stand-in (cut from the first message) that a
    /// model may replace once there is a conversation to name.
    pub fn set_title_auto(&self, id: SessionId, auto: bool) -> Result<()> {
        self.db().execute(
            "UPDATE sessions SET title_auto = ?1, title_turns = 0 WHERE id = ?2",
            params![auto, id],
        )?;
        Ok(())
    }

    pub fn title_is_auto(&self, id: SessionId) -> Result<bool> {
        Ok(self.title_state(id)?.0 == TITLE_STAND_IN)
    }

    pub fn title_state(&self, id: SessionId) -> Result<(i64, i64)> {
        self.db()
            .query_row("SELECT title_auto, title_turns FROM sessions WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?
            .ok_or_else(|| not_found("session", id))
    }

    pub fn set_generated_title(
        &self,
        id: SessionId,
        title: &str,
        force: bool,
        turns: usize,
    ) -> Result<bool> {
        let title = title.trim();
        if title.is_empty() {
            return Err(StoreError("a title needs some text".into()));
        }
        let db = self.db();
        let n = db.execute(
            "UPDATE sessions SET title = ?1, title_auto = ?2, title_turns = ?3, updated_at = ?4
             WHERE id = ?5 AND (title_auto > 0 OR ?6)",
            params![title, TITLE_GENERATED, turns as i64, now(), id, force],
        )?;
        if n > 0 {
            Store::changed(&db, "session", id)?;
        }
        Ok(n > 0)
    }

    /// Give a session its real title. Unless `force`, only a stand-in title is
    /// replaced, so a name someone chose is never overwritten.
    pub fn set_title(&self, id: SessionId, title: &str, force: bool) -> Result<bool> {
        let title = title.trim();
        if title.is_empty() {
            return Err(StoreError("a title needs some text".into()));
        }
        let db = self.db();
        let n = db.execute(
            "UPDATE sessions SET title = ?1, title_auto = 0, updated_at = ?2
             WHERE id = ?3 AND (title_auto > 0 OR ?4)",
            params![title, now(), id, force],
        )?;
        if n > 0 {
            Store::changed(&db, "session", id)?;
        }
        Ok(n > 0)
    }

    pub fn set_model(&self, id: SessionId, model: Option<&str>) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE sessions SET model = ?1, updated_at = ?2 WHERE id = ?3",
            params![model, now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    pub fn set_effort(&self, id: SessionId, effort: Option<&str>) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE sessions SET effort = ?1, updated_at = ?2 WHERE id = ?3",
            params![effort, now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    pub fn set_agent_session(&self, id: SessionId, agent_session: &str) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE sessions SET agent_session = ?1, updated_at = ?2 WHERE id = ?3",
            params![agent_session, now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    pub fn spend(&self, now_ms: i64) -> Result<agentbench_core::Spend> {
        let today = now_ms.div_euclid(MS_PER_DAY);
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT project_id, COALESCE(SUM(CASE WHEN day = ?1 THEN usd ELSE 0 END), 0), SUM(usd) \
             FROM spend GROUP BY project_id ORDER BY project_id",
        )?;
        let projects = stmt
            .query_map([today], |r| {
                Ok(agentbench_core::ProjectSpend {
                    project_id: r.get(0)?,
                    today_usd: r.get(1)?,
                    total_usd: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(agentbench_core::Spend {
            today_usd: projects.iter().map(|p| p.today_usd).sum(),
            total_usd: projects.iter().map(|p| p.total_usd).sum(),
            projects,
        })
    }

    pub fn add_usage(&self, id: SessionId, delta: &Usage) -> Result<()> {
        let session = self.session(id)?;
        let mut usage = session.usage;
        usage.add(delta);
        let db = self.db();
        if delta.cost_usd > 0.0 {
            db.execute(
                "INSERT INTO spend (day, project_id, usd) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(day, project_id) DO UPDATE SET usd = usd + excluded.usd",
                params![now().div_euclid(MS_PER_DAY), session.project_id, delta.cost_usd],
            )?;
        }
        db.execute(
            "UPDATE sessions SET usage = ?1, updated_at = ?2 WHERE id = ?3",
            params![serde_json::to_string(&usage).unwrap_or_default(), now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    pub fn delete_session(&self, id: SessionId) -> Result<()> {
        let db = self.db();
        let tree: Option<(Id, Id)> = db
            .query_row(
                "SELECT w.id, w.project_id FROM sessions s JOIN worktrees w ON w.id = s.worktree_id
                 WHERE s.id = ?1 AND w.missing = 1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if db.execute("DELETE FROM sessions WHERE id = ?1", [id])? == 0 {
            return Err(not_found("session", id));
        }
        db.execute("DELETE FROM settings WHERE key = ?1", [format!("no_plan_{id}")])?;
        Store::changed(&db, "session", id)?;
        // A vanished worktree only stays listed so its sessions can be read; once the
        // last one is deleted there is nothing left to show.
        if let Some((wid, pid)) = tree {
            let left: i64 =
                db.query_row("SELECT COUNT(*) FROM sessions WHERE worktree_id = ?1", [wid], |r| {
                    r.get(0)
                })?;
            if left == 0 {
                db.execute("DELETE FROM worktrees WHERE id = ?1", [wid])?;
                Store::changed(&db, "project", pid)?;
            }
        }
        Ok(())
    }

    pub fn memory_store(&self) -> &Memory {
        &self.memory
    }

    pub fn memory_root(&self) -> &Path {
        self.memory.root()
    }

    /// What was remembered about a project, as (topic, note), oldest topic first.
    pub fn notes(&self, project_id: Id) -> Result<Vec<(String, String)>> {
        Ok(self.memory.list(Scope::Project(project_id)))
    }

    pub fn add_watch(&self, watched: SessionId, watcher: SessionId) -> Result<()> {
        self.db().execute(
            "INSERT OR IGNORE INTO watches (watched, watcher) VALUES (?1, ?2)",
            params![watched, watcher],
        )?;
        Ok(())
    }

    pub fn add_worker(&self, session: SessionId, issue: Id, project: Id) -> Result<()> {
        let db = self.db();
        db.execute(
            "INSERT OR REPLACE INTO autopilot_workers (session, issue, project, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session, issue, project, now()],
        )?;
        Store::changed(&db, "autopilot", 0)
    }

    pub fn workers(&self) -> Result<Vec<Worker>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT session, issue, project, state, nudges, reworks, green, asked_at
             FROM autopilot_workers ORDER BY created_at, rowid",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Worker {
                session: r.get(0)?,
                issue: r.get(1)?,
                project: r.get(2)?,
                state: r.get(3)?,
                nudges: r.get(4)?,
                reworks: r.get(5)?,
                green: r.get::<_, i64>(6)? != 0,
                asked_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_worker(&self, session: SessionId) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM autopilot_workers WHERE session = ?1", [session])?;
        Store::changed(&db, "autopilot", 0)
    }

    pub fn set_worker(&self, session: SessionId, state: &str, green: bool) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE autopilot_workers SET state = ?1, green = ?2 WHERE session = ?3",
            params![state, green as i64, session],
        )?;
        Store::changed(&db, "autopilot", 0)
    }

    pub fn set_worker_asked(&self, session: SessionId, at: i64) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE autopilot_workers SET state = 'judging', asked_at = ?1 WHERE session = ?2",
            params![at, session],
        )?;
        Store::changed(&db, "autopilot", 0)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_verdict(
        &self,
        session: SessionId,
        issue: Option<Id>,
        head: Option<&str>,
        verdict: &str,
        reason: &str,
        followup: Option<&str>,
        reply: &str,
        judge: SessionId,
    ) -> Result<Judged> {
        let db = self.db();
        db.execute(
            "INSERT INTO verdicts (session, issue, head, verdict, reason, followup, reply, judge, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![session, issue, head, verdict, reason, followup, reply, judge, now()],
        )?;
        let id = db.last_insert_rowid();
        Store::changed(&db, "autopilot", 0)?;
        Ok(db.query_row("SELECT * FROM verdicts WHERE id = ?1", [id], judged_row)?)
    }

    pub fn latest_verdict(&self, session: SessionId) -> Result<Option<Judged>> {
        Ok(self
            .db()
            .query_row(
                "SELECT * FROM verdicts WHERE session = ?1 ORDER BY id DESC LIMIT 1",
                [session],
                judged_row,
            )
            .optional()?)
    }

    pub fn settle_verdicts(&self, session: SessionId) -> Result<()> {
        self.db().execute("UPDATE verdicts SET acted = 1 WHERE session = ?1", [session])?;
        Ok(())
    }

    pub fn bump_worker(&self, session: SessionId, column: &str) -> Result<()> {
        let column = match column {
            "nudges" => "nudges",
            "reworks" => "reworks",
            other => return Err(StoreError(format!("no worker counter {other}"))),
        };
        self.db().execute(
            &format!("UPDATE autopilot_workers SET {column} = {column} + 1 WHERE session = ?1"),
            [session],
        )?;
        Ok(())
    }

    pub fn judge_marks(&self) -> Result<std::collections::HashMap<Id, (String, i64)>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT id, judge_state, judge_at FROM issues WHERE judge_state != ''")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_judge_mark(&self, id: Id, state: &str, at: i64) -> Result<()> {
        self.db().execute("UPDATE issues SET judge_state = ?2, judge_at = ?3 WHERE id = ?1", params![id, state, at])?;
        Ok(())
    }

    pub fn log_autopilot(&self, project: Option<Id>, issue: Option<Id>, text: &str) -> Result<()> {
        let db = self.db();
        db.execute(
            "INSERT INTO autopilot_log (project_id, issue_id, text, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![project, issue, text, now()],
        )?;
        db.execute(
            "DELETE FROM autopilot_log WHERE id <= (SELECT MAX(id) FROM autopilot_log) - 500",
            [],
        )?;
        Store::changed(&db, "autopilot", 0)
    }

    pub fn autopilot_log(&self, limit: usize) -> Result<Vec<agentbench_core::Activity>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, project_id, issue_id, text, created_at FROM autopilot_log
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |r| {
            Ok(agentbench_core::Activity {
                id: r.get(0)?,
                project_id: r.get(1)?,
                issue_id: r.get(2)?,
                text: r.get(3)?,
                at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn watchers(&self, watched: SessionId) -> Result<Vec<SessionId>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT watcher FROM watches WHERE watched = ?1")?;
        let found = stmt
            .query_map([watched], |r| r.get::<_, SessionId>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(found)
    }

    pub fn defer(
        &self,
        session: SessionId,
        source: &str,
        worker: Option<SessionId>,
        urgent: bool,
        text: &str,
    ) -> Result<Id> {
        let db = self.db();
        db.execute(
            "INSERT INTO deferred (session, source, worker, urgent, text, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![session, source, worker, urgent as i64, text, now()],
        )?;
        let id = db.last_insert_rowid();
        Store::changed(&db, "session", session)?;
        Ok(id)
    }

    pub fn deferred(&self, session: SessionId) -> Result<Vec<Deferred>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, source, worker, urgent, text, created_at FROM deferred
             WHERE session = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([session], |r| {
            Ok(Deferred {
                id: r.get(0)?,
                source: r.get(1)?,
                worker: r.get(2)?,
                urgent: r.get::<_, i64>(3)? != 0,
                text: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn deferred_count(&self, session: SessionId) -> Result<usize> {
        let n: i64 = self.db().query_row(
            "SELECT COUNT(*) FROM deferred WHERE session = ?1",
            [session],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    pub fn remove_deferred(&self, session: SessionId, ids: &[Id]) -> Result<()> {
        let db = self.db();
        for id in ids {
            db.execute("DELETE FROM deferred WHERE session = ?1 AND id = ?2", params![session, id])?;
        }
        Store::changed(&db, "session", session)
    }

    pub fn add_open_item(
        &self,
        session: SessionId,
        kind: &str,
        target: Option<&str>,
        head: Option<&str>,
        text: &str,
        log_id: Option<Id>,
    ) -> Result<Id> {
        let db = self.db();
        db.execute(
            "INSERT INTO open_items (session, kind, target, head, text, delivered_at, log_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![session, kind, target, head, text, now(), log_id],
        )?;
        Ok(db.last_insert_rowid())
    }

    pub fn open_items(&self, session: SessionId) -> Result<Vec<OpenItem>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, kind, target, head, text, delivered_at, log_id FROM open_items
             WHERE session = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([session], |r| {
            Ok(OpenItem {
                id: r.get(0)?,
                kind: r.get(1)?,
                target: r.get(2)?,
                head: r.get(3)?,
                text: r.get(4)?,
                delivered_at: r.get(5)?,
                log_id: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_log(&self, item: &NewLogItem) -> Result<Id> {
        let db = self.db();
        let at = now();
        let closed = (!item.needs_answer).then_some(at);
        let action = if item.needs_answer { "" } else { "no_action_needed" };
        db.execute(
            "INSERT INTO supervisor_log (supervisor, kind, asker, asker_title, session, project_id, issue_id,
             question, arrived_at, needs_answer, closed_at, action)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                item.supervisor.map(|s| s.to_string()),
                item.kind,
                item.asker,
                item.asker_title,
                item.session.map(|s| s.to_string()),
                item.project_id,
                item.issue_id,
                item.question,
                at,
                item.needs_answer as i64,
                closed,
                action
            ],
        )?;
        let id = db.last_insert_rowid();
        Store::changed(&db, "supervisor_log", id)?;
        Ok(id)
    }

    pub fn close_log(&self, id: Id, closing: &Closing) -> Result<bool> {
        let db = self.db();
        let n = db.execute(
            "UPDATE supervisor_log SET closed_at = ?2, action = ?3, detail = ?4, outcome = ?5, reply = ?6,
             issue_id = COALESCE(issue_id, ?7) WHERE id = ?1 AND closed_at IS NULL",
            params![id, now(), closing.action, closing.detail, closing.outcome, closing.reply, closing.issue_id],
        )?;
        if n > 0 {
            Store::changed(&db, "supervisor_log", id)?;
        }
        Ok(n > 0)
    }

    pub fn set_log_outcome(&self, id: Id, outcome: &str, reply: &str) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE supervisor_log SET outcome = CASE WHEN outcome = '' THEN ?2 ELSE outcome END,
             reply = CASE WHEN reply = '' THEN ?3 ELSE reply END WHERE id = ?1",
            params![id, outcome, reply],
        )?;
        Store::changed(&db, "supervisor_log", id)
    }

    pub fn set_log_merge(&self, session: SessionId, merge: &str) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE supervisor_log SET merge = ?2 WHERE id = (
                SELECT id FROM supervisor_log WHERE session = ?1 AND kind = 'judge' ORDER BY id DESC LIMIT 1)",
            params![session.to_string(), merge],
        )?;
        Store::changed(&db, "supervisor_log", 0)
    }

    pub fn log_item(&self, id: Id) -> Result<Option<agentbench_core::LogItem>> {
        let db = self.db();
        Ok(db.query_row(&format!("{LOG_SELECT} WHERE l.id = ?1"), [id], log_row).optional()?)
    }

    pub fn log_items(&self, q: &agentbench_core::LogQuery) -> Result<agentbench_core::SupervisorLog> {
        let mut wheres: Vec<String> = Vec::new();
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        let mut arg = |v: rusqlite::types::Value| {
            args.push(v);
            format!("?{}", args.len())
        };
        if let Some(kind) = q.kind.as_deref().map(str::trim).filter(|k| !k.is_empty() && *k != "all") {
            let kinds: Vec<String> =
                kind.split(',').map(|k| arg(k.trim().to_string().into())).collect();
            wheres.push(format!("l.kind IN ({})", kinds.join(",")));
        }
        if let Some(p) = q.project.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            match p.parse::<Id>() {
                Ok(id) => wheres.push(format!("l.project_id = {}", arg(id.into()))),
                Err(_) => wheres.push(format!("p.name = {}", arg(p.to_string().into()))),
            }
        }
        match q.state.as_deref().unwrap_or("all") {
            "open" => wheres.push("l.closed_at IS NULL".into()),
            "closed" => wheres.push("l.closed_at IS NOT NULL".into()),
            _ => {}
        }
        if let Some(n) = q.needs_answer {
            wheres.push(format!("l.needs_answer = {}", arg((n as i64).into())));
        }
        if let Some(t) = q.since {
            wheres.push(format!("l.arrived_at >= {}", arg(t.into())));
        }
        if let Some(t) = q.until {
            wheres.push(format!("l.arrived_at < {}", arg(t.into())));
        }
        if let Some(b) = q.before {
            wheres.push(format!("l.id < {}", arg(b.into())));
        }
        for word in q.q.as_deref().unwrap_or_default().split_whitespace() {
            let like = arg(format!("%{}%", word.to_lowercase()).into());
            wheres.push(format!(
                "lower(l.question || ' ' || l.outcome || ' ' || l.reply || ' ' || l.asker_title || ' ' || l.detail || ' ' || COALESCE(p.name, '') || ' #' || COALESCE(l.issue_id, '')) LIKE {like}"
            ));
        }
        let limit = q
            .limit
            .unwrap_or(agentbench_core::supervisor_log::LOG_PAGE)
            .clamp(1, agentbench_core::supervisor_log::LOG_PAGE_MAX);
        let filter = if wheres.is_empty() { String::new() } else { format!(" WHERE {}", wheres.join(" AND ")) };
        let sql = format!("{LOG_SELECT}{filter} ORDER BY l.id DESC LIMIT {}", limit + 1);
        let db = self.db();
        let mut stmt = db.prepare(&sql)?;
        let mut items: Vec<agentbench_core::LogItem> =
            stmt.query_map(rusqlite::params_from_iter(args.iter()), log_row)?.collect::<rusqlite::Result<_>>()?;
        let more = items.len() > limit;
        items.truncate(limit);
        let open: i64 =
            db.query_row("SELECT COUNT(*) FROM supervisor_log WHERE closed_at IS NULL", [], |r| r.get(0))?;
        Ok(agentbench_core::SupervisorLog {
            next_before: if more { items.last().map(|i| i.id) } else { None },
            items,
            open: open as usize,
        })
    }

    pub fn log_waiting_for_reply(&self, supervisor: SessionId) -> Result<Vec<(Id, i64)>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, arrived_at FROM supervisor_log WHERE supervisor = ?1 AND kind = 'user'
             AND closed_at IS NOT NULL AND reply = '' AND arrived_at >= ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![supervisor.to_string(), now() - MS_PER_DAY], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn prune_log(&self, before: i64) -> Result<usize> {
        let db = self.db();
        Ok(db.execute("DELETE FROM supervisor_log WHERE arrived_at < ?1", [before])?)
    }

    pub fn open_items_targeting(&self, kind: &str, target: &str) -> Result<Vec<OpenItem>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, kind, target, head, text, delivered_at, log_id FROM open_items
             WHERE kind = ?1 AND target = ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![kind, target], |r| {
            Ok(OpenItem {
                id: r.get(0)?,
                kind: r.get(1)?,
                target: r.get(2)?,
                head: r.get(3)?,
                text: r.get(4)?,
                delivered_at: r.get(5)?,
                log_id: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn close_open_items(&self, ids: &[Id]) -> Result<()> {
        let db = self.db();
        for id in ids {
            db.execute("DELETE FROM open_items WHERE id = ?1", params![id])?;
        }
        Ok(())
    }

    pub fn sessions_with_deferred(&self) -> Result<Vec<SessionId>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT DISTINCT session FROM deferred ORDER BY session")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_queue_system(&self, id: SessionId, on: bool) -> Result<()> {
        let db = self.db();
        db.execute("UPDATE sessions SET queue_system = ?1 WHERE id = ?2", params![on as i64, id])?;
        Store::changed(&db, "session", id)
    }

    pub fn take_watchers(&self, watched: SessionId) -> Result<Vec<SessionId>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT watcher FROM watches WHERE watched = ?1")?;
        let found = stmt
            .query_map([watched], |r| r.get::<_, SessionId>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        db.execute("DELETE FROM watches WHERE watched = ?1", [watched])?;
        Ok(found)
    }

    pub fn note_hash(&self, project_id: Id, topic: &str) -> Result<String> {
        Ok(self.memory.hash(Scope::Project(project_id), topic))
    }

    pub fn set_note(
        &self,
        project_id: Id,
        topic: &str,
        body: &str,
        _author: &str,
        _session: Option<SessionId>,
        based_on: Option<&str>,
    ) -> Result<bool> {
        let name = self.project(project_id)?.name;
        self.memory
            .set(Scope::Project(project_id), &name, topic, body, based_on)
            .map_err(StoreError)
    }

    pub fn proposals(&self, state: agentbench_core::ProposalState) -> Result<Vec<agentbench_core::Proposal>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, body, state, created_at FROM triage_proposals WHERE state = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([state.as_str()], proposal_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().flatten().collect())
    }

    pub fn proposal(&self, id: Id) -> Result<agentbench_core::Proposal> {
        self.db()
            .query_row(
                "SELECT id, body, state, created_at FROM triage_proposals WHERE id = ?1",
                [id],
                proposal_row,
            )
            .optional()?
            .flatten()
            .ok_or_else(|| not_found("proposal", id))
    }

    pub fn proposal_known(&self, fingerprint: &str) -> Result<bool> {
        Ok(self
            .db()
            .query_row(
                "SELECT 1 FROM triage_proposals WHERE fingerprint = ?1
                 AND state IN ('pending', 'rejected')",
                [fingerprint],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn add_proposal(
        &self,
        fingerprint: &str,
        proposal: &agentbench_core::Proposal,
    ) -> Result<Id> {
        let db = self.db();
        db.execute(
            "INSERT INTO triage_proposals (fingerprint, body, state, created_at)
             VALUES (?1, ?2, 'pending', ?3)",
            params![fingerprint, serde_json::to_string(proposal).unwrap_or_default(), now()],
        )?;
        let id = db.last_insert_rowid();
        Store::changed(&db, "triage", id)?;
        Ok(id)
    }

    #[cfg(test)]
    pub fn backdate_proposal(&self, id: Id, by_ms: i64) -> Result<()> {
        self.db().execute(
            "UPDATE triage_proposals SET created_at = created_at - ?1 WHERE id = ?2",
            params![by_ms, id],
        )?;
        Ok(())
    }

    pub fn decide_proposal(
        &self,
        id: Id,
        state: agentbench_core::ProposalState,
    ) -> Result<agentbench_core::Proposal> {
        {
            let db = self.db();
            let n = db.execute(
                "UPDATE triage_proposals SET state = ?1, decided_at = ?2
                 WHERE id = ?3 AND state = 'pending'",
                params![state.as_str(), now(), id],
            )?;
            if n == 0 {
                return Err(StoreError("that proposal was already decided".into()));
            }
            Store::changed(&db, "triage", id)?;
        }
        self.proposal(id)
    }

    fn checked_goal(&self, project_id: Id, goal: Option<&str>) -> Result<Option<String>> {
        let Some(goal) = goal.map(str::trim).filter(|g| !g.is_empty()) else {
            return Ok(None);
        };
        match self.vision(project_id)? {
            Some(v) if v.goal(goal).is_some() => Ok(Some(goal.to_string())),
            _ => Err(StoreError(format!("project {project_id} has no goal `{goal}`"))),
        }
    }

    pub fn vision(&self, project_id: Id) -> Result<Option<Vision>> {
        Ok(self.vision_versions(project_id)?.into_iter().next())
    }

    pub fn vision_versions(&self, project_id: Id) -> Result<Vec<Vision>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, project_id, body, author, created_at FROM vision_versions
             WHERE project_id = ?1 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map([project_id], |r| {
            let body: String = r.get(2)?;
            Ok(Vision {
                id: r.get(0)?,
                project_id: r.get(1)?,
                body: serde_json::from_str(&body).unwrap_or_default(),
                author: r.get(3)?,
                updated_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn visions(&self) -> Result<Vec<Vision>> {
        let mut out = Vec::new();
        for p in self.projects()? {
            out.extend(self.vision(p.id)?);
        }
        Ok(out)
    }

    pub fn set_vision(&self, project_id: Id, body: VisionBody, author: &str) -> Result<Vision> {
        self.project(project_id)?;
        let mut body = body;
        body.summary = body.summary.trim().to_string();
        body.non_goals =
            body.non_goals.iter().map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).collect();
        body.goals.retain(|g| !g.text.trim().is_empty());
        let mut seen: Vec<String> = Vec::new();
        for g in &mut body.goals {
            g.text = g.text.trim().to_string();
            g.id = g.id.trim().to_string();
            if !g.id.is_empty() {
                if seen.contains(&g.id) {
                    return Err(StoreError(format!("goal id `{}` is used twice", g.id)));
                }
                seen.push(g.id.clone());
            }
        }
        let mut next = 1;
        for g in &mut body.goals {
            if g.id.is_empty() {
                while seen.contains(&format!("g{next}")) {
                    next += 1;
                }
                g.id = format!("g{next}");
                seen.push(g.id.clone());
            }
        }
        let id = {
            let db = self.db();
            db.execute(
                "INSERT INTO vision_versions (project_id, body, author, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![project_id, serde_json::to_string(&body).unwrap_or_default(), author, now()],
            )?;
            let id = db.last_insert_rowid();
            Store::changed(&db, "project", project_id)?;
            id
        };
        self.vision_versions(project_id)?
            .into_iter()
            .find(|v| v.id == id)
            .ok_or_else(|| not_found("vision", id))
    }

    pub fn note_topics(&self, project_id: Id) -> Result<Vec<String>> {
        Ok(self.memory.list(Scope::Project(project_id)).into_iter().map(|(t, _)| t).collect())
    }

    pub fn restore_note(&self, _project_id: Id, version: Id) -> Result<()> {
        Err(StoreError(format!("no note version {version}: notes are plain files and keep no history")))
    }

    pub fn global_notes(&self) -> Result<Vec<(String, String)>> {
        Ok(self.memory.list(Scope::Global))
    }

    pub fn set_global_note(&self, topic: &str, body: &str) -> Result<bool> {
        let current = self.memory.hash(Scope::Global, topic);
        let based_on = (!current.is_empty()).then_some(current.as_str());
        self.memory.set(Scope::Global, "", topic, body, based_on).map_err(StoreError)
    }

    pub fn reset_running(&self) -> Result<Vec<SessionId>> {
        let db = self.db();
        let ids = db
            .prepare("SELECT id FROM sessions WHERE status IN ('running', 'waiting')")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<SessionId>>>()?;
        db.execute(
            "UPDATE sessions SET status = 'stopped' WHERE status IN ('running', 'waiting')",
            [],
        )?;
        Ok(ids)
    }

    pub fn add_message(&self, session_id: SessionId, entry: &Entry) -> Result<Message> {
        let db = self.db();
        let t = now();
        db.execute(
            "INSERT INTO messages (session_id, entry, created_at) VALUES (?1, ?2, ?3)",
            params![session_id, serde_json::to_string(entry).unwrap_or_default(), t],
        )?;
        let id = db.last_insert_rowid();
        db.execute("UPDATE sessions SET updated_at = ?1 WHERE id = ?2", params![t, session_id])?;
        Store::changed(&db, "message", session_id)?;
        Ok(Message { id, session_id, entry: entry.clone(), created_at: t })
    }

    /// Keep a file sent with a message. It goes when the session does.
    pub fn add_attachment(
        &self,
        session_id: SessionId,
        name: &str,
        mime: &str,
        data: &[u8],
    ) -> Result<agentbench_core::Attachment> {
        let db = self.db();
        db.execute(
            "INSERT INTO attachments (session_id, name, mime, data, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, name, mime, data, now()],
        )?;
        Ok(agentbench_core::Attachment {
            id: db.last_insert_rowid(),
            name: name.to_string(),
            mime: mime.to_string(),
            size: data.len() as u64,
        })
    }

    /// An attachment and its bytes.
    pub fn attachment(&self, id: Id) -> Result<(agentbench_core::Attachment, Vec<u8>)> {
        self.db()
            .query_row("SELECT name, mime, data FROM attachments WHERE id = ?1", [id], |r| {
                let data: Vec<u8> = r.get("data")?;
                Ok((
                    agentbench_core::Attachment {
                        id,
                        name: r.get("name")?,
                        mime: r.get("mime")?,
                        size: data.len() as u64,
                    },
                    data,
                ))
            })
            .optional()?
            .ok_or_else(|| not_found("attachment", id))
    }

    /// The attachment's bytes, only if it belongs to `session`.
    pub fn session_attachment(
        &self,
        session: SessionId,
        id: Id,
    ) -> Result<(agentbench_core::Attachment, Vec<u8>)> {
        let (a, data) = self.attachment(id)?;
        let owner: SessionId =
            self.db().query_row("SELECT session_id FROM attachments WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        if owner != session {
            return Err(not_found("attachment", id));
        }
        Ok((a, data))
    }

    pub fn update_message(&self, id: Id, entry: &Entry) -> Result<()> {
        let db = self.db();
        let session_id: SessionId =
            db.query_row("SELECT session_id FROM messages WHERE id = ?1", [id], |r| r.get(0))?;
        db.execute(
            "UPDATE messages SET entry = ?1 WHERE id = ?2",
            params![serde_json::to_string(entry).unwrap_or_default(), id],
        )?;
        Store::changed(&db, "message", session_id)
    }

    pub fn messages_page(
        &self,
        session_id: SessionId,
        after: Id,
        before: Option<Id>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT * FROM (SELECT * FROM messages WHERE session_id = ?1 AND id > ?2 AND id < ?3 \
             ORDER BY id DESC LIMIT ?4) ORDER BY id",
        )?;
        let rows = stmt.query_map(
            params![session_id, after, before.unwrap_or(i64::MAX), limit as i64],
            message_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn messages(&self, session_id: SessionId, after: Id) -> Result<Vec<Message>> {
        let db = self.db();
        let mut stmt =
            db.prepare("SELECT * FROM messages WHERE session_id = ?1 AND id > ?2 ORDER BY id")?;
        let rows = stmt.query_map(params![session_id, after], message_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

#[cfg(test)]
pub mod testing {
    use super::*;

    pub fn project(store: &Store, path: &str) -> (Project, Worktree) {
        let p = store.add_project("demo", path, Some("main"), None).unwrap();
        store
            .sync_worktrees(
                p.id,
                &[SeenTree {
                    path: path.into(),
                    branch: Some("main".into()),
                    head: None,
                    main: true,
                }],
                None,
            )
            .unwrap();
        let main = store.main_worktree(p.id).unwrap();
        (p, main)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::project;
    use super::*;

    fn archives(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("legacy-notes-"))
            .collect();
        found.sort();
        found
    }

    fn managed_for(s: &Store, id: SessionId) -> bool {
        s.attention_rows().unwrap().into_iter().find(|r| r.id == id.to_string()).unwrap().managed
    }

    #[test]
    fn a_session_is_managed_by_a_parent_or_the_autopilot_until_released() {
        let s = Store::memory().unwrap();
        let (p, main) = project(&s, "/a");
        let boss = s.add_session(main.id, "native", "boss", None, None).unwrap();
        let child = s.add_session(main.id, "native", "child", None, None).unwrap();
        let plain = s.add_session(main.id, "native", "plain", None, None).unwrap();
        let worker = s.add_session(main.id, "native", "worker", None, None).unwrap();
        let issue = s
            .add_issue(&agentbench_core::NewIssue { project_id: p.id, title: "t".into(), ..Default::default() })
            .unwrap();
        s.set_parent(child.id, Some(boss.id)).unwrap();
        s.add_worker(worker.id, issue.id, p.id).unwrap();
        assert!(managed_for(&s, child.id));
        assert!(managed_for(&s, worker.id));
        assert!(!managed_for(&s, plain.id));
        assert!(!managed_for(&s, boss.id));

        let feed = crate::attention::Attention::new();
        feed.refresh(s.attention_rows().unwrap());
        let first = feed.feed(0, None);
        crate::autopilot::release(&s, child.id, "user").unwrap();
        crate::autopilot::release(&s, worker.id, "user").unwrap();
        assert!(!managed_for(&s, child.id));
        assert!(!managed_for(&s, worker.id));
        feed.refresh(s.attention_rows().unwrap());
        let diff = feed.feed(first.cursor, Some(first.epoch));
        let flips: Vec<_> = diff.changes.iter().map(|c| c["managed"].clone()).collect();
        assert_eq!(flips, vec![serde_json::json!(false), serde_json::json!(false)]);
    }

    #[test]
    fn legacy_note_rows_are_archived_before_the_tables_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agentbench.db");
        {
            let conn = Connection::open(&path).unwrap();
            let last = SCHEMA.iter().position(|s| s.starts_with("DROP TABLE IF EXISTS notes")).unwrap();
            for (i, step) in SCHEMA[..last].iter().enumerate() {
                conn.execute_batch(&format!("BEGIN; {step}; PRAGMA user_version = {}; COMMIT;", i + 1)).unwrap();
            }
            conn.execute_batch(
                "INSERT INTO projects (id, name, path, created_at) VALUES (1, 'Alpha', '/a', 0);
                 INSERT INTO notes (project_id, topic, body, updated_at) VALUES (1, 'tests', 'cargo test \"quoted\"', 5), (1, 'style', 'plain', 6);
                 INSERT INTO note_versions (project_id, topic, body, hash, author, session, created_at)
                   VALUES (1, 'tests', 'old body', 'h1', 'agent', NULL, 3), (1, 'tests', 'cargo test', 'h2', 'user', 's1', 4);
                 INSERT INTO global_notes (topic, body, updated_at) VALUES ('rules', 'no dashes', 7);",
            )
            .unwrap();
        }
        let memory = dir.path().join("memory");
        let s = Store::open_with_memory(&path, memory.clone()).unwrap();
        let left: i64 = s
            .db()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('notes', 'note_versions', 'global_notes')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
        let files = archives(dir.path());
        assert_eq!(files.len(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
        assert_eq!(json["notes"].as_array().unwrap().len(), 2);
        assert_eq!(json["note_versions"].as_array().unwrap().len(), 2);
        assert_eq!(json["global_notes"].as_array().unwrap().len(), 1);
        let tests = json["notes"].as_array().unwrap().iter().find(|n| n["topic"] == "tests").unwrap();
        assert_eq!(tests["body"], "cargo test \"quoted\"");
        assert_eq!(tests["project"], "Alpha");
        assert_eq!(json["global_notes"][0]["body"], "no dashes");
        drop(s);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let _ = Store::open_with_memory(&path, memory).unwrap();
        assert_eq!(archives(dir.path()).len(), 1);
    }

    #[test]
    fn a_fresh_database_writes_no_legacy_archive() {
        let dir = tempfile::tempdir().unwrap();
        let _ = Store::open(&dir.path().join("agentbench.db")).unwrap();
        assert!(archives(dir.path()).is_empty());
    }

    #[test]
    fn the_legacy_note_tables_are_dropped() {
        let s = Store::memory().unwrap();
        let left: i64 = s
            .db()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('notes', 'note_versions', 'global_notes')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
    }

    #[cfg(unix)]
    #[test]
    fn open_keeps_database_and_directory_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("data");
        let path = dir.join("agentbench.db");
        let store = Store::open(&path).unwrap();
        store.set_setting("probe", "1").unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        for sibling in ["agentbench.db-wal", "agentbench.db-shm"] {
            let p = dir.join(sibling);
            if p.exists() {
                assert_eq!(mode(&p), 0o600, "{sibling}");
            }
        }
    }

    fn tree(path: &str, branch: &str, main: bool) -> SeenTree {
        SeenTree { path: path.into(), branch: Some(branch.into()), head: Some("abc".into()), main }
    }

    fn issue(store: &Store, project_id: Id, title: &str) -> Issue {
        store
            .add_issue(&NewIssue { project_id, title: title.into(), ..Default::default() })
            .unwrap()
    }

    fn vision_body(goals: &[(&str, &str)]) -> VisionBody {
        VisionBody {
            summary: " Ship it ".into(),
            goals: goals
                .iter()
                .map(|(id, text)| agentbench_core::Goal {
                    id: id.to_string(),
                    text: text.to_string(),
                    status: Default::default(),
                })
                .collect(),
            non_goals: vec!["  ".into(), "no cloud".into()],
        }
    }

    #[test]
    fn a_vision_round_trips_and_keeps_every_version() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/v");
        assert!(s.vision(p.id).unwrap().is_none());
        let a = s.set_vision(p.id, vision_body(&[("", "first"), ("x", "second"), ("", "third")]), "user").unwrap();
        assert_eq!(a.body.summary, "Ship it");
        let ids: Vec<_> = a.body.goals.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, ["g1", "x", "g2"]);
        assert_eq!(a.body.non_goals, ["no cloud"]);
        assert_eq!(a.author, "user");
        let b = s.set_vision(p.id, vision_body(&[("g1", "only")]), "supervisor").unwrap();
        assert_eq!(s.vision(p.id).unwrap().unwrap(), b);
        let versions = s.vision_versions(p.id).unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[1], a);
        assert!(s.set_vision(p.id, vision_body(&[("a", "1"), ("a", "2")]), "user").is_err());
        assert_eq!(s.vision_versions(p.id).unwrap().len(), 2);
    }

    #[test]
    fn an_issue_waits_on_unfinished_dependencies_across_projects() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/d1");
        let (q, _) = project(&s, "/d2");
        let a = issue(&s, p.id, "a");
        let b = s
            .add_issue(&NewIssue { project_id: q.id, title: "b".into(), depends_on: vec![a.id], ..Default::default() })
            .unwrap();
        assert_eq!((b.depends_on.clone(), b.waiting_on.clone()), (vec![a.id], vec![a.id]));
        assert_eq!(s.issue(a.id).unwrap().blocks, [b.id]);
        s.update_issue(a.id, &IssuePatch { state: Some(IssueState::Done), ..Default::default() }).unwrap();
        let b = s.issue(b.id).unwrap();
        assert_eq!((b.depends_on, b.waiting_on), (vec![a.id], vec![]));
        assert!(s.add_issue(&NewIssue { project_id: p.id, title: "x".into(), depends_on: vec![999], ..Default::default() }).is_err());
        assert_eq!(s.issues().unwrap().len(), 2);
    }

    #[test]
    fn a_dependency_cycle_is_rejected_with_the_chain() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/c");
        let a = issue(&s, p.id, "a");
        let b = issue(&s, p.id, "b");
        let c = issue(&s, p.id, "c");
        s.set_deps(b.id, &[a.id]).unwrap();
        s.set_deps(c.id, &[b.id]).unwrap();
        let e = s.set_deps(a.id, &[c.id]).unwrap_err().0;
        assert!(e.contains(&format!("#{} -> #{} -> #{} -> #{}", a.id, c.id, b.id, a.id)), "{e}");
        assert!(s.issue(a.id).unwrap().depends_on.is_empty());
        assert!(s.set_deps(a.id, &[a.id]).is_err());
        let e = s.update_issue(a.id, &IssuePatch { depends_on: Some(vec![c.id]), ..Default::default() });
        assert!(e.is_err());
    }

    #[test]
    fn finishing_the_last_dependency_releases_a_backlog_dependent_with_a_goal() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/u");
        s.set_vision(p.id, vision_body(&[("g1", "one")]), "user").unwrap();
        let a = issue(&s, p.id, "a");
        let b = issue(&s, p.id, "b");
        let goal = |state, goal: &str| NewIssue {
            project_id: p.id,
            title: "w".into(),
            state: Some(state),
            goal: Some(goal.into()).filter(|g: &String| !g.is_empty()),
            depends_on: vec![a.id, b.id],
            ..Default::default()
        };
        let with = s.add_issue(&goal(IssueState::Backlog, "g1")).unwrap();
        let without = s.add_issue(&goal(IssueState::Backlog, "")).unwrap();
        let done = IssuePatch { state: Some(IssueState::Done), ..Default::default() };
        s.update_issue(a.id, &done).unwrap();
        assert_eq!(s.issue(with.id).unwrap().state, IssueState::Backlog);
        assert!(s.comments(with.id).unwrap().is_empty());
        s.update_issue(b.id, &done).unwrap();
        assert_eq!(s.issue(with.id).unwrap().state, IssueState::Todo);
        assert_eq!(s.issue(without.id).unwrap().state, IssueState::Backlog);
        for id in [with.id, without.id] {
            let comments = s.comments(id).unwrap();
            assert!(comments.iter().any(|c| c.body.starts_with(&format!("unblocked by #{}", b.id))));
        }
        let log = s.autopilot_log(20).unwrap();
        assert!(log.iter().any(|l| l.text == format!("unblocked #{} (all dependencies done)", with.id)), "{log:?}");
    }

    #[test]
    fn a_deleted_dependency_does_not_block() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/x");
        let a = issue(&s, p.id, "a");
        let b = issue(&s, p.id, "b");
        s.set_deps(b.id, &[a.id]).unwrap();
        s.delete_issue(a.id).unwrap();
        assert!(s.issue(b.id).unwrap().waiting_on.is_empty());
    }

    #[test]
    fn an_issue_links_to_a_goal_of_its_project_or_is_off_vision() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/g");
        let i = issue(&s, p.id, "plain");
        assert_eq!((i.goal.clone(), i.off_vision), (None, false));
        assert!(s.update_issue(i.id, &IssuePatch { goal: Some("g1".into()), ..Default::default() }).is_err());
        s.set_vision(p.id, vision_body(&[("g1", "one")]), "user").unwrap();
        let linked = s.update_issue(i.id, &IssuePatch { goal: Some("g1".into()), ..Default::default() }).unwrap();
        assert_eq!(linked.goal.as_deref(), Some("g1"));
        let off = s.update_issue(i.id, &IssuePatch { off_vision: Some(true), ..Default::default() }).unwrap();
        assert_eq!((off.goal, off.off_vision), (None, true));
        let cleared = s.update_issue(i.id, &IssuePatch { goal: Some("".into()), ..Default::default() }).unwrap();
        assert_eq!(cleared.goal, None);
        let born = s
            .add_issue(&NewIssue { project_id: p.id, title: "t".into(), goal: Some("g1".into()), ..Default::default() })
            .unwrap();
        assert_eq!(born.goal.as_deref(), Some("g1"));
        assert!(s
            .add_issue(&NewIssue { project_id: p.id, title: "t".into(), goal: Some("nope".into()), ..Default::default() })
            .is_err());
    }

    #[test]
    fn a_path_is_one_project() {
        let s = Store::memory().unwrap();
        project(&s, "/r");
        assert!(s.add_project("again", "/r", None, None).is_err());
        assert!(s.project_by_path("/r").unwrap().is_some());
    }

    #[test]
    fn syncing_adds_updates_and_drops_worktrees() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/r");
        let seen = vec![tree("/r", "main", true), tree("/r-a", "a", false)];
        assert!(s.sync_worktrees(p.id, &seen, Some("/r-a")).unwrap());
        assert!(!s.sync_worktrees(p.id, &seen, None).unwrap(), "nothing new");
        let all = s.worktrees(Some(p.id)).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].main);
        assert!(all[1].owned);

        let renamed = vec![tree("/r", "main", true), tree("/r-a", "a2", false)];
        assert!(s.sync_worktrees(p.id, &renamed, None).unwrap());
        assert_eq!(s.worktrees(Some(p.id)).unwrap()[1].branch.as_deref(), Some("a2"));

        s.sync_worktrees(p.id, &[tree("/r", "main", true)], None).unwrap();
        assert_eq!(
            s.worktrees(Some(p.id)).unwrap().len(),
            1,
            "an unused vanished tree is forgotten"
        );
    }

    #[test]
    fn a_vanished_worktree_with_sessions_is_kept_as_missing() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/r");
        s.sync_worktrees(p.id, &[tree("/r", "main", true), tree("/r-a", "a", false)], None)
            .unwrap();
        let wt = s.worktrees(Some(p.id)).unwrap().into_iter().find(|w| !w.main).unwrap();
        let sess = s.add_session(wt.id, "pi", "t", None, None).unwrap();
        assert_eq!(sess.cwd, "/r-a");

        s.sync_worktrees(p.id, &[tree("/r", "main", true)], None).unwrap();
        let wt = s.worktree(wt.id).unwrap();
        assert!(wt.missing);
        assert!(
            s.add_session(wt.id, "pi", "t", None, None).is_err(),
            "no new sessions in a missing tree"
        );
        assert_eq!(s.session(sess.id).unwrap().cwd, "/r-a", "the old session keeps its history");

        s.sync_worktrees(p.id, &[tree("/r", "main", true), tree("/r-a", "a", false)], None)
            .unwrap();
        assert!(!s.worktree(wt.id).unwrap().missing, "a tree that comes back is live again");
        s.forget_worktree(wt.id).unwrap();
        assert!(s.session(sess.id).is_err());
    }

    #[test]
    fn deleting_the_last_session_of_a_missing_worktree_drops_it() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/r");
        s.sync_worktrees(p.id, &[tree("/r", "main", true), tree("/r-a", "a", false)], None)
            .unwrap();
        let wt = s.worktrees(Some(p.id)).unwrap().into_iter().find(|w| !w.main).unwrap();
        let a = s.add_session(wt.id, "pi", "t", None, None).unwrap();
        let b = s.add_session(wt.id, "pi", "t", None, None).unwrap();
        s.sync_worktrees(p.id, &[tree("/r", "main", true)], None).unwrap();

        s.delete_session(a.id).unwrap();
        assert!(s.worktree(wt.id).is_ok(), "another session still needs it");
        s.delete_session(b.id).unwrap();
        assert!(s.worktree(wt.id).is_err(), "nothing left, so the gone marker goes");
    }

    #[test]
    fn closing_a_session_forgets_its_missing_plan_marker() {
        let s = Store::memory().unwrap();
        let (_, wt) = testing::project(&s, "/r");
        let a = s.add_session(wt.id, "pi", "t", None, None).unwrap();
        let b = s.add_session(wt.id, "pi", "t", None, None).unwrap();
        s.set_setting(&format!("no_plan_{}", a.id), "15").unwrap();
        s.set_setting(&format!("no_plan_{}", b.id), "15").unwrap();
        s.delete_session(a.id).unwrap();
        assert_eq!(s.setting(&format!("no_plan_{}", a.id)).unwrap(), None);
        assert_eq!(s.setting(&format!("no_plan_{}", b.id)).unwrap().as_deref(), Some("15"));
    }

    #[test]
    fn issues_round_trip_and_patch() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/r");
        let i = s
            .add_issue(&NewIssue {
                project_id: p.id,
                title: "  fix it ".into(),
                body: "body".into(),
                labels: vec!["bug".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(i.title, "fix it");
        assert_eq!(i.state, IssueState::Todo);
        let patched = s
            .update_issue(
                i.id,
                &IssuePatch {
                    state: Some(IssueState::Doing),
                    priority: Some(3),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(patched.state, IssueState::Doing);
        assert_eq!(patched.body, "body");
        assert!(
            s.update_issue(i.id, &IssuePatch { title: Some(" ".into()), ..Default::default() })
                .is_err()
        );
        assert!(
            s.add_issue(&NewIssue { project_id: 99, title: "x".into(), ..Default::default() })
                .is_err()
        );
    }

    #[test]
    fn issues_sort_by_priority_then_recency() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/r");
        let a = issue(&s, p.id, "a");
        let b = issue(&s, p.id, "b");
        s.update_issue(a.id, &IssuePatch { priority: Some(5), ..Default::default() }).unwrap();
        assert_eq!(s.issues().unwrap()[0].id, a.id);
        s.update_issue(b.id, &IssuePatch { priority: Some(9), ..Default::default() }).unwrap();
        assert_eq!(s.issues().unwrap()[0].id, b.id);
    }

    #[test]
    fn comments_belong_to_an_existing_issue() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/r");
        let i = issue(&s, p.id, "a");
        s.add_comment(i.id, &NewComment { author: None, body: "hello".into() }).unwrap();
        assert_eq!(s.comments(i.id).unwrap()[0].author, "you");
        assert!(s.add_comment(999, &NewComment { author: None, body: "x".into() }).is_err());
        s.delete_issue(i.id).unwrap();
        assert!(s.comments(i.id).unwrap().is_empty());
    }

    #[test]
    fn deleting_a_project_takes_its_tree_with_it() {
        let s = Store::memory().unwrap();
        let (p, main) = project(&s, "/r");
        let sess = s.add_session(main.id, "pi", "t", None, None).unwrap();
        s.add_message(sess.id, &Entry::user("hi")).unwrap();
        issue(&s, p.id, "a");
        s.delete_project(p.id).unwrap();
        assert!(s.sessions().unwrap().is_empty());
        assert!(s.worktrees(None).unwrap().is_empty());
        assert!(s.issues().unwrap().is_empty());
    }

    #[test]
    fn messages_page_after_an_id() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/r");
        let sess = s.add_session(main.id, "pi", "t", None, None).unwrap();
        let m1 = s.add_message(sess.id, &Entry::user("hi")).unwrap();
        s.add_message(sess.id, &Entry::Assistant { text: "yo".into() }).unwrap();
        assert_eq!(s.messages(sess.id, 0).unwrap().len(), 2);
        assert_eq!(
            s.messages(sess.id, m1.id).unwrap()[0].entry,
            Entry::Assistant { text: "yo".into() }
        );
        s.update_message(m1.id, &Entry::user("edited")).unwrap();
        assert_eq!(s.messages(sess.id, 0).unwrap()[0].entry.text(), "edited");
    }

    #[test]
    fn usage_accumulates() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/r");
        let sess = s.add_session(main.id, "pi", "t", None, None).unwrap();
        let u = Usage {
            input_tokens: 10,
            output_tokens: 2,
            cost_usd: 0.5,
            turns: 1,
            ..Default::default()
        };
        s.add_usage(sess.id, &u).unwrap();
        s.add_usage(sess.id, &u).unwrap();
        let got = s.session(sess.id).unwrap().usage;
        assert_eq!(got.input_tokens, 20);
        assert_eq!(got.turns, 2);
        assert_eq!(got.cost_usd, 1.0);
    }

    #[test]
    fn spend_is_summed_per_project_and_per_day() {
        let s = Store::memory().unwrap();
        let (p, main) = project(&s, "/r");
        let sess = s.add_session(main.id, "pi", "t", None, None).unwrap();
        let u = Usage { cost_usd: 0.25, turns: 1, ..Default::default() };
        s.add_usage(sess.id, &u).unwrap();
        s.add_usage(sess.id, &u).unwrap();
        let spend = s.spend(now()).unwrap();
        assert_eq!((spend.today_usd, spend.total_usd), (0.5, 0.5));
        assert_eq!(spend.projects.len(), 1);
        assert_eq!(spend.projects[0].project_id, p.id);
        let later = s.spend(now() + 2 * MS_PER_DAY).unwrap();
        assert_eq!((later.today_usd, later.total_usd), (0.0, 0.5));
    }

    #[test]
    fn clearing_the_agent_empties_the_context_but_keeps_the_totals() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/r");
        let sess = s.add_session(main.id, "pi", "t", None, None).unwrap();
        let u = Usage {
            input_tokens: 10,
            context: 900,
            window: 1000,
            model: "m".into(),
            host: "h".into(),
            ..Default::default()
        };
        s.add_usage(sess.id, &u).unwrap();
        s.clear_agent_session(sess.id).unwrap();
        let got = s.session(sess.id).unwrap().usage;
        assert_eq!((got.context, got.window, got.input_tokens), (0, 1000, 10));
        assert_eq!((got.model.as_str(), got.host.as_str()), ("m", "h"));
    }

    #[test]
    fn old_native_chats_become_what_was_sent_and_others_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.db");
        let (native, pi) = {
            let s = Store::open(&path).unwrap();
            let (_, main) = project(&s, "/r");
            let native = s.add_session(main.id, "native", "t", None, None).unwrap();
            let pi = s.add_session(main.id, "pi", "u", None, None).unwrap();
            for e in [
                Entry::user("hi"),
                Entry::Tool {
                    call_id: "c1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                    output: Some("a".into()),
                    error: false,
                },
                Entry::Notice { text: "note".into() },
                Entry::Summary { text: "so far".into() },
            ] {
                s.add_message(native.id, &e).unwrap();
            }
            s.add_message(pi.id, &Entry::user("there")).unwrap();
            s.db().execute("DELETE FROM settings WHERE key = ?1", [WIRE_TRANSCRIPTS]).unwrap();
            (native, pi)
        };
        let s = Store::open(&path).unwrap();
        let rows: Vec<Entry> =
            s.messages(native.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        let wire = |e: &Entry| match e {
            Entry::Wire { messages } => messages.iter().map(|m| m.message.clone()).collect(),
            _ => Vec::new(),
        };
        assert_eq!(wire(&rows[0]), [serde_json::json!({"role": "user", "content": "hi"})]);
        assert_eq!(wire(&rows[1])[1]["content"], "a");
        assert_eq!(rows[2], Entry::Notice { text: "note".into() });
        assert_eq!(rows[3], Entry::Summary { text: "so far".into() });
        let shown: Vec<Entry> = agentbench_core::shown(&s.messages(native.id, 0).unwrap())
            .into_iter()
            .map(|m| m.entry)
            .collect();
        assert_eq!(shown[0], Entry::user("hi"));
        assert!(matches!(&shown[1], Entry::Tool { output: Some(o), .. } if o == "a"));
        assert_eq!(s.messages(pi.id, 0).unwrap()[0].entry, Entry::user("there"));
        s.add_message(native.id, &Entry::user("later")).unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.messages(native.id, 0).unwrap()[4].entry,
            Entry::user("later"),
            "converted once"
        );
    }

    #[test]
    fn every_write_moves_the_cursor() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/r");
        let c0 = s.cursor().unwrap();
        let i = issue(&s, p.id, "a");
        let c1 = s.cursor().unwrap();
        assert!(c1 > c0);
        s.add_comment(i.id, &NewComment { author: None, body: "x".into() }).unwrap();
        assert_eq!(
            s.changes_since(c1).unwrap(),
            vec![(c1 + 1, "issue".to_string(), i.id.to_string())]
        );
    }

    #[test]
    fn running_sessions_are_stopped_on_restart() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/r");
        let sess = s.add_session(main.id, "pi", "t", None, None).unwrap();
        let asking = s.add_session(main.id, "pi", "u", None, None).unwrap();
        let idle = s.add_session(main.id, "pi", "v", None, None).unwrap();
        s.set_status(sess.id, SessionStatus::Running).unwrap();
        s.set_status(asking.id, SessionStatus::Waiting).unwrap();
        let mut stopped = s.reset_running().unwrap();
        stopped.sort();
        let mut want = vec![sess.id, asking.id];
        want.sort();
        assert_eq!(stopped, want);
        assert_eq!(s.session(sess.id).unwrap().status, SessionStatus::Stopped);
        assert_eq!(s.session(asking.id).unwrap().status, SessionStatus::Stopped);
        assert_eq!(s.session(idle.id).unwrap().status, SessionStatus::Idle);
    }

    #[test]
    fn the_schema_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.db");
        {
            let s = Store::open(&path).unwrap();
            project(&s, "/r");
            s.set_setting("token", "abc").unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.projects().unwrap()[0].path, "/r");
        assert_eq!(s.setting("token").unwrap().as_deref(), Some("abc"));
    }

    #[test]
    fn the_supervisor_log_survives_a_reopen_and_pages_with_filters() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.db");
        let pid;
        {
            let s = Store::open(&path).unwrap();
            pid = project(&s, "/r").0.id;
            for n in 0..5 {
                let kind = if n % 2 == 0 { "judge" } else { "user" };
                let id = s
                    .add_log(&NewLogItem {
                        kind,
                        asker: "user",
                        project_id: (n < 3).then_some(pid),
                        question: &format!("question {n} about bananas{n}"),
                        needs_answer: true,
                        ..Default::default()
                    })
                    .unwrap();
                if n < 2 {
                    s.close_log(id, &Closing::new("replied").outcome(format!("answer {n}"))).unwrap();
                }
            }
        }
        let s = Store::open(&path).unwrap();
        let all = s.log_items(&Default::default()).unwrap();
        assert_eq!(all.items.len(), 5);
        assert_eq!(all.open, 3);
        assert_eq!(all.items[0].question, "question 4 about bananas4", "newest first");
        assert_eq!(all.items[4].outcome, "answer 0");
        let page = |before| agentbench_core::LogQuery { limit: Some(2), before, ..Default::default() };
        let first = s.log_items(&page(None)).unwrap();
        assert_eq!(first.items.len(), 2);
        let second = s.log_items(&page(first.next_before)).unwrap();
        assert_eq!(second.items[0].question, "question 2 about bananas2");
        let last = s.log_items(&page(second.next_before)).unwrap();
        assert_eq!((last.items.len(), last.next_before), (1, None));
        let q = |q: agentbench_core::LogQuery| {
            s.log_items(&q).unwrap().items.iter().map(|i| i.question.chars().nth(9).unwrap()).collect::<String>()
        };
        assert_eq!(q(agentbench_core::LogQuery { kind: Some("judge".into()), ..Default::default() }), "420");
        assert_eq!(q(agentbench_core::LogQuery { state: Some("open".into()), ..Default::default() }), "432");
        assert_eq!(q(agentbench_core::LogQuery { state: Some("closed".into()), ..Default::default() }), "10");
        assert_eq!(q(agentbench_core::LogQuery { project: Some("demo".into()), ..Default::default() }), "210");
        assert_eq!(q(agentbench_core::LogQuery { project: Some(pid.to_string()), ..Default::default() }), "210");
        assert_eq!(q(agentbench_core::LogQuery { q: Some("BANANAS3".into()), ..Default::default() }), "3");
        assert_eq!(q(agentbench_core::LogQuery { q: Some("answer 1".into()), ..Default::default() }), "1");
        let future = agentbench_core::LogQuery { since: Some(now() + 60_000), ..Default::default() };
        assert!(s.log_items(&future).unwrap().items.is_empty());
        assert_eq!(s.log_item(all.items[0].id).unwrap().unwrap().project, "");
        assert_eq!(s.log_item(all.items[2].id).unwrap().unwrap().project, "demo");
    }

    #[test]
    fn an_old_database_migrates_and_keeps_its_issues() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let conn = Connection::open(&path).unwrap();
            for (i, step) in SCHEMA.iter().enumerate() {
                conn.execute_batch(&format!("BEGIN; {step}; PRAGMA user_version = {}; COMMIT;", i + 1)).unwrap();
            }
            conn.execute_batch(
                "INSERT INTO projects (id, name, path, created_at) VALUES (1, 'old', '/o', 0);
                 INSERT INTO issues (id, project_id, title, state, created_at, updated_at) VALUES (5, 1, 'kept', 'todo', 1, 1);
                 INSERT INTO comments (issue_id, author, body, created_at) VALUES (5, 'me', 'hi', 1);",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let issue = s.issue(5).unwrap();
        assert_eq!(issue.title, "kept");
        assert_eq!(issue.source, IssueSource::Internal);
        assert_eq!(issue.external_number, None);
        assert_eq!(s.comments(5).unwrap()[0].external_id, None);
        let p = s.project(1).unwrap();
        assert_eq!(p.tracker, Tracker::Internal);
        assert_eq!(p.post_mode, PostMode::Draft);
        assert_eq!(s.db().query_row("SELECT count(*) FROM pull_requests", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn a_github_issue_number_cannot_be_mirrored_twice() {
        let s = Store::memory().unwrap();
        let (p, _) = testing::project(&s, "/tmp");
        let insert = |project: Id, number: i64, source: &str| {
            s.db().execute(
                "INSERT INTO issues (project_id, title, state, source, external_number, created_at, updated_at) VALUES (?1, 't', 'todo', ?3, ?2, 0, 0)",
                params![project, number, source],
            )
        };
        insert(p.id, 7, "github").unwrap();
        assert!(insert(p.id, 7, "github").is_err());
        insert(p.id, 8, "github").unwrap();
        let other = s.add_project("other", "/other", Some("main"), None).unwrap();
        insert(other.id, 7, "github").unwrap();
        insert(p.id, 7, "internal").unwrap();
        insert(p.id, 7, "internal").unwrap();
    }

    #[test]
    fn a_mirrored_comment_cannot_be_stored_twice() {
        let s = Store::memory().unwrap();
        let (p, _) = testing::project(&s, "/tmp");
        let i = issue(&s, p.id, "x");
        let insert = |ext: Option<&str>| {
            s.db().execute(
                "INSERT INTO comments (issue_id, author, body, created_at, external_id) VALUES (?1, 'a', 'b', 0, ?2)",
                params![i.id, ext],
            )
        };
        insert(Some("c1")).unwrap();
        assert!(insert(Some("c1")).is_err());
        insert(None).unwrap();
        insert(None).unwrap();
    }

    #[test]
    fn tracker_and_post_mode_persist_and_switching_back_is_lossless() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let s = Store::open(&path).unwrap();
        let (p, _) = testing::project(&s, "/tmp");
        let a = issue(&s, p.id, "internal one");
        s.update_project(
            p.id,
            &ProjectPatch {
                tracker: Some(Tracker::Github { repo: " o/r ".into() }),
                post_mode: Some(PostMode::Auto),
                ..Default::default()
            },
        )
        .unwrap();
        drop(s);
        let s = Store::open(&path).unwrap();
        let p = s.project(p.id).unwrap();
        assert_eq!(p.tracker, Tracker::Github { repo: "o/r".into() });
        assert_eq!(p.post_mode, PostMode::Auto);
        assert_eq!(s.issue(a.id).unwrap().source, IssueSource::Internal);
        s.update_project(p.id, &ProjectPatch { tracker: Some(Tracker::Internal), ..Default::default() }).unwrap();
        let p = s.project(p.id).unwrap();
        assert_eq!(p.tracker, Tracker::Internal);
        assert_eq!(p.post_mode, PostMode::Auto);
        assert_eq!(s.issues().unwrap().iter().filter(|i| i.project_id == p.id).count(), 1);
    }

    #[test]
    fn only_a_stand_in_title_is_replaced() {
        let s = Store::memory().unwrap();
        let (_, main) = testing::project(&s, "/tmp");
        let a = s.add_session(main.id, "x", "first words", None, None).unwrap();
        assert!(!s.title_is_auto(a.id).unwrap(), "a title is chosen unless marked otherwise");
        assert!(!s.set_title(a.id, "Model title", false).unwrap());
        assert_eq!(s.session(a.id).unwrap().title, "first words");
        s.set_title_auto(a.id, true).unwrap();
        assert!(s.set_title(a.id, "  Model title ", false).unwrap());
        assert_eq!(s.session(a.id).unwrap().title, "Model title");
        assert!(!s.title_is_auto(a.id).unwrap());
        assert!(s.set_title(a.id, "Forced", true).unwrap());
        assert!(s.set_title(a.id, " ", true).is_err());
    }

    #[test]
    fn a_deleted_chats_id_is_never_given_to_the_next() {
        let s = Store::memory().unwrap();
        let (_, main) = testing::project(&s, "/tmp");
        let a = s.add_session(main.id, "x", "a", None, None).unwrap();
        s.add_message(a.id, &Entry::user("old")).unwrap();
        s.delete_session(a.id).unwrap();
        let b = s.add_session(main.id, "x", "b", None, None).unwrap();
        assert_ne!(a.id, b.id);
        assert!(s.messages(b.id, 0).unwrap().is_empty());
    }

    #[test]
    fn a_project_keeps_its_pin_and_cleaned_tags() {
        let s = Store::memory().unwrap();
        let (p, _) = testing::project(&s, "/r");
        assert!(!p.pinned && p.tags.is_empty());
        let patch = ProjectPatch {
            name: None,
            pinned: Some(true),
            tags: Some(vec![" Infra ".into(), "infra".into(), "".into(), "games".into()]),
            ..Default::default()
        };
        let p = s.update_project(p.id, &patch).unwrap();
        assert!(p.pinned);
        assert_eq!(p.tags, ["infra", "games"]);
        let p = s
            .update_project(p.id, &ProjectPatch { pinned: Some(false), ..Default::default() })
            .unwrap();
        assert!(!p.pinned);
        assert_eq!(s.project(p.id).unwrap().tags, ["infra", "games"]);
    }

    #[test]
    fn a_projects_automation_level_and_risky_toggles_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.db");
        let id = {
            let s = Store::open(&path).unwrap();
            let p = s.add_project("billing", "/r", None, None).unwrap();
            assert_eq!((p.automation, p.risky.len(), p.deploy.clone()), (None, 0, None));
            let patch = ProjectPatch {
                automation: Some(Some(Automation::FullAuto)),
                risky: Some(vec![Risky::Deploy, Risky::Deploy, Risky::Publish]),
                deploy: Some("  ./deploy.sh  ".into()),
                ..Default::default()
            };
            s.update_project(p.id, &patch).unwrap();
            p.id
        };
        let s = Store::open(&path).unwrap();
        let p = s.project(id).unwrap();
        assert_eq!(p.automation, Some(Automation::FullAuto));
        assert_eq!(p.risky, [Risky::Deploy, Risky::Publish]);
        assert_eq!(p.deploy.as_deref(), Some("./deploy.sh"));
        let p = s
            .update_project(id, &ProjectPatch { pinned: Some(true), ..Default::default() })
            .unwrap();
        assert_eq!(p.automation, Some(Automation::FullAuto), "untouched by other patches");
        let cleared = ProjectPatch { automation: Some(None), ..Default::default() };
        assert_eq!(s.update_project(id, &cleared).unwrap().automation, None);
        let json: ProjectPatch = serde_json::from_str(r#"{"automation": null}"#).unwrap();
        assert_eq!(json.automation, Some(None), "null clears, absence keeps");
        let json: ProjectPatch = serde_json::from_str(r#"{"name": "x"}"#).unwrap();
        assert_eq!(json.automation, None);
    }

    #[test]
    fn a_project_can_be_renamed_without_moving() {
        let s = Store::memory().unwrap();
        let (p, _) = testing::project(&s, "/r");
        let renamed = ProjectPatch { name: Some("  Billing API  ".into()), ..Default::default() };
        let q = s.update_project(p.id, &renamed).unwrap();
        assert_eq!(q.name, "Billing API");
        assert_eq!(q.path, p.path);
        assert_eq!(s.project(p.id).unwrap().name, "Billing API");
        let blank = ProjectPatch { name: Some("   ".into()), ..Default::default() };
        assert!(s.update_project(p.id, &blank).is_err());
        assert_eq!(s.project(p.id).unwrap().name, "Billing API");
    }

    #[test]
    fn a_database_at_the_latest_version_missing_columns_is_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.db");
        {
            let s = Store::open(&path).unwrap();
            s.add_project("demo", "/r", None, None).unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "ALTER TABLE projects DROP COLUMN pinned; ALTER TABLE projects DROP COLUMN tags;",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let p = s.projects().unwrap().into_iter().find(|p| p.name == "demo").unwrap();
        assert!(!p.pinned && p.tags.is_empty());
    }

    #[test]
    fn numbered_sessions_get_uuids_and_keep_their_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.db");
        {
            let conn = Connection::open(&path).unwrap();
            for step in &SCHEMA[..3] {
                conn.execute_batch(step).unwrap();
            }
            conn.pragma_update(None, "user_version", 3).unwrap();
            conn.execute_batch(
                r#"
                INSERT INTO projects VALUES (1, 'demo', '/r', 'main', NULL, 0);
                INSERT INTO worktrees VALUES (1, 1, '/r', 'main', NULL, 1, 0, 0, 0);
                INSERT INTO sessions VALUES (7, 1, 1, 'pi', 'kept', NULL, NULL, 'idle', 'pi-1', '{}', 5, 6, 1);
                INSERT INTO sessions VALUES (9, 1, 1, 'pi', 'other', NULL, NULL, 'idle', NULL, '{}', 5, 6, 0);
                INSERT INTO messages VALUES (1, 7, '{"kind":"user","text":"hello"}', 5);
                INSERT INTO messages VALUES (2, 9, '{"kind":"user","text":"there"}', 5);
                INSERT INTO attachments VALUES (1, 7, 'a.png', 'image/png', x'00', 5);
                INSERT INTO changes VALUES (40, 'session', 7, 5);
                "#,
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let all = s.sessions().unwrap();
        assert_eq!(all.len(), 2);
        let kept = all.iter().find(|x| x.title == "kept").unwrap();
        let other = all.iter().find(|x| x.title == "other").unwrap();
        assert_ne!(kept.id, other.id);
        assert_eq!(kept.agent_session.as_deref(), Some("pi-1"));
        assert!(s.title_is_auto(kept.id).unwrap());
        assert_eq!(kept.id.0.get_version_num(), 4);
        let msgs = s.messages(kept.id, 0).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].entry.text(), "hello");
        assert_eq!(s.attachment(1).unwrap().0.name, "a.png");
        assert_eq!(s.cursor().unwrap(), 40, "the change cursor carries on");
        s.delete_session(kept.id).unwrap();
        assert!(s.messages(kept.id, 0).unwrap().is_empty(), "deletes still cascade");
        assert_eq!(s.messages(other.id, 0).unwrap().len(), 1);
    }
    #[test]
    fn a_sessions_parent_survives_reopening_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let (parent, child) = {
            let s = Store::open(&path).unwrap();
            let (_, main) = testing::project(&s, "/a");
            let parent = s.add_session(main.id, "pi", "p", None, None).unwrap();
            let child = s.add_session(main.id, "pi", "c", None, None).unwrap();
            s.set_parent(child.id, Some(parent.id)).unwrap();
            (parent.id, child.id)
        };
        let s = Store::open(&path).unwrap();
        assert_eq!(s.session(child).unwrap().parent, Some(parent));
    }
}
