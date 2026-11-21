use super::Backend;
use super::app::{App, Cmd, Reply, Tone};
use agent_native::{Agent, Api, CancellationToken, Context, Event, Io, Native, Steer, Turn, async_trait};
use agentbench_core::{
    AgentInfo, Entry, Id, Message, Project, Question, SECRET_SHOWN, Said, Sent, Session,
    SessionId, SessionStatus, Usage,
};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

const PUBLISH_EVERY: Duration = Duration::from_millis(50);
const MAX_FOLDS: usize = 6;

struct Chat {
    session: SessionId,
    title: String,
    model: Option<String>,
    effort: Option<String>,
    rows: Vec<Message>,
    next: Id,
    text: Option<Id>,
    thinking: Option<Id>,
    dirty: BTreeSet<Id>,
    usage: Usage,
    status: SessionStatus,
    inbox: Vec<String>,
    waiting: HashMap<String, oneshot::Sender<Vec<String>>>,
    errored: bool,
    running: bool,
    stop: CancellationToken,
    started: i64,
}

impl Chat {
    fn new(session: SessionId) -> Chat {
        Chat {
            session,
            title: "chat".into(),
            model: None,
            effort: None,
            rows: Vec::new(),
            next: 1,
            text: None,
            thinking: None,
            dirty: BTreeSet::new(),
            usage: Usage::default(),
            status: SessionStatus::Idle,
            inbox: Vec::new(),
            waiting: HashMap::new(),
            errored: false,
            running: false,
            stop: CancellationToken::new(),
            started: crate::store::now(),
        }
    }

    fn add(&mut self, entry: Entry) -> Id {
        let id = self.next;
        self.next += 1;
        self.rows.push(Message {
            id,
            session_id: self.session,
            entry,
            created_at: crate::store::now(),
        });
        self.dirty.insert(id);
        id
    }

    fn entry(&mut self, entry: Entry) -> Id {
        self.text = None;
        self.thinking = None;
        if matches!(entry, Entry::Error { .. }) {
            self.errored = true;
        }
        self.add(entry)
    }

    fn set(&mut self, id: Id, entry: Entry) {
        if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
            row.entry = entry;
            self.dirty.insert(id);
        }
    }

    fn append(&mut self, delta: &str, thinking: bool) {
        let slot = if thinking { self.thinking } else { self.text };
        if let Some(id) = slot {
            if let Some(row) = self.rows.iter_mut().find(|r| r.id == id) {
                match &mut row.entry {
                    Entry::Assistant { text } | Entry::Thinking { text } => text.push_str(delta),
                    _ => {}
                }
                self.dirty.insert(id);
            }
            return;
        }
        let delta = delta.trim_start().to_string();
        if delta.is_empty() {
            return;
        }
        let entry = if thinking {
            Entry::Thinking { text: delta }
        } else {
            Entry::Assistant { text: delta }
        };
        let id = self.add(entry);
        if thinking {
            self.text = None;
            self.thinking = Some(id);
        } else {
            self.thinking = None;
            self.text = Some(id);
        }
    }

    fn sent(&mut self, messages: Vec<Sent>) {
        let streamed = match messages.first() {
            Some(s) if s.said == Said::Kept => None,
            Some(s) if s.message["role"] == "assistant" => self.text.take(),
            _ => None,
        };
        self.thinking = None;
        self.text = None;
        let entry = Entry::Wire { messages };
        match streamed {
            Some(id) => self.set(id, entry),
            None => {
                self.add(entry);
            }
        }
    }

    fn rewrite(&mut self, mut change: impl FnMut(&mut Entry) -> bool) {
        for row in &mut self.rows {
            if change(&mut row.entry) {
                self.dirty.insert(row.id);
            }
        }
    }

    fn entries(&self) -> Vec<Entry> {
        self.rows.iter().map(|r| r.entry.clone()).collect()
    }

    fn snapshot(&self, cwd: &Path) -> Session {
        Session {
            id: self.session,
            project_id: 0,
            worktree_id: 0,
            agent: "native".into(),
            title: self.title.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            issue_id: None,
            parent: None,
            status: self.status,
            agent_session: None,
            usage: self.usage.clone(),
            created_at: self.started,
            updated_at: crate::store::now(),
            queued: self.inbox.clone(),
            no_plan_reads: None,
            deferred: 0,
            queue_system: false,
            paused_until: None,
            released: false,
            stuck: false,
        }
    }
}

struct Inner {
    chat: Mutex<Chat>,
    tx: UnboundedSender<Reply>,
    cwd: PathBuf,
    save: Option<PathBuf>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, Chat> {
        self.chat.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self) {
        let mut chat = self.lock();
        let session = chat.snapshot(&self.cwd);
        let ids = std::mem::take(&mut chat.dirty);
        let rows: Vec<Message> =
            chat.rows.iter().filter(|r| ids.contains(&r.id)).cloned().collect();
        drop(chat);
        let _ = self.tx.send(Reply::Refreshed(session, rows));
    }

    fn note(&self, entry: Entry) {
        self.lock().entry(entry);
    }

    fn save_transcript(&self) {
        let Some(path) = &self.save else { return };
        let text = transcript_text(&self.lock().rows);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, text);
    }
}

pub fn transcript_text(rows: &[Message]) -> String {
    let mut out = String::new();
    for m in agentbench_core::shown(rows) {
        match &m.entry {
            Entry::User { text, .. } => out.push_str(&format!("you: {text}\n\n")),
            Entry::Assistant { text } => out.push_str(&format!("agent: {text}\n\n")),
            Entry::Tool { name, input, .. } if name != agentbench_core::SYSTEM_PROMPT => {
                out.push_str(&format!("[{name} {input}]\n\n"))
            }
            Entry::Summary { text } if !text.is_empty() => {
                out.push_str(&format!("[summary] {text}\n\n"))
            }
            Entry::Error { text } | Entry::Warning { text } | Entry::Notice { text } => {
                out.push_str(&format!("[{text}]\n\n"))
            }
            _ => {}
        }
    }
    out
}

struct NoApi;

impl Api for NoApi {
    fn call(&self, tool: &str, _args: &Value, _session: SessionId) -> Result<Value, String> {
        Err(format!("{tool} needs the agentbench server, and this chat is standalone"))
    }
}

struct Sink {
    inner: Arc<Inner>,
    stop: CancellationToken,
}

impl Sink {
    fn ask_all(
        &self,
        id: String,
        open: Entry,
    ) -> impl std::future::Future<Output = Result<Vec<String>, String>> + '_ {
        let (tx, rx) = oneshot::channel();
        let row = {
            let mut chat = self.inner.lock();
            chat.waiting.insert(id.clone(), tx);
            chat.status = SessionStatus::Waiting;
            chat.entry(open)
        };
        let stop = self.stop.clone();
        let inner = self.inner.clone();
        async move {
            let answer = tokio::select! {
                _ = stop.cancelled() => Err("stopped".to_string()),
                a = rx => a.map_err(|_| "the question was withdrawn".to_string()),
            };
            let mut chat = inner.lock();
            chat.waiting.remove(&id);
            chat.status = SessionStatus::Running;
            if let Ok(a) = &answer {
                let entry = chat.rows.iter().find(|r| r.id == row).map(|r| r.entry.clone());
                if let Some(entry) = entry {
                    chat.set(row, answered_entry(entry, a));
                }
            }
            answer
        }
    }
}

fn answered_entry(entry: Entry, answers: &[String]) -> Entry {
    match entry {
        Entry::Question { id, question, .. } => Entry::Question {
            id,
            answer: Some(if question.secret { SECRET_SHOWN.into() } else { answers.join(", ") }),
            question,
        },
        Entry::Questions { id, questions, .. } => {
            Entry::Questions { id, questions, answers: Some(answers.to_vec()) }
        }
        other => other,
    }
}

#[async_trait]
impl Io for Sink {
    fn emit(&self, event: Event) {
        let mut chat = self.inner.lock();
        match event {
            Event::Text { delta } => chat.append(&delta, false),
            Event::Thinking { text } => chat.append(&text, true),
            Event::Usage(u) => chat.usage.add(&u),
            Event::Notice(text) => {
                chat.entry(Entry::Notice { text });
            }
            Event::Warning(text) => {
                chat.entry(Entry::Warning { text });
            }
            Event::Paused { text, .. } => {
                chat.entry(Entry::Warning { text });
            }
            Event::Diagnostics { text, broken } => {
                chat.entry(Entry::Diagnostics { text, broken });
            }
            Event::Error(text) => {
                chat.entry(Entry::Error { text });
            }
            Event::PicturesRemoved => chat.rewrite(agentbench_core::remove_pictures_from),
            Event::Trimmed(trims) => {
                chat.rewrite(|entry| agentbench_core::apply_trims_to(entry, &trims))
            }
            Event::Sent(messages) => chat.sent(messages),
            Event::Summary(text) => {
                chat.entry(Entry::Summary { text });
            }
            Event::Done => {
                chat.text = None;
                chat.thinking = None;
            }
            Event::Started { .. }
            | Event::ToolStart { .. }
            | Event::ToolEnd { .. }
            | Event::Steered { .. }
            | Event::Resumed => {}
        }
    }

    fn stop(&self) -> &CancellationToken {
        &self.stop
    }

    fn steer(&self) -> Option<Steer> {
        let mut chat = self.inner.lock();
        if chat.inbox.is_empty() {
            return None;
        }
        Some(Steer::from(chat.inbox.remove(0).as_str()))
    }

    async fn ask(&self, question: Question) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let open = Entry::Question { id: id.clone(), question, answer: None };
        let answers = self.ask_all(id, open).await?;
        Ok(answers.join(", "))
    }

    async fn ask_many(&self, questions: Vec<Question>) -> Result<Vec<String>, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let n = questions.len();
        let open = Entry::Questions { id: id.clone(), questions, answers: None };
        let mut answers = self.ask_all(id, open).await?;
        answers.resize(n, String::new());
        Ok(answers)
    }

    fn api(&self) -> &dyn Api {
        &NoApi
    }
}

async fn fold_older(native: &Native, inner: &Inner, stop: &CancellationToken, force: bool) {
    for _ in 0..MAX_FOLDS {
        let (entries, model, full) = {
            let chat = inner.lock();
            (chat.entries(), chat.model.clone(), chat.usage.past(native.compact_at()))
        };
        if !full && !force {
            return;
        }
        match native.fold(&entries, model.as_deref(), stop).await {
            Ok(Some(f)) => {
                let mut chat = inner.lock();
                chat.entry(Entry::Section { text: f.text, covers: f.covers, topic: f.topic.clone() });
                chat.entry(Entry::Notice {
                    text: format!("folded {} older messages (topic: {}) into a summary", f.covers, f.topic),
                });
                if f.before > 0 {
                    chat.usage.context =
                        (chat.usage.context as u128 * f.after as u128 / f.before as u128) as u64;
                }
            }
            Ok(None) => {
                if force {
                    inner.note(Entry::Notice { text: "nothing to fold yet".into() });
                }
                return;
            }
            Err(e) => {
                inner.note(Entry::Warning { text: format!("could not fold the older messages: {e}") });
                return;
            }
        }
        if force {
            return;
        }
    }
}

async fn run_turns(
    native: Arc<Native>,
    inner: Arc<Inner>,
    context: Context,
    cwd: PathBuf,
    stop: CancellationToken,
    first: String,
) {
    let sink = Sink { inner: inner.clone(), stop: stop.clone() };
    let mut prompt = first;
    loop {
        let (history, model, effort) = {
            let mut chat = inner.lock();
            chat.errored = false;
            chat.status = SessionStatus::Running;
            (chat.entries(), chat.model.clone(), chat.effort.clone())
        };
        let turn = Turn {
            prompt: &prompt,
            files: &[],
            cwd: &cwd,
            model: model.as_deref(),
            effort: effort.as_deref(),
            resume: None,
            history: &history,
            context: &context,
        };
        let result = {
            let run = native.run(&turn, &sink);
            tokio::pin!(run);
            let mut ticker = tokio::time::interval(PUBLISH_EVERY);
            loop {
                tokio::select! {
                    r = &mut run => break r,
                    _ = ticker.tick() => inner.publish(),
                }
            }
        };
        let errored = inner.lock().errored;
        let status = if stop.is_cancelled() {
            inner.note(Entry::Notice { text: "stopped".into() });
            SessionStatus::Stopped
        } else {
            match result {
                Ok(()) if !errored => SessionStatus::Idle,
                Ok(()) => SessionStatus::Failed,
                Err(e) => {
                    if !e.is_empty() {
                        inner.note(Entry::Error { text: e });
                    }
                    SessionStatus::Failed
                }
            }
        };
        if status == SessionStatus::Idle {
            fold_older(&native, &inner, &stop, false).await;
        }
        let queued = std::mem::take(&mut inner.lock().inbox);
        if status != SessionStatus::Idle || queued.is_empty() {
            let mut chat = inner.lock();
            chat.status = status;
            chat.running = false;
            drop(chat);
            inner.save_transcript();
            inner.publish();
            return;
        }
        prompt = queued.join("\n\n");
    }
}

pub struct Solo {
    native: Arc<Native>,
    context: Context,
    cwd: PathBuf,
    inner: Arc<Inner>,
    save: Option<PathBuf>,
}

impl Solo {
    pub fn new(
        native: Arc<Native>,
        context: Context,
        cwd: PathBuf,
        tx: UnboundedSender<Reply>,
        save_dir: Option<PathBuf>,
    ) -> Solo {
        let save = save_dir.map(|dir| {
            dir.join(format!("chat-{}.md", crate::store::now()))
        });
        let inner = Arc::new(Inner {
            chat: Mutex::new(Chat::new(context.session)),
            tx,
            cwd: cwd.clone(),
            save: save.clone(),
        });
        Solo { native, context, cwd, inner, save }
    }

    pub fn app(&self) -> App {
        let info = AgentInfo {
            name: "native".into(),
            label: "Native".into(),
            available: true,
            resumable: false,
            about: self.native.about(),
        };
        let project = Project {
            id: 0,
            name: self.context.project.clone(),
            path: self.cwd.to_string_lossy().into_owned(),
            branch: None,
            remote: None,
            created_at: 0,
            home: false,
            supervisor: false,
            pinned: false,
            tags: Vec::new(),
            automation: None,
            risky: Vec::new(),
            deploy: None,
        };
        let mut app = App::new(project, vec![info], "native".into());
        app.standalone = true;
        app
    }

    pub fn saved(&self) -> Option<PathBuf> {
        self.save.clone().filter(|p| p.is_file())
    }

    fn reset(&mut self) {
        self.context.session = SessionId(uuid::Uuid::new_v4());
        let mut chat = self.inner.lock();
        chat.stop.cancel();
        *chat = Chat::new(self.context.session);
    }

    fn start(&mut self, prompt: String) {
        let stop = {
            let mut chat = self.inner.lock();
            chat.running = true;
            chat.stop = CancellationToken::new();
            chat.stop.clone()
        };
        tokio::spawn(run_turns(
            self.native.clone(),
            self.inner.clone(),
            self.context.clone(),
            self.cwd.clone(),
            stop,
            prompt,
        ));
    }

    fn note(&self, app: &mut App, tone: Tone, text: &str) {
        app.say(tone, text);
    }

    fn slash(&mut self, app: &mut App, name: &str, arg: &str) -> bool {
        match name {
            "clear" => {
                if self.inner.lock().running {
                    self.note(app, Tone::Error, "stop the agent before clearing");
                } else {
                    self.inner.note(Entry::Summary { text: String::new() });
                    self.inner.note(Entry::Notice {
                        text: "the agent starts afresh from here".into(),
                    });
                    self.inner.publish();
                }
            }
            "model" => {
                let model = Some(arg.to_string()).filter(|m| !m.is_empty());
                let shown = model.clone().unwrap_or_else(|| "the default model".into());
                self.inner.lock().model = model;
                self.inner.note(Entry::Notice { text: format!("this chat now uses {shown}") });
                self.inner.publish();
            }
            "thinking" => {
                let arg = arg.to_lowercase();
                let effort = Some(arg.clone()).filter(|e| !e.is_empty());
                if effort.as_deref().is_some_and(|e| !agentbench_core::EFFORTS.contains(&e)) {
                    self.note(
                        app,
                        Tone::Error,
                        &format!("thinking is one of {}", agentbench_core::EFFORTS.join(", ")),
                    );
                } else {
                    self.inner.lock().effort = effort;
                    self.inner.note(Entry::Notice { text: format!("thinking is now {arg}") });
                    self.inner.publish();
                }
            }
            "compact" => {
                if self.inner.lock().running {
                    self.note(app, Tone::Error, "wait for the agent to finish first");
                } else {
                    let (native, inner) = (self.native.clone(), self.inner.clone());
                    let stop = {
                        let mut chat = inner.lock();
                        chat.running = true;
                        chat.status = SessionStatus::Running;
                        chat.stop = CancellationToken::new();
                        chat.stop.clone()
                    };
                    tokio::spawn(async move {
                        fold_older(&native, &inner, &stop, true).await;
                        let mut chat = inner.lock();
                        chat.running = false;
                        chat.status = SessionStatus::Idle;
                        drop(chat);
                        inner.publish();
                    });
                }
            }
            "title" => self.note(app, Tone::Info, "standalone chats are not titled"),
            _ => return false,
        }
        true
    }
}

impl Backend for Solo {
    fn command(&mut self, app: &mut App, cmd: Cmd) -> bool {
        match cmd {
            Cmd::Quit => {
                self.inner.lock().stop.cancel();
                self.inner.save_transcript();
                return true;
            }
            Cmd::Send(text) => {
                if app.session.is_none() {
                    self.reset();
                    let mut chat = self.inner.lock();
                    chat.model = app.model.clone();
                    chat.title = agentbench_core::short_title(&text, 40);
                    let session = chat.snapshot(&self.cwd);
                    drop(chat);
                    let _ = self.inner.tx.send(Reply::Created(session));
                }
                if let Some((cmd, arg)) = agentbench_core::parse_slash(&text)
                    && self.slash(app, cmd.name, arg)
                {
                    return false;
                }
                let mut chat = self.inner.lock();
                if chat.running {
                    chat.inbox.push(text);
                    drop(chat);
                    self.inner.publish();
                } else {
                    drop(chat);
                    let text = crate::runner::expand_prompt(&self.cwd.to_string_lossy(), &text)
                        .map_or(text, |e| e.text);
                    self.start(text);
                }
            }
            Cmd::Answer { question, answers } => {
                let waiting = self.inner.lock().waiting.remove(&question);
                if let Some(tx) = waiting {
                    let _ = tx.send(answers);
                }
            }
            Cmd::Interrupt => {
                let (id, queued) = {
                    let mut chat = self.inner.lock();
                    chat.stop.cancel();
                    (chat.session, std::mem::take(&mut chat.inbox))
                };
                let _ = self.inner.tx.send(Reply::Interrupted(id, queued));
            }
            Cmd::Delete(_) => self.reset(),
            Cmd::Open(_) | Cmd::Sessions => {}
            Cmd::Refresh => self.inner.publish(),
        }
        false
    }

    fn handle(&mut self, app: &mut App, reply: Reply) {
        if let Some(next) = app.reply(reply) {
            self.command(app, next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::native::{Model, Scope};
    use std::io::{Read, Write};

    fn stub(replies: Vec<Vec<Value>>) -> (String, std::sync::mpsc::Receiver<Value>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (seen, asked) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (stream, events) in listener.incoming().zip(replies) {
                let Ok(mut s) = stream else { return };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if s.read(&mut byte).unwrap_or(0) != 1 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&head).to_lowercase();
                let len = text
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                let _ = s.read_exact(&mut body);
                let _ = seen.send(serde_json::from_slice(&body).unwrap_or(Value::Null));
                let mut sse = String::new();
                for e in events {
                    sse.push_str(&format!("data: {e}\n\n"));
                }
                sse.push_str("data: [DONE]\n\n");
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                        sse.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (url, asked)
    }

    fn say(text: &str) -> Value {
        serde_json::json!({"choices":[{"delta":{"content":text}}]})
    }

    fn solo(
        url: &str,
        cwd: &Path,
        scope: Scope,
        save: Option<PathBuf>,
    ) -> (Solo, tokio::sync::mpsc::UnboundedReceiver<Reply>, App) {
        let mut native = Native::new(Model { url: url.into(), model: "fake".into(), ..Model::default() });
        native.scope = scope;
        native.mcp = Arc::new(agent_core::mcp::Mcp::new(agent_core::mcp::Settings {
            files: Vec::new(),
            ..agent_native::native::mcp_settings()
        }));
        let context = Context {
            session: SessionId(uuid::Uuid::new_v4()),
            project_id: 0,
            project: "here".into(),
            project_path: cwd.to_string_lossy().into_owned(),
            branch: None,
            home: false,
            supervisor: false,
            issue: None,
            api_url: String::new(),
            token: String::new(),
            notes: vec![("style".into(), "plain words".into())],
            global_notes: Vec::new(),
            automation: String::new(),
            quota: String::new(),
            vision: String::new(),
        };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let solo = Solo::new(Arc::new(native), context, cwd.to_path_buf(), tx, save);
        let app = solo.app();
        (solo, rx, app)
    }

    async fn settle(
        solo: &mut Solo,
        app: &mut App,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Reply>,
    ) {
        let end = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let reply = tokio::time::timeout_at(end, rx.recv()).await.expect("the turn ended").unwrap();
            solo.handle(app, reply);
            if app.session.as_ref().is_some_and(|s| s.status == SessionStatus::Idle)
                && !solo.inner.lock().running
            {
                return;
            }
        }
    }

    fn shown_text(app: &App) -> String {
        agentbench_core::shown(&app.messages)
            .iter()
            .filter_map(|m| match &m.entry {
                Entry::Assistant { text } | Entry::User { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[tokio::test]
    async fn a_standalone_chat_answers_and_leaves_the_data_dir_untouched() {
        let data = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let (url, asked) = stub(vec![vec![say("hello "), say("there")], vec![say("again")]]);
        let (mut solo, mut rx, mut app) = solo(&url, work.path(), Scope::standalone(), None);
        solo.command(&mut app, Cmd::Send("hi".into()));
        settle(&mut solo, &mut app, &mut rx).await;
        assert_eq!(shown_text(&app), "hi|hello there");
        let first = asked.recv().unwrap();
        let tools = first["tools"].to_string();
        assert!(!tools.contains("list_issues") && !tools.contains("\"remember\""), "{tools}");
        assert!(tools.contains("\"bash\""));
        let prompt = first["messages"][0]["content"].as_str().unwrap();
        assert!(prompt.contains("not tracked") && prompt.contains("plain words"), "{prompt}");
        solo.command(&mut app, Cmd::Send("more".into()));
        settle(&mut solo, &mut app, &mut rx).await;
        assert_eq!(shown_text(&app), "hi|hello there|more|again");
        let second = asked.recv().unwrap();
        assert_eq!(second["messages"].as_array().unwrap().len(), 4, "the chat is held in memory");
        assert_eq!(std::fs::read_dir(data.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn saving_writes_a_plain_transcript_only_when_asked() {
        let data = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let (url, _asked) = stub(vec![vec![say("noted")]]);
        let dir = data.path().join("chats");
        let (mut solo, mut rx, mut app) =
            solo(&url, work.path(), Scope::standalone(), Some(dir.join("one.md")));
        assert!(solo.saved().is_none());
        solo.command(&mut app, Cmd::Send("remember this".into()));
        settle(&mut solo, &mut app, &mut rx).await;
        let saved = solo.saved().expect("the transcript is written");
        let text = std::fs::read_to_string(saved).unwrap();
        assert!(text.contains("you: remember this") && text.contains("agent: noted"), "{text}");
    }
}
