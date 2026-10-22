use crate::agent::{Agent, Api, Context, Event, Io, Turn};
use crate::store::{Store, StoreError};
use foreman_core::{Entry, Id, Question, Session, SessionStatus};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const TEXT_FLUSH: Duration = Duration::from_millis(250);
const ANSWER_PATIENCE: Duration = Duration::from_secs(24 * 3600);

struct Live {
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<String>>>,
    questions: Arc<Mutex<HashMap<String, Sender<String>>>>,
}

#[derive(Clone)]
pub struct Runner {
    store: Store,
    agents: Arc<HashMap<String, Arc<dyn Agent>>>,
    live: Arc<Mutex<HashMap<Id, Live>>>,
    api: Arc<dyn Api>,
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
            base: Arc::new(Mutex::new(Base::default())),
            changed,
        }
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

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<Id, Live>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn is_running(&self, session: Id) -> bool {
        self.live().contains_key(&session)
    }

    pub fn interrupt(&self, session: Id) -> bool {
        let live = self.live();
        let Some(l) = live.get(&session) else {
            return false;
        };
        l.stop.store(true, Ordering::Relaxed);
        l.questions.lock().unwrap_or_else(|e| e.into_inner()).clear();
        l.inbox.lock().unwrap_or_else(|e| e.into_inner()).clear();
        true
    }

    pub fn answer(&self, session: Id, question: &str, answer: &str) -> Result<(), StoreError> {
        let live = self.live();
        let tx = live
            .get(&session)
            .and_then(|l| l.questions.lock().unwrap_or_else(|e| e.into_inner()).remove(question))
            .ok_or_else(|| StoreError("that question is no longer waiting".into()))?;
        tx.send(answer.to_string()).map_err(|_| StoreError("the agent stopped waiting".into()))
    }

    pub fn send(&self, session: &Session, text: &str) -> Result<(), StoreError> {
        let text = text.trim();
        if let Some((cmd, arg)) = foreman_core::parse_slash(text) {
            return self.command(session, cmd.name, arg);
        }
        self.send_text(session, text, Mode::Chat)
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
            "compact" => {
                if self.is_running(session.id) {
                    return Err(StoreError(
                        "stop the agent or let it finish before compacting".into(),
                    ));
                }
                self.send_text(session, COMPACT_PROMPT, Mode::Compact)
            }
            other => Err(StoreError(format!("/{other} is handled by the window"))),
        }
    }

    fn send_text(&self, session: &Session, text: &str, mode: Mode) -> Result<(), StoreError> {
        let agent = self
            .agent(&session.agent)
            .ok_or_else(|| StoreError(format!("no agent named {}", session.agent)))?;
        if text.is_empty() {
            return Err(StoreError("nothing to send".into()));
        }
        {
            let live = self.live();
            if let Some(l) = live.get(&session.id) {
                l.inbox.lock().unwrap_or_else(|e| e.into_inner()).push(text.to_string());
                drop(live);
                if !agent.steerable() {
                    let note = format!("queued for when this turn ends: {text}");
                    self.store.add_message(session.id, &Entry::Notice { text: note })?;
                }
                self.changed.notify_waiters();
                return Ok(());
            }
        }
        let live = Live {
            stop: Arc::new(AtomicBool::new(false)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            questions: Arc::new(Mutex::new(HashMap::new())),
        };
        let (stop, inbox, questions) =
            (live.stop.clone(), live.inbox.clone(), live.questions.clone());
        self.live().insert(session.id, live);
        let runner = self.clone();
        let session_id = session.id;
        let first = text.to_string();
        let handle = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let _guard = handle.enter();
            let mut next = Some(first);
            while let Some(prompt) = next.take() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let session = match runner.store.session(session_id) {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let status =
                    runner.turn(agent.as_ref(), &session, &prompt, &stop, &inbox, &questions, mode);
                let _ = runner.store.set_status(session_id, status);
                runner.changed.notify_waiters();
                if status != SessionStatus::Idle || mode == Mode::Compact {
                    break;
                }
                let mut queued = inbox.lock().unwrap_or_else(|e| e.into_inner());
                if !queued.is_empty() {
                    next = Some(queued.drain(..).collect::<Vec<_>>().join("\n\n"));
                }
            }
            runner.live().remove(&session_id);
            runner.changed.notify_waiters();
        });
        Ok(())
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
    fn turn(
        &self,
        agent: &dyn Agent,
        session: &Session,
        prompt: &str,
        stop: &Arc<AtomicBool>,
        inbox: &Arc<Mutex<Vec<String>>>,
        questions: &Arc<Mutex<HashMap<String, Sender<String>>>>,
        mode: Mode,
    ) -> SessionStatus {
        let history: Vec<Entry> = self
            .store
            .messages(session.id, 0)
            .unwrap_or_default()
            .into_iter()
            .map(|m| m.entry)
            .collect();
        let shown = match mode {
            Mode::Chat => Entry::User { text: prompt.to_string() },
            Mode::Compact => Entry::Notice { text: "compacting the conversation".into() },
        };
        if self.store.add_message(session.id, &shown).is_err() {
            return SessionStatus::Failed;
        }
        let summary = history.iter().rev().find_map(|e| match e {
            Entry::Summary { text } => Some(text.clone()),
            _ => None,
        });
        let fresh_after_summary =
            session.agent_session.is_none() && summary.as_ref().is_some_and(|s| !s.is_empty());
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
        let _ = self.store.set_status(session.id, SessionStatus::Running);
        self.changed.notify_waiters();
        let cwd = PathBuf::from(&session.cwd);
        let context = self.context(session);
        let resume = session.agent_session.as_deref().filter(|_| agent.resumable());
        let turn = Turn {
            prompt,
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
        };
        let result = agent.run(&turn, &sink);
        sink.flush();
        let errored = sink.state.lock().unwrap_or_else(|e| e.into_inner()).errored;
        if stop.load(Ordering::Relaxed) {
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
    session: Id,
    changed: Arc<Notify>,
    state: Mutex<SinkState>,
    stop: Arc<AtomicBool>,
    inbox: Arc<Mutex<Vec<String>>>,
    questions: Arc<Mutex<HashMap<String, Sender<String>>>>,
    steerable: bool,
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
            Event::Steered { text } => {
                self.entry(Entry::User { text });
            }
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

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn steer(&self) -> Option<String> {
        if !self.steerable {
            return None;
        }
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        (!inbox.is_empty()).then(|| inbox.remove(0))
    }

    fn ask(&self, question: Question) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = channel();
        self.questions.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), tx);
        let entry = Entry::Question { id: id.clone(), question: question.clone(), answer: None };
        let msg = self.entry(entry);
        let _ = self.store.set_status(self.session, SessionStatus::Waiting);
        self.changed.notify_waiters();
        let started = Instant::now();
        let answer = loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(a) => break Ok(a),
                Err(RecvTimeoutError::Disconnected) => {
                    break Err("the question was withdrawn".to_string());
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.stopped() {
                        break Err("stopped".to_string());
                    }
                    if started.elapsed() > ANSWER_PATIENCE {
                        break Err("nobody answered in a day".to_string());
                    }
                }
            }
        };
        self.questions.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        if let (Some(m), Ok(a)) = (msg, &answer) {
            let entry = Entry::Question { id, question, answer: Some(a.clone()) };
            let _ = self.store.update_message(m, &entry);
        }
        let _ = self.store.set_status(self.session, SessionStatus::Running);
        self.changed.notify_waiters();
        answer
    }

    fn api(&self) -> &dyn Api {
        self.api.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::JsonCommand;

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

    async fn wait_idle(runner: &Runner, id: Id) -> Session {
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
        assert_eq!(msgs[0].entry, Entry::User { text: "hello".into() });
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

    struct Asker;

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
        fn run(&self, _turn: &Turn, io: &dyn Io) -> Result<(), String> {
            let a = io.ask(Question {
                text: "red or blue?".into(),
                options: vec!["red".into(), "blue".into()],
            })?;
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
        assert!(runner.answer(s.id, "nope", "x").is_err());
        runner.answer(s.id, &qid, "blue").unwrap();
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

    struct Echo(Mutex<Vec<(String, Option<String>, usize)>>);

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
        fn run(&self, turn: &Turn, io: &dyn Io) -> Result<(), String> {
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

    #[tokio::test]
    async fn slash_commands_compact_clear_and_pick_a_model() {
        let store = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&store, "/tmp");
        let s = store.add_session(main.id, "echo", "t", None, None).unwrap();
        let seen = std::sync::Arc::new(Echo(Mutex::new(Vec::new())));
        struct Shared(std::sync::Arc<Echo>);
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
            fn run(&self, t: &Turn, io: &dyn Io) -> Result<(), String> {
                self.0.run(t, io)
            }
        }
        let runner = Runner::new(store.clone(), vec![Box::new(Shared(seen.clone()))]);
        let fresh = |runner: &Runner| runner.store().session(s.id).unwrap();

        runner.send(&fresh(&runner), "hello").unwrap();
        wait_idle(&runner, s.id).await;
        assert_eq!(fresh(&runner).agent_session.as_deref(), Some("s1"));

        runner.send(&fresh(&runner), "/model qwen-big").unwrap();
        assert_eq!(fresh(&runner).model.as_deref(), Some("qwen-big"));

        runner.send(&fresh(&runner), "/compact").unwrap();
        wait_idle(&runner, s.id).await;
        let msgs: Vec<Entry> =
            store.messages(s.id, 0).unwrap().into_iter().map(|m| m.entry).collect();
        assert!(msgs.contains(&Entry::Summary { text: "We said hello.".into() }), "{msgs:?}");
        assert!(
            !msgs
                .iter()
                .any(|e| matches!(e, Entry::User { text } if text.starts_with("Summarise"))),
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
