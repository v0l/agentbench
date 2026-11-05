use agentbench_core::{
    Automation, Comment, Entry, Id, Issue, IssuePatch, IssueState, Message, NewComment, NewIssue,
    NoteVersion, Project, ProjectPatch, Risky, Session, SessionId, SessionStatus, Usage, Worktree,
};
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
];

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
}

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
}

pub fn hash_note(body: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in body.trim().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn note_version_row(r: &Row) -> rusqlite::Result<NoteVersion> {
    Ok(NoteVersion {
        id: r.get(0)?,
        topic: r.get(1)?,
        body: r.get(2)?,
        hash: r.get(3)?,
        author: r.get(4)?,
        session: r.get(5)?,
        created_at: r.get(6)?,
    })
}

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
    })
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
    })
}

fn comment_row(row: &Row) -> rusqlite::Result<Comment> {
    Ok(Comment {
        id: row.get("id")?,
        issue_id: row.get("issue_id")?,
        author: row.get("author")?,
        body: row.get("body")?,
        created_at: row.get("created_at")?,
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
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError(e.to_string()))?;
        }
        Store::setup(Connection::open(path)?)
    }

    pub fn memory() -> Result<Store> {
        Store::setup(Connection::open_in_memory()?)
    }

    fn setup(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
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
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Store { conn: Arc::new(Mutex::new(conn)) };
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
        {
            let db = self.db();
            db.execute(
                "UPDATE projects SET name = ?1, pinned = ?2, tags = ?3, automation = ?4, risky = ?5, deploy = ?6 WHERE id = ?7",
                params![
                    project.name,
                    project.pinned,
                    serde_json::to_string(&project.tags).unwrap_or_else(|_| "[]".into()),
                    project.automation.map(Automation::as_str),
                    serde_json::to_string(&project.risky).unwrap_or_else(|_| "[]".into()),
                    project.deploy,
                    id
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
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn issue(&self, id: Id) -> Result<Issue> {
        self.db()
            .query_row("SELECT * FROM issues WHERE id = ?1", [id], issue_row)
            .optional()?
            .ok_or_else(|| not_found("issue", id))
    }

    pub fn add_issue(&self, new: &NewIssue) -> Result<Issue> {
        if new.title.trim().is_empty() {
            return Err(StoreError("an issue needs a title".into()));
        }
        self.project(new.project_id)?;
        let t = now();
        let id = {
            let db = self.db();
            db.execute(
                "INSERT INTO issues (project_id, title, body, state, priority, labels, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![
                    new.project_id,
                    new.title.trim(),
                    new.body,
                    new.state.unwrap_or(IssueState::Todo).as_str(),
                    new.priority.unwrap_or(0),
                    serde_json::to_string(&new.labels).unwrap_or_else(|_| "[]".into()),
                    t
                ],
            )?;
            let id = db.last_insert_rowid();
            Store::changed(&db, "issue", id)?;
            id
        };
        self.issue(id)
    }

    pub fn update_issue(&self, id: Id, patch: &IssuePatch) -> Result<Issue> {
        let mut issue = self.issue(id)?;
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
        {
            let db = self.db();
            db.execute(
                "UPDATE issues SET title = ?1, body = ?2, state = ?3, priority = ?4,
                 labels = ?5, updated_at = ?6 WHERE id = ?7",
                params![
                    issue.title,
                    issue.body,
                    issue.state.as_str(),
                    issue.priority,
                    serde_json::to_string(&issue.labels).unwrap_or_else(|_| "[]".into()),
                    now(),
                    id
                ],
            )?;
            Store::changed(&db, "issue", id)?;
        }
        self.issue(id)
    }

    pub fn delete_issue(&self, id: Id) -> Result<()> {
        let db = self.db();
        if db.execute("DELETE FROM issues WHERE id = ?1", [id])? == 0 {
            return Err(not_found("issue", id));
        }
        Store::changed(&db, "issue", id)
    }

    pub fn comments(&self, issue_id: Id) -> Result<Vec<Comment>> {
        let db = self.db();
        let mut stmt = db.prepare("SELECT * FROM comments WHERE issue_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([issue_id], comment_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_comment(&self, issue_id: Id, new: &NewComment) -> Result<Comment> {
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

    pub fn add_usage(&self, id: SessionId, delta: &Usage) -> Result<()> {
        let mut usage = self.session(id)?.usage;
        usage.add(delta);
        let db = self.db();
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

    /// What was remembered about a project, as (topic, note), oldest topic first.
    pub fn notes(&self, project_id: Id) -> Result<Vec<(String, String)>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT topic, body FROM notes WHERE project_id = ?1 ORDER BY updated_at, topic",
        )?;
        let rows = stmt.query_map([project_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
            "SELECT session, issue, project, state, nudges, reworks, green
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
        let body: Option<String> = self
            .db()
            .query_row(
                "SELECT body FROM notes WHERE project_id = ?1 AND topic = ?2",
                params![project_id, topic],
                |r| r.get(0),
            )
            .optional()?;
        Ok(body.map(|b| hash_note(&b)).unwrap_or_default())
    }

    pub fn set_note(
        &self,
        project_id: Id,
        topic: &str,
        body: &str,
        author: &str,
        session: Option<SessionId>,
        based_on: Option<&str>,
    ) -> Result<bool> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT body FROM notes WHERE project_id = ?1 AND topic = ?2",
                params![project_id, topic],
                |r| r.get(0),
            )
            .optional()?;
        let current_hash = current.as_deref().map(hash_note).unwrap_or_default();
        if let Some(base) = based_on {
            if base.trim() != current_hash {
                return Err(StoreError(format!(
                    "note `{topic}` changed since you read it (now {current_hash}); recall it again and retry with based_on set to the new hash"
                )));
            }
        } else if current.is_some() {
            return Err(StoreError(format!(
                "note `{topic}` exists; pass based_on={current_hash} (from recall) to replace it"
            )));
        }
        let body = body.trim();
        let n = if body.is_empty() {
            tx.execute(
                "DELETE FROM notes WHERE project_id = ?1 AND topic = ?2",
                params![project_id, topic],
            )?
        } else {
            tx.execute(
                "INSERT INTO notes (project_id, topic, body, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(project_id, topic) DO UPDATE
                 SET body = excluded.body, updated_at = excluded.updated_at",
                params![project_id, topic, body, now()],
            )?
        };
        if n > 0 {
            tx.execute(
                "INSERT INTO note_versions (project_id, topic, body, hash, author, session, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![project_id, topic, body, hash_note(body), author, session, now()],
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
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

    pub fn note_versions(&self, project_id: Id, topic: &str) -> Result<Vec<NoteVersion>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, topic, body, hash, author, session, created_at FROM note_versions
             WHERE project_id = ?1 AND topic = ?2 ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![project_id, topic], note_version_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn note_topics(&self, project_id: Id) -> Result<Vec<String>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT topic FROM note_versions WHERE project_id = ?1 GROUP BY topic ORDER BY MAX(id) DESC",
        )?;
        let rows = stmt.query_map([project_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn restore_note(
        &self,
        project_id: Id,
        version: Id,
        author: &str,
        session: Option<SessionId>,
    ) -> Result<()> {
        let (topic, body): (String, String) = self
            .db()
            .query_row(
                "SELECT topic, body FROM note_versions WHERE id = ?1 AND project_id = ?2",
                params![version, project_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError(format!("no note version {version}")))?;
        let current = self.note_hash(project_id, &topic)?;
        self.set_note(project_id, &topic, &body, author, session, Some(&current))?;
        Ok(())
    }

    pub fn global_notes(&self) -> Result<Vec<(String, String)>> {
        let db = self.db();
        let mut stmt =
            db.prepare("SELECT topic, body FROM global_notes ORDER BY updated_at, topic")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_global_note(&self, topic: &str, body: &str) -> Result<bool> {
        let db = self.db();
        let n = if body.trim().is_empty() {
            db.execute("DELETE FROM global_notes WHERE topic = ?1", params![topic])?
        } else {
            db.execute(
                "INSERT INTO global_notes (topic, body, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(topic) DO UPDATE
                 SET body = excluded.body, updated_at = excluded.updated_at",
                params![topic, body.trim(), now()],
            )?
        };
        Ok(n > 0)
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

    fn tree(path: &str, branch: &str, main: bool) -> SeenTree {
        SeenTree { path: path.into(), branch: Some(branch.into()), head: Some("abc".into()), main }
    }

    fn issue(store: &Store, project_id: Id, title: &str) -> Issue {
        store
            .add_issue(&NewIssue { project_id, title: title.into(), ..Default::default() })
            .unwrap()
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
}
