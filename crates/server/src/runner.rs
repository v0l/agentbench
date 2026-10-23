use crate::agent::{
    Agent, Api, Attached, CancellationToken, Context, Event, Io, Steer, Turn, async_trait,
};
use crate::store::{Store, StoreError};
use crate::titles::Titles;
use foreman_core::{Attachment, Entry, Id, Question, Session, SessionId, SessionStatus, Upload};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, oneshot};

const TEXT_FLUSH: Duration = Duration::from_millis(250);
const ANSWER_PATIENCE: Duration = Duration::from_secs(24 * 3600);

/// Questions put to the user, each waiting on the channel its answers arrive on.
type Waiting = Arc<Mutex<HashMap<String, oneshot::Sender<Vec<String>>>>>;

#[derive(Clone)]
struct Live {
    stop: CancellationToken,
    inbox: Arc<Mutex<Vec<Message>>>,
    questions: Waiting,
}

/// What the user sent: the words, and the files kept with them.
#[derive(Clone, Debug, Default)]
struct Message {
    text: String,
    attachments: Vec<Attachment>,
    /// The transcript row showing this message while it waits for the agent to read it.
    queued: Option<Id>,
}

impl Message {
    fn entry(&self) -> Entry {
        Entry::User {
            text: self.text.clone(),
            attachments: self.attachments.clone(),
            queued: false,
        }
    }

    /// Show the waiting row as sent, now the agent has it.
    fn delivered(&self, store: &Store) {
        if let Some(id) = self.queued {
            let _ = store.update_message(id, &self.entry());
        }
    }
}

#[derive(Clone)]
pub struct Runner {
    store: Store,
    agents: Arc<HashMap<String, Arc<dyn Agent>>>,
    live: Arc<Mutex<HashMap<SessionId, Live>>>,
    api: Arc<dyn Api>,
    titles: Option<Titles>,
    pub base: Arc<Mutex<Base>>,
    pub changed: Arc<Notify>,
}

#[derive(Clone, Debug, Default)]
pub struct Base {
    pub url: String,
    pub token: String,
}

impl Runner {
    pub fn new(store: Store, agents: Vec<Box<dyn Agent>>) -> Runner {
        let changed = Arc::new(Notify::new());
        let api = Arc::new(crate::tools::StoreApi::notifying(store.clone(), changed.clone()));
        let agents = agents.into_iter().map(|a| (a.name().to_string(), Arc::from(a))).collect();
        Runner {
            store,
            agents: Arc::new(agents),
            live: Arc::new(Mutex::new(HashMap::new())),
            api,
            titles: None,
            base: Arc::new(Mutex::new(Base::default())),
            changed,
        }
    }

    /// Name chats with a small model once the agent has answered.
    pub fn with_titles(mut self, titles: Titles) -> Runner {
        self.titles = Some(titles);
        self
    }

    pub fn titles(&self) -> Option<&Titles> {
        self.titles.as_ref()
    }

    /// Replace a stand-in title with one the title model writes from the
    /// conversation so far. Runs in the background; a failure leaves the
    /// stand-in in place and is tried again after the next turn.
    fn retitle(&self, session: SessionId, force: bool) {
        let Some(titles) = self.titles.clone() else { return };
        if titles.target().is_none()
            || !(force || self.store.title_is_auto(session).unwrap_or(false))
        {
            return;
        }
        let (store, changed) = (self.store.clone(), self.changed.clone());
        tokio::spawn(async move {
            let entries: Vec<Entry> = store
                .messages(session, 0)
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.entry)
                .collect();
            match titles.title(&entries).await {
                Ok(title) => {
                    if matches!(store.set_title(session, &title, force), Ok(true)) {
                        changed.notify_waiters();
                    }
                }
                Err(e) => tracing::debug!("no title for session {session}: {e}"),
            }
        });
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn agent(&self, name: &str) -> Option<Arc<dyn Agent>> {
        self.agents.get(name).cloned()
    }

    pub fn agents(&self) -> Vec<Arc<dyn Agent>> {
        let mut all: Vec<_> = self.agents.values().cloned().collect();
        all.sort_by(|a, b| a.name().cmp(b.name()));
        all
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_running(&self, session: SessionId) -> bool {
        self.live().contains_key(&session)
    }

    pub fn interrupt(&self, session: SessionId) -> bool {
        let live = self.live();
        let Some(l) = live.get(&session) else {
            return false;
        };
        l.stop.cancel();
        l.questions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let dropped: Vec<Message> =
            l.inbox.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        drop(live);
        for m in dropped {
            if let Some(id) = m.queued {
                let text = format!("not sent, the agent was stopped first: {}", m.text);
                let _ = self.store.update_message(id, &Entry::Notice { text });
            }
        }
        true
    }

    pub fn answer(
        &self,
        session: SessionId,
        question: &str,
        answers: Vec<String>,
    ) -> Result<(), StoreError> {
        let live = self.live();
        let tx = live
            .get(&session)
            .and_then(|l| l.questions.lock().unwrap_or_else(|e| e.into_inner()).remove(question))
            .ok_or_else(|| StoreError("that question is no longer waiting".into()))?;
        tx.send(answers).map_err(|_| StoreError("the agent stopped waiting".into()))
    }

    pub fn send(&self, session: &Session, text: &str) -> Result<(), StoreError> {
        self.send_with(session, text, &[])
    }

    /// Send a message with files attached. The files are kept with the session.
    pub fn send_with(
        &self,
        session: &Session,
        text: &str,
        uploads: &[Upload],
    ) -> Result<(), StoreError> {
        let text = text.trim();
        if uploads.is_empty()
            && let Some((cmd, arg)) = foreman_core::parse_slash(text)
        {
            return self.command(session, cmd.name, arg);
        }
        if uploads.len() > foreman_core::ATTACHMENTS_MAX {
            return Err(StoreError(format!(
                "at most {} files go with one message",
                foreman_core::ATTACHMENTS_MAX
            )));
        }
        let decoded = uploads
            .iter()
            .map(|u| {
                use base64::Engine;
                let data = base64::engine::general_purpose::STANDARD
                    .decode(u.data.trim())
                    .map_err(|e| StoreError(format!("{} is not base64: {e}", u.name)))?;
                if data.len() > foreman_core::ATTACHMENT_MAX {
                    return Err(StoreError(format!(
                        "{} is over {} MB",
                        u.name,
                        foreman_core::ATTACHMENT_MAX >> 20
                    )));
                }
                Ok((u, data))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if text.is_empty() && decoded.is_empty() {
            return Err(StoreError("nothing to send".into()));
        }
        let mut attachments = Vec::new();
        for (u, data) in decoded {
            let name = file_name(&u.name);
            let mime = Some(u.mime.trim())
                .filter(|m| !m.is_empty())
                .or_else(|| foreman_core::mime_for(&name))
                .unwrap_or("application/octet-stream");
            attachments.push(self.store.add_attachment(session.id, &name, mime, &data)?);
        }
        self.send_text(
            session,
            Message { text: text.to_string(), attachments, queued: None },
            Mode::Chat,
        )
    }

    /// The files of a message, written to a folder of this session's so an agent can read them.
    fn materialise(&self, session: SessionId, attachments: &[Attachment]) -> Vec<Attached> {
        let dir = std::env::temp_dir().join("foreman").join(session.to_string());
        let _ = std::fs::create_dir_all(&dir);
        attachments
            .iter()
            .filter_map(|a| {
                let path = dir.join(a.file_name());
                if !path.is_file() {
                    let (_, data) = self.store.attachment(a.id).ok()?;
                    std::fs::write(&path, data).ok()?;
                }
                Some(Attached { name: a.name.clone(), mime: a.mime.clone(), path })
            })
            .collect()
    }

    fn command(&self, session: &Session, name: &str, arg: &str) -> Result<(), StoreError> {
        let note = |text: String| -> Result<(), StoreError> {
            self.store.add_message(session.id, &Entry::Notice { text })?;
            self.changed.notify_waiters();
            Ok(())
        };
        match name {
            "stop" => {
                if !self.interrupt(session.id) {
                    note("nothing to stop".into())?;
                }
                Ok(())
            }
            "clear" => {
                if self.is_running(session.id) {
                    return Err(StoreError("stop the agent before clearing".into()));
                }
                self.store.add_message(session.id, &Entry::Summary { text: String::new() })?;
                self.store.clear_agent_session(session.id)?;
                note("the agent starts afresh from here; the transcript above is kept".into())
            }
            "model" => {
                let model = Some(arg).filter(|m| !m.is_empty());
                self.store.set_model(session.id, model)?;
                note(match model {
                    Some(m) => format!("this chat now uses {m}"),
                    None => "this chat now uses the agent's default model".into(),
                })
            }
            "title" => {
                if !arg.is_empty() {
                    self.store.set_title(session.id, &foreman_core::short_title(arg, 80), true)?;
                    self.changed.notify_waiters();
                    return Ok(());
                }
                if self.titles.as_ref().and_then(|t| t.target()).is_none() {
                    return Err(StoreError(
                        "no title model: set one under [title_model] in foreman.toml, or give a title: /title my title".into(),
                    ));
                }
                self.retitle(session.id, true);
                note("asking the title model for a new title".into())
            }
            "compact" => {
                if self.is_running(session.id) {
                    return Err(StoreError(
                        "stop the agent or let it finish before compacting".into(),
                    ));
                }
                let prompt =
                    Message { text: COMPACT_PROMPT.into(), attachments: Vec::new(), queued: None };
                self.send_text(session, prompt, Mode::Compact)
            }
            other => Err(StoreError(format!("/{other} is handled by the window"))),
        }
    }

    fn send_text(&self, session: &Session, msg: Message, mode: Mode) -> Result<(), StoreError> {
        let agent = self
            .agent(&session.agent)
            .ok_or_else(|| StoreError(format!("no agent named {}", session.agent)))?;
        if msg.text.is_empty() && msg.attachments.is_empty() {
            return Err(StoreError("nothing to send".into()));
        }
        {
            let live = self.live();
            if let Some(l) = live.get(&session.id) {
                // Show it at once. A steerable agent only reads the inbox between steps, which
                // can be a while, and the composer has already cleared by then. It shows as
                // queued until the agent takes it.
                let mut msg = msg;
                let shown = if agent.steerable() {
                    Entry::User {
                        text: msg.text.clone(),
                        attachments: msg.attachments.clone(),
                        queued: true,
                    }
                } else {
                    Entry::Notice { text: format!("queued for when this turn ends: {}", msg.text) }
                };
                let row = self.store.add_message(session.id, &shown)?;
                msg.queued = agent.steerable().then_some(row.id);
                l.inbox.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
                drop(live);
                self.changed.notify_waiters();
                return Ok(());
            }
        }
        let live = Live {
            stop: CancellationToken::new(),
            inbox: Arc::new(Mutex::new(Vec::new())),
            questions: Arc::new(Mutex::new(HashMap::new())),
        };
        self.live().insert(session.id, live.clone());
        let runner = self.clone();
        let session_id = session.id;
        let first = msg;
        tokio::spawn(async move {
            let Live { stop, inbox, questions } = live;
            let mut next = Some(first);
            // The first prompt is not on screen yet. A steer that missed its step and runs as
            // the next turn was shown when it was sent, if the agent is steerable.
            let mut shown = false;
            while let Some(prompt) = next.take() {
                if stop.is_cancelled() {
                    break;
                }
                let session = match runner.store.session(session_id) {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let status = runner
                    .turn(agent.as_ref(), &session, &prompt, shown, &stop, &inbox, &questions, mode)
                    .await;
                let _ = runner.store.set_status(session_id, status);
                runner.changed.notify_waiters();
                if status == SessionStatus::Idle && mode == Mode::Chat {
                    runner.retitle(session_id, false);
                }
                if status != SessionStatus::Idle || mode == Mode::Compact {
                    break;
                }
                if runner.full(agent.as_ref(), session_id) {
                    let status = runner
                        .auto_compact(agent.as_ref(), session_id, &stop, &inbox, &questions)
                        .await;
                    let _ = runner.store.set_status(session_id, status);
                    runner.changed.notify_waiters();
                    if status != SessionStatus::Idle {
                        break;
                    }
                }
                let mut queued = inbox.lock().unwrap_or_else(|e| e.into_inner());
                if !queued.is_empty() {
                    let all: Vec<Message> = queued.drain(..).collect();
                    all.iter().for_each(|m| m.delivered(&runner.store));
                    next = Some(Message {
                        queued: None,
                        text: all.iter().map(|m| m.text.as_str()).collect::<Vec<_>>().join("\n\n"),
                        attachments: all.into_iter().flat_map(|m| m.attachments).collect(),
                    });
                    shown = agent.steerable();
                }
            }
            // A turn that failed or was stopped never reads what was waiting.
            let left: Vec<Message> =
                inbox.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
            for m in left {
                if let Some(id) = m.queued {
                    let text = format!("not sent, the turn ended first: {}", m.text);
                    let _ = runner.store.update_message(id, &Entry::Notice { text });
                }
            }
            runner.live().remove(&session_id);
            runner.changed.notify_waiters();
        });
        Ok(())
    }

    /// Whether the chat has outgrown the agent's context enough to summarise it now.
    fn full(&self, agent: &dyn Agent, id: SessionId) -> bool {
        self.store.session(id).is_ok_and(|s| s.usage.past(agent.compact_at()))
    }

    /// Summarise a chat that has filled up, so the next turn starts from the summary.
    async fn auto_compact(
        &self,
        agent: &dyn Agent,
        id: SessionId,
        stop: &CancellationToken,
        inbox: &Arc<Mutex<Vec<Message>>>,
        questions: &Waiting,
    ) -> SessionStatus {
        let Ok(session) = self.store.session(id) else { return SessionStatus::Failed };
        let used = session.usage.fill().unwrap_or_default();
        let _ = self.store.add_message(
            id,
            &Entry::Notice {
                text: format!("the context is {:.0}% full, so it is being compacted", used * 100.0),
            },
        );
        let prompt = Message { text: COMPACT_PROMPT.into(), attachments: Vec::new(), queued: None };
        self.turn(agent, &session, &prompt, false, stop, inbox, questions, Mode::Compact).await
    }

    fn context(&self, session: &Session) -> Context {
        let project = self.store.project(session.project_id).ok();
        let branch = self.store.worktree(session.worktree_id).ok().and_then(|w| w.branch);
        let base = self.base.lock().unwrap_or_else(|e| e.into_inner()).clone();
        Context {
            session: session.id,
            project_id: session.project_id,
            project: project.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
            project_path: project.map(|p| p.path).unwrap_or_default(),
            branch,
            issue: session.issue_id,
            api_url: base.url,
            token: base.token,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn turn(
        &self,
        agent: &dyn Agent,
        session: &Session,
        msg: &Message,
        shown: bool,
        stop: &CancellationToken,
        inbox: &Arc<Mutex<Vec<Message>>>,
        questions: &Waiting,
        mode: Mode,
    ) -> SessionStatus {
        let history: Vec<Entry> = self
            .store
            .messages(session.id, 0)
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.entry)
            .collect();
        let entry = match mode {
            Mode::Chat => msg.entry(),
            Mode::Compact => Entry::Notice { text: "compacting the conversation".into() },
        };
        if !shown && self.store.add_message(session.id, &entry).is_err() {
            return SessionStatus::Failed;
        }
        let summary = history.iter().rev().find_map(|e| match e {
            Entry::Summary { text } => Some(text.clone()),
            _ => None,
        });
        let fresh_after_summary =
            session.agent_session.is_none() && summary.as_ref().is_some_and(|s| !s.is_empty());
        let prompt = msg.text.as_str();
        let prompt_text;
        let prompt = if fresh_after_summary && !agent.steerable() {
            prompt_text = format!(
                "Here is a summary of our conversation so far. Carry on from it.\n\n{}\n\n---\n\n{prompt}",
                summary.unwrap_or_default()
            );
            prompt_text.as_str()
        } else {
            prompt
        };
        let files = self.materialise(session.id, &msg.attachments);
        let _ = self.store.set_status(session.id, SessionStatus::Running);
        self.changed.notify_waiters();
        let cwd = PathBuf::from(&session.cwd);
        let context = self.context(session);
        let resume = session.agent_session.as_deref().filter(|_| agent.resumable());
        let turn = Turn {
            prompt,
            files: &files,
            cwd: &cwd,
            model: session.model.as_deref(),
            resume,
            history: &history,
            context: &context,
        };
        let sink = Sink {
            store: self.store.clone(),
            session: session.id,
            changed: self.changed.clone(),
            state: Mutex::new(SinkState { capture: mode == Mode::Compact, ..SinkState::default() }),
            stop: stop.clone(),
            inbox: inbox.clone(),
            questions: questions.clone(),
            steerable: agent.steerable(),
            api: self.api.clone(),
            runner: self.clone(),
        };
        let result = agent.run(&turn, &sink).await;
        sink.flush();
        let errored = sink.state.lock().unwrap_or_else(|e| e.into_inner()).errored;
        if stop.is_cancelled() {
            sink.entry(Entry::Notice { text: "stopped".into() });
            return SessionStatus::Stopped;
        }
        if mode == Mode::Compact && result.is_ok() && !errored {
            let text = std::mem::take(&mut sink.lock().captured);
            if text.trim().is_empty() {
                sink.entry(Entry::Error { text: "the agent gave no summary".into() });
                return SessionStatus::Failed;
            }
            let _ = self
                .store
                .add_message(session.id, &Entry::Summary { text: text.trim().to_string() });
            let _ = self.store.clear_agent_session(session.id);
            self.changed.notify_waiters();
            return SessionStatus::Idle;
        }
        match result {
            Ok(()) if !errored => SessionStatus::Idle,
            Ok(()) => SessionStatus::Failed,
            Err(e) => {
                if !e.is_empty() {
                    sink.entry(Entry::Error { text: e });
                }
                SessionStatus::Failed
            }
        }
    }
}

/// A name safe to write to disk and show: the last part of what was sent, never empty.
fn file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    let clean: String =
        base.chars().filter(|c| !c.is_control()).collect::<String>().chars().take(120).collect();
    if clean.is_empty() || clean == "." || clean == ".." { "file".to_string() } else { clean }
}

const COMPACT_PROMPT: &str = "Summarise this conversation so another agent can carry on from it without the transcript. \
Keep: what the user asked for, decisions made and why, files changed, what is done, what is left, and anything you were about to do. \
Be specific with file paths and names. Reply with the summary only, as markdown, no preamble.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Chat,
    Compact,
}

#[derive(Default)]
struct SinkState {
    capture: bool,
    captured: String,
    text: Option<(Id, String)>,
    dirty: Option<Instant>,
    thinking: Option<(Id, String)>,
    tools: HashMap<String, (Id, serde_json::Value)>,
    errored: bool,
}

struct Sink {
    store: Store,
    session: SessionId,
    changed: Arc<Notify>,
    state: Mutex<SinkState>,
    stop: CancellationToken,
    inbox: Arc<Mutex<Vec<Message>>>,
    questions: Waiting,
    steerable: bool,
    runner: Runner,
    api: Arc<dyn Api>,
}

impl Sink {
    fn lock(&self) -> std::sync::MutexGuard<'_, SinkState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn entry(&self, entry: Entry) -> Option<Id> {
        self.flush();
        let mut st = self.lock();
        st.text = None;
        st.thinking = None;
        if matches!(entry, Entry::Error { .. }) {
            st.errored = true;
        }
        drop(st);
        let id = self.store.add_message(self.session, &entry).ok().map(|m| m.id);
        self.changed.notify_waiters();
        id
    }

    fn flush(&self) {
        let mut st = self.lock();
        if st.dirty.take().is_none() {
            return;
        }
        if let Some((id, text)) = &st.text {
            let _ = self.store.update_message(*id, &Entry::Assistant { text: text.clone() });
        }
        if let Some((id, text)) = &st.thinking {
            let _ = self.store.update_message(*id, &Entry::Thinking { text: text.clone() });
        }
        drop(st);
        self.changed.notify_waiters();
    }

    fn append(&self, delta: &str, thinking: bool) {
        let mut st = self.lock();
        if st.capture {
            if !thinking {
                st.captured.push_str(delta);
            }
            return;
        }
        let slot = if thinking { &mut st.thinking } else { &mut st.text };
        if let Some((_, text)) = slot {
            text.push_str(delta);
            let due = st.dirty.get_or_insert_with(Instant::now).elapsed() >= TEXT_FLUSH;
            drop(st);
            if due {
                self.flush();
            }
            return;
        }
        let delta = delta.trim_start().to_string();
        if delta.is_empty() {
            return;
        }
        drop(st);
        let entry = if thinking {
            Entry::Thinking { text: delta.clone() }
        } else {
            Entry::Assistant { text: delta.clone() }
        };
        self.flush();
        let mut st = self.lock();
        if thinking {
            st.text = None;
        } else {
            st.thinking = None;
        }
        drop(st);
        let id = self.store.add_message(self.session, &entry).ok().map(|m| m.id);
        self.changed.notify_waiters();
        let mut st = self.lock();
        let slot = if thinking { &mut st.thinking } else { &mut st.text };
        *slot = id.map(|id| (id, delta));
    }
}

impl Sink {
    async fn wait(
        &self,
        id: &str,
        open: Entry,
        answered: impl FnOnce(&[String]) -> Entry,
    ) -> Result<Vec<String>, String> {
        let (tx, rx) = oneshot::channel();
        self.questions.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_string(), tx);
        let msg = self.entry(open);
        let _ = self.store.set_status(self.session, SessionStatus::Waiting);
        self.changed.notify_waiters();
        let answer = tokio::select! {
            _ = self.stop.cancelled() => Err("stopped".to_string()),
            _ = tokio::time::sleep(ANSWER_PATIENCE) => Err("nobody answered in a day".to_string()),
            a = rx => a.map_err(|_| "the question was withdrawn".to_string()),
        };
        self.questions.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        if let (Some(m), Ok(a)) = (msg, &answer) {
            let _ = self.store.update_message(m, &answered(a));
        }
        let _ = self.store.set_status(self.session, SessionStatus::Running);
        self.changed.notify_waiters();
        answer
    }
}

#[async_trait]
impl Io for Sink {
    fn emit(&self, event: Event) {
        match event {
            Event::Started { agent_session } => {
                let _ = self.store.set_agent_session(self.session, &agent_session);
            }
            Event::Text { delta } => self.append(&delta, false),
            Event::Thinking { text } => self.append(&text, true),
            Event::ToolStart { call_id, name, input } => {
                let entry = Entry::Tool {
                    call_id: call_id.clone(),
                    name,
                    input: input.clone(),
                    output: None,
                    error: false,
                };
                if let Some(id) = self.entry(entry) {
                    self.lock().tools.insert(call_id, (id, input));
                }
            }
            Event::ToolEnd { call_id, name, output, error } => {
                let known = self.lock().tools.remove(&call_id);
                match known {
                    Some((id, input)) => {
                        let entry =
                            Entry::Tool { call_id, name, input, output: Some(output), error };
                        let _ = self.store.update_message(id, &entry);
                        self.changed.notify_waiters();
                    }
                    None => {
                        self.entry(Entry::Tool {
                            call_id,
                            name,
                            input: serde_json::Value::Null,
                            output: Some(output),
                            error,
                        });
                    }
                }
            }
            // The message was already shown when it was sent, see send_text.
            Event::Steered { .. } => {}
            Event::Usage(u) => {
                let _ = self.store.add_usage(self.session, &u);
            }
            Event::Notice(text) => {
                self.entry(Entry::Notice { text });
            }
            Event::Error(text) => {
                self.entry(Entry::Error { text });
            }
            Event::Done => self.flush(),
        }
    }

    fn stop(&self) -> &CancellationToken {
        &self.stop
    }

    fn steer(&self) -> Option<Steer> {
        if !self.steerable {
            return None;
        }
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        let msg = (!inbox.is_empty()).then(|| inbox.remove(0))?;
        drop(inbox);
        msg.delivered(&self.store);
        self.runner.changed.notify_waiters();
        let files = self.runner.materialise(self.session, &msg.attachments);
        Some(Steer { text: msg.text, files })
    }

    async fn ask(&self, question: Question) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let open = Entry::Question { id: id.clone(), question: question.clone(), answer: None };
        let answers = self
            .wait(&id, open, |a| Entry::Question {
                id: id.clone(),
                question: question.clone(),
                answer: Some(a.join(", ")),
            })
            .await?;
        Ok(answers.join(", "))
    }

    async fn ask_many(&self, questions: Vec<Question>) -> Result<Vec<String>, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let open = Entry::Questions { id: id.clone(), questions: questions.clone(), answers: None };
        let n = questions.len();
        let mut answers = self
            .wait(&id, open, |a| Entry::Questions {
                id: id.clone(),
                questions: questions.clone(),
                answers: Some(a.to_vec()),
            })
            .await?;
        answers.resize(n, String::new());
        Ok(answers)
    }

    fn api(&self) -> &dyn Api {
        self.api.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::JsonCommand;
    use foreman_core::Usage;

    fn script_agent(name: &str, script: &str) -> Box<dyn Agent> {
        Box::new(JsonCommand {
            name: name.into(),
            label: None,
            program: "sh".into(),
            args: vec!["-c".into(), script.into(), "sh".into()],
            resume_args: vec!["{session}".into()],
            model_args: vec![],
            prompt_on_stdin: true,
            session: Some("/session".into()),
            text: Some("/text".into()),
            error: Some("/error".into()),
            input_tokens: Some("/in".into()),
            output_tokens: None,
            plain: false,
        })
    }

    async fn wait_idle(runner: &Runner, id: SessionId) -> Session {
        for _ in 0..300 {
            if !runner.is_running(id) {
                tokio::time::sleep(Duration::from_millis(20)).await;
                return runner.store().session(id).unwrap();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("session {id} never finished");
    }

    fn setup(agent: Box<dyn Agent>) -> (Runner, Session) {
        let store = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&store, "/tmp");
        let s = store.add_session(main.id, agent.name(), "t", None, None).unwrap();
        (Runner::new(store, vec![agent]), s)
    }

    #[tokio::test]
    async fn a_turn_streams_into_the_store() {
        let script = r#"read p; echo '{"session":"abc"}'; echo "{\"text\":\"got $p in $FOREMAN_SESSION\"}"; echo '{"in":7}'"#;
        let (runner, s) = setup(script_agent("echo", script));
        runner.send(&s, "hello").unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Idle);
        assert_eq!(done.agent_session.as_deref(), Some("abc"));
        assert_eq!(done.usage.input_tokens, 7);
        let msgs = runner.store().messages(s.id, 0).unwrap();
        assert_eq!(msgs[0].entry, Entry::user("hello"));
        assert_eq!(msgs[1].entry, Entry::Assistant { text: format!("got hello in {}", s.id) });
    }

    #[tokio::test]
    async fn the_next_turn_resumes_the_agent_session() {
        let script = r#"echo "{\"session\":\"s1\",\"text\":\"args:$*\"}""#;
        let (runner, s) = setup(script_agent("echo", script));
        runner.send(&s, "one").unwrap();
        wait_idle(&runner, s.id).await;
        let s = runner.store().session(s.id).unwrap();
        runner.send(&s, "two").unwrap();
        wait_idle(&runner, s.id).await;
        let msgs = runner.store().messages(s.id, 0).unwrap();
        assert_eq!(msgs.last().unwrap().entry.text(), "args:s1");
    }

    #[tokio::test]
    async fn a_message_sent_while_working_runs_next() {
        let script = r#"read p; sleep 0.4; echo "{\"text\":\"did $p\"}""#;
        let (runner, s) = setup(script_agent("slow", script));
        runner.send(&s, "first").unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        runner.send(&s, "second").unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Idle);
        let said: Vec<String> = runner
            .store()
            .messages(s.id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::Assistant { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(said, ["did first", "did second"]);
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_stderr() {
        let (runner, s) = setup(script_agent("bad", "echo 'no api key' >&2; exit 3"));
        runner.send(&s, "x").unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Failed);
        let last = runner.store().messages(s.id, 0).unwrap().pop().unwrap();
        assert!(
            matches!(&last.entry, Entry::Error { text } if text.contains("exit 3") && text.contains("no api key"))
        );
    }

    #[tokio::test]
    async fn an_agent_error_fails_the_session() {
        let (runner, s) = setup(script_agent("e", r#"echo '{"error":"quota"}'"#));
        runner.send(&s, "x").unwrap();
        assert_eq!(wait_idle(&runner, s.id).await.status, SessionStatus::Failed);
    }

    #[tokio::test]
    async fn interrupt_kills_the_whole_process_group() {
        let marker = format!("/tmp/foreman-test-{}", uuid::Uuid::new_v4());
        let script =
            format!(r#"echo '{{"text":"working"}}'; (sleep 30; touch {marker}) & sleep 30"#);
        let (runner, s) = setup(script_agent("slow", &script));
        runner.send(&s, "x").unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(runner.interrupt(s.id));
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Stopped);
        let msgs = runner.store().messages(s.id, 0).unwrap();
        assert_eq!(msgs.last().unwrap().entry, Entry::Notice { text: "stopped".into() });
        let out = std::process::Command::new("pgrep").args(["-f", &marker]).output().unwrap();
        assert!(out.stdout.is_empty(), "the backgrounded child survived");
    }

    struct Steerable;

    #[async_trait]

    impl Agent for Steerable {
        fn name(&self) -> &str {
            "steerable"
        }
        fn label(&self) -> &str {
            "Steerable"
        }
        fn available(&self) -> bool {
            true
        }
        fn about(&self) -> String {
            "test".into()
        }
        fn resumable(&self) -> bool {
            false
        }
        fn steerable(&self) -> bool {
            true
        }
        async fn run(&self, _turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
            for _ in 0..200 {
                if let Some(steer) = io.steer() {
                    io.emit(Event::Steered { text: steer.text });
                    io.emit(Event::Done);
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err("never steered".into())
        }
    }

    #[tokio::test]
    async fn a_steer_shows_as_soon_as_it_is_sent_and_only_once() {
        let (runner, s) = setup(Box::new(Steerable));
        runner.send(&s, "go").unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        runner.send(&s, "left instead").unwrap();
        let users = |runner: &Runner| -> Vec<String> {
            runner
                .store()
                .messages(s.id, 0)
                .unwrap()
                .into_iter()
                .filter_map(|m| match m.entry {
                    Entry::User { text, .. } => Some(text),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(users(&runner), ["go", "left instead"]);
        let queued = |runner: &Runner| -> Vec<bool> {
            runner
                .store()
                .messages(s.id, 0)
                .unwrap()
                .into_iter()
                .filter_map(|m| match m.entry {
                    Entry::User { queued, .. } => Some(queued),
                    _ => None,
                })
                .collect()
        };
        wait_idle(&runner, s.id).await;
        assert_eq!(users(&runner), ["go", "left instead"]);
        assert_eq!(queued(&runner), [false, false], "once read it no longer shows as waiting");
    }

    struct Looker(Mutex<Vec<Steer>>);

    #[async_trait]
    impl Agent for Looker {
        fn name(&self) -> &str {
            "looker"
        }
        fn label(&self) -> &str {
            "Looker"
        }
        fn available(&self) -> bool {
            true
        }
        fn about(&self) -> String {
            "test".into()
        }
        fn resumable(&self) -> bool {
            false
        }
        fn steerable(&self) -> bool {
            true
        }
        async fn run(&self, turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .push(Steer { text: turn.prompt.into(), files: turn.files.to_vec() });
            for _ in 0..200 {
                if let Some(steer) = io.steer() {
                    self.0.lock().unwrap().push(steer);
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err("never steered".into())
        }
    }

    #[tokio::test]
    async fn a_file_sent_mid_turn_reaches_the_agent_with_its_steer() {
        use base64::Engine;
        let seen = Arc::new(Looker(Mutex::new(Vec::new())));
        struct Shared(Arc<Looker>);
        #[async_trait]
        impl Agent for Shared {
            fn name(&self) -> &str {
                "looker"
            }
            fn label(&self) -> &str {
                "Looker"
            }
            fn available(&self) -> bool {
                true
            }
            fn about(&self) -> String {
                "test".into()
            }
            fn resumable(&self) -> bool {
                false
            }
            fn steerable(&self) -> bool {
                true
            }
            async fn run(&self, t: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
                self.0.run(t, io).await
            }
        }
        let (runner, s) = setup(Box::new(Shared(seen.clone())));
        runner.send(&s, "go").unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let upload = Upload {
            name: "clip.wav".into(),
            mime: String::new(),
            data: base64::engine::general_purpose::STANDARD.encode(b"RIFF"),
        };
        runner.send_with(&s, "and this", &[upload]).unwrap();
        wait_idle(&runner, s.id).await;
        let got = seen.0.lock().unwrap().clone();
        assert_eq!(got[0].text, "go");
        assert!(got[0].files.is_empty());
        assert_eq!(got[1].text, "and this");
        assert_eq!(got[1].files[0].name, "clip.wav");
        assert_eq!(got[1].files[0].mime, "audio/wav", "the type is worked out from the name");
        assert_eq!(std::fs::read(&got[1].files[0].path).unwrap(), b"RIFF");
        let shown = runner.store().messages(s.id, 0).unwrap();
        assert!(matches!(
            &shown[1].entry,
            Entry::User { text, attachments, .. } if text == "and this" && attachments.len() == 1
        ));
    }

    struct Asker;

    #[async_trait]

    impl Agent for Asker {
        fn name(&self) -> &str {
            "asker"
        }
        fn label(&self) -> &str {
            "Asker"
        }
        fn available(&self) -> bool {
            true
        }
        fn about(&self) -> String {
            "test".into()
        }
        fn resumable(&self) -> bool {
            false
        }
        async fn run(&self, _turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
            let a = io
                .ask(Question {
                    text: "red or blue?".into(),
                    options: vec!["red".into(), "blue".into()],
                    ..Question::default()
                })
                .await?;
            io.emit(Event::Text { delta: format!("you picked {a}") });
            io.emit(Event::Done);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_question_waits_for_its_answer() {
        let (runner, s) = setup(Box::new(Asker));
        runner.send(&s, "pick").unwrap();
        let mut qid = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(Entry::Question { id, .. }) = runner
                .store()
                .messages(s.id, 0)
                .unwrap()
                .into_iter()
                .map(|m| m.entry)
                .find(|e| matches!(e, Entry::Question { .. }))
            {
                qid = Some(id);
                break;
            }
        }
        let qid = qid.expect("a question");
        assert_eq!(runner.store().session(s.id).unwrap().status, SessionStatus::Waiting);
        assert!(runner.answer(s.id, "nope", vec!["x".into()]).is_err());
        runner.answer(s.id, &qid, vec!["blue".into()]).unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Idle);
        let msgs: Vec<Entry> =
            runner.store().messages(s.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        assert!(
            msgs.iter()
                .any(|e| matches!(e, Entry::Question { answer: Some(a), .. } if a == "blue"))
        );
        assert_eq!(msgs.last().unwrap().text(), "you picked blue");
    }

    struct Pair;

    #[async_trait]

    impl Agent for Pair {
        fn name(&self) -> &str {
            "pair"
        }
        fn label(&self) -> &str {
            "Pair"
        }
        fn available(&self) -> bool {
            true
        }
        fn about(&self) -> String {
            "test".into()
        }
        fn resumable(&self) -> bool {
            false
        }
        async fn run(&self, _turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
            let q = |t: &str, multiple| Question {
                text: t.into(),
                options: vec!["a".into(), "b".into()],
                multiple,
            };
            let a = io.ask_many(vec![q("one?", false), q("two?", true)]).await?;
            io.emit(Event::Text { delta: a.join("+") });
            io.emit(Event::Done);
            Ok(())
        }
    }

    #[tokio::test]
    async fn several_questions_wait_for_all_their_answers() {
        let (runner, s) = setup(Box::new(Pair));
        runner.send(&s, "go").unwrap();
        let mut qid = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if let Some(Entry::Questions { id, .. }) = runner
                .store()
                .messages(s.id, 0)
                .unwrap()
                .into_iter()
                .map(|m| m.entry)
                .find(|e| matches!(e, Entry::Questions { .. }))
            {
                qid = Some(id);
                break;
            }
        }
        let qid = qid.expect("a set of questions");
        assert_eq!(runner.store().session(s.id).unwrap().status, SessionStatus::Waiting);
        runner.answer(s.id, &qid, vec!["a".into(), "a, b".into()]).unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Idle);
        let msgs: Vec<Entry> =
            runner.store().messages(s.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        assert!(msgs.iter().any(|e| matches!(
            e,
            Entry::Questions { answers: Some(a), .. } if a == &["a", "a, b"]
        )));
        assert_eq!(msgs.last().unwrap().text(), "a+a, b");
    }

    struct Echo(Mutex<Vec<(String, Option<String>, usize)>>);

    #[async_trait]

    impl Agent for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn label(&self) -> &str {
            "Echo"
        }
        fn available(&self) -> bool {
            true
        }
        fn about(&self) -> String {
            "test".into()
        }
        fn resumable(&self) -> bool {
            true
        }
        async fn run(&self, turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
            self.0.lock().unwrap().push((
                turn.prompt.to_string(),
                turn.model.map(str::to_string),
                foreman_core::since_summary(turn.history).len(),
            ));
            io.emit(Event::Started { agent_session: "s1".into() });
            let reply = if turn.prompt.starts_with("Summarise") { "We said hello." } else { "ok" };
            io.emit(Event::Text { delta: reply.into() });
            Ok(())
        }
    }

    /// Reports a nearly full context on every turn, and asks to be compacted at 80%.
    struct Heavy(u64);

    #[async_trait]
    impl Agent for Heavy {
        fn name(&self) -> &str {
            "heavy"
        }
        fn label(&self) -> &str {
            "Heavy"
        }
        fn available(&self) -> bool {
            true
        }
        fn about(&self) -> String {
            "test".into()
        }
        fn resumable(&self) -> bool {
            true
        }
        fn compact_at(&self) -> u64 {
            self.0
        }
        async fn run(&self, turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
            io.emit(Event::Started { agent_session: "s1".into() });
            let reply = if turn.prompt.starts_with("Summarise") { "A summary." } else { "ok" };
            io.emit(Event::Text { delta: reply.into() });
            io.emit(Event::Usage(Usage {
                context: 900,
                window: 1000,
                model: "big".into(),
                host: "gpu.lan".into(),
                ..Usage::default()
            }));
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_chat_that_fills_its_window_is_compacted_after_the_turn() {
        let (runner, s) = setup(Box::new(Heavy(80)));
        runner.send(&s, "hello").unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Idle);
        let msgs: Vec<Entry> =
            runner.store().messages(s.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        assert!(msgs.contains(&Entry::Summary { text: "A summary.".into() }), "{msgs:?}");
        assert!(msgs.iter().any(|e| matches!(e, Entry::Notice { text } if text.contains("90%"))));
        assert_eq!(done.agent_session, None, "the agent starts afresh from the summary");
        assert_eq!(done.usage.context, 0, "the new session holds nothing yet");
        assert_eq!((done.usage.model.as_str(), done.usage.host.as_str()), ("big", "gpu.lan"));
    }

    #[tokio::test]
    async fn a_chat_is_left_alone_when_auto_compact_is_off() {
        let (runner, s) = setup(Box::new(Heavy(0)));
        runner.send(&s, "hello").unwrap();
        let done = wait_idle(&runner, s.id).await;
        let msgs: Vec<Entry> =
            runner.store().messages(s.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        assert!(!msgs.iter().any(|e| matches!(e, Entry::Summary { .. })));
        assert_eq!(done.agent_session.as_deref(), Some("s1"));
        assert_eq!(done.usage.context, 900);
    }

    #[tokio::test]
    async fn slash_commands_compact_clear_and_pick_a_model() {
        let store = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&store, "/tmp");
        let s = store.add_session(main.id, "echo", "t", None, None).unwrap();
        let seen = std::sync::Arc::new(Echo(Mutex::new(Vec::new())));
        struct Shared(std::sync::Arc<Echo>);
        #[async_trait]
        impl Agent for Shared {
            fn name(&self) -> &str {
                self.0.name()
            }
            fn label(&self) -> &str {
                self.0.label()
            }
            fn available(&self) -> bool {
                true
            }
            fn about(&self) -> String {
                self.0.about()
            }
            fn resumable(&self) -> bool {
                true
            }
            async fn run(&self, t: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
                self.0.run(t, io).await
            }
        }
        let runner = Runner::new(store.clone(), vec![Box::new(Shared(seen.clone()))]);
        let fresh = |runner: &Runner| runner.store().session(s.id).unwrap();

        runner.send(&fresh(&runner), "hello").unwrap();
        wait_idle(&runner, s.id).await;
        assert_eq!(fresh(&runner).agent_session.as_deref(), Some("s1"));

        runner.send(&fresh(&runner), "/model qwen-big").unwrap();
        assert_eq!(fresh(&runner).model.as_deref(), Some("qwen-big"));
        runner.send(&fresh(&runner), "/model high").unwrap();
        assert_eq!(fresh(&runner).model.as_deref(), Some("high"));
        runner.send(&fresh(&runner), "/model qwen-big").unwrap();

        runner.send(&fresh(&runner), "/compact").unwrap();
        wait_idle(&runner, s.id).await;
        let msgs: Vec<Entry> =
            store.messages(s.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        assert!(msgs.contains(&Entry::Summary { text: "We said hello.".into() }), "{msgs:?}");
        assert!(
            !msgs
                .iter()
                .any(|e| matches!(e, Entry::User { text, .. } if text.starts_with("Summarise"))),
            "the compact prompt is not shown as yours"
        );
        assert!(
            !msgs
                .iter()
                .any(|e| matches!(e, Entry::Assistant { text } if text == "We said hello.")),
            "the summary is not shown twice"
        );
        assert_eq!(
            fresh(&runner).agent_session,
            None,
            "the agent starts a new session from the summary"
        );

        runner.send(&fresh(&runner), "next").unwrap();
        wait_idle(&runner, s.id).await;
        let calls = seen.0.lock().unwrap().clone();
        let last = calls.last().unwrap();
        assert!(
            last.0.contains("We said hello.") && last.0.ends_with("next"),
            "a resumable cli agent gets the summary in its prompt: {}",
            last.0
        );
        assert_eq!(last.1.as_deref(), Some("qwen-big"));

        runner.send(&fresh(&runner), "/clear").unwrap();
        let s2 = fresh(&runner);
        assert_eq!(s2.agent_session, None);
        runner.send(&s2, "after").unwrap();
        wait_idle(&runner, s.id).await;
        let calls = seen.0.lock().unwrap().clone();
        assert_eq!(calls.last().unwrap().0, "after", "clear forgets, with no summary to carry");
        assert_eq!(
            calls.last().unwrap().2,
            2,
            "the history starts at the clear: its marker and its notice"
        );

        assert!(runner.send(&fresh(&runner), "/stop").is_ok());
        let notes: Vec<String> = store
            .messages(s.id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::Notice { text } => Some(text),
                _ => None,
            })
            .collect();
        assert!(notes.iter().any(|n| n == "nothing to stop"));
    }

    fn titled(url: String, agent: Box<dyn Agent>, auto: bool) -> (Runner, Session) {
        use crate::agent::native::{Model, Shared};
        use crate::titles::{TitleModel, Titles};
        let (runner, s) = setup(agent);
        let main = Shared::new(Model::default());
        let config = TitleModel { url, model: "tiny".into(), ..TitleModel::default() };
        let runner = runner.with_titles(Titles::new(config, main));
        runner.store().set_title_auto(s.id, auto).unwrap();
        (runner, s)
    }

    async fn title_becomes(runner: &Runner, id: SessionId, want: &str) -> bool {
        for _ in 0..100 {
            if runner.store().session(id).unwrap().title == want {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    #[tokio::test]
    async fn a_stand_in_title_is_replaced_once_the_agent_has_answered() {
        let (url, seen) = crate::titles::fake::server(vec![(
            200,
            crate::titles::fake::completion("Say hello to the agent"),
        )]);
        let script = r#"read p; echo "{\"text\":\"hi\"}""#;
        let (runner, s) = titled(url, script_agent("echo", script), true);
        runner.send(&s, "hello there").unwrap();
        wait_idle(&runner, s.id).await;
        assert!(title_becomes(&runner, s.id, "Say hello to the agent").await);
        assert!(!runner.store().title_is_auto(s.id).unwrap(), "named once, not again");
        let sent = seen.lock().unwrap();
        let prompt = sent[0]["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.contains("Person: hello there") && prompt.contains("Agent: hi"), "{prompt}");
    }

    #[tokio::test]
    async fn a_title_someone_chose_is_left_alone() {
        let (url, seen) =
            crate::titles::fake::server(vec![(200, crate::titles::fake::completion("Not this"))]);
        let script = r#"read p; echo "{\"text\":\"hi\"}""#;
        let (runner, s) = titled(url, script_agent("echo", script), false);
        runner.send(&s, "hello").unwrap();
        wait_idle(&runner, s.id).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(runner.store().session(s.id).unwrap().title, "t");
        assert!(seen.lock().unwrap().is_empty(), "the model is not even asked");
    }

    #[tokio::test]
    async fn a_failed_title_leaves_the_stand_in_for_the_next_turn() {
        let (url, _) = crate::titles::fake::server(vec![
            (500, "down".into()),
            (200, crate::titles::fake::completion("Second try")),
        ]);
        let script = r#"read p; echo "{\"text\":\"hi\"}""#;
        let (runner, s) = titled(url, script_agent("echo", script), true);
        runner.send(&s, "one").unwrap();
        wait_idle(&runner, s.id).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(runner.store().session(s.id).unwrap().title, "t");
        assert!(runner.store().title_is_auto(s.id).unwrap());
        runner.send(&runner.store().session(s.id).unwrap(), "two").unwrap();
        wait_idle(&runner, s.id).await;
        assert!(title_becomes(&runner, s.id, "Second try").await);
    }

    #[tokio::test]
    async fn the_title_command_renames_or_asks_the_model() {
        let (url, _) = crate::titles::fake::server(vec![(
            200,
            crate::titles::fake::completion("From the model"),
        )]);
        let script = r#"read p; echo "{\"text\":\"hi\"}""#;
        let (runner, s) = titled(url, script_agent("echo", script), false);
        runner.send(&s, "/title My own name").unwrap();
        assert_eq!(runner.store().session(s.id).unwrap().title, "My own name");
        runner.send(&s, "hello").unwrap();
        wait_idle(&runner, s.id).await;
        runner.send(&runner.store().session(s.id).unwrap(), "/title").unwrap();
        assert!(title_becomes(&runner, s.id, "From the model").await, "a bare /title asks again");

        let (bare, s2) = setup(script_agent("echo", script));
        let err = bare.send(&s2, "/title").unwrap_err();
        assert!(err.0.contains("no title model"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_program_fails_cleanly() {
        let agent = JsonCommand {
            name: "ghost".into(),
            label: None,
            program: "/nonexistent/agent".into(),
            args: vec![],
            resume_args: vec![],
            model_args: vec![],
            prompt_on_stdin: true,
            session: None,
            text: None,
            error: None,
            input_tokens: None,
            output_tokens: None,
            plain: true,
        };
        let (runner, s) = setup(Box::new(agent));
        runner.send(&s, "x").unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Failed);
        let last = runner.store().messages(s.id, 0).unwrap().pop().unwrap();
        assert!(matches!(&last.entry, Entry::Error { text } if text.contains("could not start")));
    }
}
