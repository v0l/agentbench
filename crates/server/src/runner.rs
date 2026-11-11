use crate::agent::{
    Agent, Api, Attached, CancellationToken, Context, Event, Io, Steer, Turn, async_trait,
};
use crate::store::{Deferred, Store, StoreError};
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

/// What the user sent: the words, and the files kept with them. `from` names the sources of a
/// message the system sent instead.
#[derive(Clone, Debug, Default)]
struct Message {
    text: String,
    attachments: Vec<Attachment>,
    from: Vec<String>,
}

impl Message {
    fn entry(&self) -> Entry {
        Entry::User {
            text: self.text.clone(),
            attachments: self.attachments.clone(),
            from: self.from.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Origin {
    pub source: &'static str,
    pub worker: Option<SessionId>,
    pub urgent: bool,
}

impl Origin {
    pub fn new(source: &'static str, worker: Option<SessionId>) -> Origin {
        Origin { source, worker, urgent: false }
    }

    pub fn urgent(self, urgent: bool) -> Origin {
        Origin { urgent, ..self }
    }
}

pub const URGENT_AFTER_SETTING: &str = "supervisor_urgent_secs";
const URGENT_AFTER_DEFAULT: u64 = 600;
const DIGEST_MAX: usize = 12;
const MAX_FOLDS: usize = 6;

#[derive(Clone)]
pub struct Runner {
    store: Store,
    agents: Arc<HashMap<String, Arc<dyn Agent>>>,
    live: Arc<Mutex<HashMap<SessionId, Live>>>,
    routed: Arc<Mutex<HashMap<SessionId, Routed>>>,
    folded: Arc<Mutex<HashMap<SessionId, i64>>>,
    api: Arc<dyn Api>,
    titles: Option<Titles>,
    pub base: Arc<Mutex<Base>>,
    pub changed: Arc<Notify>,
    pub terminals: crate::terminal::Terminals,
    pub quotas: crate::quota::Quotas,
    /// This runner, as the tracker tools see it, for starting and messaging sessions.
    me: crate::tools::Handle,
}

const SUPERVISOR_PATIENCE_SETTING: &str = "supervisor_answer_secs";
const SUPERVISOR_PATIENCE_DEFAULT: u64 = 600;

#[derive(Clone)]
struct Routed {
    question: String,
    escalate: Arc<Notify>,
    recommendation: Arc<Mutex<Option<String>>>,
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
            routed: Arc::new(Mutex::new(HashMap::new())),
            folded: Arc::new(Mutex::new(HashMap::new())),
            api,
            titles: None,
            base: Arc::new(Mutex::new(Base::default())),
            changed,
            terminals: Default::default(),
            quotas: Default::default(),
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

    pub fn observe_quota(&self, quota: agentbench_core::Quota) {
        let now = crate::quota::now();
        if let Some(hint) = self.quotas.observe(quota, now) {
            self.broadcast_notice(&hint.text);
        }
        self.changed.notify_waiters();
    }

    pub fn expire_quotas(&self) {
        let now = crate::quota::now();
        for q in self.quotas.expire(now) {
            self.broadcast_notice(&format!("Quota {} reset", q.window));
        }
        self.quotas.unblock(now);
    }

    fn broadcast_notice(&self, text: &str) {
        let ids: Vec<SessionId> = self.live().keys().copied().collect();
        for id in ids {
            let _ = self.store.add_message(id, &Entry::Notice { text: text.to_string() });
        }
    }

    pub fn titles(&self) -> Option<&Titles> {
        self.titles.as_ref()
    }

    /// Replace a stand-in title with one the title model writes from the
    /// conversation so far. Runs in the background; a failure leaves the
    /// stand-in in place and is tried again after the next turn.
    fn retitle(&self, session: SessionId, force: bool) {
        let Some(titles) = self.titles.clone() else { return };
        if titles.target().is_none() {
            return;
        }
        let (auto, at) = self.store.title_state(session).unwrap_or_default();
        if !force && auto == 0 {
            return;
        }
        let (store, changed) = (self.store.clone(), self.changed.clone());
        tokio::spawn(async move {
            let entries: Vec<Entry> =
                agentbench_core::shown(&store.messages(session, 0).unwrap_or_default())
                    .into_iter()
                    .map(|m| m.entry)
                    .collect();
            let turns = crate::titles::user_turns(&entries);
            let due = auto == crate::store::TITLE_GENERATED
                && turns >= at as usize + crate::titles::REFRESH_EVERY;
            if !force && auto == crate::store::TITLE_GENERATED && !due {
                return;
            }
            let current = store.session(session).map(|s| s.title).unwrap_or_default();
            let named = if auto == crate::store::TITLE_GENERATED {
                titles.refresh(&entries, &current).await
            } else {
                titles.title(&entries).await
            };
            match named {
                Ok(title) => {
                    if matches!(store.set_generated_title(session, &title, force, turns), Ok(true))
                    {
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

    pub fn tools(&self) -> &dyn Api {
        self.api.as_ref()
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
            Place::New { .. } if project.builtin() => {
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
                if project.supervisor {
                    format!("{} supervising", agent.label())
                } else if project.home {
                    format!("{} in ~", agent.label())
                } else {
                    format!("{} on {}", agent.label(), worktree.label())
                }
            });
        let model = new
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .or_else(|| {
                project
                    .supervisor
                    .then(|| crate::providers::supervisor_model(&store, agent.name()))
                    .flatten()
            });
        let session =
            store.add_session(worktree.id, agent.name(), title.trim(), model.as_deref(), new.issue_id)?;
        if let Some(parent) = new.parent {
            store.set_parent(session.id, Some(parent))?;
        }
        if stand_in {
            store.set_title_auto(session.id, true)?;
        }
        let prompt = new.prompt.clone().filter(|p| !p.trim().is_empty()).or_else(|| {
            issue.as_ref().map(|i| issue_prompt(i, &store.comments(i.id).unwrap_or_default()))
        });
        let prompt = match (prompt, new.parent) {
            (Some(p), Some(parent)) if !p.contains("PARENT=") => Some(format!("{p}\n\nPARENT={parent}")),
            (p, _) => p,
        };
        if prompt.is_some() || !new.attachments.is_empty() {
            self.send_with(&session, prompt.as_deref().unwrap_or_default(), &new.attachments)?;
        }
        self.changed.notify_waiters();
        store.session(session.id)
    }

    pub fn supervisor_session(&self) -> Option<Session> {
        let project = self.store.supervisor_project().ok().flatten()?;
        self.store
            .sessions()
            .ok()?
            .into_iter()
            .filter(|s| s.project_id == project.id)
            .min_by_key(|s| s.created_at)
    }

    /// The one standing supervisor chat, made on first use.
    pub fn open_supervisor(&self, agent: &str) -> Result<Session, StoreError> {
        if let Some(s) = self.supervisor_session() {
            if s.model.is_none() {
                let model = crate::providers::supervisor_model(&self.store, &s.agent);
                self.store.set_model(s.id, model.as_deref())?;
                return self.store.session(s.id);
            }
            return Ok(s);
        }
        let project = self
            .store
            .supervisor_project()?
            .ok_or_else(|| StoreError("the supervisor is not set up on this server".into()))?;
        self.start_session(NewSession {
            project_id: project.id,
            agent: agent.to_string(),
            title: Some("Supervisor".into()),
            ..Default::default()
        })
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
            s.no_plan_reads = self
                .store
                .setting(&format!("no_plan_{}", s.id))
                .ok()
                .flatten()
                .and_then(|n| n.parse().ok());
            s.deferred = self.store.deferred_count(s.id).unwrap_or_default();
            s.paused_until = crate::quota::paused_until(&self.store, s.id);
        }
        sessions
    }

    fn queues_system(&self, session: &Session) -> bool {
        session.queue_system
            || self.store.project(session.project_id).is_ok_and(|p| p.supervisor)
    }

    /// Send a message the system wrote rather than the user. A supervisor, or a session set to
    /// queue them, gets these only when it is idle, gathered into one digest; anyone else is
    /// steered with it as with any message.
    pub fn tell(&self, session: &Session, text: &str, origin: Origin) -> Result<(), StoreError> {
        if !self.queues_system(session) {
            return self.send(session, text);
        }
        let text = text.trim();
        if text.is_empty() {
            return Err(StoreError("nothing to send".into()));
        }
        self.store.defer(session.id, origin.source, origin.worker, origin.urgent, text)?;
        self.changed.notify_waiters();
        self.deliver_deferred(session.id)
    }

    fn deliver_deferred(&self, id: SessionId) -> Result<(), StoreError> {
        let session = self.store.session(id)?;
        self.dispatch(&session, Mode::Chat, |running| {
            if running { None } else { self.digest(id, |_| true) }
        })
    }

    /// Hand every idle session the messages that waited for it while the server was down.
    pub fn deliver_backlog(&self) {
        for id in self.store.sessions_with_deferred().unwrap_or_default() {
            if let Err(e) = self.deliver_deferred(id) {
                tracing::warn!("could not deliver the waiting messages to {id}: {}", e.0);
            }
        }
    }

    /// The deferred messages that `pick` chooses, taken out of the queue as one message.
    fn digest(&self, id: SessionId, pick: impl Fn(&Deferred) -> bool) -> Option<Message> {
        let all: Vec<Deferred> =
            self.store.deferred(id).ok()?.into_iter().filter(|d| pick(d)).collect();
        if all.is_empty() {
            return None;
        }
        let ids: Vec<Id> = all.iter().map(|d| d.id).collect();
        let _ = self.store.remove_deferred(id, &ids);
        Some(digest_of(all))
    }

    fn not_sent(&self, session: SessionId, why: &str, dropped: Vec<Message>) {
        for m in dropped {
            let text = format!("not sent, {why}: {}", m.text);
            let _ = self.store.add_message(session, &Entry::Warning { text });
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

    fn routed(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Routed>> {
        self.routed.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn answer_routed(&self, worker: SessionId, answers: Vec<String>) -> Result<(), StoreError> {
        let question = self
            .routed()
            .get(&worker)
            .map(|r| r.question.clone())
            .ok_or_else(|| StoreError("that session has no question waiting on you".into()))?;
        self.answer(worker, &question, answers)
    }

    pub fn escalate_routed(&self, worker: SessionId, recommendation: &str) -> Result<(), StoreError> {
        let routed = self
            .routed()
            .get(&worker)
            .cloned()
            .ok_or_else(|| StoreError("that session has no question waiting on you".into()))?;
        *routed.recommendation.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(recommendation.trim().to_string());
        routed.escalate.notify_one();
        Ok(())
    }

    fn supervisor_of(&self, session: SessionId) -> Option<Session> {
        let parent = self.store.session(session).ok()?.parent?;
        let parent = self.store.session(parent).ok()?;
        self.store.project(parent.project_id).ok()?.supervisor.then_some(parent)
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
        self.send_text(session, Message { text: text.to_string(), attachments, from: Vec::new() }, Mode::Chat)
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
                self.store.set_title_auto(session.id, true)?;
                note("the agent starts afresh from here; the transcript above is kept".into())
            }
            "model" => {
                let model = Some(arg).filter(|m| !m.is_empty());
                if let Some(pinned) = self.pinned_model(session) {
                    let pinned = pinned.unwrap_or_default();
                    if model.is_some_and(|m| m != pinned) {
                        return Err(StoreError(format!(
                            "the supervisor is pinned to the supervisor model ({pinned}); change which model that is in settings"
                        )));
                    }
                    self.store.set_model(session.id, Some(pinned.as_str()).filter(|m| !m.is_empty()))?;
                    return note(format!("the supervisor stays on {pinned}"));
                }
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
                let prompt = Message { text: COMPACT_PROMPT.into(), ..Default::default() };
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
        if msg.text.is_empty() && msg.attachments.is_empty() {
            return Err(StoreError("nothing to send".into()));
        }
        self.dispatch(session, mode, |_| Some(msg))
    }

    /// Start a turn with what `make` gives, or put it in the inbox of the turn already running.
    /// `make` is told whether one is running, and is called under the lock that decides it.
    fn dispatch(
        &self,
        session: &Session,
        mode: Mode,
        make: impl FnOnce(bool) -> Option<Message>,
    ) -> Result<(), StoreError> {
        let agent = self
            .agent(&session.agent)
            .ok_or_else(|| StoreError(format!("no agent named {}", session.agent)))?;
        let mut all = self.live();
        if let Some(l) = all.get(&session.id) {
            let Some(msg) = make(true) else { return Ok(()) };
            l.inbox.lock().unwrap_or_else(|e| e.into_inner()).push(msg);
            drop(all);
            self.store.touch_session(session.id)?;
            self.changed.notify_waiters();
            return Ok(());
        }
        let Some(first) = make(false) else { return Ok(()) };
        let live = Live {
            stop: CancellationToken::new(),
            inbox: Arc::new(Mutex::new(Vec::new())),
            questions: Arc::new(Mutex::new(HashMap::new())),
        };
        all.insert(session.id, live.clone());
        drop(all);
        let runner = self.clone();
        let session_id = session.id;
        tokio::spawn(runner.drive(agent, session_id, live, Some(first), mode));
        Ok(())
    }

    async fn drive(
        self,
        agent: Arc<dyn Agent>,
        session_id: SessionId,
        live: Live,
        first: Option<Message>,
        mode: Mode,
    ) {
        let runner = self;
        let Live { stop, inbox, questions } = live;
        let mut gone = false;
        let mut next = first;
        let ran = next.is_some();
        if next.is_none() {
            runner.fold_sections(agent.as_ref(), session_id, &stop, true).await;
            next = runner.next_message(session_id, &inbox, &mut gone);
        }
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
                runner.fold_sections(agent.as_ref(), session_id, &stop, false).await;
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
            next = runner.next_message(session_id, &inbox, &mut gone);
        }
        // A turn that failed or was stopped never reads what was waiting.
        let left: Vec<Message> =
            inbox.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        if !gone {
            runner.live().remove(&session_id);
        }
        if !left.is_empty() {
            runner.not_sent(session_id, "the turn ended first", left);
        }
        runner.changed.notify_waiters();
        if ran {
            runner.tell_watchers(session_id);
        }
        if runner.store.session(session_id).is_ok_and(|s| s.status == SessionStatus::Idle) {
            let _ = runner.deliver_deferred(session_id);
        }
    }

    fn next_message(
        &self,
        session_id: SessionId,
        inbox: &Arc<Mutex<Vec<Message>>>,
        gone: &mut bool,
    ) -> Option<Message> {
        let mut all = self.live();
        let queued: Vec<Message> =
            inbox.lock().unwrap_or_else(|e| e.into_inner()).drain(..).collect();
        let next = if queued.is_empty() {
            self.digest(session_id, |_| true)
        } else {
            Some(Message {
                text: queued.iter().map(|m| m.text.as_str()).collect::<Vec<_>>().join("\n\n"),
                attachments: queued.into_iter().flat_map(|m| m.attachments).collect(),
                from: Vec::new(),
            })
        };
        if next.is_none() {
            all.remove(&session_id);
            *gone = true;
        }
        next
    }

    pub fn fold_idle(&self, now: i64) -> usize {
        let Ok(sessions) = self.store.sessions() else { return 0 };
        let mut started = 0;
        for session in sessions {
            if session.status != SessionStatus::Idle {
                continue;
            }
            let Some(agent) = self.agent(&session.agent) else { continue };
            let Some(folding) = agent.folding() else { continue };
            if !session.usage.past(folding.at)
                || now.saturating_sub(session.updated_at) < folding.idle.as_millis() as i64
                || self.folded_at().get(&session.id) == Some(&session.updated_at)
            {
                continue;
            }
            let mut all = self.live();
            if all.contains_key(&session.id) {
                continue;
            }
            let live = Live {
                stop: CancellationToken::new(),
                inbox: Arc::new(Mutex::new(Vec::new())),
                questions: Arc::new(Mutex::new(HashMap::new())),
            };
            all.insert(session.id, live.clone());
            drop(all);
            self.folded_at().insert(session.id, session.updated_at);
            started += 1;
            let runner = self.clone();
            let id = session.id;
            tokio::spawn(async move {
                runner.clone().drive(agent, id, live, None, Mode::Chat).await;
                if let Ok(after) = runner.store.session(id) {
                    runner.folded_at().insert(id, after.updated_at);
                }
            });
        }
        started
    }

    fn folded_at(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, i64>> {
        self.folded.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn fold_sections(
        &self,
        agent: &dyn Agent,
        id: SessionId,
        stop: &CancellationToken,
        idle: bool,
    ) {
        if agent.folding().is_none() {
            return;
        }
        let mut rounds = 0;
        while rounds < MAX_FOLDS && (idle && rounds == 0 || !idle && self.full(agent, id)) {
            rounds += 1;
            if !self.fold_once(agent, id, stop, idle).await {
                break;
            }
        }
    }

    async fn fold_once(
        &self,
        agent: &dyn Agent,
        id: SessionId,
        stop: &CancellationToken,
        idle: bool,
    ) -> bool {
        let Ok(session) = self.store.session(id) else { return false };
        let entries: Vec<Entry> = self
            .store
            .messages(id, 0)
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.entry)
            .collect();
        let chosen = match self.pinned_model(&session) {
            Some(pinned) => pinned,
            None => session.model.clone(),
        };
        let model = self.cli_model(&session.agent, chosen.as_deref());
        let folded = tokio::select! {
            _ = stop.cancelled() => return false,
            done = agent.fold(&entries, model.as_deref(), stop) => done,
        };
        if stop.is_cancelled() {
            return false;
        }
        match folded {
            Ok(Some(f)) => {
                let _ = self.store.add_message(
                    id,
                    &Entry::Section { text: f.text, covers: f.covers, topic: f.topic.clone() },
                );
                let while_idle = if idle { " while idle" } else { "" };
                let _ = self.store.add_message(
                    id,
                    &Entry::Notice {
                        text: format!(
                            "folded {} older messages (topic: {}) into a summary{while_idle}",
                            f.covers, f.topic
                        ),
                    },
                );
                if f.before > 0 {
                    let context = session.usage.context as u128 * f.after as u128 / f.before as u128;
                    let _ = self.store.set_context(id, context as u64);
                }
                self.changed.notify_waiters();
                true
            }
            Ok(None) => false,
            Err(e) => {
                let _ = self
                    .store
                    .add_message(id, &Entry::Warning { text: format!("could not fold the older messages: {e}") });
                self.changed.notify_waiters();
                false
            }
        }
    }

    fn tell_watchers(&self, finished: SessionId) {
        let Ok(mut watchers) = self.store.take_watchers(finished) else { return };
        let Ok(done) = self.store.session(finished) else { return };
        if let Some(parent) = done.parent {
            if self.store.session(parent).is_ok() && !watchers.contains(&parent) {
                watchers.push(parent);
            }
            let asked_by_autopilot = done.status == SessionStatus::Idle
                && self.store.workers().is_ok_and(|w| w.iter().any(|w| w.session == finished));
            if asked_by_autopilot {
                watchers.retain(|w| *w != parent);
            }
        }
        if watchers.is_empty() {
            return;
        }
        let plain = format!(
            "Session {finished} (\"{}\", project {}) changed state to {}. Read its reply with read_session.",
            done.title,
            done.project_id,
            done.status.as_str(),
        );
        let child_plain = format!(
            "{plain} Its last reply: {}",
            crate::autopilot::last_reply(&self.store, finished).chars().take(400).collect::<String>(),
        );
        for id in watchers {
            if let Ok(watcher) = self.store.session(id) {
                let judges = done.parent == Some(id)
                    && done.status == SessionStatus::Idle
                    && self.store.project(watcher.project_id).is_ok_and(|p| p.supervisor);
                let (text, source) = if judges {
                    (crate::autopilot::judge_request(&self.store, &done), "judge")
                } else if done.parent == Some(id) {
                    (child_plain.clone(), "watch")
                } else {
                    (plain.clone(), "watch")
                };
                let _ = self.tell(&watcher, &text, Origin::new(source, Some(finished)));
            }
        }
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
        let prompt = Message { text: COMPACT_PROMPT.into(), ..Default::default() };
        self.turn(agent, &session, &prompt, stop, inbox, questions, Mode::Compact).await
    }

    fn cli_model(&self, agent: &str, chosen: Option<&str>) -> Option<String> {
        let chosen = chosen.map(str::trim).filter(|m| !m.is_empty())?;
        if !matches!(agent, "claude" | "codex") || !agentbench_core::LEVELS.contains(&chosen) {
            return Some(chosen.to_string());
        }
        crate::providers::level_model(&self.store, agent, chosen)
    }

    fn pinned_model(&self, session: &Session) -> Option<Option<String>> {
        let project = self.store.project(session.project_id).ok()?;
        project.supervisor.then(|| crate::providers::supervisor_model(&self.store, &session.agent))
    }

    fn context(&self, session: &Session) -> Context {
        let project = self.store.project(session.project_id).ok();
        let branch = self.store.worktree(session.worktree_id).ok().and_then(|w| w.branch);
        let base = self.base.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let home = project.as_ref().is_some_and(|p| p.home);
        let supervisor = project.as_ref().is_some_and(|p| p.supervisor);
        Context {
            session: session.id,
            project_id: session.project_id,
            project: project.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
            project_path: project.map(|p| p.path).unwrap_or_default(),
            branch,
            home,
            supervisor,
            issue: session.issue_id,
            api_url: base.url,
            token: base.token,
            notes: self.store.notes(session.project_id).unwrap_or_default(),
            global_notes: self.store.global_notes().unwrap_or_default(),
            automation: if supervisor { crate::autopilot::summary(&self.store) } else { String::new() },
            quota: if supervisor { self.quotas.summary(crate::store::now()) } else { String::new() },
            vision: if supervisor {
                crate::autopilot::visions(&self.store)
            } else {
                self.store
                    .vision(session.project_id)
                    .ok()
                    .flatten()
                    .map(|v| v.brief())
                    .unwrap_or_default()
            },
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
        crate::quota::set_paused(&self.store, session.id, None);
        self.changed.notify_waiters();
        let cwd = PathBuf::from(&session.cwd);
        let context = self.context(session);
        let resume = session.agent_session.as_deref().filter(|_| agent.resumable());
        let chosen = match self.pinned_model(session) {
            Some(pinned) => {
                if pinned != session.model {
                    let _ = self.store.set_model(session.id, pinned.as_deref());
                }
                pinned
            }
            None => session.model.clone(),
        };
        let cli_model = self.cli_model(&session.agent, chosen.as_deref());
        let turn = Turn {
            prompt,
            files: &files,
            cwd: &cwd,
            model: cli_model.as_deref(),
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
                from: msg.from.clone(),
                typed: (mode == Mode::Chat).then_some(prompt_row.id),
                ..SinkState::default()
            }),
            stop: stop.clone(),
            inbox: inbox.clone(),
            questions: questions.clone(),
            steerable: agent.steerable(),
            api: self.api.clone(),
            runner: self.clone(),
            began: Instant::now(),
            watched: mode == Mode::Chat && (session.issue_id.is_some() || session.parent.is_some()),
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

fn clock(ms: i64) -> String {
    let secs = ms.div_euclid(1000).rem_euclid(86_400);
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn label(d: &Deferred) -> String {
    match d.worker {
        Some(w) => format!("{} {w}", d.source),
        None => d.source.clone(),
    }
}

/// Messages that waited for an idle session, as one: oldest first, repeats dropped, and only the
/// newest `DIGEST_MAX` written out.
fn digest_of(all: Vec<Deferred>) -> Message {
    let mut kept: Vec<Deferred> = Vec::new();
    for d in all {
        if !kept.iter().any(|k| k.worker == d.worker && k.source == d.source && k.text == d.text) {
            kept.push(d);
        }
    }
    let folded = kept.len().saturating_sub(DIGEST_MAX);
    let mut from: Vec<String> = Vec::new();
    for d in &kept {
        let l = label(d);
        if !from.contains(&l) {
            from.push(l);
        }
    }
    let mut lines = match kept.len() {
        1 => Vec::new(),
        n => vec![format!("{n} messages for you, sent while you were busy, oldest first:")],
    };
    if folded > 0 {
        let older: Vec<String> = kept[..folded].iter().map(label).collect();
        lines.push(format!(
            "({folded} older ones are folded in and not repeated here, from: {})",
            older.join(", ")
        ));
    }
    for d in &kept[folded..] {
        lines.push(format!("[{} UTC] {}: {}", clock(d.created_at), label(d), d.text));
    }
    Message { text: lines.join("\n\n"), attachments: Vec::new(), from }
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
    from: Vec<String>,
    typed: Option<Id>,
    captured: String,
    text: Option<(Id, String)>,
    dirty: Option<Instant>,
    thinking: Option<(Id, String)>,
    tools: HashMap<String, (Id, serde_json::Value)>,
    errored: bool,
    progress: crate::agent::progress::Watchdog,
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
    began: Instant,
    watched: bool,
}

impl Sink {
    fn watch_progress(&self, tool: &str) {
        if !self.watched || !self.steerable {
            return;
        }
        let nudge = self.lock().progress.observe(tool, self.began.elapsed());
        let key = format!("no_plan_{}", self.session);
        match nudge {
            None => {
                if !crate::agent::progress::reads_only(tool) && self.store.setting(&key).ok().flatten().is_some() {
                    let _ = self.store.delete_setting(&key);
                }
            }
            Some(crate::agent::progress::Nudge::Remind(n)) => {
                let _ = self.store.set_setting(&key, &n.to_string());
                let text = crate::agent::progress::reminder(n);
                self.inbox
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Message { text, ..Default::default() });
            }
            Some(crate::agent::progress::Nudge::Escalate(n)) => {
                let _ = self.store.set_setting(&key, &n.to_string());
                let text = format!(
                    "Session {} has made {n} read-only calls with no plan and no edit. Tell it to write its steps with todo_write, or stop it.",
                    self.session
                );
                for id in self.store.watchers(self.session).unwrap_or_default() {
                    if let Ok(watcher) = self.store.session(id) {
                        let origin = Origin::new("watchdog", Some(self.session)).urgent(true);
                        let _ = self.runner.tell(&watcher, &text, origin);
                    }
                }
            }
        }
    }

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
    fn urgent(&self) -> Option<Message> {
        let after = self
            .store
            .setting(URGENT_AFTER_SETTING)
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(URGENT_AFTER_DEFAULT);
        if self.began.elapsed() < Duration::from_secs(after) {
            return None;
        }
        self.runner.digest(self.session, |d| d.urgent)
    }

    fn pictures_removed(&self) {
        self.rewrite(agentbench_core::remove_pictures_from);
    }

    fn trimmed(&self, trims: &[agentbench_core::Trim]) {
        self.rewrite(|entry| agentbench_core::apply_trims_to(entry, trims));
    }

    fn rewrite(&self, mut change: impl FnMut(&mut Entry) -> bool) {
        if self.lock().capture || !self.lock().raw {
            return;
        }
        self.flush();
        let Ok(rows) = self.store.messages(self.session, 0) else { return };
        for mut row in rows {
            if change(&mut row.entry) {
                let _ = self.store.update_message(row.id, &row.entry);
            }
        }
        self.changed.notify_waiters();
    }

    fn sent(&self, mut messages: Vec<agentbench_core::Sent>) {
        if self.lock().capture || !self.lock().raw {
            return;
        }
        self.flush();
        let mut st = self.lock();
        let mut typed = None;
        for s in &mut messages {
            if let agentbench_core::Said::Typed { attachments, from } = &mut s.said {
                *attachments = std::mem::take(&mut st.prompt);
                *from = std::mem::take(&mut st.from);
                typed = st.typed.take();
            }
        }
        let streamed = match messages.first() {
            Some(s) if s.said == agentbench_core::Said::Kept => None,
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

    fn tell_supervisor(&self, supervisor: &Session, asked: &str, answerable: bool) {
        let me = self.store.session(self.session).ok();
        let title = me.as_ref().map(|m| m.title.clone()).unwrap_or_default();
        let issue = me
            .and_then(|m| m.issue_id)
            .map(|i| format!(", issue #{i}"))
            .unwrap_or_default();
        let how = if answerable {
            format!(
                "Reply with answer_session id={} answer=..., or escalate_question id={} recommendation=... to put it to the user. Decide yourself when you can.",
                self.session, self.session
            )
        } else {
            "This goes to the user; you are only told.".to_string()
        };
        let text = format!("Worker {} (\"{title}\"{issue}) asks:\n{asked}\n{how}", self.session);
        let origin = Origin::new("question", Some(self.session)).urgent(!answerable);
        let _ = self.runner.tell(supervisor, &text, origin);
    }

    fn tell_parent_waiting(&self, asked: &str) {
        let Some(me) = self.store.session(self.session).ok() else { return };
        let Some(parent) = me.parent.and_then(|p| self.store.session(p).ok()) else { return };
        let text = format!(
            "Session {} (\"{}\", project {}) changed state to waiting on a question: {asked}",
            me.id, me.title, me.project_id
        );
        let _ = self.runner.tell(&parent, &text, Origin::new("watch", Some(me.id)));
    }

    async fn wait(
        &self,
        id: &str,
        open: Entry,
        answered: impl FnOnce(&[String]) -> Entry,
    ) -> Result<Vec<String>, String> {
        let (tx, rx) = oneshot::channel();
        self.questions.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_string(), tx);
        let secret = match &open {
            Entry::Question { question, .. } => question.secret,
            Entry::Questions { questions, .. } => questions.iter().any(|q| q.secret),
            _ => false,
        };
        let supervisor = self.runner.supervisor_of(self.session);
        let asked = describe_question(&open);
        let msg = self.entry(open);
        let mut rx = rx;
        let mut early = None;
        if let Some(sup) = supervisor.as_ref().filter(|_| !secret) {
            let routed = Routed {
                question: id.to_string(),
                escalate: Arc::new(Notify::new()),
                recommendation: Arc::new(Mutex::new(None)),
            };
            self.runner.routed().insert(self.session, routed.clone());
            self.tell_supervisor(sup, &asked, true);
            let patience = Duration::from_secs(
                self.store
                    .setting(SUPERVISOR_PATIENCE_SETTING)
                    .ok()
                    .flatten()
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(SUPERVISOR_PATIENCE_DEFAULT),
            );
            tokio::select! {
                _ = self.stop.cancelled() => early = Some(Err("stopped".to_string())),
                _ = tokio::time::sleep(patience) => {}
                _ = routed.escalate.notified() => {}
                a = &mut rx => early = Some(a.map_err(|_| "the question was withdrawn".to_string())),
            }
            self.runner.routed().remove(&self.session);
            if early.is_none() {
                let note = routed
                    .recommendation
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .map(|r| format!("the supervisor recommends: {r}"))
                    .unwrap_or_else(|| "the supervisor did not answer, so it is yours".into());
                self.entry(Entry::Notice { text: note });
            }
        } else if let Some(sup) = supervisor.as_ref() {
            self.tell_supervisor(sup, &asked, false);
        }
        if supervisor.is_none() {
            self.tell_parent_waiting(&asked);
        }
        let answer = match early {
            Some(a) => a,
            None => {
                let _ = self.store.set_status(self.session, SessionStatus::Waiting);
                crate::tools::desktop_notice("needs you", "a chat is waiting for your answer");
                self.changed.notify_waiters();
                tokio::select! {
                    _ = self.stop.cancelled() => Err("stopped".to_string()),
                    _ = tokio::time::sleep(ANSWER_PATIENCE) => Err("nobody answered in a day".to_string()),
                    a = rx => a.map_err(|_| "the question was withdrawn".to_string()),
                }
            }
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

fn describe_question(open: &Entry) -> String {
    let one = |q: &Question| {
        if q.options.is_empty() {
            q.text.clone()
        } else {
            format!("{} (options: {})", q.text, q.options.join(" | "))
        }
    };
    match open {
        Entry::Question { question, .. } => one(question),
        Entry::Questions { questions, .. } => questions
            .iter()
            .enumerate()
            .map(|(i, q)| format!("{}. {}", i + 1, one(q)))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
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
                self.watch_progress(&name);
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
                let quota = u.quota.clone();
                let _ = self.store.add_usage(self.session, &u);
                if let Some(q) = quota {
                    self.runner.observe_quota(q);
                }
            }
            Event::Paused { until, text, long, model } => {
                crate::quota::set_paused(&self.store, self.session, Some(until));
                self.runner.quotas.block(&model, until);
                self.entry(Entry::Warning { text: text.clone() });
                let _ = self.store.set_status(self.session, SessionStatus::Waiting);
                if long {
                    crate::tools::desktop_notice("quota exhausted", &text);
                }
                self.changed.notify_waiters();
            }
            Event::Resumed => {
                crate::quota::set_paused(&self.store, self.session, None);
                self.runner.quotas.unblock(crate::quota::now());
                let _ = self.store.set_status(self.session, SessionStatus::Running);
                self.changed.notify_waiters();
            }
            Event::Notice(text) => {
                self.entry(Entry::Notice { text });
            }
            Event::Warning(text) => {
                self.entry(Entry::Warning { text });
            }
            Event::PicturesRemoved => self.pictures_removed(),
            Event::Trimmed(trims) => self.trimmed(&trims),
            Event::Diagnostics { text, broken } => {
                self.entry(Entry::Diagnostics { text, broken });
            }
            Event::Sent(messages) => self.sent(messages),
            Event::Summary(text) => {
                self.flush();
                let _ = self.store.add_message(self.session, &Entry::Summary { text });
                let _ = self.store.clear_agent_session(self.session);
                self.changed.notify_waiters();
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
        if !self.steerable || self.lock().capture {
            return None;
        }
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        let waiting = (!inbox.is_empty()).then(|| inbox.remove(0));
        drop(inbox);
        let msg = waiting.or_else(|| self.urgent())?;
        let id = self.entry(msg.entry());
        let mut st = self.lock();
        st.prompt = msg.attachments.clone();
        st.from = msg.from.clone();
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
    async fn a_claude_refusal_is_stored_and_fails_the_session() {
        let script = r#"echo '{"type":"assistant","message":{"stop_reason":"refusal","content":[{"type":"text","text":"I cannot assist with that."}]}}'"#;
        let (runner, s) = setup(script_agent("c", script));
        runner.send(&s, "x").unwrap();
        assert_eq!(wait_idle(&runner, s.id).await.status, SessionStatus::Failed);
        let all = runner.store().messages(s.id, 0).unwrap();
        assert!(all.iter().any(|m| matches!(&m.entry, Entry::Error { text } if text.contains("I cannot assist with that."))));
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

    struct Scripted(Vec<&'static str>, Mutex<Vec<String>>);

    struct SharedScripted(Arc<Scripted>);

    #[async_trait]
    impl Agent for SharedScripted {
        fn name(&self) -> &str {
            "scripted"
        }
        fn label(&self) -> &str {
            "Scripted"
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
            for (i, tool) in self.0.0.iter().enumerate() {
                io.emit(Event::ToolStart {
                    call_id: i.to_string(),
                    name: (*tool).into(),
                    input: serde_json::Value::Null,
                });
                io.emit(Event::ToolEnd {
                    call_id: i.to_string(),
                    name: (*tool).into(),
                    output: String::new(),
                    error: false,
                });
                while let Some(steer) = io.steer() {
                    self.0.1.lock().unwrap().push(steer.text);
                }
            }
            Ok(())
        }
    }

    async fn run_scripted(
        tools: Vec<&'static str>,
        with_issue: bool,
        with_parent: bool,
    ) -> (Vec<String>, Runner, Session) {
        let script = Arc::new(Scripted(tools, Mutex::new(Vec::new())));
        let agent = SharedScripted(script.clone());
        let store = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&store, "/tmp");
        let issue = with_issue.then(|| {
            store
                .add_issue(&agentbench_core::NewIssue {
                    project_id: main.project_id,
                    title: "t".into(),
                    ..Default::default()
                })
                .unwrap()
                .id
        });
        let s = store.add_session(main.id, "scripted", "t", None, issue).unwrap();
        if with_parent {
            let lead = store.add_session(main.id, "scripted", "lead", None, None).unwrap();
            store.set_parent(s.id, Some(lead.id)).unwrap();
        }
        let s = store.session(s.id).unwrap();
        let runner = Runner::new(store, vec![Box::new(agent)]);
        runner.send(&s, "go").unwrap();
        wait_idle(&runner, s.id).await;
        let got = script.1.lock().unwrap().clone();
        (got, runner, s)
    }

    async fn reminders_for(tools: Vec<&'static str>) -> Vec<String> {
        run_scripted(tools, true, false).await.0
    }

    #[tokio::test]
    async fn a_worker_that_only_reads_is_told_to_write_its_steps_then_a_planner_is_left_alone() {
        let mut reads = vec!["read_file"; 16];
        reads.push("todo_write");
        reads.extend(vec!["read_file"; 40]);
        let (got, runner, s) = run_scripted(reads, true, false).await;
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("read 15 files") && got[0].contains("todo_write"));
        let lines: Vec<String> = runner
            .store()
            .messages(s.id, 0)
            .unwrap()
            .into_iter()
            .map(|m| match m.entry {
                Entry::Tool { name, .. } => format!("tool {name}"),
                Entry::User { text, .. } => format!("user {text}"),
                other => format!("{other:?}"),
            })
            .collect();
        let at = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
        println!("{}", lines.join("\n"));
        assert!(at("read 15 files") > 14 && at("read 15 files") < at("tool todo_write"));
        assert_eq!(runner.store().setting(&format!("no_plan_{}", s.id)).unwrap(), None);
        let mut planned = vec!["todo_write"];
        planned.extend(vec!["read_file"; 40]);
        assert!(reminders_for(planned).await.is_empty());
    }

    #[tokio::test]
    async fn bash_exploring_does_not_silence_the_watchdog() {
        let got = reminders_for(vec!["bash"; 15]).await;
        assert_eq!(got.len(), 1, "{got:?}");
    }

    #[tokio::test]
    async fn a_chat_with_neither_issue_nor_parent_is_not_watched_but_a_parented_one_is() {
        assert!(run_scripted(vec!["read_file"; 16], false, false).await.0.is_empty());
        assert_eq!(run_scripted(vec!["read_file"; 16], false, true).await.0.len(), 1);
    }

    #[tokio::test]
    async fn the_plan_gap_shows_on_the_session_and_goes_with_it() {
        let (_, runner, s) = run_scripted(vec!["read_file"; 20], true, false).await;
        let shown = runner.with_queued(vec![runner.store().session(s.id).unwrap()]);
        assert_eq!(shown[0].no_plan_reads, Some(15));
        runner.store().delete_session(s.id).unwrap();
        assert_eq!(runner.store().setting(&format!("no_plan_{}", s.id)).unwrap(), None);
    }

    #[tokio::test]
    async fn forty_five_reads_tell_the_watcher() {
        let script = Arc::new(Scripted(vec!["read_file"; 46], Mutex::new(Vec::new())));
        let store = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&store, "/tmp");
        let issue = store
            .add_issue(&agentbench_core::NewIssue {
                project_id: main.project_id,
                title: "t".into(),
                ..Default::default()
            })
            .unwrap();
        let worker = store.add_session(main.id, "scripted", "w", None, Some(issue.id)).unwrap();
        let lead = store.add_session(main.id, "scripted", "lead", None, None).unwrap();
        store.add_watch(worker.id, lead.id).unwrap();
        let runner = Runner::new(store, vec![Box::new(SharedScripted(script))]);
        runner.send(&worker, "go").unwrap();
        wait_idle(&runner, worker.id).await;
        wait_idle(&runner, lead.id).await;
        let said = runner.store().messages(lead.id, 0).unwrap();
        assert!(
            said.iter().any(|m| matches!(&m.entry, Entry::User { text, .. } if text.contains("46 read-only calls") || text.contains("45 read-only calls"))),
            "{said:?}"
        );
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

    struct Eager(Mutex<Vec<String>>);

    #[async_trait]
    impl Agent for Eager {
        fn name(&self) -> &str {
            "eager"
        }
        fn label(&self) -> &str {
            "Eager"
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
        fn compact_at(&self) -> u64 {
            80
        }
        async fn run(&self, turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
            let compacting = turn.prompt.starts_with("Summarise");
            if compacting {
                io.emit(Event::Text { delta: "A summary.".into() });
                for _ in 0..30 {
                    if let Some(steer) = io.steer() {
                        self.0.lock().unwrap().push(format!("stolen: {}", steer.text));
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                return Ok(());
            }
            self.0.lock().unwrap().push(format!("turn: {}", turn.prompt));
            io.emit(Event::Usage(Usage {
                context: if turn.prompt == "hello" { 900 } else { 0 },
                window: 1000,
                ..Usage::default()
            }));
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_message_sent_during_compaction_waits_for_the_next_turn() {
        let seen = Arc::new(Eager(Mutex::new(Vec::new())));
        struct Shared(Arc<Eager>);
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
                "test".into()
            }
            fn resumable(&self) -> bool {
                false
            }
            fn steerable(&self) -> bool {
                true
            }
            fn compact_at(&self) -> u64 {
                80
            }
            async fn run(&self, turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
                self.0.run(turn, io).await
            }
        }
        let (runner, s) = setup(Box::new(Shared(seen.clone())));
        runner.send(&s, "hello").unwrap();
        for _ in 0..200 {
            let started =
                runner.store().messages(s.id, 0).unwrap().iter().any(
                    |m| matches!(&m.entry, Entry::Notice { text } if text.contains("compacted")),
                );
            if started {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        runner.send(&s, "later").unwrap();
        wait_idle(&runner, s.id).await;
        let log = seen.0.lock().unwrap().clone();
        assert!(!log.iter().any(|l| l.starts_with("stolen")), "{log:?}");
        assert_eq!(log, ["turn: hello", "turn: later"]);
        let msgs: Vec<Entry> =
            runner.store().messages(s.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        assert!(msgs.contains(&Entry::Summary { text: "A summary.".into() }), "{msgs:?}");
    }

    #[derive(Clone)]
    struct Foldy {
        log: Arc<Mutex<Vec<String>>>,
        context: u64,
        compact_at: u64,
        shrinks: Arc<Mutex<Vec<usize>>>,
        delay: u64,
        idle: u64,
    }

    impl Foldy {
        fn new(context: u64, compact_at: u64, shrinks: &[usize]) -> Foldy {
            Foldy {
                log: Arc::default(),
                context,
                compact_at,
                shrinks: Arc::new(Mutex::new(shrinks.to_vec())),
                delay: 0,
                idle: 240,
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Agent for Foldy {
        fn name(&self) -> &str {
            "foldy"
        }
        fn label(&self) -> &str {
            "Foldy"
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
        fn compact_at(&self) -> u64 {
            self.compact_at
        }
        fn folding(&self) -> Option<crate::agent::Folding> {
            Some(crate::agent::Folding { idle: Duration::from_secs(self.idle), at: 50 })
        }
        async fn fold(
            &self,
            _entries: &[Entry],
            _model: Option<&str>,
            _stop: &CancellationToken,
        ) -> Result<Option<crate::agent::Folded>, String> {
            tokio::time::sleep(Duration::from_millis(self.delay)).await;
            self.log.lock().unwrap().push("fold".into());
            let mut shrinks = self.shrinks.lock().unwrap();
            if shrinks.is_empty() {
                return Ok(None);
            }
            Ok(Some(crate::agent::Folded {
                covers: 2,
                topic: "login".into(),
                text: "we fixed login".into(),
                before: 1000,
                after: shrinks.remove(0),
            }))
        }
        async fn run(&self, turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
            if turn.prompt.starts_with("Summarise") {
                io.emit(Event::Text { delta: "A summary.".into() });
                return Ok(());
            }
            self.log.lock().unwrap().push(format!("turn: {}", turn.prompt));
            if turn.prompt == "slow" {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            io.emit(Event::Usage(Usage {
                context: self.context,
                window: 1000,
                ..Usage::default()
            }));
            Ok(())
        }
    }

    fn rows(runner: &Runner, id: SessionId) -> Vec<Entry> {
        runner.store().messages(id, 0).unwrap().into_iter().map(|m| m.entry).collect()
    }

    fn folds(rows: &[Entry]) -> usize {
        rows.iter().filter(|e| matches!(e, Entry::Section { .. })).count()
    }

    async fn idle_chat(agent: &Foldy) -> (Runner, Session) {
        let (runner, s) = setup(Box::new(agent.clone()));
        runner.send(&s, "hello").unwrap();
        let done = wait_idle(&runner, s.id).await;
        (runner, done)
    }

    #[tokio::test]
    async fn an_idle_chat_is_folded_once_the_delay_has_passed() {
        let agent = Foldy::new(600, 0, &[400]);
        let (runner, s) = idle_chat(&agent).await;
        let delay = 240_000;
        assert_eq!(runner.fold_idle(s.updated_at + delay - 1), 0);
        assert_eq!(agent.log(), ["turn: hello"]);
        assert_eq!(runner.fold_idle(s.updated_at + delay), 1);
        wait_idle(&runner, s.id).await;
        assert_eq!(agent.log(), ["turn: hello", "fold"]);
        let rows = rows(&runner, s.id);
        assert!(
            rows.contains(&Entry::Section {
                text: "we fixed login".into(),
                covers: 2,
                topic: "login".into()
            }),
            "{rows:?}"
        );
        assert!(rows.iter().any(|e| matches!(e, Entry::Notice { text }
            if text == "folded 2 older messages (topic: login) into a summary while idle")));
        let after = runner.store().session(s.id).unwrap();
        assert_eq!(after.usage.context, 240, "the reading shrinks with the replay");
        assert_eq!(runner.fold_idle(i64::MAX / 2), 0, "once per quiet spell");
    }

    #[tokio::test]
    async fn a_chat_under_the_threshold_is_not_folded() {
        let agent = Foldy::new(400, 0, &[100]);
        let (runner, _) = idle_chat(&agent).await;
        assert_eq!(runner.fold_idle(i64::MAX / 2), 0);
        assert_eq!(agent.log(), ["turn: hello"]);
    }

    #[tokio::test]
    async fn a_chat_with_a_turn_running_is_not_folded() {
        let agent = Foldy::new(600, 0, &[100]);
        let (runner, s) = setup(Box::new(agent.clone()));
        runner.send(&s, "slow").unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(runner.fold_idle(i64::MAX / 2), 0);
        wait_idle(&runner, s.id).await;
        assert_eq!(agent.log(), ["turn: slow"]);
    }

    #[tokio::test]
    async fn a_message_during_an_idle_fold_is_delivered_after_it() {
        let mut agent = Foldy::new(600, 0, &[400]);
        agent.delay = 200;
        let (runner, s) = idle_chat(&agent).await;
        assert_eq!(runner.fold_idle(s.updated_at + 240_000), 1);
        runner.send(&s, "later").unwrap();
        for _ in 0..200 {
            if agent.log().len() == 3 && !runner.is_running(s.id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(agent.log(), ["turn: hello", "fold", "turn: later"]);
        let rows = rows(&runner, s.id);
        assert_eq!(folds(&rows), 1);
        assert!(rows.contains(&Entry::user("later")));
    }

    #[tokio::test]
    async fn stopping_an_idle_fold_writes_nothing() {
        let mut agent = Foldy::new(600, 0, &[400]);
        agent.delay = 300;
        let (runner, s) = idle_chat(&agent).await;
        assert_eq!(runner.fold_idle(s.updated_at + 240_000), 1);
        assert!(runner.interrupt(s.id));
        wait_idle(&runner, s.id).await;
        assert_eq!(folds(&rows(&runner, s.id)), 0);
    }

    #[tokio::test]
    async fn a_full_chat_is_folded_before_it_is_compacted() {
        let agent = Foldy::new(900, 80, &[400]);
        let (runner, s) = setup(Box::new(agent.clone()));
        runner.send(&s, "hello").unwrap();
        wait_idle(&runner, s.id).await;
        let rows = rows(&runner, s.id);
        assert_eq!(folds(&rows), 1);
        assert!(!rows.iter().any(|e| matches!(e, Entry::Summary { .. })), "{rows:?}");
        assert_eq!(agent.log(), ["turn: hello", "fold"]);
    }

    #[tokio::test]
    async fn full_compaction_is_the_last_resort_when_folds_cannot_make_room() {
        let agent = Foldy::new(900, 80, &[900]);
        let (runner, s) = setup(Box::new(agent.clone()));
        runner.send(&s, "hello").unwrap();
        let done = wait_idle(&runner, s.id).await;
        let rows = rows(&runner, s.id);
        assert_eq!(folds(&rows), 1);
        assert!(rows.contains(&Entry::Summary { text: "A summary.".into() }), "{rows:?}");
        assert_eq!(done.usage.context, 0);
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

    struct Pacer(Arc<tokio::sync::Notify>);

    #[async_trait]
    impl Agent for Pacer {
        fn name(&self) -> &str {
            "pacer"
        }
        fn label(&self) -> &str {
            "Pacer"
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
            let reading = |pct: f64, resets: i64| Usage {
                quota: Some(agentbench_core::Quota {
                    host: "h".into(),
                    window: "5h".into(),
                    used_pct: pct,
                    resets_at: resets,
                    ..Default::default()
                }),
                ..Usage::default()
            };
            let resets = crate::quota::now() + 7_200_000;
            io.emit(Event::Usage(reading(78.0, resets)));
            io.emit(Event::Usage(reading(80.0, resets)));
            io.emit(Event::Paused { until: resets, text: "h: quota exhausted, resumes at 14:30 UTC".into(), long: true, model: String::new() });
            self.0.notified().await;
            io.emit(Event::Resumed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_quota_pause_waits_instead_of_failing_and_a_hint_goes_to_each_running_chat_once() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let (runner, s) = setup(Box::new(Pacer(gate.clone())));
        runner.send(&s, "go").unwrap();
        for _ in 0..200 {
            if runner.store().session(s.id).unwrap().status == SessionStatus::Waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let paused = runner.store().session(s.id).unwrap();
        assert_eq!(paused.status, SessionStatus::Waiting);
        assert!(runner.with_queued(vec![paused])[0].paused_until.is_some());
        assert!(runner.quotas.exhausted_until("", crate::quota::now()).is_some());
        let msgs = runner.store().messages(s.id, 0).unwrap();
        let hints = msgs
            .iter()
            .filter(|m| matches!(&m.entry, Entry::Notice { text } if text.starts_with("Quota 5h at 78%")))
            .count();
        assert_eq!(hints, 1, "{msgs:?}");
        assert!(msgs.iter().any(|m| matches!(&m.entry, Entry::Warning { text } if text.contains("quota exhausted, resumes at"))));
        gate.notify_one();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Idle);
        assert!(crate::quota::paused_until(runner.store(), s.id).is_none());
    }

    #[tokio::test]
    async fn the_real_yalr_rate_limit_error_waits_for_the_reset_instead_of_failing() {
        use crate::agent::native::{Model, Native};
        use crate::quota::fake::{ok, server};
        use serde_json::json;
        let refusal = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"error": {"message": "Provider error: Rate limit exceeded. Retry after: 387042000ms"}})
        );
        let done = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": {"content": "back"}}]})
        );
        let (url, _) = server(vec![ok(refusal), ok(done)], None);
        let start = crate::quota::now();
        let mut native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        native.pace.clock = crate::quota::Clock::fake(start);
        let clock = native.pace.clock.clone();
        let (runner, s) = setup(Box::new(native));
        runner.send(&s, "go").unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Idle, "not failed");
        assert_eq!(clock.now(), start + 387_042_000, "resumed at the reset");
        let msgs = runner.store().messages(s.id, 0).unwrap();
        assert!(!msgs.iter().any(|m| matches!(m.entry, Entry::Error { .. })), "{msgs:?}");
        let wait = msgs
            .iter()
            .find_map(|m| match &m.entry {
                Entry::Warning { text } if text.contains("quota exhausted, resumes at") => Some(text.clone()),
                _ => None,
            })
            .expect("a waiting notice in the chat");
        assert!(wait.contains("4d 11h"), "{wait}");
        let notices = crate::tools::NOTICES.lock().unwrap();
        assert!(notices.iter().any(|(t, m)| t == "quota exhausted" && m.contains("resumes at")), "{notices:?}");
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

    struct Trimmer;

    #[async_trait::async_trait]
    impl Agent for Trimmer {
        fn name(&self) -> &str {
            "trimmer"
        }
        fn label(&self) -> &str {
            "Trimmer"
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
            io.emit(Event::Sent(vec![agentbench_core::Sent {
                message: serde_json::json!({"role": "user", "content": [
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,OLD"}},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,NEW"}},
                ]}),
                said: agentbench_core::Said::Plain,
            }]));
            io.emit(Event::Trimmed(vec![agentbench_core::Trim::Picture {
                url: "data:image/png;base64,OLD".into(),
                text: "(picture removed to fit the request)".into(),
            }]));
            io.emit(Event::Done);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_trim_is_kept_in_the_stored_rows() {
        let (runner, s) = setup(Box::new(Trimmer));
        runner.send(&s, "look").unwrap();
        wait_idle(&runner, s.id).await;
        let rows = serde_json::to_string(&runner.store().messages(s.id, 0).unwrap()).unwrap();
        assert!(!rows.contains("base64,OLD"), "{rows}");
        assert!(rows.contains("base64,NEW"), "{rows}");
        assert!(rows.contains("to fit the request"), "{rows}");
    }

    struct Refused;

    #[async_trait::async_trait]
    impl Agent for Refused {
        fn name(&self) -> &str {
            "refused"
        }
        fn label(&self) -> &str {
            "Refused"
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
            io.emit(Event::Sent(vec![agentbench_core::Sent {
                message: serde_json::json!({"role": "user", "content": [
                    {"type": "text", "text": "look"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}},
                ]}),
                said: agentbench_core::Said::Plain,
            }]));
            io.emit(Event::PicturesRemoved);
            io.emit(Event::Warning("retrying in 2s (1/3)".into()));
            io.emit(Event::Done);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_removed_picture_is_gone_from_the_stored_rows_and_a_retry_is_a_warning() {
        let (runner, s) = setup(Box::new(Refused));
        runner.send(&s, "look").unwrap();
        wait_idle(&runner, s.id).await;
        let rows = runner.store().messages(s.id, 0).unwrap();
        assert!(!serde_json::to_string(&rows).unwrap().contains("image_url"));
        assert!(serde_json::to_string(&rows).unwrap().contains("picture removed"));
        assert!(rows.iter().any(|m| matches!(&m.entry, Entry::Warning { text } if text.contains("retrying"))));
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
    async fn clearing_a_chat_names_it_again_from_the_new_conversation() {
        let (url, seen) = crate::titles::fake::server(vec![
            (200, crate::titles::fake::completion("First topic")),
            (200, crate::titles::fake::completion("Second topic")),
        ]);
        let script = r#"read p; echo "{\"text\":\"hi\"}""#;
        let (runner, s) = titled(url, script_agent("echo", script), true);
        runner.send(&s, "first question").unwrap();
        wait_idle(&runner, s.id).await;
        assert!(title_becomes(&runner, s.id, "First topic").await);
        assert!(!runner.store().title_is_auto(s.id).unwrap());
        let s = runner.store().session(s.id).unwrap();
        runner.send(&s, "/clear").unwrap();
        assert!(runner.store().title_is_auto(s.id).unwrap(), "clearing frees the title");
        runner.send(&s, "second question").unwrap();
        wait_idle(&runner, s.id).await;
        assert!(title_becomes(&runner, s.id, "Second topic").await);
        let sent = seen.lock().unwrap();
        let prompt = sent[1]["messages"][1]["content"].as_str().unwrap();
        assert!(
            prompt.contains("second question") && !prompt.contains("first question"),
            "{prompt}"
        );
    }

    #[tokio::test]
    async fn a_generated_title_is_refreshed_every_few_turns() {
        let (url, seen) = crate::titles::fake::server(vec![
            (200, crate::titles::fake::completion("First topic")),
            (200, crate::titles::fake::completion("Moved on to tests")),
        ]);
        let script = r#"read p; echo "{\"text\":\"hi\"}""#;
        let (runner, s) = titled(url, script_agent("echo", script), true);
        for n in 1..=5 {
            let s = runner.store().session(s.id).unwrap();
            runner.send(&s, &format!("question {n}")).unwrap();
            wait_idle(&runner, s.id).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let asked = seen.lock().unwrap().len();
            match n {
                1..=4 => assert_eq!(asked, 1.min(n), "turn {n}"),
                _ => assert_eq!(asked, 2, "turn {n}"),
            }
        }
        assert!(title_becomes(&runner, s.id, "Moved on to tests").await);
        let sent = seen.lock().unwrap();
        let prompt = sent[1]["messages"][1]["content"].as_str().unwrap();
        assert!(prompt.contains("Current title: First topic"), "{prompt}");
        assert!(prompt.contains("question 5") && !prompt.contains("question 1\n"), "{prompt}");
    }

    #[tokio::test]
    async fn a_title_someone_set_by_hand_is_never_refreshed() {
        let (url, seen) =
            crate::titles::fake::server(vec![(200, crate::titles::fake::completion("Not this"))]);
        let script = r#"read p; echo "{\"text\":\"hi\"}""#;
        let (runner, s) = titled(url, script_agent("echo", script), false);
        for n in 1..=6 {
            let s = runner.store().session(s.id).unwrap();
            runner.send(&s, &format!("q{n}")).unwrap();
            wait_idle(&runner, s.id).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(seen.lock().unwrap().is_empty());
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
    async fn a_watcher_is_told_once_when_the_watched_session_finishes() {
        let script = r#"read p; sleep 0.2; echo "{\"text\":\"all done\"}""#;
        let (runner, watched) = setup(script_agent("echo", script));
        let main = runner.store().main_worktree(watched.project_id).unwrap();
        let watcher = runner.store().add_session(main.id, "echo", "boss", None, None).unwrap();
        runner.store().add_watch(watched.id, watcher.id).unwrap();

        runner.send(&watched, "do it").unwrap();
        wait_idle(&runner, watched.id).await;
        for _ in 0..200 {
            if runner.store().messages(watcher.id, 0).unwrap().iter().any(
                |m| matches!(&m.entry, Entry::User { text, .. } if text.contains("changed state to idle")),
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let told: Vec<String> = runner
            .store()
            .messages(watcher.id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::User { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].contains(&format!("Session {}", watched.id)), "{told:?}");
        assert!(!told[0].contains("all done"), "{told:?}");
        assert!(told[0].contains("changed state to idle"), "{told:?}");
        assert!(runner.store().take_watchers(watched.id).unwrap().is_empty(), "fires once");
    }

    struct Rig {
        runner: Runner,
        store: Store,
        boss: Session,
        worker: Session,
        _dir: tempfile::TempDir,
    }

    fn rig(agent: Box<dyn Agent>, parented: bool) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let store = Store::memory().unwrap();
        let p = crate::projects::add(&store, &main, None, false).unwrap();
        let tree = store.main_worktree(p.id).unwrap();
        let sup = crate::projects::ensure_supervisor(&store, &dir.path().join("sup")).unwrap();
        let sup_tree = store.main_worktree(sup.id).unwrap();
        let name = agent.name().to_string();
        let boss = store.add_session(sup_tree.id, "idle", "boss", None, None).unwrap();
        let worker = store.add_session(tree.id, &name, "worker", None, None).unwrap();
        if parented {
            store.set_parent(worker.id, Some(boss.id)).unwrap();
        }
        let runner = Runner::new(store.clone(), vec![agent, script_agent("idle", "read p; sleep 30")]);
        Rig { runner, store, boss, worker, _dir: dir }
    }

    async fn until(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..200 {
            if f() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn boss_was_told(r: &Rig) -> Vec<String> {
        r.store
            .messages(r.boss.id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::User { text, .. } => Some(text),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_supervisor_watching_its_worker_is_asked_for_a_verdict_when_its_turn_ends() {
        let r = rig(script_agent("echo", r#"read p; echo '{"text":"Pushed. Let me know if you want X."}'"#), true);
        r.store.add_watch(r.worker.id, r.boss.id).unwrap();
        r.runner.send(&r.worker, "go").unwrap();
        wait_idle(&r.runner, r.worker.id).await;
        assert!(until(|| !boss_was_told(&r).is_empty()).await);
        let told = boss_was_told(&r).remove(0);
        assert!(told.contains("judge_session") && told.contains("Let me know if you want X"), "{told}");
    }

    #[tokio::test]
    async fn a_workers_question_goes_to_the_supervisor_and_its_answer_returns() {
        use crate::agent::Api;
        let r = rig(Box::new(Asker), true);
        r.runner.send(&r.worker, "pick").unwrap();
        assert!(until(|| !boss_was_told(&r).is_empty()).await, "the supervisor is told");
        let told = boss_was_told(&r).remove(0);
        assert!(told.contains("red or blue?") && told.contains("red | blue"), "{told}");
        assert!(told.contains(&r.worker.id.to_string()) && told.contains("worker"), "{told}");
        let from: Vec<Vec<String>> = r
            .store
            .messages(r.boss.id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::User { from, .. } => Some(from),
                _ => None,
            })
            .collect();
        assert_eq!(from[0], [format!("question {}", r.worker.id)], "a routed question is a system message");
        assert_eq!(r.store.session(r.worker.id).unwrap().status, SessionStatus::Running);
        let args = serde_json::json!({"id": r.worker.id.to_string(), "answer": "blue"});
        r.runner.api.call("answer_session", &args, r.boss.id).unwrap();
        wait_idle(&r.runner, r.worker.id).await;
        let msgs: Vec<Entry> =
            r.store.messages(r.worker.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        assert_eq!(msgs.last().unwrap().text(), "you picked blue");
    }

    #[tokio::test]
    async fn escalating_puts_the_question_to_the_user_with_a_recommendation() {
        use crate::agent::Api;
        let r = rig(Box::new(Asker), true);
        r.runner.send(&r.worker, "pick").unwrap();
        assert!(until(|| !boss_was_told(&r).is_empty()).await);
        let args = serde_json::json!({"id": r.worker.id.to_string(), "recommendation": "blue"});
        r.runner.api.call("escalate_question", &args, r.boss.id).unwrap();
        assert!(
            until(|| r.store.session(r.worker.id).unwrap().status == SessionStatus::Waiting).await
        );
        let notes: Vec<String> = r
            .store
            .messages(r.worker.id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::Notice { text } => Some(text),
                _ => None,
            })
            .collect();
        assert!(notes.iter().any(|n| n.contains("recommends: blue")), "{notes:?}");
    }

    #[tokio::test]
    async fn a_child_waiting_on_a_question_tells_a_non_supervisor_parent_once() {
        let r = rig(Box::new(Asker), false);
        let tree = r.store.main_worktree(r.worker.project_id).unwrap();
        let lead = r.store.add_session(tree.id, "idle", "lead", None, None).unwrap();
        r.store.set_parent(r.worker.id, Some(lead.id)).unwrap();
        r.runner.send(&r.worker, "pick").unwrap();
        assert!(until(|| r.store.session(r.worker.id).unwrap().status == SessionStatus::Waiting).await);
        assert!(until(|| r.store.deferred_count(lead.id).unwrap() + r.store.messages(lead.id, 0).unwrap().len() > 0).await);
        let told: Vec<String> = r
            .store
            .messages(lead.id, 0)
            .unwrap()
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::User { text, .. } => Some(text),
                _ => None,
            })
            .filter(|t| t.contains("red or blue?"))
            .collect();
        assert_eq!(told.len() + r.store.deferred_count(lead.id).unwrap() as usize, 1, "{told:?}");
    }

    #[tokio::test]
    async fn an_unanswered_routed_question_falls_to_the_user_after_the_timeout() {
        let r = rig(Box::new(Asker), true);
        r.store.set_setting(SUPERVISOR_PATIENCE_SETTING, "0").unwrap();
        r.runner.send(&r.worker, "pick").unwrap();
        assert!(
            until(|| r.store.session(r.worker.id).unwrap().status == SessionStatus::Waiting).await
        );
    }

    #[tokio::test]
    async fn a_secret_always_goes_to_the_user_and_the_supervisor_is_only_told() {
        let r = rig(Box::new(SecretAsker), true);
        r.runner.send(&r.worker, "key").unwrap();
        assert!(
            until(|| r.store.session(r.worker.id).unwrap().status == SessionStatus::Waiting).await
        );
        let told = boss_was_told(&r).join("\n");
        assert!(told.contains("only told"), "{told}");
    }

    #[tokio::test]
    async fn a_session_without_a_supervisor_still_asks_the_user() {
        let r = rig(Box::new(Asker), false);
        r.runner.send(&r.worker, "pick").unwrap();
        assert!(
            until(|| r.store.session(r.worker.id).unwrap().status == SessionStatus::Waiting).await
        );
        assert!(boss_was_told(&r).is_empty());
    }

    #[tokio::test]
    async fn the_supervisor_stops_and_closes_another_projects_session() {
        use crate::agent::Api;
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let store = Store::memory().unwrap();
        let p = crate::projects::add(&store, &main, None, false).unwrap();
        let tree = store.main_worktree(p.id).unwrap();
        let sup = crate::projects::ensure_supervisor(&store, &dir.path().join("sup")).unwrap();
        let sup_tree = store.main_worktree(sup.id).unwrap();
        let boss = store.add_session(sup_tree.id, "echo", "boss", None, None).unwrap();
        let worker = store.add_session(tree.id, "echo", "worker", None, None).unwrap();
        let script = r#"read p; sleep 5; echo "{\"text\":\"late\"}""#;
        let runner = Runner::new(store.clone(), vec![script_agent("echo", script)]);
        runner.send(&worker, "slow").unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(runner.is_running(worker.id));

        let id = serde_json::json!({"id": worker.id.to_string()});
        let out = runner.api.call("stop_session", &id, boss.id).unwrap();
        assert_eq!(out["was_running"], true);
        wait_idle(&runner, worker.id).await;
        assert_eq!(store.session(worker.id).unwrap().status, SessionStatus::Stopped);

        runner.api.call("close_session", &id, boss.id).unwrap();
        assert!(store.session(worker.id).is_err(), "the chat is gone");
        assert!(store.main_worktree(p.id).is_ok(), "the worktree stays");
    }

    #[tokio::test]
    async fn the_supervisor_chat_defaults_to_the_high_model_level_and_workers_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let store = Store::memory().unwrap();
        let p = crate::projects::add(&store, &main, None, false).unwrap();
        crate::projects::ensure_supervisor(&store, &dir.path().join("sup")).unwrap();
        let runner = Runner::new(store.clone(), vec![script_agent("echo", "read p")]);

        let sup = runner.open_supervisor("echo").unwrap();
        assert_eq!(sup.model.as_deref(), Some("high"));
        store.set_model(sup.id, None).unwrap();
        assert_eq!(runner.open_supervisor("echo").unwrap().model.as_deref(), Some("high"));

        let worker = runner
            .start_session(NewSession {
                project_id: p.id,
                agent: "echo".into(),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(worker.model, None);
    }

    #[tokio::test]
    async fn the_supervisor_runs_every_turn_on_the_supervisor_model_and_refuses_model_down() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let store = Store::memory().unwrap();
        let p = crate::projects::add(&store, &main, None, false).unwrap();
        crate::projects::ensure_supervisor(&store, &dir.path().join("sup")).unwrap();
        let seen = Arc::new(Echo(Mutex::new(Vec::new())));
        struct Shared(Arc<Echo>);
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
        let pinned = crate::providers::supervisor_model(&store, "echo");

        let sup = runner.open_supervisor("echo").unwrap();
        store.set_model(sup.id, Some("high")).unwrap();
        runner.send(&store.session(sup.id).unwrap(), "hello").unwrap();
        wait_idle(&runner, sup.id).await;
        assert_eq!(seen.0.lock().unwrap().last().unwrap().1, pinned);

        let err = runner.send(&store.session(sup.id).unwrap(), "/model low").unwrap_err();
        assert!(err.0.contains("pinned"), "{}", err.0);
        assert_ne!(store.session(sup.id).unwrap().model.as_deref(), Some("low"));
        let err = runner.send(&store.session(sup.id).unwrap(), "/model medium").unwrap_err();
        assert!(err.0.contains("pinned"), "{}", err.0);
        runner.send(&store.session(sup.id).unwrap(), "/model high").unwrap();
        assert_eq!(store.session(sup.id).unwrap().model.as_deref(), Some("high"));

        let worker = runner
            .start_session(NewSession { project_id: p.id, agent: "echo".into(), ..Default::default() })
            .unwrap();
        runner.send(&worker, "/model low").unwrap();
        assert_eq!(store.session(worker.id).unwrap().model.as_deref(), Some("low"));
        runner.send(&store.session(worker.id).unwrap(), "hi").unwrap();
        wait_idle(&runner, worker.id).await;
        assert_eq!(seen.0.lock().unwrap().last().unwrap().1.as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn the_supervisor_hears_back_from_a_session_it_started() {
        use crate::agent::Api;
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        crate::git::testing::repo(&main);
        let store = Store::memory().unwrap();
        let p = crate::projects::add(&store, &main, None, false).unwrap();
        let sup = crate::projects::ensure_supervisor(&store, &dir.path().join("sup")).unwrap();
        let sup_tree = store.main_worktree(sup.id).unwrap();
        let boss = store.add_session(sup_tree.id, "echo", "boss", None, None).unwrap();
        let script = r#"read p; sleep 0.2; echo "{\"text\":\"built it\"}""#;
        let runner = Runner::new(store.clone(), vec![script_agent("echo", script)]);

        let out = runner
            .api
            .call("start_session", &serde_json::json!({"project": p.id, "prompt": "go"}), boss.id)
            .unwrap();
        let child: SessionId = out["session"].as_str().unwrap().parse().unwrap();
        wait_idle(&runner, child).await;
        for _ in 0..200 {
            if !store.messages(boss.id, 0).unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let first = store.messages(boss.id, 0).unwrap().remove(0).entry;
        assert!(first.text().contains(&format!("Judge worker {child}")), "{first:?}");
        assert!(first.text().contains("built it") && first.text().contains("judge_session"));
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

#[cfg(test)]
mod deferred_tests {
    use super::*;

    struct Recorder {
        name: &'static str,
        log: Arc<Mutex<Vec<String>>>,
        ms: u64,
    }

    #[async_trait]
    impl Agent for Recorder {
        fn name(&self) -> &str {
            self.name
        }
        fn label(&self) -> &str {
            self.name
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
            self.log.lock().unwrap().push(format!("turn: {}", turn.prompt));
            let end = Instant::now() + Duration::from_millis(self.ms);
            while Instant::now() < end {
                if let Some(s) = io.steer() {
                    self.log.lock().unwrap().push(format!("steer: {}", s.text));
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            io.emit(Event::Text { delta: "ok".into() });
            Ok(())
        }
    }

    struct Rig {
        runner: Runner,
        store: Store,
        boss: Session,
        worker: Session,
        boss_log: Arc<Mutex<Vec<String>>>,
        worker_log: Arc<Mutex<Vec<String>>>,
        _dir: tempfile::TempDir,
    }

    fn rig_in(dir: tempfile::TempDir, store: Store) -> Rig {
        let boss_log = Arc::new(Mutex::new(Vec::new()));
        let worker_log = Arc::new(Mutex::new(Vec::new()));
        let sup = match store.supervisor_project().unwrap() {
            Some(p) => p,
            None => crate::projects::ensure_supervisor(&store, &dir.path().join("sup")).unwrap(),
        };
        let existing = store.sessions().unwrap();
        let (boss, worker) = match existing.iter().find(|s| s.project_id == sup.id) {
            Some(b) => (b.clone(), existing.iter().find(|s| s.project_id != sup.id).unwrap().clone()),
            None => {
                let main = dir.path().join("r");
                crate::git::testing::repo(&main);
                let p = crate::projects::add(&store, &main, None, false).unwrap();
                let tree = store.main_worktree(p.id).unwrap();
                let sup_tree = store.main_worktree(sup.id).unwrap();
                let boss = store.add_session(sup_tree.id, "boss", "boss", None, None).unwrap();
                let worker = store.add_session(tree.id, "worker", "worker", None, None).unwrap();
                store.set_parent(worker.id, Some(boss.id)).unwrap();
                (boss, worker)
            }
        };
        let runner = Runner::new(
            store.clone(),
            vec![
                Box::new(Recorder { name: "boss", log: boss_log.clone(), ms: 800 }),
                Box::new(Recorder { name: "worker", log: worker_log.clone(), ms: 300 }),
            ],
        );
        Rig { runner, store, boss, worker, boss_log, worker_log, _dir: dir }
    }

    fn rig() -> Rig {
        rig_in(tempfile::tempdir().unwrap(), Store::memory().unwrap())
    }

    async fn idle(runner: &Runner, id: SessionId) {
        for _ in 0..400 {
            if !runner.is_running(id) {
                tokio::time::sleep(Duration::from_millis(30)).await;
                if !runner.is_running(id) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("session {id} never finished");
    }

    async fn started(r: &Rig, log: &Arc<Mutex<Vec<String>>>) {
        for _ in 0..200 {
            if !log.lock().unwrap().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("never started: {:?}", r.boss_log);
    }

    fn log(l: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        l.lock().unwrap().clone()
    }

    fn said_to(r: &Rig, id: SessionId) -> Vec<(String, Vec<String>)> {
        agentbench_core::shown(&r.store.messages(id, 0).unwrap())
            .into_iter()
            .filter_map(|m| match m.entry {
                Entry::User { text, from, .. } => Some((text, from)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_system_message_waits_for_the_supervisor_to_finish_its_turn() {
        let r = rig();
        r.runner.send(&r.boss, "review the queue").unwrap();
        started(&r, &r.boss_log).await;
        let origin = Origin::new("watch", Some(r.worker.id));
        r.runner.tell(&r.boss, "worker finished", origin).unwrap();
        assert_eq!(r.store.deferred_count(r.boss.id).unwrap(), 1);
        let shown = r.runner.with_queued(vec![r.store.session(r.boss.id).unwrap()]);
        assert_eq!(shown[0].deferred, 1);
        assert!(shown[0].queued.is_empty(), "not in the steer inbox");
        idle(&r.runner, r.boss.id).await;
        let seen = log(&r.boss_log);
        assert!(!seen.iter().any(|l| l.starts_with("steer")), "{seen:?}");
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[1].starts_with("turn: [") && seen[1].contains("worker finished"), "{seen:?}");
        assert!(seen[1].contains(&format!("watch {}", r.worker.id)), "{seen:?}");
        let told = said_to(&r, r.boss.id);
        assert_eq!(told.last().unwrap().1, [format!("watch {}", r.worker.id)]);
        assert_eq!(r.store.deferred_count(r.boss.id).unwrap(), 0);
    }

    #[tokio::test]
    async fn the_user_still_steers_a_busy_supervisor() {
        let r = rig();
        r.runner.send(&r.boss, "review the queue").unwrap();
        started(&r, &r.boss_log).await;
        r.runner.send(&r.boss, "stop that, look at the login bug").unwrap();
        idle(&r.runner, r.boss.id).await;
        assert_eq!(
            log(&r.boss_log),
            ["turn: review the queue", "steer: stop that, look at the login bug"]
        );
    }

    #[tokio::test]
    async fn messages_queued_during_a_turn_arrive_as_one_digest_in_order_without_repeats() {
        let r = rig();
        r.runner.send(&r.boss, "go").unwrap();
        started(&r, &r.boss_log).await;
        let w = Some(r.worker.id);
        r.runner.tell(&r.boss, "first", Origin::new("watch", w)).unwrap();
        r.runner.tell(&r.boss, "second", Origin::new("autopilot", w)).unwrap();
        r.runner.tell(&r.boss, "first", Origin::new("watch", w)).unwrap();
        r.runner.tell(&r.boss, "third", Origin::new("session", None)).unwrap();
        idle(&r.runner, r.boss.id).await;
        let seen = log(&r.boss_log);
        assert_eq!(seen.len(), 2, "one digest turn: {seen:?}");
        let digest = &seen[1];
        assert!(digest.contains("3 messages"), "{digest}");
        assert_eq!(digest.matches(": first").count(), 1, "{digest}");
        let at = |s: &str| digest.find(s).unwrap();
        assert!(at(": first") < at(": second") && at(": second") < at(": third"), "{digest}");
        let from = said_to(&r, r.boss.id).pop().unwrap().1;
        assert_eq!(
            &from,
            &[
                format!("watch {}", r.worker.id),
                format!("autopilot {}", r.worker.id),
                "session".to_string()
            ]
        );
    }

    #[test]
    fn a_burst_is_capped_to_the_newest_and_says_how_many_were_folded() {
        let all: Vec<Deferred> = (0..20)
            .map(|i| Deferred {
                id: i,
                source: "watch".into(),
                worker: None,
                urgent: false,
                text: format!("finish number {i:02}"),
                created_at: 0,
            })
            .collect();
        let m = digest_of(all);
        assert!(m.text.starts_with("20 messages"), "{}", m.text);
        assert!(m.text.contains(&format!("{} older ones are folded in", 20 - DIGEST_MAX)), "{}", m.text);
        assert!(!m.text.contains("finish number 07"), "{}", m.text);
        assert!(m.text.contains("finish number 08") && m.text.contains("finish number 19"));
        assert_eq!(m.text.matches("finish number").count(), DIGEST_MAX);
    }

    #[tokio::test]
    async fn an_urgent_message_waits_until_the_turn_is_long_then_interrupts() {
        let r = rig();
        r.runner.send(&r.boss, "go").unwrap();
        started(&r, &r.boss_log).await;
        let urgent = Origin::new("session", Some(r.worker.id)).urgent(true);
        r.runner.tell(&r.boss, "BLOCKED early", urgent).unwrap();
        idle(&r.runner, r.boss.id).await;
        let seen = log(&r.boss_log);
        assert!(!seen.iter().any(|l| l.starts_with("steer")), "under the threshold it waits: {seen:?}");
        assert!(seen.last().unwrap().contains("BLOCKED early"));

        r.boss_log.lock().unwrap().clear();
        r.store.set_setting(URGENT_AFTER_SETTING, "0").unwrap();
        r.runner.send(&r.store.session(r.boss.id).unwrap(), "go again").unwrap();
        started(&r, &r.boss_log).await;
        r.runner.tell(&r.boss, "a plain finish", Origin::new("watch", None)).unwrap();
        r.runner.tell(&r.boss, "BLOCKED late", urgent).unwrap();
        idle(&r.runner, r.boss.id).await;
        let seen = log(&r.boss_log);
        assert!(seen.iter().any(|l| l.starts_with("steer: ") && l.contains("BLOCKED late")), "{seen:?}");
        assert!(!seen.iter().any(|l| l.starts_with("steer: ") && l.contains("a plain finish")), "{seen:?}");
        assert!(seen.last().unwrap().starts_with("turn: ") && seen.last().unwrap().contains("a plain finish"));
    }

    #[tokio::test]
    async fn a_worker_is_still_steered_by_the_supervisor_and_the_autopilot() {
        let r = rig();
        r.runner.send(&r.worker, "build it").unwrap();
        started(&r, &r.worker_log).await;
        let args = serde_json::json!({"id": r.worker.id.to_string(), "text": "use the new api"});
        r.runner.api.call("send_message", &args, r.boss.id).unwrap();
        r.runner.tell(&r.worker, "Autopilot: no output", Origin::new("autopilot", None)).unwrap();
        idle(&r.runner, r.worker.id).await;
        let seen = log(&r.worker_log);
        assert!(seen.iter().any(|l| l.starts_with("steer: ") && l.contains("use the new api")), "{seen:?}");
        assert!(seen.iter().any(|l| l == "steer: Autopilot: no output"), "{seen:?}");
        assert_eq!(r.store.deferred_count(r.worker.id).unwrap(), 0);
    }

    #[tokio::test]
    async fn a_session_set_to_queue_system_messages_waits_too() {
        let r = rig();
        r.store.set_queue_system(r.worker.id, true).unwrap();
        let worker = r.store.session(r.worker.id).unwrap();
        r.runner.send(&worker, "build it").unwrap();
        started(&r, &r.worker_log).await;
        r.runner.tell(&worker, "Autopilot: no output", Origin::new("autopilot", None)).unwrap();
        idle(&r.runner, r.worker.id).await;
        let seen = log(&r.worker_log);
        assert!(!seen.iter().any(|l| l.starts_with("steer")), "{seen:?}");
        assert_eq!(seen.len(), 2, "{seen:?}");
    }

    #[tokio::test]
    async fn a_restart_keeps_the_waiting_messages_and_an_idle_supervisor_gets_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let (boss, worker) = {
            let r = rig_in(tempfile::tempdir_in(dir.path()).unwrap(), Store::open(&path).unwrap());
            let keep = r._dir.path().to_path_buf();
            r.runner.send(&r.boss, "go").unwrap();
            started(&r, &r.boss_log).await;
            r.runner.tell(&r.boss, "finished while you worked", Origin::new("watch", Some(r.worker.id))).unwrap();
            r.runner.interrupt(r.boss.id);
            idle(&r.runner, r.boss.id).await;
            let _ = keep;
            (r.boss.id, r.worker.id)
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(store.deferred_count(boss).unwrap(), 1, "kept across the restart");
        let r = rig_in(tempfile::tempdir().unwrap(), store);
        assert_eq!((r.boss.id, r.worker.id), (boss, worker));
        r.runner.deliver_backlog();
        started(&r, &r.boss_log).await;
        idle(&r.runner, boss).await;
        let seen = log(&r.boss_log);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("finished while you worked"), "{seen:?}");
        assert_eq!(r.store.deferred_count(boss).unwrap(), 0);
    }

    #[tokio::test]
    async fn watch_notices_and_judge_requests_reach_the_supervisor_as_system_messages() {
        let r = rig();
        r.store.add_watch(r.worker.id, r.boss.id).unwrap();
        r.runner.send(&r.boss, "go").unwrap();
        started(&r, &r.boss_log).await;
        r.runner.send(&r.worker, "build").unwrap();
        idle(&r.runner, r.worker.id).await;
        assert_eq!(r.store.deferred_count(r.boss.id).unwrap(), 1, "the judge request waits");
        idle(&r.runner, r.boss.id).await;
        let from = said_to(&r, r.boss.id).pop().unwrap().1;
        assert_eq!(from, [format!("judge {}", r.worker.id)], "its own idle child is judged");

        r.store.set_parent(r.worker.id, None).unwrap();
        r.store.add_watch(r.worker.id, r.boss.id).unwrap();
        r.runner.send(&r.store.session(r.boss.id).unwrap(), "again").unwrap();
        r.runner.send(&r.store.session(r.worker.id).unwrap(), "build").unwrap();
        idle(&r.runner, r.worker.id).await;
        assert_eq!(r.store.deferred_count(r.boss.id).unwrap(), 1, "the watch notice waits");
        idle(&r.runner, r.boss.id).await;
        let from = said_to(&r, r.boss.id).pop().unwrap().1;
        assert_eq!(from, [format!("watch {}", r.worker.id)]);
    }
    #[tokio::test]
    async fn a_child_reports_every_turn_end_to_its_parent_without_a_watch_call() {
        let r = rig();
        r.runner.send(&r.worker, "build").unwrap();
        idle(&r.runner, r.worker.id).await;
        idle(&r.runner, r.boss.id).await;
        let args = serde_json::json!({"id": r.worker.id.to_string(), "text": "more"});
        r.runner.api.call("send_message", &args, r.boss.id).unwrap();
        idle(&r.runner, r.worker.id).await;
        idle(&r.runner, r.boss.id).await;
        let told: Vec<_> = said_to(&r, r.boss.id).into_iter().filter(|(_, f)| f == &[format!("judge {}", r.worker.id)]).collect();
        assert_eq!(told.len(), 2, "{told:?}");
        assert!(r.store.watchers(r.worker.id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_grandchild_reports_to_its_own_parent_and_not_the_supervisor() {
        let r = rig();
        let tree = r.store.main_worktree(r.worker.project_id).unwrap();
        let grand = r.store.add_session(tree.id, "worker", "grand", None, None).unwrap();
        r.store.set_parent(grand.id, Some(r.worker.id)).unwrap();
        r.runner.send(&grand, "build").unwrap();
        idle(&r.runner, grand.id).await;
        idle(&r.runner, r.worker.id).await;
        let to_worker = said_to(&r, r.worker.id);
        assert!(to_worker.iter().any(|(t, _)| t.contains(&grand.id.to_string()) && t.contains("changed state to idle")), "{to_worker:?}");
        assert!(said_to(&r, r.boss.id).iter().all(|(t, _)| !t.contains(&grand.id.to_string())));
    }

    #[tokio::test]
    async fn a_closed_child_stops_notifying_its_parent() {
        let r = rig();
        r.runner.send(&r.worker, "build").unwrap();
        started(&r, &r.worker_log).await;
        let args = serde_json::json!({"id": r.worker.id.to_string()});
        r.runner.api.call("close_session", &args, r.boss.id).unwrap();
        idle(&r.runner, r.worker.id).await;
        assert!(said_to(&r, r.boss.id).is_empty());
        assert_eq!(r.store.deferred_count(r.boss.id).unwrap(), 0);
    }

    #[tokio::test]
    async fn a_failed_child_notifies_its_parent_once() {
        let r = rig();
        r.store.set_status(r.worker.id, SessionStatus::Failed).unwrap();
        r.runner.tell_watchers(r.worker.id);
        idle(&r.runner, r.boss.id).await;
        let told = said_to(&r, r.boss.id);
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(told[0].0.contains("changed state to failed"), "{told:?}");
    }
    #[tokio::test]
    async fn a_session_started_by_a_worker_records_the_worker_as_its_parent() {
        let r = rig();
        let args = serde_json::json!({"prompt": "go"});
        let out = r.runner.api.call("start_session", &args, r.worker.id).unwrap();
        let id: SessionId = serde_json::from_value(out["session"].clone()).unwrap();
        assert_eq!(r.store.session(id).unwrap().parent, Some(r.worker.id));
        idle(&r.runner, id).await;
        idle(&r.runner, r.worker.id).await;
        let first = r.store.messages(id, 0).unwrap();
        assert!(
            matches!(&first[0].entry, Entry::User { text, .. } if text.ends_with(&format!("PARENT={}", r.worker.id))),
            "{first:?}"
        );
        assert!(said_to(&r, r.worker.id).iter().any(|(t, _)| t.contains(&id.to_string())));
        assert!(said_to(&r, r.boss.id).iter().all(|(t, _)| !t.contains(&id.to_string())));
    }

    #[tokio::test]
    async fn a_released_child_tells_nobody_and_an_adopted_one_is_judged_again() {
        let r = rig();
        let id = r.worker.id;
        r.store.add_watch(id, r.boss.id).unwrap();
        assert!(crate::autopilot::release(&r.store, id, "user").unwrap());
        r.runner.send(&r.store.session(id).unwrap(), "build").unwrap();
        idle(&r.runner, id).await;
        assert!(said_to(&r, r.boss.id).is_empty(), "released: no report, no judge request");
        assert_eq!(r.store.deferred_count(r.boss.id).unwrap(), 0);

        crate::autopilot::adopt(&r.store, id, r.boss.id).unwrap();
        r.runner.send(&r.store.session(id).unwrap(), "build").unwrap();
        idle(&r.runner, id).await;
        idle(&r.runner, r.boss.id).await;
        let from = said_to(&r, r.boss.id).pop().unwrap().1;
        assert_eq!(from, [format!("judge {id}")]);
    }
}
