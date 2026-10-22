use crate::client::{Client, Reply, Server};
use egui::{Align, Layout, Margin, Sense, Stroke, Ui, Vec2};
use egui_bench::key::Key;
use egui_bench::markdown;
use egui_bench::prelude::*;
use foreman_core::{
    Comment, Entry, Id, Issue, IssuePatch, IssueState, Message, NewComment, NewIssue, NewProject,
    NewSession, NewWorktree, Overview, Place, Session, SessionStatus, Worktree,
};
use std::collections::{HashMap, HashSet};
use web_time::{Duration, Instant};

const SIDE_W: f32 = 320.0;
const RETRY: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Issues,
    Session,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    Open,
    All,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Where {
    Local,
    Remote,
}

struct Connect {
    place: Where,
    url: String,
    token: String,
}

pub struct LocalServer {
    pub start: Box<dyn Fn() -> Result<Server, String> + Send + Sync>,
}

struct AddProject {
    path: String,
    name: String,
}

struct FileIssue {
    project: Id,
    title: String,
    body: String,
}

#[derive(Clone, PartialEq)]
enum PlaceChoice {
    Main,
    Existing(Id),
    New,
}

struct Start {
    project: Id,
    issue: Option<Id>,
    agent: String,
    model: String,
    place: PlaceChoice,
    branch: String,
    prompt: String,
}

enum Confirm {
    Project(Id, String),
    Worktree(Id, String, bool),
    Session(Id, String),
    Issue(Id, String),
}

#[derive(Default)]
struct Transcript {
    messages: Vec<Message>,
    last: Id,
    loading: bool,
}

pub struct App {
    client: Option<Client>,
    connect: Option<Connect>,
    local: Option<std::sync::Arc<LocalServer>>,
    remote: bool,
    starting: Option<std::sync::mpsc::Receiver<Result<Server, String>>>,
    overview: Option<Overview>,
    cursor: Option<Id>,
    watching: bool,
    offline: Option<(String, Instant)>,
    notice: Option<(bool, String)>,
    tab: Tab,
    filter: Filter,
    project: Option<Id>,
    session: Option<Id>,
    issue: Option<Id>,
    collapsed: HashSet<Id>,
    transcripts: HashMap<Id, Transcript>,
    comments: HashMap<Id, Vec<Comment>>,
    draft: String,
    comment: String,
    add_project: Option<AddProject>,
    file_issue: Option<FileIssue>,
    start: Option<Start>,
    new_worktree: Option<(Id, String)>,
    confirm: Option<Confirm>,
    markdown: markdown::Cache,
    follow: bool,
}

impl App {
    pub fn new(
        ctx: &egui::Context,
        storage: Option<&dyn eframe::Storage>,
        default_url: Option<String>,
    ) -> App {
        App::with_local(ctx, storage, default_url, None)
    }

    pub fn with_local(
        ctx: &egui::Context,
        storage: Option<&dyn eframe::Storage>,
        default_url: Option<String>,
        local: Option<LocalServer>,
    ) -> App {
        install(ctx);
        let url = storage.and_then(|s| s.get_string("url")).or(default_url).unwrap_or_default();
        let token = storage.and_then(|s| s.get_string("token")).unwrap_or_default();
        let remote = local.is_none()
            || storage.and_then(|s| s.get_string("remote")).as_deref() == Some("true");
        let mut app = App {
            client: None,
            connect: None,
            local: local.map(std::sync::Arc::new),
            remote,
            starting: None,
            overview: None,
            cursor: None,
            watching: false,
            offline: None,
            notice: None,
            tab: Tab::Issues,
            filter: Filter::Open,
            project: None,
            session: None,
            issue: None,
            collapsed: HashSet::new(),
            transcripts: HashMap::new(),
            comments: HashMap::new(),
            draft: String::new(),
            comment: String::new(),
            add_project: None,
            file_issue: None,
            start: None,
            new_worktree: None,
            confirm: None,
            markdown: markdown::Cache::default(),
            follow: true,
        };
        if !app.remote {
            app.start_local(ctx);
        } else if url.is_empty() || token.is_empty() {
            app.connect = Some(Connect { place: Where::Remote, url, token });
        } else {
            app.connect_to(ctx, Server { url, token });
        }
        app
    }

    pub fn store(&self, storage: &mut dyn eframe::Storage) {
        storage.set_string("remote", self.remote.to_string());
        if let Some(c) = self.client.as_ref().filter(|_| self.remote) {
            storage.set_string("url", c.server.url.clone());
            storage.set_string("token", c.server.token.clone());
        }
    }

    pub fn is_local(&self) -> bool {
        !self.remote
    }

    fn start_local(&mut self, ctx: &egui::Context) {
        let Some(local) = self.local.clone() else {
            return;
        };
        self.client = None;
        self.overview = None;
        self.offline = None;
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((local.start)());
            ctx.request_repaint();
        });
        self.starting = Some(rx);
    }

    fn poll_starting(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.starting else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(server)) => {
                self.starting = None;
                self.connect_to(ctx, server);
            }
            Ok(Err(e)) => {
                self.starting = None;
                self.offline = Some((e, Instant::now()));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.starting = None,
        }
    }

    fn connect_to(&mut self, ctx: &egui::Context, server: Server) {
        let ctx = ctx.clone();
        let mut client = Client::new(server, move || ctx.request_repaint());
        client.overview();
        self.client = Some(client);
        self.overview = None;
        self.cursor = None;
        self.watching = false;
        self.offline = None;
        self.transcripts.clear();
        self.comments.clear();
    }

    fn sessions_of(&self, worktree: Id) -> Vec<&Session> {
        let mut out: Vec<&Session> = self
            .overview
            .iter()
            .flat_map(|o| &o.sessions)
            .filter(|s| s.worktree_id == worktree)
            .collect();
        out.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        out
    }

    pub fn transcript_len(&self, session: Id) -> usize {
        self.transcripts.get(&session).map_or(0, |t| t.messages.len())
    }

    pub fn overview(&self) -> Option<&Overview> {
        self.overview.as_ref()
    }

    fn current_session(&self) -> Option<&Session> {
        let id = self.session?;
        self.overview.as_ref()?.sessions.iter().find(|s| s.id == id)
    }

    fn worktree(&self, id: Id) -> Option<&Worktree> {
        self.overview.as_ref()?.worktrees.iter().find(|w| w.id == id)
    }

    fn handle(&mut self, reply: Reply) {
        match reply {
            Reply::Overview(o) => {
                self.offline = None;
                let cursor = o.cursor;
                if self.project.is_none_or(|p| !o.projects.iter().any(|x| x.id == p)) {
                    self.project = o.projects.first().map(|p| p.id);
                }
                if self.session.is_some_and(|s| !o.sessions.iter().any(|x| x.id == s)) {
                    self.session = None;
                    if self.tab == Tab::Session {
                        self.tab = Tab::Issues;
                    }
                }
                self.overview = Some(o);
                self.cursor = Some(cursor);
                if let Some(sid) = self.session {
                    self.fetch_messages(sid);
                }
                if let Some(iid) = self.issue
                    && let Some(c) = &mut self.client
                {
                    c.comments(iid);
                }
            }
            Reply::Changed(cursor) => {
                self.watching = false;
                if Some(cursor) != self.cursor
                    && let Some(c) = &mut self.client
                {
                    c.overview();
                }
            }
            Reply::Messages(sid, msgs) => {
                let t = self.transcripts.entry(sid).or_default();
                t.loading = false;
                for m in msgs {
                    t.last = t.last.max(m.id);
                    match t.messages.iter_mut().find(|x| x.id == m.id) {
                        Some(x) => *x = m,
                        None => t.messages.push(m),
                    }
                }
            }
            Reply::Comments(iid, c) => {
                self.comments.insert(iid, c);
            }
            Reply::Done(said) => {
                if !said.is_empty() {
                    self.notice = Some((true, said));
                }
            }
            Reply::Failed(e) => self.notice = Some((false, e)),
            Reply::Offline(e) => {
                self.watching = false;
                self.offline = Some((e, Instant::now()));
            }
        }
    }

    fn fetch_messages(&mut self, sid: Id) {
        let t = self.transcripts.entry(sid).or_default();
        if t.loading {
            return;
        }
        t.loading = true;
        let tail = t.messages.iter().rev().take(3).map(|m| m.id).min().unwrap_or(0);
        let after = if t.messages.is_empty() { 0 } else { tail.saturating_sub(1) };
        if let Some(c) = &mut self.client {
            c.messages(sid, after);
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        self.poll_starting(ctx);
        if self.client.is_none() {
            if let (false, Some((_, at)), None) = (self.remote, &self.offline, &self.starting)
                && at.elapsed() >= RETRY
            {
                self.start_local(ctx);
            }
            return;
        }
        let Some(client) = &mut self.client else {
            return;
        };
        let replies = client.poll();
        for r in replies {
            self.handle(r);
        }
        let Some(client) = &mut self.client else {
            return;
        };
        match &self.offline {
            Some((_, at)) if at.elapsed() >= RETRY && !self.remote && self.local.is_some() => {
                self.client = None;
                self.offline = None;
                self.start_local(ctx);
            }
            Some((_, at)) if at.elapsed() >= RETRY => {
                self.offline = None;
                client.overview();
            }
            Some(_) => {}
            None => {
                if let (Some(cursor), false) = (self.cursor, self.watching) {
                    self.watching = true;
                    client.watch(cursor);
                }
            }
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("FOREMAN")
                    .font(legend_font(17.0))
                    .color(VALUE)
                    .extra_letter_spacing(2.5),
            );
            ui.add_space(18.0);
            let open = self.overview.as_ref().map_or(0, |o| {
                o.issues
                    .iter()
                    .filter(|i| Some(i.project_id) == self.project && i.state.is_open())
                    .count()
            });
            let session = self.current_session().map(|s| s.title.clone());
            let issues_label = format!("issues {open}");
            let mut opts: Vec<(Tab, &str)> = vec![(Tab::Issues, issues_label.as_str())];
            if let Some(title) = &session {
                opts.push((Tab::Session, title.as_str()));
            }
            let w = (ui.available_width() - 360.0).clamp(200.0, 560.0);
            ui.allocate_ui(Vec2::new(w, 24.0), |ui| tabs(ui, &mut self.tab, &opts));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let ready = self.overview.is_some();
                if ui
                    .add(
                        Key::new("start agent")
                            .solid(READOUT)
                            .enabled(ready && self.project.is_some()),
                    )
                    .clicked()
                {
                    self.open_start(None);
                }
                if ui
                    .add(
                        Key::new("file issue")
                            .outline(VALUE)
                            .enabled(ready && self.project.is_some()),
                    )
                    .clicked()
                    && let Some(p) = self.project
                {
                    self.file_issue =
                        Some(FileIssue { project: p, title: String::new(), body: String::new() });
                }
                if ui.add(Key::more()).clicked() {
                    let (url, token) = self
                        .client
                        .as_ref()
                        .filter(|_| self.remote)
                        .map(|c| (c.server.url.clone(), c.server.token.clone()))
                        .unwrap_or_default();
                    let place = if self.remote { Where::Remote } else { Where::Local };
                    self.connect = Some(Connect { place, url, token });
                }
                if !self.remote {
                    chip(ui, "this machine", OK, self.overview.is_some());
                }
            });
        });
    }

    fn open_start(&mut self, issue: Option<&Issue>) {
        let Some(o) = &self.overview else {
            return;
        };
        let project = issue.map(|i| i.project_id).or(self.project);
        let Some(project) = project else {
            return;
        };
        let agent = o
            .agents
            .iter()
            .find(|a| a.available && a.name == "pi")
            .or_else(|| o.agents.iter().find(|a| a.available))
            .map(|a| a.name.clone())
            .unwrap_or_default();
        let busy_main = o.sessions.iter().any(|s| {
            s.project_id == project
                && s.status == SessionStatus::Running
                && o.worktrees.iter().any(|w| w.id == s.worktree_id && w.main)
        });
        self.start = Some(Start {
            project,
            issue: issue.map(|i| i.id),
            agent,
            model: String::new(),
            place: if issue.is_some() || busy_main { PlaceChoice::New } else { PlaceChoice::Main },
            branch: String::new(),
            prompt: String::new(),
        });
    }

    fn side(&mut self, ui: &mut Ui) {
        let Some(o) = &self.overview else {
            match &self.offline {
                Some((e, _)) => {
                    status(ui, false, e);
                    hint(ui, "retrying every few seconds");
                }
                None if self.starting.is_some() => note(ui, "starting the foreman server", LEGEND),
                None => note(ui, "connecting", LEGEND),
            }
            return;
        };
        let projects = o.projects.clone();
        let worktrees = o.worktrees.clone();
        if projects.is_empty() {
            section(ui, "no projects yet", "", |ui| {
                note(
                    ui,
                    "Add a git repository. Its main checkout is the project, and every worktree off it is a branch your agents can work in.",
                    LEGEND,
                );
            });
        }
        for p in &projects {
            let selected = self.project == Some(p.id);
            let rail = if selected { Some(READOUT) } else { None };
            let trees: Vec<&Worktree> = worktrees.iter().filter(|w| w.project_id == p.id).collect();
            let open = self.overview.as_ref().map_or(0, |o| {
                o.issues.iter().filter(|i| i.project_id == p.id && i.state.is_open()).count()
            });
            let collapsed = self.collapsed.contains(&p.id);
            let mut pick = false;
            let head = card(
                ui,
                rail,
                |ui| {
                    Line::new().legend(&p.name).show(ui);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        Line::new().note(format!("{open} open")).size(10.5).show(ui);
                    });
                },
                |ui| {
                    let r = Line::new()
                        .value(p.branch.clone().unwrap_or("detached".into()))
                        .size(11.0)
                        .gap(8.0)
                        .note(short_path(&p.path))
                        .elided(ui);
                    if r.interact(Sense::click()).clicked() {
                        pick = true;
                    }
                    if collapsed {
                        return;
                    }
                    for w in &trees {
                        ui.add_space(2.0);
                        self.worktree_row(ui, w);
                    }
                },
            );
            let r = head.response.interact(Sense::click());
            if pick || r.clicked() {
                self.project = Some(p.id);
                self.tab = Tab::Issues;
            }
            r.context_menu(|ui| {
                if ui
                    .add(
                        Key::new(if collapsed { "expand" } else { "collapse" })
                            .outline(VALUE)
                            .min_width(160.0),
                    )
                    .clicked()
                {
                    if collapsed {
                        self.collapsed.remove(&p.id);
                    } else {
                        self.collapsed.insert(p.id);
                    }
                    ui.close();
                }
                if ui.add(Key::new("new worktree").outline(VALUE).min_width(160.0)).clicked() {
                    self.new_worktree = Some((p.id, String::new()));
                    ui.close();
                }
                if ui.add(Key::new("rescan worktrees").outline(VALUE).min_width(160.0)).clicked() {
                    if let Some(c) = &mut self.client {
                        c.sync_project(p.id);
                    }
                    ui.close();
                }
                if ui.add(Key::new("remove project").outline(FAULT).min_width(160.0)).clicked() {
                    self.confirm = Some(Confirm::Project(p.id, p.name.clone()));
                    ui.close();
                }
            });
            ui.add_space(8.0);
        }
        ui.add(Key::new("add project").outline(VALUE).min_width(ui.available_width()))
            .clicked()
            .then(|| {
                self.add_project = Some(AddProject { path: String::new(), name: String::new() });
            });
    }

    fn worktree_row(&mut self, ui: &mut Ui, w: &Worktree) {
        let sessions: Vec<Session> = self.sessions_of(w.id).into_iter().cloned().collect();
        let running = sessions.iter().any(|s| s.status == SessionStatus::Running);
        let tint = if w.missing {
            FAULT
        } else if running {
            TRACE
        } else if w.main {
            VALUE
        } else {
            LEGEND
        };
        let label = if w.main { format!("{} (main)", w.label()) } else { w.label() };
        let resp = ui
            .horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 16.0), Sense::hover());
                ui.painter().line_segment(
                    [rect.center_top(), rect.center_bottom()],
                    Stroke::new(1.0, ETCH),
                );
                ui.painter()
                    .line_segment([rect.center(), rect.right_center()], Stroke::new(1.0, ETCH));
                Line::new().value(label).tint(tint).size(11.5).elided(ui)
            })
            .inner
            .interact(Sense::click());
        if resp.clicked() {
            self.project = Some(w.project_id);
        }
        resp.on_hover_text(&w.path).context_menu(|ui| {
            if ui
                .add(
                    Key::new("start agent here")
                        .outline(READOUT)
                        .min_width(170.0)
                        .enabled(!w.missing),
                )
                .clicked()
            {
                self.open_start(None);
                if let Some(s) = &mut self.start {
                    s.project = w.project_id;
                    s.place = if w.main { PlaceChoice::Main } else { PlaceChoice::Existing(w.id) };
                }
                ui.close();
            }
            if !w.main {
                let label = if w.missing { "forget worktree" } else { "remove worktree" };
                if ui.add(Key::new(label).outline(FAULT).min_width(170.0)).clicked() {
                    self.confirm = Some(Confirm::Worktree(w.id, w.label(), w.missing));
                    ui.close();
                }
            }
        });
        for s in &sessions {
            self.session_row(ui, s);
        }
    }

    fn session_row(&mut self, ui: &mut Ui, s: &Session) {
        let on = self.session == Some(s.id);
        let (tint, word) = match s.status {
            SessionStatus::Running => (TRACE, "working"),
            SessionStatus::Failed => (FAULT, "failed"),
            SessionStatus::Stopped => (LEGEND, "stopped"),
            SessionStatus::Idle => (OK, "idle"),
        };
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(14.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 16.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.0, tint);

                Line::new()
                    .value(&s.title)
                    .tint(if on { READOUT } else { VALUE })
                    .size(11.5)
                    .gap(6.0)
                    .note(format!("{} {}", s.agent, word))
                    .size(10.5)
                    .elided(ui)
            })
            .inner
            .interact(Sense::click());
        if resp.clicked() {
            self.open_session(s.id, s.project_id);
        }
        resp.context_menu(|ui| {
            if s.status == SessionStatus::Running
                && ui.add(Key::new("stop").outline(FAULT).min_width(150.0)).clicked()
            {
                if let Some(c) = &mut self.client {
                    c.interrupt(s.id);
                }
                ui.close();
            }
            if ui.add(Key::new("delete session").outline(FAULT).min_width(150.0)).clicked() {
                self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
                ui.close();
            }
        });
    }

    pub fn open_session(&mut self, sid: Id, project: Id) {
        self.session = Some(sid);
        self.project = Some(project);
        self.tab = Tab::Session;
        self.follow = true;
        self.draft.clear();
        self.fetch_messages(sid);
    }

    fn issues(&mut self, ui: &mut Ui) {
        let Some(o) = &self.overview else {
            return;
        };
        let Some(pid) = self.project else {
            note(ui, "Add a project to file issues against it.", LEGEND);
            return;
        };
        let all: Vec<Issue> = o.issues.iter().filter(|i| i.project_id == pid).cloned().collect();
        let open = all.iter().filter(|i| i.state.is_open()).count();
        segments(
            ui,
            &mut self.filter,
            &[(Filter::Open, "open", Some(open)), (Filter::All, "all", Some(all.len()))],
        );
        ui.add_space(8.0);
        let shown: Vec<&Issue> =
            all.iter().filter(|i| self.filter == Filter::All || i.state.is_open()).collect();
        if shown.is_empty() {
            section(ui, "nothing to do", "", |ui| {
                note(
                    ui,
                    "File an issue, then start an agent on it. It gets its own worktree and the issue moves to doing.",
                    LEGEND,
                );
            });
            return;
        }
        let sessions = o.sessions.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for state in IssueState::ALL {
                let group: Vec<&&Issue> = shown.iter().filter(|i| i.state == state).collect();
                if group.is_empty() {
                    continue;
                }
                heading(ui, &format!("{} {}", state.as_str(), group.len()));
                for issue in group {
                    self.issue_card(ui, issue, &sessions);
                    ui.add_space(6.0);
                }
                ui.add_space(6.0);
            }
        });
    }

    fn issue_card(&mut self, ui: &mut Ui, issue: &Issue, sessions: &[Session]) {
        let mine: Vec<&Session> =
            sessions.iter().filter(|s| s.issue_id == Some(issue.id)).collect();
        let working = mine.iter().any(|s| s.status == SessionStatus::Running);
        let rail = match issue.state {
            IssueState::Done => Some(OK),
            IssueState::Review => Some(READOUT),
            _ if working => Some(TRACE),
            _ => None,
        };
        let open = self.issue == Some(issue.id);
        let r = card(
            ui,
            rail,
            |ui| {
                Line::new()
                    .value(format!("#{}", issue.id))
                    .tint(LEGEND)
                    .size(11.0)
                    .gap(8.0)
                    .value(&issue.title)
                    .size(12.5)
                    .elided(ui);
            },
            |ui| {
                ui.horizontal(|ui| {
                    let mut state = issue.state;
                    ui.allocate_ui(Vec2::new(120.0, KEY_H), |ui| {
                        if choice(
                            ui,
                            ("state", issue.id),
                            &mut state,
                            IssueState::ALL.map(|s| (s, s.as_str().to_string())),
                        ) && let Some(c) = &mut self.client
                        {
                            c.update_issue(
                                issue.id,
                                &IssuePatch { state: Some(state), ..Default::default() },
                            );
                        }
                    });
                    for s in &mine {
                        let tint = if s.status == SessionStatus::Running { TRACE } else { LEGEND };
                        if chip(ui, &s.agent, tint, s.status == SessionStatus::Running)
                            .interact(Sense::click())
                            .clicked()
                        {
                            self.open_session(s.id, s.project_id);
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                Key::new("start agent")
                                    .outline(READOUT)
                                    .id_salt(("start", issue.id)),
                            )
                            .clicked()
                        {
                            self.open_start(Some(issue));
                        }
                        if ui
                            .add(
                                Key::new(if open { "less" } else { "more" })
                                    .outline(VALUE)
                                    .id_salt(("more", issue.id)),
                            )
                            .clicked()
                        {
                            if open {
                                self.issue = None;
                            } else {
                                self.issue = Some(issue.id);
                                self.comment.clear();
                                if let Some(c) = &mut self.client {
                                    c.comments(issue.id);
                                }
                            }
                        }
                    });
                });
                if open {
                    self.issue_detail(ui, issue);
                }
            },
        );
        r.response.context_menu(|ui| {
            if ui.add(Key::new("delete issue").outline(FAULT).min_width(150.0)).clicked() {
                self.confirm = Some(Confirm::Issue(issue.id, issue.title.clone()));
                ui.close();
            }
        });
    }

    fn issue_detail(&mut self, ui: &mut Ui, issue: &Issue) {
        ui.add_space(4.0);
        if issue.body.trim().is_empty() {
            hint(ui, "no description");
        } else {
            markdown::show(ui, &mut self.markdown, &issue.body, issue.id as usize);
        }
        let comments = self.comments.get(&issue.id).cloned().unwrap_or_default();
        for c in &comments {
            ui.add_space(4.0);
            Line::new().legend(&c.author).gap(8.0).note(ago(c.created_at)).size(10.5).show(ui);
            markdown::show(ui, &mut self.markdown, &c.body, 100_000 + c.id as usize);
        }
        ui.add_space(4.0);
        let mut post = false;
        let has_text = !self.comment.trim().is_empty();
        field_then(ui, &mut self.comment, "add a comment", 90.0, |ui| {
            post = ui.add(Key::new("comment").outline(VALUE).enabled(has_text)).clicked();
        });
        if post {
            if let Some(c) = &mut self.client {
                c.add_comment(
                    issue.id,
                    &NewComment { author: None, body: self.comment.trim().to_string() },
                );
                c.comments(issue.id);
            }
            self.comment.clear();
        }
    }

    fn session_view(&mut self, ui: &mut Ui) {
        let Some(s) = self.current_session().cloned() else {
            note(ui, "Pick a session on the left.", LEGEND);
            return;
        };
        let tree = self.worktree(s.worktree_id).cloned();
        ui.horizontal(|ui| {
            let (tint, word) = match s.status {
                SessionStatus::Running => (TRACE, "working"),
                SessionStatus::Failed => (FAULT, "failed"),
                SessionStatus::Stopped => (LEGEND, "stopped"),
                SessionStatus::Idle => (OK, "idle"),
            };
            lamp(ui, word, s.status != SessionStatus::Stopped, s.status == SessionStatus::Failed);
            ui.add_space(6.0);
            let mut line = Line::new().legend("agent").gap(6.0).value(&s.agent).size(11.0);
            if let Some(m) = &s.model {
                line = line.gap(14.0).legend("model").gap(6.0).set(m).size(11.0);
            }
            if let Some(w) = &tree {
                line = line.gap(14.0).legend("worktree").gap(6.0).value(w.label()).size(11.0);
            }
            line.show(ui);
            let _ = tint;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                Line::new()
                    .measured(format!(
                        "{} in",
                        tokens(s.usage.input_tokens + s.usage.cache_read_tokens)
                    ))
                    .size(11.0)
                    .gap(8.0)
                    .measured(format!("{} out", tokens(s.usage.output_tokens)))
                    .size(11.0)
                    .gap(8.0)
                    .measured(if s.usage.cost_usd > 0.0 {
                        format!("${:.2}", s.usage.cost_usd)
                    } else {
                        String::new()
                    })
                    .size(11.0)
                    .show(ui);
            });
        });
        ui.add_space(6.0);

        let input_h = 96.0;
        let avail = ui.available_height() - input_h;
        let t = self.transcripts.entry(s.id).or_default();
        let messages = t.messages.clone();
        let scroll = egui::ScrollArea::vertical()
            .max_height(avail.max(120.0))
            .auto_shrink([false, false])
            .stick_to_bottom(self.follow)
            .show(ui, |ui| {
                ui.set_max_width(ui.available_width());
                if messages.is_empty() {
                    hint(ui, "no messages yet");
                }
                for m in &messages {
                    self.entry(ui, m);
                    ui.add_space(6.0);
                }
            });
        let at_bottom =
            scroll.state.offset.y + scroll.inner_rect.height() >= scroll.content_size.y - 4.0;
        self.follow = at_bottom;

        ui.add_space(6.0);
        let running = s.status == SessionStatus::Running;
        let missing = tree.as_ref().is_some_and(|w| w.missing);
        let hint_text = if missing {
            "this worktree is gone, the session can only be read"
        } else if running {
            "the agent is working"
        } else {
            "tell the agent what to do next"
        };
        let edit = prose(ui, &mut self.draft, hint_text, 3);
        let enter = edit.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.command);
        ui.horizontal(|ui| {
            hint(ui, "ctrl+enter sends");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can_send = !running && !missing && !self.draft.trim().is_empty();
                if (ui
                    .add(Key::new("send").solid(READOUT).enabled(can_send).min_width(100.0))
                    .clicked()
                    || (enter && can_send))
                    && let Some(c) = &mut self.client
                {
                    c.send_message(s.id, self.draft.trim());
                    self.draft.clear();
                    self.follow = true;
                }
                if running
                    && ui.add(Key::new("stop").outline(FAULT).min_width(100.0)).clicked()
                    && let Some(c) = &mut self.client
                {
                    c.interrupt(s.id);
                }
            });
        });
    }

    fn entry(&mut self, ui: &mut Ui, m: &Message) {
        match &m.entry {
            Entry::User { text } => {
                card(
                    ui,
                    Some(READOUT),
                    |ui| {
                        Line::new()
                            .legend("you")
                            .gap(8.0)
                            .note(ago(m.created_at))
                            .size(10.5)
                            .show(ui);
                    },
                    |ui| {
                        markdown::show(ui, &mut self.markdown, text, m.id as usize);
                    },
                );
            }
            Entry::Assistant { text } => {
                markdown::show(ui, &mut self.markdown, text, m.id as usize);
            }
            Entry::Thinking { text } => {
                let first = text.lines().next().unwrap_or_default();
                egui::CollapsingHeader::new(
                    egui::RichText::new(format!("thinking  {first}"))
                        .font(figure(11.0))
                        .color(LEGEND),
                )
                .id_salt(("think", m.id))
                .show(ui, |ui| note(ui, text, LEGEND));
            }
            Entry::Tool { name, input, output, error, .. } => {
                let rail = match (output, error) {
                    (None, _) => TRACE,
                    (Some(_), true) => FAULT,
                    (Some(_), false) => ETCH,
                };
                card(
                    ui,
                    Some(rail),
                    |ui| {
                        Line::new()
                            .legend(name)
                            .gap(8.0)
                            .value(tool_summary(name, input))
                            .size(11.0)
                            .elided(ui);
                    },
                    |ui| match output {
                        None => hint(ui, "running"),
                        Some(out) if out.trim().is_empty() => hint(ui, "no output"),
                        Some(out) => {
                            let shown: String = out.lines().take(14).collect::<Vec<_>>().join("\n");
                            ui.label(
                                egui::RichText::new(shown).font(mono(11.0)).color(if *error {
                                    FAULT
                                } else {
                                    VALUE
                                }),
                            );
                            let more = out.lines().count().saturating_sub(14);
                            if more > 0 {
                                hint(ui, &format!("{more} more lines"));
                            }
                        }
                    },
                );
            }
            Entry::Error { text } => status(ui, false, text),
            Entry::Notice { text } => note(ui, text, LEGEND),
        }
    }

    fn modals(&mut self, ctx: &egui::Context) {
        self.connect_modal(ctx);
        self.add_project_modal(ctx);
        self.file_issue_modal(ctx);
        self.start_modal(ctx);
        self.worktree_modal(ctx);
        self.confirm_modal(ctx);
    }

    fn connect_modal(&mut self, ctx: &egui::Context) {
        let Some(c) = &mut self.connect else {
            return;
        };
        let (mut go, mut close) = (false, false);
        let can_close = self.client.is_some() || self.starting.is_some();
        let has_local = self.local.is_some();
        let m = egui::Modal::new(egui::Id::new("connect"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(480.0);
                modal_title(ui, "Which foreman server");
                if has_local {
                    segments(
                        ui,
                        &mut c.place,
                        &[(Where::Local, "this machine", None), (Where::Remote, "remote", None)],
                    );
                    ui.add_space(8.0);
                }
                match c.place {
                    Where::Local => section(ui, "this machine", "runs beside the window", |ui| {
                        note(
                            ui,
                            "Foreman starts its server in the background on this machine and keeps it running after the window closes, so agents carry on working.",
                            LEGEND,
                        );
                    }),
                    Where::Remote => section(ui, "remote", "a foreman server elsewhere", |ui| {
                        row_help(
                            ui,
                            "address",
                            "Where the server listens, through whatever tunnel or DNAT you put in front of it.",
                            |ui| {
                                field(ui, &mut c.url, "https://foreman.example.com");
                            },
                        );
                        row_help(ui, "token", "Printed by `foreman token` on that machine.", |ui| {
                            secret(ui, &mut c.token);
                        });
                    }),
                };
                footer(ui, |ui| {
                    let ready = c.place == Where::Local
                        || (!c.url.trim().is_empty() && !c.token.trim().is_empty());
                    if ui
                        .add(Key::new("connect").solid(READOUT).enabled(ready).min_width(100.0))
                        .clicked()
                    {
                        go = true;
                    }
                    if can_close
                        && ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked()
                    {
                        close = true;
                    }
                });
            });
        if m.should_close() && can_close {
            close = true;
        }
        if go {
            let place = c.place;
            let server =
                Server { url: c.url.trim().to_string(), token: c.token.trim().to_string() };
            self.connect = None;
            match place {
                Where::Local => {
                    self.remote = false;
                    self.start_local(ctx);
                }
                Where::Remote => {
                    self.remote = true;
                    self.connect_to(ctx, server);
                }
            }
        } else if close {
            self.connect = None;
        }
    }

    fn add_project_modal(&mut self, ctx: &egui::Context) {
        let Some(a) = &mut self.add_project else {
            return;
        };
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::new("add-project")).backdrop_color(egui::Color32::from_black_alpha(150)).show(ctx, |ui| {
            ui.set_width(500.0);
            modal_title(ui, "Add a project");
            section(ui, "repository", "a folder on the server", |ui| {
                row_help(ui, "path", "Any folder inside the repo works, even a worktree. The main checkout becomes the project.", |ui| {
                    field(ui, &mut a.path, "~/git/snort");
                });
                row_help(ui, "name", "Taken from the origin remote when left empty.", |ui| {
                    field(ui, &mut a.name, "optional");
                });
            });
            footer(ui, |ui| {
                if ui.add(Key::new("add").solid(READOUT).enabled(!a.path.trim().is_empty()).min_width(100.0)).clicked() {
                    go = true;
                }
                if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                    close = true;
                }
            });
        });
        if m.should_close() {
            close = true;
        }
        if go {
            let new = NewProject {
                path: a.path.trim().to_string(),
                name: Some(a.name.trim().to_string()).filter(|n| !n.is_empty()),
            };
            if let Some(c) = &mut self.client {
                c.add_project(&new);
            }
            self.add_project = None;
        } else if close {
            self.add_project = None;
        }
    }

    fn file_issue_modal(&mut self, ctx: &egui::Context) {
        let Some(f) = &mut self.file_issue else {
            return;
        };
        let name = self
            .overview
            .as_ref()
            .and_then(|o| o.projects.iter().find(|p| p.id == f.project))
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::new("file-issue"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(520.0);
                modal_title(ui, &format!("File an issue in {name}"));
                section(ui, "issue", "what needs doing", |ui| {
                    row(ui, "title", |ui| {
                        field(ui, &mut f.title, "fix the login loop");
                    });
                    prose(ui, &mut f.body, "details, steps, what done looks like", 6);
                });
                footer(ui, |ui| {
                    if ui
                        .add(
                            Key::new("file")
                                .solid(READOUT)
                                .enabled(!f.title.trim().is_empty())
                                .min_width(100.0),
                        )
                        .clicked()
                    {
                        go = true;
                    }
                    if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                        close = true;
                    }
                });
            });
        if m.should_close() {
            close = true;
        }
        if go {
            let new = NewIssue {
                project_id: f.project,
                title: f.title.trim().to_string(),
                body: f.body.trim().to_string(),
                ..Default::default()
            };
            if let Some(c) = &mut self.client {
                c.add_issue(&new);
            }
            self.file_issue = None;
        } else if close {
            self.file_issue = None;
        }
    }

    fn start_modal(&mut self, ctx: &egui::Context) {
        let Some(o) = &self.overview else {
            return;
        };
        let Some(s) = &mut self.start else {
            return;
        };
        let agents = o.agents.clone();
        let trees: Vec<Worktree> = o
            .worktrees
            .iter()
            .filter(|w| w.project_id == s.project && !w.missing)
            .cloned()
            .collect();
        let issue = s.issue.and_then(|id| o.issues.iter().find(|i| i.id == id)).cloned();
        let project = o
            .projects
            .iter()
            .find(|p| p.id == s.project)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::new("start")).backdrop_color(egui::Color32::from_black_alpha(150)).show(ctx, |ui| {
            ui.set_width(520.0);
            modal_title(ui, &match &issue {
                Some(i) => format!("Start an agent on #{}", i.id),
                None => format!("Start an agent in {project}"),
            });
            section(ui, "agent", "who does the work", |ui| {
                row(ui, "agent", |ui| {
                    choice(ui, "agent", &mut s.agent, agents.iter().map(|a| {
                        let label = if a.available { a.label.clone() } else { format!("{} (not installed)", a.label) };
                        (a.name.clone(), label)
                    }));
                });
                row_help(ui, "model", "Passed to the agent as is. Empty uses its default.", |ui| {
                    field(ui, &mut s.model, "default");
                });
            });
            ui.add_space(8.0);
            section(ui, "worktree", "where it works", |ui| {
                let mut opts: Vec<(PlaceChoice, String)> = vec![(PlaceChoice::New, "a new worktree".into())];
                for w in &trees {
                    let label = if w.main { format!("{} (main checkout)", w.label()) } else { w.label() };
                    opts.push((if w.main { PlaceChoice::Main } else { PlaceChoice::Existing(w.id) }, label));
                }
                row(ui, "place", |ui| {
                    choice(ui, "place", &mut s.place, opts);
                });
                if s.place == PlaceChoice::New {
                    row_help(ui, "branch", "Made off the main checkout's branch, beside the repo in .worktrees.", |ui| {
                        let hint_text = issue.as_ref().map(|i| format!("issue-{}-{}", i.id, foreman_core::branch_slug(&i.title))).unwrap_or("fix-login".into());
                        field(ui, &mut s.branch, &hint_text);
                    });
                }
                let busy = trees.iter().any(|w| {
                    let here = match &s.place {
                        PlaceChoice::Main => w.main,
                        PlaceChoice::Existing(id) => w.id == *id,
                        PlaceChoice::New => false,
                    };
                    here && o.sessions.iter().any(|x| x.worktree_id == w.id && x.status == SessionStatus::Running)
                });
                if busy {
                    note(ui, "Another agent is working in this worktree. Two agents in one checkout will trip over each other.", WARN);
                }
            });
            ui.add_space(8.0);
            section(ui, "prompt", if issue.is_some() { "empty sends the issue" } else { "the first message" }, |ui| {
                prose(ui, &mut s.prompt, "what should it do", 4);
            });
            let available = agents.iter().any(|a| a.name == s.agent && a.available);
            let needs_branch = s.place == PlaceChoice::New && s.branch.trim().is_empty() && issue.is_none();
            if !available {
                status(ui, false, "that agent is not installed on the server");
            } else if needs_branch {
                status(ui, false, "name the branch for the new worktree");
            }
            footer(ui, |ui| {
                let ready = available && !needs_branch;
                if ui.add(Key::new("start").solid(READOUT).enabled(ready).min_width(100.0)).clicked() {
                    go = true;
                }
                if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                    close = true;
                }
            });
        });
        if m.should_close() {
            close = true;
        }
        if go {
            let place = match &s.place {
                PlaceChoice::Main => Place::Main,
                PlaceChoice::Existing(id) => Place::Existing { worktree_id: *id },
                PlaceChoice::New => Place::New { branch: s.branch.trim().to_string() },
            };
            let new = NewSession {
                project_id: s.project,
                agent: s.agent.clone(),
                place,
                title: None,
                model: Some(s.model.trim().to_string()).filter(|m| !m.is_empty()),
                issue_id: s.issue,
                prompt: Some(s.prompt.trim().to_string()).filter(|p| !p.is_empty()),
            };
            if let Some(c) = &mut self.client {
                match s.issue {
                    Some(iid) => c.start_issue(iid, &new),
                    None => c.add_session(&new),
                }
            }
            self.start = None;
        } else if close {
            self.start = None;
        }
    }

    fn worktree_modal(&mut self, ctx: &egui::Context) {
        let Some((pid, branch)) = &mut self.new_worktree else {
            return;
        };
        let (mut go, mut close) = (false, false);
        let m = egui::Modal::new(egui::Id::new("worktree")).backdrop_color(egui::Color32::from_black_alpha(150)).show(ctx, |ui| {
            ui.set_width(460.0);
            modal_title(ui, "New worktree");
            section(ui, "branch", "checked out beside the repo", |ui| {
                row_help(ui, "name", "An existing branch is checked out, a new one is made off the main checkout.", |ui| {
                    field(ui, branch, "fix-login");
                });
            });
            footer(ui, |ui| {
                if ui.add(Key::new("make it").solid(READOUT).enabled(!branch.trim().is_empty()).min_width(100.0)).clicked() {
                    go = true;
                }
                if ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked() {
                    close = true;
                }
            });
        });
        if m.should_close() {
            close = true;
        }
        if go {
            let (pid, branch) = (*pid, branch.trim().to_string());
            if let Some(c) = &mut self.client {
                c.add_worktree(pid, &NewWorktree { branch, base: None });
            }
            self.new_worktree = None;
        } else if close {
            self.new_worktree = None;
        }
    }

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(c) = &self.confirm else {
            return;
        };
        let (title, body, key) = match c {
            Confirm::Project(_, name) => (
                format!("Remove {name}?"),
                "Its issues, sessions and transcripts are deleted from foreman. The repository and its worktrees stay on disk.",
                "remove",
            ),
            Confirm::Worktree(_, name, missing) => (
                format!("Remove the {name} worktree?"),
                if *missing {
                    "It is already gone from disk. Its sessions and their transcripts are deleted."
                } else {
                    "The checkout is deleted, uncommitted changes with it. The branch is kept. Its sessions and transcripts are deleted."
                },
                "remove",
            ),
            Confirm::Session(_, name) => (
                format!("Delete {name}?"),
                "The transcript is deleted. The worktree and the agent's own session files stay.",
                "delete",
            ),
            Confirm::Issue(_, name) => (
                format!("Delete {name}?"),
                "Its comments go with it. Sessions that worked on it are kept.",
                "delete",
            ),
        };
        let mut done = false;
        let m = egui::Modal::new(egui::Id::new("confirm"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(400.0);
                modal_title(ui, &title);
                note(ui, body, LEGEND);
                footer(ui, |ui| {
                    if ui.add(Key::new(key).solid(FAULT).min_width(100.0)).clicked() {
                        if let Some(client) = &mut self.client {
                            match c {
                                Confirm::Project(id, _) => client.remove_project(*id),
                                Confirm::Worktree(id, _, _) => client.remove_worktree(*id, true),
                                Confirm::Session(id, _) => client.delete_session(*id),
                                Confirm::Issue(id, _) => client.delete_issue(*id),
                            }
                        }
                        done = true;
                    }
                    if ui.add(Key::new("keep it").outline(VALUE).min_width(100.0)).clicked() {
                        done = true;
                    }
                });
            });
        if done || m.should_close() {
            self.confirm = None;
        }
    }

    pub fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.tick(&ctx);
        let narrow = ctx.content_rect().width() < 720.0;

        egui::Panel::top("header")
            .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(Margin {
                left: 12,
                right: 12,
                top: 8,
                bottom: 6,
            }))
            .show(ui, |ui| self.header(ui));
        if let Some((ok, text)) = self.notice.clone() {
            egui::Panel::bottom("notice")
                .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(Margin::symmetric(12, 4)))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        status(ui, ok, &text);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.small_button("x").clicked() {
                                self.notice = None;
                            }
                        });
                    });
                });
        }
        let side_w = if narrow { ctx.content_rect().width() * 0.42 } else { SIDE_W };
        egui::Panel::left("side")
            .exact_size(side_w)
            .resizable(false)
            .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(8))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    self.side(ui);
                });
            });
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let r = ui.max_rect();
            ui.painter().rect_filled(r, 0.0, CHASSIS);
            ui.painter().line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, ETCH));
            egui::Frame::NONE.inner_margin(Margin::same(10)).show(ui, |ui| match self.tab {
                Tab::Issues => self.issues(ui),
                Tab::Session => self.session_view(ui),
            });
        });
        self.modals(&ctx);
        self.markdown.sweep();
        if let Some((_, at)) = &self.offline {
            ctx.request_repaint_after(
                RETRY.saturating_sub(at.elapsed()) + Duration::from_millis(50),
            );
        }
        if self.current_session().is_some_and(|s| s.status == SessionStatus::Running) {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.trim_end_matches('/').rsplit('/').take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.1}k", n as f64 / 1e3),
        n => n.to_string(),
    }
}

fn ago(ms: i64) -> String {
    let now = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    let s = ((now - ms) / 1000).max(0);
    match s {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let text = pick("command")
        .or_else(|| pick("path"))
        .or_else(|| pick("file_path"))
        .or_else(|| pick("pattern"))
        .or_else(|| pick("query"))
        .or_else(|| pick("url"))
        .unwrap_or_else(|| if input.is_null() { String::new() } else { input.to_string() });
    let line = text.lines().next().unwrap_or_default().to_string();
    let _ = name;
    line.chars().take(160).collect()
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.store(storage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_shorten() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(12_400), "12.4k");
        assert_eq!(tokens(3_200_000), "3.2M");
    }

    #[test]
    fn tool_summaries_pick_the_telling_field() {
        assert_eq!(tool_summary("bash", &serde_json::json!({"command": "ls -la\necho"})), "ls -la");
        assert_eq!(tool_summary("read", &serde_json::json!({"path": "/a/b.rs"})), "/a/b.rs");
        assert_eq!(tool_summary("x", &serde_json::Value::Null), "");
    }

    #[test]
    fn paths_shorten_to_their_last_two_parts() {
        assert_eq!(short_path("/home/k/git/snort"), "git/snort");
        assert_eq!(short_path("snort/"), "snort");
    }
}
