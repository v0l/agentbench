use crate::agent::{
    Agent, Api, Attached, CancellationToken, Context, Event, Io, Steer, Turn, async_trait,
};
use crate::store::{Store, StoreError};
use crate::titles::Titles;
use agentbench_core::{
    Attachment, Entry, Id, NewSession, Place, Question, Session, SessionId, SessionStatus, Upload,
    branch_slug, issue_prompt,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, oneshot};

const TEXT_FLUSH: Duration = Duration::from_millis(250);
const TITLE_MAX: usize = 40;
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
}

impl Message {
    fn entry(&self) -> Entry {
        Entry::User { text: self.text.clone(), attachments: self.attachments.clone() }
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
    pub terminals: crate::terminal::Terminals,
    /// This runner, as the tracker tools see it, for starting and messaging sessions.
    me: crate::tools::Handle,
}

#[derive(Clone, Debug, Default)]
pub struct Base {
    pub url: String,
    pub token: String,
}

impl Runner {
    pub fn new(store: Store, agents: Vec<Box<dyn Agent>>) -> Runner {
        let changed = Arc::new(Notify::new());
        let me = crate::tools::Handle::default();
        let api =
            Arc::new(crate::tools::StoreApi::notifying(store.clone(), changed.clone(), me.clone()));
        let agents = agents.into_iter().map(|a| (a.name().to_string(), Arc::from(a))).collect();
        let runner = Runner {
            store,
            agents: Arc::new(agents),
            live: Arc::new(Mutex::new(HashMap::new())),
            api,
            titles: None,
            base: Arc::new(Mutex::new(Base::default())),
            changed,
            terminals: Default::default(),
            me,
        };
        runner.me.set(runner.clone());
        runner
    }

    /// Name chats with a small model once the agent has answered.
    pub fn with_titles(mut self, titles: Titles) -> Runner {
        self.titles = Some(titles);
        self.me.set(self.clone());
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
            let entries: Vec<Entry> =
                agentbench_core::shown(&store.messages(session, 0).unwrap_or_default())
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

    /// Make a session, and a worktree for it when asked, and send it its first prompt. A prompt
    /// is taken from the issue when none is given. Must run inside the tokio runtime, because the
    /// first turn is spawned.
    pub fn start_session(&self, new: NewSession) -> Result<Session, StoreError> {
        let bad = |t: String| StoreError(t);
        let store = &self.store;
        let agent =
            self.agent(&new.agent).ok_or_else(|| bad(format!("no agent named {}", new.agent)))?;
        if !agent.available() {
            return Err(bad(format!("{} is not ready: {}", agent.label(), agent.about())));
        }
        let project = store.project(new.project_id)?;
        let issue = new.issue_id.map(|id| store.issue(id)).transpose()?;
        if let Some(i) = issue.as_ref().filter(|i| i.project_id != project.id) {
            return Err(bad(format!("issue #{} belongs to another project", i.id)));
        }
        let worktree = match new.place.clone() {
            Place::Main => store.main_worktree(project.id)?,
            Place::Existing { worktree_id } => {
                let w = store.worktree(worktree_id)?;
                if w.project_id != project.id {
                    return Err(bad("that worktree belongs to another project".into()));
                }
                w
            }
            Place::New { .. } if project.home => {
                return Err(
                    bad("the home folder is not a git repo, so it has no worktrees".into()),
                );
            }
            Place::New { branch } => {
                let branch = match branch.trim() {
                    "" => issue
                        .as_ref()
                        .map(|i| format!("issue-{}-{}", i.id, branch_slug(&i.title)))
                        .ok_or_else(|| bad("name the branch for the new worktree".into()))?,
                    b => b.to_string(),
                };
                crate::projects::add_worktree(store, project.id, &branch, None)?
            }
        };
        if worktree.missing || !std::path::Path::new(&worktree.path).is_dir() {
            return Err(bad(format!("{} no longer exists", worktree.path)));
        }
        let chosen = new
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| issue.as_ref().map(|i| format!("#{} {}", i.id, i.title)));
        let stand_in = chosen.is_none();
        let title = chosen
            .or_else(|| new.prompt.as_deref().map(|p| agentbench_core::short_title(p, TITLE_MAX)))
            .unwrap_or_else(|| {
                if project.home {
                    format!("{} in ~", agent.label())
                } else {
                    format!("{} on {}", agent.label(), worktree.label())
                }
            });
        let model = new.model.as_deref().map(str::trim).filter(|m| !m.is_empty());
        let session =
            store.add_session(worktree.id, agent.name(), title.trim(), model, new.issue_id)?;
        if stand_in {
            store.set_title_auto(session.id, true)?;
        }
        let prompt = new.prompt.clone().filter(|p| !p.trim().is_empty()).or_else(|| {
            issue.as_ref().map(|i| issue_prompt(i, &store.comments(i.id).unwrap_or_default()))
        });
        if prompt.is_some() || !new.attachments.is_empty() {
            self.send_with(&session, prompt.as_deref().unwrap_or_default(), &new.attachments)?;
        }
        self.changed.notify_waiters();
        store.session(session.id)
    }

    pub fn agent(&self, name: &str) -> Option<Arc<dyn Agent>> {
        self.agents.get(name).cloned()
    }

    pub fn end(&self, session: SessionId) {
        self.terminals.close(session);
        for agent in self.agents.values() {
            agent.end(session);
        }
    }

    pub fn jobs(&self, session: SessionId) -> Vec<agentbench_core::Job> {
        self.agents.values().flat_map(|a| a.jobs(session)).collect()
    }

    pub async fn kill_job(&self, session: SessionId, handle: &str) -> Result<(), String> {
        for agent in self.agents.values() {
            if agent.jobs(session).iter().any(|j| j.handle == handle) {
                return agent.kill_job(session, handle).await;
            }
        }
        Err(format!("no background command called {handle:?}"))
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

    pub fn queued(&self, session: SessionId) -> Vec<String> {
        self.live()
            .get(&session)
            .map(|l| {
                l.inbox
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .map(|m| m.text.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn with_queued(&self, mut sessions: Vec<Session>) -> Vec<Session> {
        for s in &mut sessions {
            s.queued = self.queued(s.id);
        }
        sessions
    }

    fn not_sent(&self, session: SessionId, why: &str, dropped: Vec<Message>) {
        for m in dropped {
            let text = format!("not sent, {why}: {}", m.text);
            let _ = self.store.add_message(session, &Entry::Notice { text });
        }
        let _ = self.store.touch_session(session);
        self.changed.notify_waiters();
    }

    pub fn interrupt(&self, session: SessionId) -> bool {
        let Some(dropped) = self.stop(session) else {
            return false;
        };
        self.not_sent(session, "the agent was stopped first", dropped);
        true
    }

    pub fn interrupt_returning_queued(&self, session: SessionId) -> Option<Vec<String>> {
        let dropped = self.stop(session)?;
        let texts = dropped.iter().map(|m| m.text.clone()).collect();
        let files: Vec<Message> =
            dropped.into_iter().filter(|m| !m.attachments.is_empty()).collect();
        self.not_sent(session, "the agent was stopped first", files);
        Some(texts)
    }

    fn stop(&self, session: SessionId) -> Option<Vec<Message>> {
        let live = self.live();
        let l = live.get(&session)?;
        l.stop.cancel();
        l.questions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let dropped = l.inbox.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        Some(dropped)
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
            && let Some((cmd, arg)) = agentbench_core::parse_slash(text)
        {
            return self.command(session, cmd.name, arg);
        }
        if uploads.len() > agentbench_core::ATTACHMENTS_MAX {
            return Err(StoreError(format!(
                "at most {} files go with one message",
                agentbench_core::ATTACHMENTS_MAX
            )));
        }
        let decoded = uploads
            .iter()
            .map(|u| {
                use base64::Engine;
                let data = base64::engine::general_purpose::STANDARD
                    .decode(u.data.trim())
                    .map_err(|e| StoreError(format!("{} is not base64: {e}", u.name)))?;
                if data.len() > agentbench_core::ATTACHMENT_MAX {
                    return Err(StoreError(format!(
                        "{} is over {} MB",
                        u.name,
                        agentbench_core::ATTACHMENT_MAX >> 20
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
                .or_else(|| agentbench_core::mime_for(&name))
                .unwrap_or("application/octet-stream");
            attachments.push(self.store.add_attachment(session.id, &name, mime, &data)?);
        }
        self.send_text(session, Message { text: text.to_string(), attachments }, Mode::Chat)
    }

    /// The files of a message, written to a folder of this session's so an agent can read them.
    fn materialise(&self, session: SessionId, attachments: &[Attachment]) -> Vec<Attached> {
        let dir = std::env::temp_dir().join("agentbench").join(session.to_string());
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
            "thinking" => {
                let arg = arg.to_lowercase();
                let effort = Some(arg.as_str()).filter(|e| !e.is_empty());
                if let Some(e) = effort
                    && !agentbench_core::EFFORTS.contains(&e)
                {
                    return Err(StoreError(format!(
                        "thinking is one of {}",
                        agentbench_core::EFFORTS.join(", ")
                    )));
                }
                self.store.set_effort(session.id, effort)?;
                note(match effort {
                    Some(e) => format!("this chat now thinks at {e}"),
                    None => "this chat now thinks as set under agent settings".into(),
                })
            }
            "title" => {
                if !arg.is_empty() {
                    self.store.set_title(
                        session.id,
                        &agentbench_core::short_title(arg, 80),
                        true,
                    )?;
                    self.changed.notify_waiters();
                    return Ok(());
                }
                if self.titles.as_ref().and_then(|t| t.target()).is_none() {
                    return Err(StoreError(
                        "no title model: set one under [title_model] in agentbench.toml, or give a title: /title my title".into(),
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
                let prompt = Message { text: COMPACT_PROMPT.into(), attachments: Vec::new() };
                self.send_text(session, prompt, Mode::Compact)
            }
            other => Err(StoreError(format!("/{other} is handled by the window"))),
        }
    }

    pub fn resume(&self, sessions: &[SessionId]) {
        for id in sessions {
            let Ok(session) = self.store.session(*id) else { continue };
            if let Err(e) = self.send(&session, RESUME_PROMPT) {
                tracing::warn!("could not resume session {id}: {}", e.0);
            }
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
                l.inbox.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
                drop(live);
                self.store.touch_session(session.id)?;
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
            while let Some(prompt) = next.take() {
                if stop.is_cancelled() {
                    break;
                }
                let session = match runner.store.session(session_id) {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let status = runner
                    .turn(agent.as_ref(), &session, &prompt, &stop, &inbox, &questions, mode)
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
                    next = Some(Message {
                        text: all.iter().map(|m| m.text.as_str()).collect::<Vec<_>>().join("\n\n"),
                        attachments: all.into_iter().flat_map(|m| m.attachments).collect(),
                    });
                }
            }
            // A turn that failed or was stopped never reads what was waiting.
            let left: Vec<Message> =
                inbox.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
            runner.live().remove(&session_id);
            if !left.is_empty() {
                runner.not_sent(session_id, "the turn ended first", left);
            }
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
        let prompt = Message { text: COMPACT_PROMPT.into(), attachments: Vec::new() };
        self.turn(agent, &session, &prompt, stop, inbox, questions, Mode::Compact).await
    }

    fn context(&self, session: &Session) -> Context {
        let project = self.store.project(session.project_id).ok();
        let branch = self.store.worktree(session.worktree_id).ok().and_then(|w| w.branch);
        let base = self.base.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let home = project.as_ref().is_some_and(|p| p.home);
        Context {
            session: session.id,
            project_id: session.project_id,
            project: project.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
            project_path: project.map(|p| p.path).unwrap_or_default(),
            branch,
            home,
            issue: session.issue_id,
            api_url: base.url,
            token: base.token,
            notes: self.store.notes(session.project_id).unwrap_or_default(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn turn(
        &self,
        agent: &dyn Agent,
        session: &Session,
        msg: &Message,
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
        let Ok(prompt_row) = self.store.add_message(session.id, &entry) else {
            return SessionStatus::Failed;
        };
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
            effort: session.effort.as_deref(),
            resume,
            history: &history,
            context: &context,
        };
        let sink = Sink {
            store: self.store.clone(),
            session: session.id,
            changed: self.changed.clone(),
            state: Mutex::new(SinkState {
                capture: mode == Mode::Compact,
                raw: agent.writes_log(),
                prompt: msg.attachments.clone(),
                typed: (mode == Mode::Chat).then_some(prompt_row.id),
                ..SinkState::default()
            }),
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

const RESUME_PROMPT: &str =
    "The agentbench server was restarted and stopped your turn. Carry on where you left off.";

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
    raw: bool,
    prompt: Vec<Attachment>,
    typed: Option<Id>,
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
    fn sent(&self, mut messages: Vec<agentbench_core::Sent>) {
        if self.lock().capture || !self.lock().raw {
            return;
        }
        self.flush();
        let mut st = self.lock();
        let mut typed = None;
        for s in &mut messages {
            if let agentbench_core::Said::Typed { attachments } = &mut s.said {
                *attachments = std::mem::take(&mut st.prompt);
                typed = st.typed.take();
            }
        }
        let streamed = match messages.first() {
            Some(s) if s.message["role"] == "assistant" => st.text.take().map(|(id, _)| id),
            Some(s) if s.message["role"] == "system" => st.typed.take(),
            _ => typed,
        };
        st.thinking = None;
        drop(st);
        let entry = Entry::Wire { messages };
        match streamed {
            Some(id) => {
                let _ = self.store.update_message(id, &entry);
            }
            None => {
                let _ = self.store.add_message(self.session, &entry);
            }
        }
        self.changed.notify_waiters();
    }

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
            Event::ToolStart { .. } | Event::ToolEnd { .. } | Event::Diagnostics { .. }
                if self.lock().raw => {}
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
            Event::Steered { .. } => {}
            Event::Usage(u) => {
                let _ = self.store.add_usage(self.session, &u);
            }
            Event::Notice(text) => {
                self.entry(Entry::Notice { text });
            }
            Event::Diagnostics { text, broken } => {
                self.entry(Entry::Diagnostics { text, broken });
            }
            Event::Sent(messages) => self.sent(messages),
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
        let id = self.entry(msg.entry());
        let mut st = self.lock();
        st.prompt = msg.attachments.clone();
        st.typed = id;
        drop(st);
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
                answer: Some(if question.secret {
                    agentbench_core::SECRET_SHOWN.into()
                } else {
                    a.join(", ")
                }),
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
    use agentbench_core::Usage;

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
        let script = r#"read p; echo '{"session":"abc"}'; echo "{\"text\":\"got $p in $AGENTBENCH_SESSION\"}"; echo '{"in":7}'"#;
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
        let marker = format!("/tmp/agentbench-test-{}", uuid::Uuid::new_v4());
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

    #[tokio::test]
    async fn stopping_hands_back_the_queued_messages_instead_of_dropping_them() {
        let (runner, s) = setup(script_agent("slow", r#"echo '{"text":"working"}'; sleep 30"#));
        runner.send(&s, "go").unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        runner.send(&s, "then this").unwrap();
        runner.send(&s, "and that").unwrap();
        assert_eq!(
            runner.interrupt_returning_queued(s.id),
            Some(vec!["then this".to_string(), "and that".to_string()])
        );
        wait_idle(&runner, s.id).await;
        let notices: Vec<String> = runner
            .store()
            .messages(s.id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::Notice { text } if text.starts_with("not sent") => Some(text),
                _ => None,
            })
            .collect();
        assert!(notices.is_empty(), "{notices:?}");
        assert_eq!(runner.interrupt_returning_queued(s.id), None);
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
    async fn a_queued_message_is_kept_aside_until_the_agent_reads_it() {
        let (runner, s) = setup(Box::new(Steerable));
        runner.send(&s, "go").unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        runner.send(&s, "left instead").unwrap();
        let users = |runner: &Runner| -> Vec<String> {
            agentbench_core::shown(&runner.store().messages(s.id, 0).unwrap())
                .into_iter()
                .filter_map(|m| match m.entry {
                    Entry::User { text, .. } => Some(text),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(users(&runner), ["go"], "not in the chat while it waits");
        assert_eq!(runner.queued(s.id), ["left instead"]);
        assert_eq!(
            runner.with_queued(vec![fresh_session(&runner, s.id)])[0].queued,
            ["left instead"]
        );
        wait_idle(&runner, s.id).await;
        assert_eq!(users(&runner), ["go", "left instead"]);
        assert!(runner.queued(s.id).is_empty());
    }

    fn fresh_session(runner: &Runner, id: SessionId) -> Session {
        runner.store().session(id).unwrap()
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

    struct SecretAsker;

    #[async_trait]
    impl Agent for SecretAsker {
        fn name(&self) -> &str {
            "secretive"
        }
        fn label(&self) -> &str {
            "Secretive"
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
                .ask(Question { text: "key?".into(), secret: true, ..Question::default() })
                .await?;
            io.emit(Event::Text { delta: format!("{} chars", a.len()) });
            io.emit(Event::Done);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_secret_answer_reaches_the_agent_but_not_the_transcript() {
        let (runner, s) = setup(Box::new(SecretAsker));
        runner.send(&s, "go").unwrap();
        let mut qid = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            qid =
                runner.store().messages(s.id, 0).unwrap().into_iter().find_map(|m| match m.entry {
                    Entry::Question { id, .. } => Some(id),
                    _ => None,
                });
            if qid.is_some() {
                break;
            }
        }
        runner.answer(s.id, &qid.expect("a question"), vec!["hunter22".into()]).unwrap();
        wait_idle(&runner, s.id).await;
        let msgs: Vec<Entry> =
            runner.store().messages(s.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        assert!(!format!("{msgs:?}").contains("hunter22"));
        assert!(msgs.iter().any(|e| matches!(e, Entry::Question { answer: Some(a), .. } if a == agentbench_core::SECRET_SHOWN)));
        assert_eq!(msgs.last().unwrap().text(), "8 chars");
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
                secret: false,
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
                agentbench_core::since_summary(turn.history).len(),
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

        runner.send(&fresh(&runner), "/thinking Low").unwrap();
        assert_eq!(fresh(&runner).effort.as_deref(), Some("low"));
        assert!(runner.send(&fresh(&runner), "/thinking turbo").is_err());
        assert_eq!(fresh(&runner).effort.as_deref(), Some("low"));
        runner.send(&fresh(&runner), "/thinking").unwrap();
        assert_eq!(fresh(&runner).effort, None);

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

    #[tokio::test]
    async fn sessions_cut_off_by_a_restart_are_resumed() {
        let script = r#"read p; echo "{\"text\":\"resumed: $p\"}""#;
        let store = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&store, "/tmp");
        let busy = store.add_session(main.id, "echo", "busy", None, None).unwrap();
        let idle = store.add_session(main.id, "echo", "idle", None, None).unwrap();
        store.set_status(busy.id, SessionStatus::Running).unwrap();
        let stale = store.reset_running().unwrap();
        let runner = Runner::new(store, vec![script_agent("echo", script)]);
        runner.resume(&stale);
        assert_eq!(wait_idle(&runner, busy.id).await.status, SessionStatus::Idle);
        let last = runner.store().messages(busy.id, 0).unwrap().pop().unwrap().entry;
        assert_eq!(last, Entry::Assistant { text: format!("resumed: {RESUME_PROMPT}") });
        assert!(runner.store().messages(idle.id, 0).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_turn_cut_off_mid_call_replays_its_answer_before_the_next_prompt() {
        use crate::agent::native::{Model, Native};
        use crate::titles::fake::{server, streamed};
        use agentbench_core::{Said, Sent};
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = server(vec![
            (200, streamed(json!({"content": "carrying on"}))),
            (200, streamed(json!({"content": "still fine"}))),
        ]);
        let store = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&store, dir.path().to_str().unwrap());
        let s = store.add_session(main.id, "native", "t", None, None).unwrap();
        let row = |message: serde_json::Value| Entry::Wire {
            messages: vec![Sent { message, said: Said::Plain }],
        };
        for e in [
            row(json!({"role": "system", "content": "s"})),
            row(json!({"role": "user", "content": "one"})),
            row(json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
            ]})),
        ] {
            store.add_message(s.id, &e).unwrap();
        }
        store.set_status(s.id, SessionStatus::Running).unwrap();
        let stale = store.reset_running().unwrap();
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let runner = Runner::new(store, vec![Box::new(native)]);
        runner.resume(&stale);
        wait_idle(&runner, s.id).await;
        runner.send(&runner.store().session(s.id).unwrap(), "two").unwrap();
        assert_eq!(wait_idle(&runner, s.id).await.status, SessionStatus::Idle);
        let roles = |body: &serde_json::Value| -> Vec<String> {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["role"].as_str().unwrap().to_string())
                .collect()
        };
        let asked = seen.lock().unwrap().clone();
        let want = ["system", "user", "assistant", "tool", "user"];
        assert_eq!(roles(&asked[0]), want);
        assert_eq!(roles(&asked[1])[..5], want, "the replay keeps the answer before the prompt");
    }

    struct Thinker;

    #[async_trait]
    impl Agent for Thinker {
        fn name(&self) -> &str {
            "thinker"
        }
        fn label(&self) -> &str {
            "Thinker"
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
        fn writes_log(&self) -> bool {
            true
        }
        async fn run(&self, _turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
            for word in ["The", " user", " wants", " a", " list."] {
                io.emit(Event::Thinking { text: word.into() });
            }
            io.emit(Event::Sent(vec![agentbench_core::Sent {
                message: serde_json::json!({"role": "assistant", "content": "", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "list_issues", "arguments": ""}},
                ]}),
                said: agentbench_core::Said::Plain,
            }]));
            io.emit(Event::Done);
            Ok(())
        }
    }

    #[tokio::test]
    async fn thinking_streamed_before_a_sent_message_is_kept_whole() {
        let (runner, s) = setup(Box::new(Thinker));
        runner.send(&s, "list it").unwrap();
        wait_idle(&runner, s.id).await;
        let thinking: Vec<String> = runner
            .store()
            .messages(s.id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::Thinking { text } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, ["The user wants a list."]);
    }

    #[tokio::test]
    async fn a_native_chat_is_resent_from_its_own_rows_byte_for_byte() {
        use crate::agent::native::{Model, Native};
        use crate::titles::fake::{server, streamed};
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "Use tabs here.\n").unwrap();
        let long = "x".repeat(5000);
        let call = json!({"tool_calls": [
            {"index": 0, "id": "c1", "function": {"name": "bash", "arguments": json!({"command": format!("echo {long}")}).to_string()}},
            {"index": 1, "id": "c2", "function": {"name": "bash", "arguments": "{\"command\": \"false\"}"}},
        ]});
        let (url, seen) = server(vec![
            (200, streamed(call)),
            (200, streamed(json!({"content": "  done"}))),
            (200, streamed(json!({"content": "again"}))),
            (200, streamed(json!({"content": "We ran echo."}))),
            (200, streamed(json!({"content": "fresh"}))),
        ]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let store = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&store, dir.path().to_str().unwrap());
        let s = store.add_session(main.id, "native", "t", None, None).unwrap();
        let runner = Runner::new(store, vec![Box::new(native)]);
        let fresh = |runner: &Runner| runner.store().session(s.id).unwrap();
        runner.send(&fresh(&runner), "one").unwrap();
        wait_idle(&runner, s.id).await;
        std::fs::write(dir.path().join("AGENTS.md"), "Use spaces now.\n").unwrap();
        runner.send(&fresh(&runner), "two").unwrap();
        wait_idle(&runner, s.id).await;
        let asked = seen.lock().unwrap().clone();
        let before = asked[1]["messages"].as_array().unwrap();
        let now = asked[2]["messages"].as_array().unwrap();
        assert_eq!(now[..before.len()], before[..], "the second turn changed what was sent");
        assert!(now[0]["content"].as_str().unwrap().contains("Use tabs here."));
        assert!(now.iter().any(|m| m["content"].as_str().is_some_and(|t| t.contains(&long))));
        assert_eq!(now[before.len()], json!({"role": "assistant", "content": "  done"}));
        assert_eq!(now[before.len() + 1], json!({"role": "user", "content": "two"}));

        let shown: Vec<Entry> = agentbench_core::shown(&runner.store().messages(s.id, 0).unwrap())
            .into_iter()
            .map(|m| m.entry)
            .collect();
        assert!(
            matches!(&shown[0], Entry::Tool { name, .. } if name == agentbench_core::SYSTEM_PROMPT)
        );
        assert_eq!(shown[1], Entry::user("one"));
        assert!(
            matches!(&shown[2], Entry::Tool { name, output: Some(o), .. } if name == "bash" && o.contains(&long))
        );
        assert!(matches!(&shown[3], Entry::Tool { error: true, .. }));
        assert_eq!(shown[4], Entry::Assistant { text: "done".into() });
        assert_eq!(shown[5], Entry::user("two"));

        runner.send(&fresh(&runner), "/compact").unwrap();
        wait_idle(&runner, s.id).await;
        runner.send(&fresh(&runner), "three").unwrap();
        wait_idle(&runner, s.id).await;
        let last = seen.lock().unwrap()[4]["messages"].as_array().unwrap().clone();
        assert_eq!(last.len(), 3, "{last:?}");
        assert!(last[0]["content"].as_str().unwrap().contains("Use spaces now."));
        assert!(last[1]["content"].as_str().unwrap().ends_with("We ran echo."));
        assert_eq!(last[2], json!({"role": "user", "content": "three"}));
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

    #[tokio::test]
    async fn one_session_can_start_another_and_wait_for_it() {
        use crate::agent::Api;
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let store = Store::memory().unwrap();
        let p = crate::projects::add(&store, &main, None, false).unwrap();
        let tree = store.main_worktree(p.id).unwrap();
        let lead = store.add_session(tree.id, "echo", "lead", None, None).unwrap();
        let issue = store
            .add_issue(&agentbench_core::NewIssue {
                project_id: p.id,
                title: "Fix the thing".into(),
                ..Default::default()
            })
            .unwrap();
        let script = r#"read p; echo "{\"text\":\"did: $p\"}""#;
        let runner = Runner::new(store.clone(), vec![script_agent("echo", script)]);
        let api = crate::tools::StoreApi::new(store.clone());
        // The test api has no runner; the real one is wired through the runner's own handle.
        let e =
            api.call("start_session", &serde_json::json!({"prompt": "x"}), lead.id).unwrap_err();
        assert!(e.contains("cannot be started"));

        let wired = runner.api.clone();
        let out =
            wired.call("start_session", &serde_json::json!({"issue": issue.id}), lead.id).unwrap();
        let child: SessionId = out["session"].as_str().unwrap().parse().unwrap();
        assert!(out["worktree"].as_str().unwrap().starts_with("issue-"), "{out}");
        assert_eq!(store.issue(issue.id).unwrap().state, agentbench_core::IssueState::Doing);
        wait_idle(&runner, child).await;
        let status = wired
            .call("session_status", &serde_json::json!({"id": child.to_string()}), lead.id)
            .unwrap();
        assert_eq!(status["busy"], false);
        assert!(status["last_reply"].as_str().unwrap().contains("Fix the thing"), "{status}");

        wired
            .call(
                "send_message",
                &serde_json::json!({"id": child.to_string(), "text": "more"}),
                lead.id,
            )
            .unwrap();
        wait_idle(&runner, child).await;
        let said = store.messages(child, 0).unwrap();
        assert!(
            said.iter()
                .any(|m| m.entry.text().contains(&format!("Message from session {}", lead.id)))
        );

        let none = wired.call("start_session", &serde_json::json!({}), lead.id).unwrap_err();
        assert!(none.contains("`prompt`"), "{none}");
        let missing = wired
            .call("start_session", &serde_json::json!({"prompt": "x", "agent": "nope"}), lead.id)
            .unwrap_err();
        assert!(missing.contains("no agent named nope"), "{missing}");
    }

    #[tokio::test]
    async fn a_session_starts_in_another_project_only_with_the_users_permission() {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        crate::git::testing::repo(&a);
        crate::git::testing::repo(&b);
        let store = Store::memory().unwrap();
        let pa = crate::projects::add(&store, &a, Some("alpha"), false).unwrap();
        let pb = crate::projects::add(&store, &b, Some("beta"), false).unwrap();
        let lead = store
            .add_session(store.main_worktree(pa.id).unwrap().id, "echo", "lead", None, None)
            .unwrap();
        let script = r#"read p; echo "{\"text\":\"did: $p\"}""#;
        let runner = Runner::new(store.clone(), vec![script_agent("echo", script)]);
        let api = runner.api.clone();
        let go = json!({"project": "beta", "prompt": "hello"});

        let listed = api.call("list_projects", &json!({}), lead.id).unwrap();
        assert_eq!(listed.as_array().unwrap().len(), 2);
        let e = api.call("start_session", &go, lead.id).unwrap_err();
        assert!(e.contains("needs the user's permission"), "{e}");
        assert!(
            !api.call("check_project", &json!({"project": "beta"}), lead.id).unwrap()["allowed"]
                .as_bool()
                .unwrap()
        );

        // Once: one session, then it asks again.
        api.call("grant_project", &json!({"project": pb.id, "scope": "once"}), lead.id).unwrap();
        let out = api.call("start_session", &go, lead.id).unwrap();
        assert_eq!(out["project"], "beta");
        let child: SessionId = out["session"].as_str().unwrap().parse().unwrap();
        assert_eq!(store.session(child).unwrap().project_id, pb.id);
        assert!(api.call("start_session", &go, lead.id).is_err());
        // The parent can follow the child in the other project; a stranger cannot.
        wait_idle(&runner, child).await;
        let st = api.call("session_status", &json!({"id": child.to_string()}), lead.id).unwrap();
        assert!(st["last_reply"].as_str().unwrap().contains("hello"));
        let stranger = store
            .add_session(store.main_worktree(pa.id).unwrap().id, "echo", "s", None, None)
            .unwrap();
        assert!(
            api.call("session_status", &json!({"id": child.to_string()}), stranger.id).is_err()
        );

        // Always for that project: no more asking, and it is remembered in the store.
        api.call("grant_project", &json!({"project": "beta", "scope": "project"}), lead.id)
            .unwrap();
        assert!(api.call("start_session", &go, stranger.id).is_ok());
        assert!(
            api.call("check_project", &json!({"project": pb.id}), lead.id).unwrap()["allowed"]
                .as_bool()
                .unwrap()
        );
        assert!(
            api.call("start_session", &json!({"project": 999, "prompt": "x"}), lead.id).is_err()
        );
    }
}
