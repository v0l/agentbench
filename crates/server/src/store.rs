use foreman_core::{
    Comment, Entry, Id, Issue, IssuePatch, IssueState, Message, NewComment, NewIssue, Project,
    Session, SessionStatus, Usage, Worktree,
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
    created_by_foreman INTEGER NOT NULL DEFAULT 0,
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
];

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

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
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
        issue_id: row.get("issue_id")?,
        status: SessionStatus::parse(&status).unwrap_or(SessionStatus::Idle),
        agent_session: row.get("agent_session")?,
        usage: serde_json::from_str(&usage).unwrap_or_default(),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
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
        created_by_foreman: row.get("created_by_foreman")?,
        created_at: row.get("created_at")?,
    })
}

fn project_row(row: &Row) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get("id")?,
        name: row.get("name")?,
        path: row.get("path")?,
        branch: row.get("branch")?,
        remote: row.get("remote")?,
        created_at: row.get("created_at")?,
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

fn not_found(what: &str, id: Id) -> StoreError {
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
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, step) in SCHEMA.iter().enumerate().skip(version.max(0) as usize) {
            conn.execute_batch(step)?;
            conn.pragma_update(None, "user_version", i as i64 + 1)?;
        }
        Ok(Store { conn: Arc::new(Mutex::new(conn)) })
    }

    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn changed(conn: &Connection, kind: &str, ref_id: Id) -> Result<()> {
        conn.execute(
            "INSERT INTO changes (kind, ref_id, created_at) VALUES (?1, ?2, ?3)",
            params![kind, ref_id, now()],
        )?;
        Ok(())
    }

    pub fn cursor(&self) -> Result<Id> {
        Ok(self.db().query_row("SELECT COALESCE(MAX(id), 0) FROM changes", [], |r| r.get(0))?)
    }

    pub fn changes_since(&self, cursor: Id) -> Result<Vec<(Id, String, Id)>> {
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
        let mut stmt = db.prepare("SELECT * FROM projects ORDER BY name")?;
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

    pub fn delete_project(&self, id: Id) -> Result<()> {
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
                "INSERT INTO worktrees (project_id, path, branch, head, main, missing, created_by_foreman, created_at)
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
        for gone in before.iter().filter(|w| !w.missing && !seen.iter().any(|t| t.path == w.path)) {
            changed = true;
            let in_use: i64 = db.query_row(
                "SELECT COUNT(*) FROM sessions WHERE worktree_id = ?1",
                [gone.id],
                |r| r.get(0),
            )?;
            if in_use > 0 {
                db.execute("UPDATE worktrees SET missing = 1 WHERE id = ?1", [gone.id])?;
            } else {
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

    pub fn session(&self, id: Id) -> Result<Session> {
        self.db()
            .query_row(&format!("{SESSION_SELECT} WHERE s.id = ?1"), [id], session_row)
            .optional()?
            .ok_or_else(|| not_found("session", id))
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
        let id = {
            let db = self.db();
            db.execute(
                "INSERT INTO sessions (project_id, worktree_id, agent, title, model, issue_id, status, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
                params![
                    worktree.project_id,
                    worktree_id,
                    agent,
                    title,
                    model,
                    issue_id,
                    SessionStatus::Idle.as_str(),
                    t
                ],
            )?;
            let id = db.last_insert_rowid();
            Store::changed(&db, "session", id)?;
            id
        };
        self.session(id)
    }

    pub fn set_status(&self, id: Id, status: SessionStatus) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.as_str(), now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    pub fn clear_agent_session(&self, id: Id) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE sessions SET agent_session = NULL, updated_at = ?1 WHERE id = ?2",
            params![now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    /// Mark a session's title as a stand-in (cut from the first message) that a
    /// model may replace once there is a conversation to name.
    pub fn set_title_auto(&self, id: Id, auto: bool) -> Result<()> {
        self.db()
            .execute("UPDATE sessions SET title_auto = ?1 WHERE id = ?2", params![auto, id])?;
        Ok(())
    }

    pub fn title_is_auto(&self, id: Id) -> Result<bool> {
        self.db()
            .query_row("SELECT title_auto FROM sessions WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| not_found("session", id))
    }

    /// Give a session its real title. Unless `force`, only a stand-in title is
    /// replaced, so a name someone chose is never overwritten.
    pub fn set_title(&self, id: Id, title: &str, force: bool) -> Result<bool> {
        let title = title.trim();
        if title.is_empty() {
            return Err(StoreError("a title needs some text".into()));
        }
        let db = self.db();
        let n = db.execute(
            "UPDATE sessions SET title = ?1, title_auto = 0, updated_at = ?2
             WHERE id = ?3 AND (title_auto = 1 OR ?4)",
            params![title, now(), id, force],
        )?;
        if n > 0 {
            Store::changed(&db, "session", id)?;
        }
        Ok(n > 0)
    }

    pub fn set_model(&self, id: Id, model: Option<&str>) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE sessions SET model = ?1, updated_at = ?2 WHERE id = ?3",
            params![model, now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    pub fn set_agent_session(&self, id: Id, agent_session: &str) -> Result<()> {
        let db = self.db();
        db.execute(
            "UPDATE sessions SET agent_session = ?1, updated_at = ?2 WHERE id = ?3",
            params![agent_session, now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    pub fn add_usage(&self, id: Id, delta: &Usage) -> Result<()> {
        let mut usage = self.session(id)?.usage;
        usage.add(delta);
        let db = self.db();
        db.execute(
            "UPDATE sessions SET usage = ?1, updated_at = ?2 WHERE id = ?3",
            params![serde_json::to_string(&usage).unwrap_or_default(), now(), id],
        )?;
        Store::changed(&db, "session", id)
    }

    pub fn delete_session(&self, id: Id) -> Result<()> {
        let db = self.db();
        if db.execute("DELETE FROM sessions WHERE id = ?1", [id])? == 0 {
            return Err(not_found("session", id));
        }
        Store::changed(&db, "session", id)
    }

    pub fn reset_running(&self) -> Result<usize> {
        let db = self.db();
        Ok(db.execute("UPDATE sessions SET status = 'stopped' WHERE status = 'running'", [])?)
    }

    pub fn add_message(&self, session_id: Id, entry: &Entry) -> Result<Message> {
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
        session_id: Id,
        name: &str,
        mime: &str,
        data: &[u8],
    ) -> Result<foreman_core::Attachment> {
        let db = self.db();
        db.execute(
            "INSERT INTO attachments (session_id, name, mime, data, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, name, mime, data, now()],
        )?;
        Ok(foreman_core::Attachment {
            id: db.last_insert_rowid(),
            name: name.to_string(),
            mime: mime.to_string(),
            size: data.len() as u64,
        })
    }

    /// An attachment and its bytes.
    pub fn attachment(&self, id: Id) -> Result<(foreman_core::Attachment, Vec<u8>)> {
        self.db()
            .query_row("SELECT name, mime, data FROM attachments WHERE id = ?1", [id], |r| {
                let data: Vec<u8> = r.get("data")?;
                Ok((
                    foreman_core::Attachment {
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

    pub fn update_message(&self, id: Id, entry: &Entry) -> Result<()> {
        let db = self.db();
        let session_id: Id =
            db.query_row("SELECT session_id FROM messages WHERE id = ?1", [id], |r| r.get(0))?;
        db.execute(
            "UPDATE messages SET entry = ?1 WHERE id = ?2",
            params![serde_json::to_string(entry).unwrap_or_default(), id],
        )?;
        Store::changed(&db, "message", session_id)
    }

    pub fn messages(&self, session_id: Id, after: Id) -> Result<Vec<Message>> {
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
        assert!(all[1].created_by_foreman);

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
        s.add_message(sess.id, &Entry::User { text: "hi".into() }).unwrap();
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
        let m1 = s.add_message(sess.id, &Entry::User { text: "hi".into() }).unwrap();
        s.add_message(sess.id, &Entry::Assistant { text: "yo".into() }).unwrap();
        assert_eq!(s.messages(sess.id, 0).unwrap().len(), 2);
        assert_eq!(
            s.messages(sess.id, m1.id).unwrap()[0].entry,
            Entry::Assistant { text: "yo".into() }
        );
        s.update_message(m1.id, &Entry::User { text: "edited".into() }).unwrap();
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
    fn every_write_moves_the_cursor() {
        let s = Store::memory().unwrap();
        let (p, _) = project(&s, "/r");
        let c0 = s.cursor().unwrap();
        let i = issue(&s, p.id, "a");
        let c1 = s.cursor().unwrap();
        assert!(c1 > c0);
        s.add_comment(i.id, &NewComment { author: None, body: "x".into() }).unwrap();
        assert_eq!(s.changes_since(c1).unwrap(), vec![(c1 + 1, "issue".to_string(), i.id)]);
    }

    #[test]
    fn running_sessions_are_stopped_on_restart() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/r");
        let sess = s.add_session(main.id, "pi", "t", None, None).unwrap();
        s.set_status(sess.id, SessionStatus::Running).unwrap();
        assert_eq!(s.reset_running().unwrap(), 1);
        assert_eq!(s.session(sess.id).unwrap().status, SessionStatus::Stopped);
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
}
