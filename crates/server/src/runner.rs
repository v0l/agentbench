use crate::agent::{Agent, Event, Turn};
use crate::store::{Store, StoreError};
use foreman_core::{Entry, Id, Session, SessionStatus};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, oneshot};

const TEXT_FLUSH: Duration = Duration::from_millis(250);
const STDERR_KEPT: usize = 4000;

#[derive(Clone)]
pub struct Runner {
    store: Store,
    agents: Arc<HashMap<String, Arc<dyn Agent>>>,
    running: Arc<Mutex<HashMap<Id, oneshot::Sender<()>>>>,
    pub changed: Arc<Notify>,
}

impl Runner {
    pub fn new(store: Store, agents: Vec<Box<dyn Agent>>) -> Runner {
        let agents = agents.into_iter().map(|a| (a.name().to_string(), Arc::from(a))).collect();
        Runner {
            store,
            agents: Arc::new(agents),
            running: Arc::new(Mutex::new(HashMap::new())),
            changed: Arc::new(Notify::new()),
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

    pub fn is_running(&self, session: Id) -> bool {
        self.running.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&session)
    }

    pub fn interrupt(&self, session: Id) -> bool {
        let stop = self.running.lock().unwrap_or_else(|e| e.into_inner()).remove(&session);
        stop.map(|tx| tx.send(()).is_ok()).unwrap_or(false)
    }

    pub fn send(&self, session: &Session, text: &str) -> Result<(), StoreError> {
        let agent = self
            .agent(&session.agent)
            .ok_or_else(|| StoreError(format!("no agent named {}", session.agent)))?;
        if text.trim().is_empty() {
            return Err(StoreError("nothing to send".into()));
        }
        let (stop_tx, stop_rx) = oneshot::channel();
        {
            let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
            if running.contains_key(&session.id) {
                return Err(StoreError("this session is already working".into()));
            }
            running.insert(session.id, stop_tx);
        }
        self.store.add_message(session.id, &Entry::User { text: text.to_string() })?;
        self.store.set_status(session.id, SessionStatus::Running)?;
        self.changed.notify_waiters();

        let runner = self.clone();
        let session = session.clone();
        let prompt = text.to_string();
        tokio::spawn(async move {
            let status = runner.turn(agent.as_ref(), &session, &prompt, stop_rx).await;
            runner.running.lock().unwrap_or_else(|e| e.into_inner()).remove(&session.id);
            let _ = runner.store.set_status(session.id, status);
            runner.changed.notify_waiters();
        });
        Ok(())
    }

    async fn turn(
        &self,
        agent: &dyn Agent,
        session: &Session,
        prompt: &str,
        mut stop: oneshot::Receiver<()>,
    ) -> SessionStatus {
        let cwd = PathBuf::from(&session.cwd);
        let resume = session.agent_session.as_deref().filter(|_| agent.resumable());
        let launch =
            agent.launch(&Turn { prompt, cwd: &cwd, model: session.model.as_deref(), resume });
        let mut sink = Sink::new(self.store.clone(), session.id, self.changed.clone());

        let mut cmd = Command::new(&launch.program);
        cmd.args(&launch.args)
            .current_dir(&cwd)
            .envs(launch.env.iter().cloned())
            .env("FOREMAN_SESSION", session.id.to_string())
            .stdin(if launch.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        if let Some(issue) = session.issue_id {
            cmd.env("FOREMAN_ISSUE", issue.to_string());
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                sink.entry(Entry::Error {
                    text: format!("could not start {}: {e}", launch.program.display()),
                });
                return SessionStatus::Failed;
            }
        };
        if let (Some(text), Some(mut stdin)) = (launch.stdin, child.stdin.take()) {
            tokio::spawn(async move {
                let _ = stdin.write_all(text.as_bytes()).await;
                let _ = stdin.shutdown().await;
            });
        }
        let stderr = child.stderr.take().map(|mut err| {
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let _ = (&mut err).take(64 * 1024).read_to_end(&mut buf).await;
                let text = String::from_utf8_lossy(&buf).trim().to_string();
                let start = text.len().saturating_sub(STDERR_KEPT);
                text[text.floor_char_boundary(start)..].to_string()
            })
        });

        let mut decoder = agent.decoder();
        let mut lines = BufReader::new(child.stdout.take().expect("stdout piped")).lines();
        let mut stopped = false;
        let mut tick = tokio::time::interval(TEXT_FLUSH);
        loop {
            tokio::select! {
                _ = &mut stop, if !stopped => {
                    stopped = true;
                    kill_tree(&mut child).await;
                }
                _ = tick.tick() => sink.flush_text(),
                line = lines.next_line() => match line {
                    Ok(Some(line)) => {
                        for event in decoder.line(&line) {
                            sink.event(event);
                        }
                    }
                    Ok(None) | Err(_) => break,
                },
            }
        }
        let exit = child.wait().await.ok();
        let stderr = match stderr {
            Some(task) => task.await.unwrap_or_default(),
            None => String::new(),
        };
        sink.flush_text();

        if stopped {
            sink.entry(Entry::Notice { text: "stopped".into() });
            return SessionStatus::Stopped;
        }
        let ok = exit.is_some_and(|s| s.success());
        if !ok && !sink.errored {
            let code =
                exit.and_then(|s| s.code()).map(|c| format!(" (exit {c})")).unwrap_or_default();
            let detail = if stderr.is_empty() { "no output".to_string() } else { stderr };
            sink.entry(Entry::Error { text: format!("{} failed{code}: {detail}", agent.name()) });
        }
        if sink.errored || !ok { SessionStatus::Failed } else { SessionStatus::Idle }
    }
}

async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
        if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_ok() {
            return;
        }
        let _ = std::process::Command::new("kill").args(["-KILL", &format!("-{pid}")]).status();
    }
    let _ = child.kill().await;
}

struct Sink {
    store: Store,
    session: Id,
    changed: Arc<Notify>,
    text: Option<(Id, String)>,
    dirty: Option<Instant>,
    tools: HashMap<String, Id>,
    errored: bool,
}

impl Sink {
    fn new(store: Store, session: Id, changed: Arc<Notify>) -> Sink {
        Sink {
            store,
            session,
            changed,
            text: None,
            dirty: None,
            tools: HashMap::new(),
            errored: false,
        }
    }

    fn entry(&mut self, entry: Entry) -> Option<Id> {
        self.flush_text();
        self.text = None;
        if matches!(entry, Entry::Error { .. }) {
            self.errored = true;
        }
        let id = self.store.add_message(self.session, &entry).ok().map(|m| m.id);
        self.changed.notify_waiters();
        id
    }

    fn flush_text(&mut self) {
        if self.dirty.take().is_none() {
            return;
        }
        if let Some((id, text)) = &self.text {
            let _ = self.store.update_message(*id, &Entry::Assistant { text: text.clone() });
            self.changed.notify_waiters();
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Started { agent_session } => {
                let _ = self.store.set_agent_session(self.session, &agent_session);
            }
            Event::Text { delta } => match &mut self.text {
                Some((_, text)) => {
                    text.push_str(&delta);
                    self.dirty.get_or_insert_with(Instant::now);
                }
                None => {
                    let delta = delta.trim_start().to_string();
                    if delta.is_empty() {
                        return;
                    }
                    let id = self.entry(Entry::Assistant { text: delta.clone() });
                    self.text = id.map(|id| (id, delta));
                }
            },
            Event::Thinking { text } => {
                self.entry(Entry::Thinking { text });
            }
            Event::ToolStart { call_id, name, input } => {
                if let Some(id) = self.entry(Entry::Tool {
                    call_id: call_id.clone(),
                    name,
                    input,
                    output: None,
                    error: false,
                }) {
                    self.tools.insert(call_id, id);
                }
            }
            Event::ToolEnd { call_id, name, output, error } => {
                let input = match self.tools.remove(&call_id) {
                    Some(id) => {
                        let input = self
                            .store
                            .messages(self.session, id - 1)
                            .ok()
                            .and_then(|m| m.into_iter().find(|m| m.id == id))
                            .and_then(|m| match m.entry {
                                Entry::Tool { input, .. } => Some(input),
                                _ => None,
                            })
                            .unwrap_or_default();
                        let entry =
                            Entry::Tool { call_id, name, input, output: Some(output), error };
                        let _ = self.store.update_message(id, &entry);
                        self.changed.notify_waiters();
                        self.text = None;
                        return;
                    }
                    None => serde_json::Value::Null,
                };
                self.entry(Entry::Tool { call_id, name, input, output: Some(output), error });
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
            Event::Done => self.flush_text(),
        }
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
        for _ in 0..200 {
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
        let script =
            r#"read p; echo '{"session":"abc"}'; echo "{\"text\":\"got $p\"}"; echo '{"in":7}'"#;
        let (runner, s) = setup(script_agent("echo", script));
        runner.send(&s, "hello").unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Idle);
        assert_eq!(done.agent_session.as_deref(), Some("abc"));
        assert_eq!(done.usage.input_tokens, 7);
        let msgs = runner.store().messages(s.id, 0).unwrap();
        assert_eq!(msgs[0].entry, Entry::User { text: "hello".into() });
        assert_eq!(msgs[1].entry, Entry::Assistant { text: "got hello".into() });
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
        assert!(runner.send(&s, "again").is_err(), "a busy session refuses a second turn");
        assert!(runner.interrupt(s.id));
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Stopped);
        let msgs = runner.store().messages(s.id, 0).unwrap();
        assert_eq!(msgs.last().unwrap().entry, Entry::Notice { text: "stopped".into() });
        let out = std::process::Command::new("pgrep").args(["-f", &marker]).output().unwrap();
        assert!(out.stdout.is_empty(), "the backgrounded child survived");
    }

    #[tokio::test]
    async fn a_missing_program_fails_cleanly() {
        let mut agent = JsonCommand {
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
            plain: false,
        };
        agent.plain = true;
        let (runner, s) = setup(Box::new(agent));
        runner.send(&s, "x").unwrap();
        let done = wait_idle(&runner, s.id).await;
        assert_eq!(done.status, SessionStatus::Failed);
        let last = runner.store().messages(s.id, 0).unwrap().pop().unwrap();
        assert!(matches!(&last.entry, Entry::Error { text } if text.contains("could not start")));
    }
}
