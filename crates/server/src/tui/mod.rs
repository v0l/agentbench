mod app;
mod editor;
mod net;
mod text;
mod ui;

pub use net::Server;

use agentbench_core::{NewSession, Place, Project, Session, SessionId};
use app::{App, Cmd, Reply};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use futures_util::StreamExt;
use net::Net;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io::{Stdout, stdout};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

const TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub agent: Option<String>,
    pub model: Option<String>,
    pub project: Option<String>,
    pub resume: bool,
    pub prompt: Option<String>,
}

fn pick_project(projects: &[Project], wanted: Option<&str>) -> Result<Project, String> {
    if let Some(name) = wanted {
        let lower = name.to_lowercase();
        return projects
            .iter()
            .find(|p| p.name.to_lowercase() == lower || p.path == name)
            .cloned()
            .ok_or_else(|| {
                let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
                format!("no project named {name}; there are {}", names.join(", "))
            });
    }
    projects
        .iter()
        .find(|p| p.home)
        .or(projects.first())
        .cloned()
        .ok_or_else(|| "the server has no projects; add one with `agentbench add <path>`".into())
}

fn pick_agent(
    agents: &[agentbench_core::AgentInfo],
    wanted: Option<&str>,
) -> Result<String, String> {
    if let Some(name) = wanted {
        return match agents.iter().find(|a| a.name == name) {
            Some(a) if a.available => Ok(a.name.clone()),
            Some(a) => Err(format!("{} is not ready: {}", a.label, a.about)),
            None => Err(format!("no agent named {name}")),
        };
    }
    agents
        .iter()
        .find(|a| a.name == "native" && a.available)
        .or_else(|| agents.iter().find(|a| a.available))
        .map(|a| a.name.clone())
        .ok_or_else(|| "no agent is ready; run `agentbench agents`".into())
}

fn spawn_watch(net: Net, since: i64, tx: UnboundedSender<Reply>) {
    tokio::spawn(async move {
        let mut cursor = since;
        loop {
            match net.changes(cursor).await {
                Ok(c) => {
                    cursor = c;
                    if tx.send(Reply::Changed(Ok(c))).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    if tx.send(Reply::Changed(Err(e))).is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });
}

async fn fetch(
    net: &Net,
    id: SessionId,
    after: i64,
) -> Result<(Session, Vec<agentbench_core::Message>), String> {
    let session = net.session(id).await?;
    let messages = net.messages(id, after).await?;
    Ok((session, messages))
}

struct Loop {
    net: Net,
    tx: UnboundedSender<Reply>,
    project: Project,
    refreshing: bool,
    again: bool,
    seen: i64,
}

impl Loop {
    fn spawn<F>(&self, work: F)
    where
        F: std::future::Future<Output = Result<Option<Reply>, String>> + Send + 'static,
    {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let reply = match work.await {
                Ok(Some(r)) => r,
                Ok(None) => return,
                Err(e) => Reply::Failed(e),
            };
            let _ = tx.send(reply);
        });
    }

    fn refresh(&mut self, app: &App) {
        let Some(session) = app.session.as_ref().map(|s| s.id) else {
            return;
        };
        if self.refreshing {
            self.again = true;
            return;
        }
        self.refreshing = true;
        let net = self.net.clone();
        let after = app.refetch_after();
        self.spawn(async move {
            let (s, m) = fetch(&net, session, after).await?;
            Ok(Some(Reply::Refreshed(s, m)))
        });
    }

    fn command(&mut self, app: &mut App, cmd: Cmd) -> bool {
        let net = self.net.clone();
        match cmd {
            Cmd::Quit => return true,
            Cmd::Send(text) => match app.session.as_ref().map(|s| s.id) {
                Some(id) => self.spawn(async move {
                    net.send(id, &text).await?;
                    Ok(None)
                }),
                None => {
                    let new = NewSession {
                        project_id: self.project.id,
                        agent: app.agent.clone(),
                        place: Place::Main,
                        model: app.model.clone(),
                        prompt: Some(text),
                        ..Default::default()
                    };
                    self.spawn(async move { Ok(Some(Reply::Created(net.create(&new).await?))) });
                }
            },
            Cmd::Answer { question, answers } => {
                if let Some(id) = app.session.as_ref().map(|s| s.id) {
                    self.spawn(async move {
                        net.answer(id, &question, &answers).await?;
                        Ok(None)
                    });
                }
            }
            Cmd::Interrupt => {
                if let Some(id) = app.session.as_ref().map(|s| s.id) {
                    self.spawn(async move {
                        let queued = net.interrupt(id).await?;
                        Ok(Some(Reply::Interrupted(id, queued)))
                    });
                }
            }
            Cmd::Open(id) => self.spawn(async move {
                let (s, m) = fetch(&net, id, 0).await?;
                Ok(Some(Reply::Opened(s, m)))
            }),
            Cmd::Delete(id) => self.spawn(async move {
                net.delete(id).await?;
                Ok(None)
            }),
            Cmd::Sessions => {
                self.spawn(async move { Ok(Some(Reply::Sessions(net.sessions().await?))) })
            }
            Cmd::Refresh => self.refresh(app),
        }
        false
    }

    fn handle(&mut self, app: &mut App, reply: Reply) {
        match &reply {
            Reply::Changed(Ok(c)) => {
                let moved = *c != self.seen;
                self.seen = *c;
                if moved || !app.online {
                    self.refresh(app);
                }
            }
            Reply::Refreshed(..) => {
                self.refreshing = false;
                if std::mem::take(&mut self.again) {
                    self.refresh(app);
                }
            }
            Reply::Failed(_) => self.refreshing = false,
            _ => {}
        }
        if let Some(next) = app.reply(reply) {
            self.command(app, next);
        }
    }
}

struct Guard {
    enhanced: bool,
}

impl Guard {
    fn enter() -> std::io::Result<Guard> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
        let enhanced = supports_keyboard_enhancement().unwrap_or(false);
        if enhanced {
            let _ = execute!(
                stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            );
        }
        Ok(Guard { enhanced })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore(self.enhanced);
    }
}

fn restore(enhanced: bool) {
    if enhanced {
        let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(stdout(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

async fn events(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    lp: &mut Loop,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Reply>,
) -> std::io::Result<()> {
    let mut stream = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    let mut view = ui::View::default();
    loop {
        if app.dirty {
            app.dirty = false;
            terminal.draw(|f| ui::draw(f, app, &mut view))?;
        }
        let page =
            terminal.size().map(|s| s.height as usize).unwrap_or(24).saturating_sub(6).max(1);
        let cmd = tokio::select! {
            ev = stream.next() => match ev {
                Some(Ok(Event::Key(k))) => app.key(k, page),
                Some(Ok(Event::Paste(p))) => { app.paste(&p); None }
                Some(Ok(Event::Mouse(m))) => { app.mouse(m); app.dirty = true; None }
                Some(Ok(Event::Resize(..))) => { app.dirty = true; app.lines_stale = true; None }
                Some(Err(e)) => return Err(e),
                None => return Ok(()),
                _ => None,
            },
            Some(reply) = rx.recv() => { lp.handle(app, reply); None }
            _ = tick.tick() => { app.tick(Instant::now()); None }
        };
        if let Some(cmd) = cmd
            && lp.command(app, cmd)
        {
            return Ok(());
        }
    }
}

pub async fn run(server: Server, opts: Options) -> Result<(), String> {
    let net = Net::new(server);
    let overview = net.overview().await.map_err(|e| format!("cannot reach the server: {e}"))?;
    let project = pick_project(&overview.projects, opts.project.as_deref())?;
    let agent = pick_agent(&overview.agents, opts.agent.as_deref())?;
    let mut app = App::new(project.clone(), overview.agents.clone(), agent);
    app.model = opts.model.clone();
    let (tx, rx) = unbounded_channel();
    let mut lp = Loop {
        net: net.clone(),
        tx: tx.clone(),
        project: project.clone(),
        refreshing: false,
        again: false,
        seen: overview.cursor,
    };
    if opts.resume {
        let latest = overview
            .sessions
            .iter()
            .filter(|s| s.project_id == project.id)
            .max_by_key(|s| s.updated_at);
        match latest {
            Some(s) => {
                let (s, m) = fetch(&net, s.id, 0).await?;
                app.open(s, m);
            }
            None => app.say(app::Tone::Info, "no earlier chat here, starting a new one"),
        }
    }
    if let Some(prompt) = opts.prompt.filter(|p| !p.trim().is_empty()) {
        lp.command(&mut app, Cmd::Send(prompt));
    }
    spawn_watch(net, overview.cursor, tx);

    let guard = Guard::enter().map_err(|e| e.to_string())?;
    let previous = std::panic::take_hook();
    let enhanced = guard.enhanced;
    std::panic::set_hook(Box::new(move |info| {
        restore(enhanced);
        previous(info);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout())).map_err(|e| e.to_string())?;
    let result = events(&mut terminal, &mut app, &mut lp, rx).await;
    drop(guard);
    result.map_err(|e| e.to_string())
}

pub fn local(home: Option<std::path::PathBuf>) -> Result<Server, String> {
    let local = crate::local::Local::open(home)?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    local.ensure(&me, &["serve"])?;
    Ok(Server { url: local.url(), token: local.token })
}

#[cfg(test)]
mod tests {
    use super::app::tests::{agent, project};
    use super::*;

    #[test]
    fn the_home_project_is_the_default_and_a_name_picks_another() {
        let mut repo = project();
        repo.id = 2;
        repo.name = "snort".into();
        repo.home = false;
        let all = vec![repo.clone(), project()];
        assert!(pick_project(&all, None).unwrap().home);
        assert_eq!(pick_project(&all, Some("SNORT")).unwrap().id, 2);
        assert!(pick_project(&all, Some("nope")).unwrap_err().contains("snort, chats"));
        assert!(pick_project(&[], None).is_err());
    }

    #[test]
    fn native_is_the_default_agent_and_an_unready_one_is_refused() {
        let all = vec![agent("claude", true), agent("native", true), agent("pi", false)];
        assert_eq!(pick_agent(&all, None).unwrap(), "native");
        assert_eq!(pick_agent(&all, Some("claude")).unwrap(), "claude");
        assert!(pick_agent(&all, Some("pi")).unwrap_err().contains("not ready"));
        assert!(pick_agent(&all, Some("zed")).is_err());
        let only = vec![agent("claude", true)];
        assert_eq!(pick_agent(&only, None).unwrap(), "claude");
    }
}
