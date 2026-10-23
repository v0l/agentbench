use crate::client::{Client, Reply, Server};
use crate::fonts::{self, Installed, Typeface};
use crate::form::{field, field_then, prose, secret};
use crate::picker::ModelPicker;
use egui::{Align, Layout, Margin, Sense, Stroke, Ui, Vec2};
use egui_bench::key::Key;
use egui_bench::markdown;
use egui_bench::prelude::*;
use foreman_core::{
    Comment, Entry, Id, Issue, IssuePatch, IssueState, Message, NewComment, NewIssue, NewProject,
    NewSession, NewWorktree, Overview, Place, Question, Session, SessionId, SessionStatus,
    Worktree,
};
use std::collections::{HashMap, HashSet};
use web_time::{Duration, Instant};

const SIDE_W: f32 = 320.0;
const RETRY: Duration = Duration::from_secs(3);
const TREE_INDENT: f32 = 16.0;
const TAB_TITLE: usize = 32;
const TREE_X: f32 = 5.0;
/// The most tiles any grid has.
const SLOTS: usize = 4;
/// The space between tiles of the grid.
const TILE_GAP: f32 = 8.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Chat,
    Issues,
}

/// How the chat area is tiled. One tile is focused; opening a chat replaces what it shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grid {
    /// One chat filling the area.
    Single,
    /// Two chats side by side.
    Columns,
    /// Two chats one above the other.
    Rows,
    /// Four chats, two by two.
    Quad,
}

impl Grid {
    pub const ALL: [Grid; 4] = [Grid::Single, Grid::Columns, Grid::Rows, Grid::Quad];

    /// Columns and rows.
    pub fn dims(self) -> (usize, usize) {
        match self {
            Grid::Single => (1, 1),
            Grid::Columns => (2, 1),
            Grid::Rows => (1, 2),
            Grid::Quad => (2, 2),
        }
    }

    /// How many chats are on screen at once.
    pub fn slots(self) -> usize {
        let (c, r) = self.dims();
        c * r
    }

    fn key(self) -> &'static str {
        match self {
            Grid::Single => "1x1",
            Grid::Columns => "2x1",
            Grid::Rows => "1x2",
            Grid::Quad => "2x2",
        }
    }

    fn from_key(key: &str) -> Option<Grid> {
        Grid::ALL.into_iter().find(|g| g.key() == key)
    }

    fn about(self) -> &'static str {
        match self {
            Grid::Single => "one chat",
            Grid::Columns => "two chats side by side",
            Grid::Rows => "two chats, one above the other",
            Grid::Quad => "four chats in a grid",
        }
    }
}

/// What fills one tile of the grid.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Tile {
    /// The form for starting a chat, in the focused tile when it has no chat.
    New,
    Chat(SessionId),
    /// A tile with nothing in it.
    Empty,
}

/// What is typed into a chat but not yet sent, kept per chat so each tile has its own.
struct Pane {
    draft: String,
    follow: bool,
}

impl Default for Pane {
    fn default() -> Self {
        Pane { draft: String::new(), follow: true }
    }
}

/// Opens `sid` in the grid: a chat already on screen is only focused, and any other
/// replaces what the focused tile shows. Returns the tile that ends up focused.
fn place(slots: &mut [Option<SessionId>], shown: usize, focus: usize, sid: SessionId) -> usize {
    if let Some(at) = slots.iter().take(shown).position(|x| *x == Some(sid)) {
        return at;
    }
    // The chat may sit in a hidden tile; it lives in one place only.
    for x in slots.iter_mut().filter(|x| **x == Some(sid)) {
        *x = None;
    }
    slots[focus] = Some(sid);
    focus
}

#[derive(Clone, PartialEq)]
struct Fresh {
    project: Id,
    place: PlaceChoice,
    branch: String,
    agent: String,
    model: String,
}

impl Fresh {
    fn new(project: Id, place: PlaceChoice, agent: String) -> Self {
        Fresh { project, place, branch: String::new(), agent, model: String::new() }
    }
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

pub struct LocalServer {
    pub start: Box<dyn Fn() -> Result<Server, String> + Send + Sync>,
}

struct AddProject {
    path: String,
    name: String,
    create: bool,
}

struct FileIssue {
    project: Id,
    title: String,
    body: String,
}

/// What has been picked and typed so far for one question.
#[derive(Clone, Default)]
struct Slot {
    picked: Vec<usize>,
    typed: String,
}

impl Slot {
    fn reply(&self, q: &Question) -> String {
        let mut parts: Vec<&str> =
            self.picked.iter().filter_map(|&i| q.options.get(i)).map(String::as_str).collect();
        let typed = self.typed.trim();
        if !typed.is_empty() {
            parts.push(typed);
        }
        parts.join(", ")
    }
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

struct Settings {
    speech: crate::voice::Settings,
    place: Where,
    url: String,
    token: String,
    face: Typeface,
    agent_error: Option<String>,
    asking_agent: bool,
    model: foreman_core::ModelSettings,
    loaded: bool,
    models: Option<Result<Vec<String>, String>>,
    asking: bool,
    title: foreman_core::TitleSettings,
    title_loaded: bool,
    title_models: Option<Result<Vec<String>, String>>,
    title_asking: bool,
}

enum Confirm {
    Project(Id, String),
    Worktree(Id, String, bool),
    Session(SessionId, String),
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
    face: Typeface,
    installed: std::sync::Arc<Installed>,
    font_scan: Option<std::sync::mpsc::Receiver<Installed>>,
    font_load: Option<std::sync::mpsc::Receiver<egui::FontDefinitions>>,
    local: Option<std::sync::Arc<LocalServer>>,
    remote: bool,
    starting: Option<std::sync::mpsc::Receiver<Result<Server, String>>>,
    overview: Option<Overview>,
    cursor: Option<Id>,
    watching: bool,
    offline: Option<(String, Instant)>,
    notice: Option<(bool, String)>,
    tab: Tab,
    fresh: Option<Fresh>,
    drafts: HashMap<String, Vec<Slot>>,
    filter: Filter,
    project: Option<Id>,
    session: Option<SessionId>,
    grid: Grid,
    narrow: bool,
    /// The chat in each tile. Tiles past the grid's size are hidden but remembered.
    slots: [Option<SessionId>; SLOTS],
    /// The focused tile: the one a chat opened from the list replaces.
    focus_at: usize,
    panes: HashMap<SessionId, Pane>,
    /// The form for a new chat has its own draft, as it has no session to key one by.
    new_pane: Pane,
    /// Set while a tile is drawn: it is small, and it is the one the keyboard and files go to.
    compact: bool,
    focused: bool,
    issue: Option<Id>,
    collapsed: HashSet<Id>,
    transcripts: HashMap<SessionId, Transcript>,
    comments: HashMap<Id, Vec<Comment>>,
    draft: String,
    comment: String,
    add_project: Option<AddProject>,
    file_issue: Option<FileIssue>,
    start: Option<Start>,
    new_worktree: Option<(Id, String)>,
    confirm: Option<Confirm>,
    settings: Option<Settings>,
    save_requested: bool,
    tray: crate::attach::Tray,
    voice: crate::voice::Voice,
    speech: crate::voice::Settings,
    /// Models on the server the foreman agent is saved to use. None until asked.
    agent_models: HashMap<String, Result<Vec<String>, String>>,
    asking_models: HashSet<String>,
    markdown: markdown::Cache,
    follow: bool,
    awaiting: Option<Id>,
    thinking: bool,
    /// Put the cursor back in this tile's composer: set when speech lands in its draft.
    refocus: Option<Tile>,
    /// The tile being drawn, so a key pressed in it knows whose draft it works on.
    drawing: Option<Tile>,
    /// The tile whose speak key started the take in progress; the words go to its draft.
    speaking_in: Option<Tile>,
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
        let face = Typeface::load(storage);
        let installed = std::sync::Arc::new(Installed::none());
        fonts::apply(ctx, &face, &installed);
        let mut app = App {
            client: None,
            face,
            installed,
            font_scan: scan_fonts(ctx),
            font_load: None,
            local: local.map(std::sync::Arc::new),
            remote,
            starting: None,
            overview: None,
            cursor: None,
            watching: false,
            offline: None,
            notice: None,
            tab: Tab::Chat,
            fresh: None,
            drafts: HashMap::new(),
            filter: Filter::Open,
            project: None,
            session: None,
            grid: storage
                .and_then(|s| s.get_string("grid"))
                .and_then(|k| Grid::from_key(&k))
                .unwrap_or(Grid::Single),
            narrow: false,
            slots: {
                let mut slots = [None; SLOTS];
                let saved = storage.and_then(|s| s.get_string("tiles")).unwrap_or_default();
                for (slot, id) in slots.iter_mut().zip(saved.split(',')) {
                    *slot = id.parse::<SessionId>().ok();
                }
                slots
            },
            focus_at: storage
                .and_then(|s| s.get_string("focus"))
                .and_then(|f| f.parse::<usize>().ok())
                .filter(|f| *f < SLOTS)
                .unwrap_or(0),
            panes: HashMap::new(),
            new_pane: Pane::default(),
            compact: false,
            focused: true,
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
            settings: None,
            save_requested: false,
            tray: crate::attach::Tray::default(),
            voice: Default::default(),
            speech: crate::voice::Settings::load(storage),
            agent_models: HashMap::new(),
            asking_models: HashSet::new(),
            markdown: markdown::Cache::default(),
            follow: true,
            awaiting: None,
            refocus: None,
            drawing: None,
            speaking_in: None,
            thinking: storage.and_then(|s| s.get_string("thinking")).as_deref() != Some("false"),
        };
        if !app.remote {
            app.start_local(ctx);
        } else if url.is_empty() || token.is_empty() {
            app.open_settings();
            if let Some(st) = &mut app.settings {
                (st.url, st.token) = (url, token);
            }
        } else {
            app.connect_to(ctx, Server { url, token });
        }
        app
    }

    pub fn store(&self, storage: &mut dyn eframe::Storage) {
        self.face.save(storage);
        storage.set_string("remote", self.remote.to_string());
        storage.set_string("thinking", self.thinking.to_string());
        self.speech.save(storage);
        storage.set_string("grid", self.grid.key().to_string());
        let tiles: Vec<String> =
            self.slots.iter().map(|s| s.map(|s| s.to_string()).unwrap_or_default()).collect();
        storage.set_string("tiles", tiles.join(","));
        storage.set_string("focus", self.focus_at.to_string());
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
        crate::spawn_blocking(move || {
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
        self.agent_models.clear();
        self.asking_models.clear();
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
        // Order by creation, not activity: `updated_at` moves on every message and
        // status change, which made chats jump around the tree while agents worked.
        out.sort_by_key(|s| std::cmp::Reverse((s.created_at, s.id)));
        out
    }

    pub fn settings_loaded(&self) -> Option<bool> {
        self.settings.as_ref().map(|s| s.loaded)
    }

    pub fn pick_fresh_agent(&mut self, agent: &str) {
        if let Some(f) = &mut self.fresh {
            f.agent = agent.into();
        }
    }

    pub fn models_for(&self, agent: &str) -> Option<usize> {
        match self.agent_models.get(agent)? {
            Ok(l) => Some(l.len()),
            Err(_) => Some(0),
        }
    }

    pub fn settings_title_loaded(&self) -> bool {
        self.settings.as_ref().is_some_and(|s| s.title_loaded)
    }

    /// Fill in one model level (low, medium or high) as the settings dialog would.
    pub fn settings_set_level(&mut self, level: &str, url: &str, model: &str) {
        if let Some(st) = &mut self.settings {
            let l = match level {
                "low" => &mut st.model.low,
                "medium" => &mut st.model.medium,
                _ => &mut st.model.high,
            };
            l.url = url.into();
            l.model = model.into();
        }
    }

    pub fn settings_set_title(&mut self, url: &str, model: &str) {
        if let Some(st) = &mut self.settings {
            st.title.url = url.into();
            st.title.model = model.into();
        }
    }

    pub fn settings_set_server(&mut self, url: &str, token: &str) {
        if let Some(st) = &mut self.settings {
            st.place = Where::Remote;
            st.url = url.into();
            st.token = token.into();
        }
    }

    pub fn settings_set_text_size(&mut self, size: f32) {
        if let Some(st) = &mut self.settings {
            st.face.size = size;
        }
    }

    /// Put what was said into the draft of the tile whose speak key was pressed. The draft on
    /// screen belongs to whichever tile is being drawn, and none is when this runs.
    pub fn add_speech(&mut self, text: &str) {
        let tile = self.speaking_in.take().unwrap_or(Tile::New);
        let draft = match tile {
            Tile::Chat(s) => &mut self.panes.entry(s).or_default().draft,
            Tile::New => &mut self.new_pane.draft,
        };
        crate::voice::append(draft, text);
        self.refocus = Some(tile);
    }

    /// The draft of the chat `session`, as the tile shows it.
    pub fn draft_of(&self, session: SessionId) -> &str {
        self.panes.get(&session).map_or("", |p| p.draft.as_str())
    }

    /// Whether the microphone is open.
    pub fn listening(&self) -> bool {
        self.voice.listening()
    }

    pub fn pending_files(&self) -> usize {
        self.tray.files.len()
    }

    pub fn thumbnails_ready(&self) -> usize {
        self.tray.ready()
    }

    pub fn text_size(&self) -> f32 {
        self.face.size
    }

    /// Read the chosen font files in the background and swap them in when they are ready.
    fn load_fonts(&mut self, ctx: &egui::Context) {
        let (tx, rx) = std::sync::mpsc::channel();
        let (face, installed, ctx) = (self.face.clone(), self.installed.clone(), ctx.clone());
        crate::spawn_blocking(move || {
            let _ = tx.send(fonts::definitions(&face, &installed));
            ctx.request_repaint();
        });
        self.font_load = Some(rx);
    }

    /// What the save button does, for tests that cannot click a painted key.
    pub fn settings_save(&mut self) {
        self.save_requested = true;
    }

    pub fn settings_model(&self) -> Option<String> {
        self.settings.as_ref().map(|s| s.model.model.clone())
    }

    pub fn settings_models(&self) -> Option<usize> {
        match &self.settings.as_ref()?.models {
            Some(Ok(l)) => Some(l.len()),
            _ => None,
        }
    }

    pub fn issue_open(&self) -> Option<Id> {
        self.issue
    }

    pub fn open(&self) -> Option<SessionId> {
        self.session
    }

    pub fn last_error(&self) -> Option<String> {
        self.notice.as_ref().filter(|(ok, _)| !ok).map(|(_, t)| t.clone())
    }

    pub fn transcript_len(&self, session: SessionId) -> usize {
        self.transcripts.get(&session).map_or(0, |t| t.messages.len())
    }

    pub fn overview(&self) -> Option<&Overview> {
        self.overview.as_ref()
    }

    fn current_session(&self) -> Option<&Session> {
        self.session_by_id(self.session?)
    }

    fn session_by_id(&self, id: SessionId) -> Option<&Session> {
        self.overview.as_ref()?.sessions.iter().find(|s| s.id == id)
    }

    /// The grid in use: a narrow window has no room to tile.
    fn grid_now(&self) -> Grid {
        if self.narrow { Grid::Single } else { self.grid }
    }

    pub fn grid(&self) -> Grid {
        self.grid
    }

    pub fn set_grid(&mut self, grid: Grid) {
        self.grid = grid;
        // A tile that is hidden cannot stay focused: its chat moves to the first tile.
        let n = self.grid_now().slots();
        if self.focus_at >= n {
            self.slots.swap(0, self.focus_at);
            self.focus_at = 0;
        }
        self.fetch_shown();
    }

    /// The focused tile, which stays inside the grid.
    fn focus_slot(&self) -> usize {
        self.focus_at.min(self.grid_now().slots() - 1)
    }

    /// What each tile of the chat area shows, first tile first.
    fn tiles(&self) -> Vec<Tile> {
        let focus = self.focus_slot();
        (0..self.grid_now().slots())
            .map(|i| match self.slots[i] {
                Some(s) => Tile::Chat(s),
                None if i == focus => Tile::New,
                None => Tile::Empty,
            })
            .collect()
    }

    /// The chats on screen, first tile first.
    pub fn shown(&self) -> Vec<SessionId> {
        self.slots[..self.grid_now().slots()].iter().flatten().copied().collect()
    }

    /// The focused tile, counting from 0 in reading order.
    pub fn focused_tile(&self) -> usize {
        self.focus_slot()
    }

    /// Forgets a chat that has been deleted, as closing it does.
    pub fn forget_chat(&mut self, sid: SessionId) {
        self.forget(sid);
    }

    fn fetch_shown(&mut self) {
        for sid in self.shown() {
            self.fetch_messages(sid);
        }
    }

    /// The chat in tile `at`, if it has one.
    pub fn tile_chat(&self, at: usize) -> Option<SessionId> {
        self.slots.get(at).copied().flatten()
    }

    /// Moves focus to a tile. Its chat, if any, becomes the open one.
    pub fn focus_tile(&mut self, at: usize) {
        self.focus_at = at;
        let next = self.slots[at];
        if self.session == next {
            return;
        }
        self.session = next;
        if self.fresh.is_none()
            && let Some(p) = next.and_then(|s| self.session_by_id(s)).map(|s| s.project_id)
        {
            self.project = Some(p);
        }
    }

    /// A chat is gone: drop what is kept for it. Its tile is left empty.
    fn forget(&mut self, sid: SessionId) {
        for x in self.slots.iter_mut().filter(|x| **x == Some(sid)) {
            *x = None;
        }
        self.panes.remove(&sid);
        self.transcripts.remove(&sid);
        if self.session == Some(sid) {
            self.session = None;
        }
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
                }
                for x in self.slots.iter_mut() {
                    if x.is_some_and(|s| !o.sessions.iter().any(|y| y.id == s)) {
                        *x = None;
                    }
                }
                self.panes.retain(|id, _| o.sessions.iter().any(|x| x.id == *id));
                // Ids can be reused once a chat is deleted, so a cached transcript must not
                // outlive its session or the next chat to take the id would show it.
                self.transcripts.retain(|id, _| o.sessions.iter().any(|x| x.id == *id));
                self.overview = Some(o);
                self.cursor = Some(cursor);
                // A window just started gets back the chat of the tile that was focused.
                if self.session.is_none() && self.fresh.is_none() {
                    self.session = self.slots[self.focus_slot()];
                }
                self.fetch_shown();
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
            Reply::Model(m) => {
                if let Some(st) = &mut self.settings {
                    if st.loaded {
                        self.settings = None;
                        self.notice =
                            Some((true, format!("foreman agent uses {} at {}", m.model, m.url)));
                    } else {
                        st.model = m;
                        st.loaded = true;
                        if let Some(c) = &mut self.client {
                            c.title();
                        }
                        if !st.model.url.trim().is_empty()
                            && let Some(c) = &mut self.client
                        {
                            st.asking = true;
                            c.models(&st.model);
                        }
                    }
                }
                self.agent_models.remove("foreman");
                if let Some(c) = &mut self.client {
                    c.overview();
                }
            }
            Reply::Models(list) => {
                if let Some(st) = &mut self.settings {
                    st.asking = false;
                    if let (Ok(l), true) = (&list, st.model.model.trim().is_empty()) {
                        st.model.model = l.first().cloned().unwrap_or_default();
                    }
                    st.models = Some(list);
                }
            }
            Reply::Title(t) => {
                if let Some(st) = self.settings.as_mut().filter(|s| !s.title_loaded) {
                    st.title = t;
                    st.title_loaded = true;
                    if !st.title.url.trim().is_empty()
                        && let Some(c) = &mut self.client
                    {
                        st.title_asking = true;
                        c.title_models(&st.title);
                    }
                }
            }
            Reply::TitleModels(list) => {
                if let Some(st) = &mut self.settings {
                    st.title_asking = false;
                    st.title_models = Some(list);
                }
            }
            Reply::AgentModels(agent, list) => {
                self.asking_models.remove(&agent);
                self.agent_models.insert(agent, list);
            }
            Reply::Started(session) => {
                self.transcripts.remove(&session.id);
                if self.awaiting == Some(session.project_id) {
                    self.awaiting = None;
                    self.open_session(session.id, session.project_id);
                }
                if let Some(c) = &mut self.client {
                    c.overview();
                }
            }
            Reply::Done(said) => {
                if !said.is_empty() {
                    self.notice = Some((true, said));
                }
            }
            Reply::Failed(e) => {
                self.awaiting = None;
                if let Some(st) = self.settings.as_mut().filter(|s| s.asking_agent && !s.loaded) {
                    st.asking_agent = false;
                    st.agent_error = Some(e.clone());
                }
                self.notice = Some((false, e));
            }
            Reply::Offline(e) => {
                self.watching = false;
                self.offline = Some((e, Instant::now()));
            }
        }
    }

    fn fetch_messages(&mut self, sid: SessionId) {
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
        if let Some(found) = self.font_scan.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.font_scan = None;
            self.installed = std::sync::Arc::new(found);
            self.load_fonts(ctx);
        }
        if let Some(defs) = self.font_load.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.font_load = None;
            ctx.set_fonts(defs);
        }
        if let Some(problem) = self.tray.poll(ctx) {
            self.notice = Some((false, problem));
        }
        for said in self.voice.poll() {
            match said {
                Ok(text) if text.trim().is_empty() => {
                    self.notice = Some((false, "no speech was heard".into()));
                }
                Ok(text) => {
                    self.add_speech(&text);
                }
                Err(e) => self.notice = Some((false, e)),
            }
        }
        if self.voice.busy() {
            ctx.request_repaint_after(Duration::from_millis(60));
        }
        self.take_dropped(ctx);
        let wants = self
            .start
            .as_ref()
            .map(|s| s.agent.clone())
            .or_else(|| {
                let fresh = self.fresh.as_ref()?;
                (self.tab == Tab::Chat).then(|| fresh.agent.clone())
            })
            .filter(|a| !a.is_empty());
        if let Some(agent) = wants
            && !self.agent_models.contains_key(&agent)
            && !self.asking_models.contains(&agent)
            && let Some(c) = &mut self.client
        {
            self.asking_models.insert(agent.clone());
            c.agent_models(&agent);
        }
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
            let issues_label = format!("issues {open}");
            let chat_label = self
                .current_session()
                .map(|s| foreman_core::short_title(&s.title, TAB_TITLE))
                .unwrap_or_else(|| "new chat".to_string());
            let opts: Vec<(Tab, &str)> =
                vec![(Tab::Chat, chat_label.as_str()), (Tab::Issues, issues_label.as_str())];
            let w = (ui.available_width() - 640.0 - if self.narrow { 0.0 } else { 120.0 })
                .clamp(200.0, 560.0);
            ui.allocate_ui(Vec2::new(w, 24.0), |ui| tabs(ui, &mut self.tab, &opts));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let ready = self.overview.is_some();
                if ui
                    .add(
                        Key::new("new chat")
                            .solid(READOUT)
                            .enabled(ready && self.project.is_some()),
                    )
                    .clicked()
                {
                    self.new_chat(None);
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
                if !self.narrow {
                    for g in Grid::ALL.into_iter().rev() {
                        if grid_button(ui, g, self.grid == g).clicked() {
                            self.set_grid(g);
                        }
                    }
                }
                let key = ui.add(Key::new("settings").outline(VALUE));
                key.widget_info(|| {
                    egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "settings")
                });
                if key.clicked() {
                    self.open_settings();
                }
                if !self.remote {
                    chip(ui, "this machine", OK, self.overview.is_some());
                }
                if let Some(o) = &self.overview {
                    status_badge(ui, &o.sessions);
                }
            });
        });
    }

    fn open_settings(&mut self) {
        let (url, token) = self
            .client
            .as_ref()
            .filter(|_| self.remote)
            .map(|c| (c.server.url.clone(), c.server.token.clone()))
            .unwrap_or_default();
        let asking_agent = self.client.is_some();
        self.settings = Some(Settings {
            speech: self.speech.clone(),
            place: if self.remote || self.local.is_none() { Where::Remote } else { Where::Local },
            url,
            token,
            face: self.face.clone(),
            agent_error: None,
            asking_agent,
            model: foreman_core::ModelSettings {
                url: String::new(),
                model: String::new(),
                key: String::new(),
                has_key: false,
                steps: 0,
                low: Default::default(),
                medium: Default::default(),
                high: Default::default(),
            },
            loaded: false,
            models: None,
            asking: false,
            title: foreman_core::TitleSettings {
                enabled: true,
                url: String::new(),
                model: String::new(),
                key: String::new(),
                has_key: false,
            },
            title_loaded: false,
            title_models: None,
            title_asking: false,
        });
        if let Some(c) = &mut self.client {
            c.model();
        }
    }

    /// Whether the server fields in the dialog name a different server from the one in use.
    fn server_changed(&self, st: &Settings) -> bool {
        match st.place {
            Where::Local => self.remote || (self.client.is_none() && self.starting.is_none()),
            Where::Remote => {
                !self.remote
                    || self.client.as_ref().is_none_or(|c| {
                        c.server.url.trim() != st.url.trim()
                            || c.server.token.trim() != st.token.trim()
                    })
            }
        }
    }

    fn settings_modal(&mut self, ctx: &egui::Context) {
        let Some(mut st) = self.settings.take() else {
            return;
        };
        let (mut save, mut close, mut probe, mut probe_title) = (false, false, false, false);
        let can_close = self.client.is_some() || self.starting.is_some();
        let has_local = self.local.is_some();
        let installed = self.installed.clone();
        let scanning = self.font_scan.is_some();
        let main_list = self.agent_models.get("foreman").cloned();
        let m = egui::Modal::new(egui::Id::new("settings"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .show(ctx, |ui| {
                ui.set_width(540.0);
                modal_title(ui, "Settings");
                let body_h = (ctx.content_rect().height() - 220.0).max(240.0);
                egui::ScrollArea::vertical().max_height(body_h).auto_shrink([false, true]).show(ui, |ui| {
                ui.set_width(ui.available_width() - 12.0);
                server_section(ui, &mut st, has_local);
                ui.add_space(14.0);
                appearance_section(ui, &mut st.face, &installed, scanning);
                ui.add_space(14.0);
                speech_section(ui, &mut st.speech);
                ui.add_space(14.0);
                if !can_close && st.place == Where::Remote {
                    hint(ui, "Connect to a server to set up its agents.");
                } else if let Some(e) = &st.agent_error {
                    section(ui, "foreman agent", "set on the server", |ui| status(ui, false, e));
                } else if !st.loaded {
                    section(ui, "foreman agent", "set on the server", |ui| {
                        hint(ui, if st.asking_agent { "asking the server" } else { "connect to a server to set up its agents" });
                    });
                } else {
                let main_list = main_list.as_ref();
                section(ui, "model server", "any OpenAI compatible endpoint", |ui| {
                    row_help(ui, "address", "The base URL, ending in /v1 for most servers.", |ui| {
                        let r = field(ui, &mut st.model.url, "http://localhost:11434/v1");
                        if r.changed() {
                            st.models = None;
                        }
                        if r.lost_focus() && st.models.is_none() && !st.asking && !st.model.url.trim().is_empty() {
                            probe = true;
                        }
                    });
                    let explain = if st.model.has_key {
                        "Sent as a bearer token. A key is saved; leave this empty to keep it."
                    } else {
                        "Sent as a bearer token. Leave empty if the server needs none."
                    };
                    row_help(ui, "key", explain, |ui| {
                        let r = secret(ui, &mut st.model.key);
                        if r.changed() {
                            st.models = None;
                        }
                        if r.lost_focus() && st.models.is_none() && !st.asking && !st.model.url.trim().is_empty() {
                            probe = true;
                        }
                    });
                    row(ui, "model", |ui| match &st.models {
                        Some(Ok(list)) if !list.is_empty() => {
                            let w = (ui.available_width() - 96.0).max(120.0);
                            ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
                                ModelPicker::new("settings", list).show(ui, &mut st.model.model);
                            });
                            probe = ui
                                .add(Key::new("refresh").outline(VALUE).enabled(!st.asking))
                                .clicked();
                        }
                        _ => {
                            field_then(ui, &mut st.model.model, "model name", 130.0, |ui| {
                                probe = ui
                                    .add(Key::new(if st.asking { "asking" } else { "list models" }).outline(VALUE).enabled(!st.asking && !st.model.url.trim().is_empty()))
                                    .clicked();
                            });
                        }
                    });
                    row_help(
                        ui,
                        "max steps",
                        "Model calls in one turn before the agent pauses and waits for you to say continue.",
                        |ui| {
                            ui.add(egui::DragValue::new(&mut st.model.steps).range(1..=100_000).speed(10));
                        },
                    );
                    match &st.models {
                        Some(Err(e)) => status(ui, false, e),
                        Some(Ok(l)) => status(ui, true, &format!("{} models on the server", l.len())),
                        None => {}
                    }
                });
                ui.add_space(8.0);
                hint(ui, "Saved on the server and used by every new turn of the foreman agent. Other agents keep their own settings.");
                ui.add_space(14.0);
                section(
                    ui,
                    "model levels",
                    "what switch_model and /model low|medium|high move a chat to",
                    |ui| {
                        let own = st.model.url.trim().to_string();
                        let levels = [
                            ("low", &mut st.model.low, "cheap and quick, for mechanical work"),
                            ("medium", &mut st.model.medium, "the usual"),
                            ("high", &mut st.model.high, "slow and strong, for the hard parts"),
                        ];
                        for (name, level, about) in levels {
                            row_help(ui, name, about, |ui| {
                                let list = level.url.trim().is_empty().then_some(main_list).flatten();
                                model_input(ui, &format!("level-{name}"), &mut level.model, "not set", list);
                            });
                            row_help(
                                ui,
                                "  server",
                                "Empty uses the foreman agent's server and key. A server of its own never gets the foreman key.",
                                |ui| {
                                    field(ui, &mut level.url, &own);
                                },
                            );
                            if !level.url.trim().is_empty() {
                                let explain = if level.has_key {
                                    "A key is saved; leave this empty to keep it."
                                } else {
                                    "Only sent to this server. Leave empty if it needs none."
                                };
                                row_help(ui, "  key", explain, |ui| {
                                    secret(ui, &mut level.key);
                                });
                            }
                        }
                    },
                );
                ui.add_space(8.0);
                hint(ui, "The agent can call switch_model to go up a level for hard work and down for easy work. A level with no model is left out. In a chat, /model high does the same by hand, and /model with nothing goes back to the default.");
                if st.title_loaded {
                    ui.add_space(14.0);
                    section(
                        ui,
                        "chat titles",
                        "a small model names each chat once the agent has answered",
                        |ui| {
                            row_help(ui, "titles", "Turn this off to keep the first words of each chat as its title.", |ui| {
                                ui.checkbox(&mut st.title.enabled, "name chats with a model");
                            });
                            row_help(
                                ui,
                                "address",
                                "Empty uses the foreman agent's server above. Any OpenAI compatible endpoint works, so a small one beside the big one is ideal.",
                                |ui| {
                                    let r = field(ui, &mut st.title.url, "same as the foreman agent");
                                    if r.changed() {
                                        st.title_models = None;
                                    }
                                    if r.lost_focus() && st.title_models.is_none() && !st.title_asking {
                                        probe_title = true;
                                    }
                                },
                            );
                            let explain = if st.title.has_key {
                                "Sent as a bearer token. A key is saved; leave this empty to keep it."
                            } else {
                                "Only sent to the address above. Empty uses the foreman agent's key when the address is empty too."
                            };
                            row_help(ui, "key", explain, |ui| {
                                let r = secret(ui, &mut st.title.key);
                                if r.changed() {
                                    st.title_models = None;
                                }
                            });
                            row(ui, "model", |ui| match &st.title_models {
                                Some(Ok(list)) if !list.is_empty() => {
                                    let w = (ui.available_width() - 96.0).max(120.0);
                                    ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
                                        ModelPicker::new("settings-title", list).show(ui, &mut st.title.model);
                                    });
                                    probe_title = ui
                                        .add(Key::new("refresh").outline(VALUE).enabled(!st.title_asking))
                                        .clicked();
                                }
                                _ => {
                                    field_then(ui, &mut st.title.model, "same as the foreman agent", 130.0, |ui| {
                                        probe_title = ui
                                            .add(Key::new(if st.title_asking { "asking" } else { "list models" }).outline(VALUE).enabled(!st.title_asking))
                                            .clicked();
                                    });
                                }
                            });
                            match &st.title_models {
                                Some(Err(e)) => status(ui, false, e),
                                Some(Ok(l)) => status(ui, true, &format!("{} models on the server", l.len())),
                                None => {}
                            }
                        },
                    );
                    ui.add_space(8.0);
                    hint(ui, "/title in a chat asks for a new title, and /title with words sets it yourself.");
                }
                }
                });
                footer(ui, |ui| {
                    let server_ok = st.place == Where::Local
                        || (!st.url.trim().is_empty() && !st.token.trim().is_empty());
                    let agent_ok = !st.loaded
                        || (!st.model.url.trim().is_empty() && !st.model.model.trim().is_empty());
                    save = ui
                        .add(Key::new("save").solid(READOUT).enabled(server_ok && agent_ok).min_width(100.0))
                        .clicked();
                    if can_close {
                        close = ui.add(Key::new("cancel").outline(VALUE).min_width(100.0)).clicked();
                    }
                });
            });
        if m.should_close() && can_close {
            close = true;
        }
        save |= std::mem::take(&mut self.save_requested);
        if probe {
            st.asking = true;
            if let Some(c) = &mut self.client {
                c.models(&st.model);
            }
        }
        if probe_title {
            st.title_asking = true;
            if let Some(c) = &mut self.client {
                c.title_models(&st.title);
            }
        }
        if save {
            self.speech = st.speech.clone();
            if st.face != self.face {
                self.face = st.face.clone();
                self.load_fonts(ctx);
            }
            if st.loaded
                && let Some(c) = &mut self.client
            {
                if st.title_loaded {
                    c.set_title(&st.title);
                }
                c.set_model(&st.model);
            }
            if self.server_changed(&st) {
                let server =
                    Server { url: st.url.trim().to_string(), token: st.token.trim().to_string() };
                match st.place {
                    Where::Local => {
                        self.remote = false;
                        self.start_local(ctx);
                    }
                    Where::Remote => {
                        self.remote = true;
                        self.connect_to(ctx, server);
                    }
                }
                return;
            }
            if st.loaded {
                self.settings = Some(st);
            }
            return;
        }
        if !close {
            self.settings = Some(st);
        }
    }

    fn default_agent(&self) -> String {
        let Some(o) = &self.overview else {
            return String::new();
        };
        let last = o.sessions.iter().max_by_key(|s| s.updated_at).map(|s| s.agent.clone());
        last.filter(|a| o.agents.iter().any(|x| &x.name == a && x.available))
            .or_else(|| o.agents.iter().find(|a| a.available).map(|a| a.name.clone()))
            .unwrap_or_default()
    }

    fn new_chat(&mut self, worktree: Option<&Worktree>) {
        let Some(project) = worktree.map(|w| w.project_id).or(self.project) else {
            return;
        };
        self.project = Some(project);
        // The form takes the focused tile; the chat that was there is only closed to view.
        let at = self.focus_slot();
        self.slots[at] = None;
        self.session = None;
        self.tab = Tab::Chat;
        self.new_pane.draft.clear();
        let agent = self.default_agent();
        let place = match worktree {
            Some(w) if !w.main => PlaceChoice::Existing(w.id),
            _ => PlaceChoice::Main,
        };
        self.fresh = Some(Fresh::new(project, place, agent));
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
                && s.status.busy()
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
            let mut picked_head = false;
            let mut pick = false;
            let mut menu = None;
            card(
                ui,
                rail,
                |ui| {
                    let r = Line::new().legend(&p.name).show(ui).interact(Sense::click());
                    picked_head = r.clicked();
                    menu = Some(r);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        Line::new().note(format!("{open} open")).size(10.5).show(ui);
                    });
                },
                |ui| {
                    let r = Line::new().note(short_path(&p.path)).size(10.5).elided(ui);
                    if r.interact(Sense::click()).clicked() {
                        pick = true;
                    }
                    if collapsed {
                        return;
                    }
                    ui.add_space(2.0);
                    let last = trees.len().saturating_sub(1);
                    for (i, w) in trees.iter().enumerate() {
                        self.worktree_row(ui, w, i == last);
                    }
                },
            );
            if pick || picked_head {
                self.project = Some(p.id);
            }
            let Some(r) = menu else {
                continue;
            };
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
                self.add_project =
                    Some(AddProject { path: String::new(), name: String::new(), create: false });
            });
    }

    fn worktree_row(&mut self, ui: &mut Ui, w: &Worktree, last: bool) {
        let sessions: Vec<Session> = self.sessions_of(w.id).into_iter().cloned().collect();
        let running = sessions.iter().any(|s| s.status.busy());
        let tint = if w.missing {
            FAULT
        } else if running {
            TRACE
        } else if w.main || !sessions.is_empty() {
            VALUE
        } else {
            LEGEND
        };
        let top = ui.cursor().top();
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(TREE_INDENT);
                let r = Line::new().value(w.label()).tint(tint).size(11.5).show(ui);
                if w.main {
                    chip(ui, "main", LEGEND, false);
                }
                if w.missing {
                    chip(ui, "gone", FAULT, true);
                }
                r
            })
            .response
            .interact(Sense::click());
        let row = resp.rect;
        for (i, s) in sessions.iter().enumerate() {
            self.session_row(ui, s, i + 1 == sessions.len());
        }
        let bottom = ui.min_rect().bottom();
        let x = row.left() + TREE_X;
        let p = ui.painter();
        let stroke = Stroke::new(1.0, ETCH);
        let elbow = row.center().y;
        p.vline(x, top..=if last { elbow } else { bottom + ui.spacing().item_spacing.y }, stroke);
        p.hline(x..=row.left() + TREE_INDENT - 4.0, elbow, stroke);
        if resp.clicked() && !w.missing {
            self.new_chat(Some(w));
        }
        resp.on_hover_text(&w.path).context_menu(|ui| {
            if ui
                .add(
                    Key::new("new chat here").outline(READOUT).min_width(170.0).enabled(!w.missing),
                )
                .clicked()
            {
                self.new_chat(Some(w));
                ui.close();
            }
            if ui.add(Key::new("start in a new worktree").outline(VALUE).min_width(170.0)).clicked()
            {
                self.open_start(None);
                if let Some(s) = &mut self.start {
                    s.project = w.project_id;
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
    }

    fn session_row(&mut self, ui: &mut Ui, s: &Session, last: bool) {
        let on = self.session == Some(s.id);
        let (tint, word) = status_look(s.status);
        let top = ui.cursor().top();
        let mut close = false;
        let resp = ui
            .horizontal(|ui| {
                ui.add_space(TREE_INDENT * 2.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 16.0), Sense::hover());
                if s.status == SessionStatus::Running {
                    spinner(ui, rect.center(), 4.5, tint);
                } else {
                    ui.painter().circle_filled(rect.center(), 3.0, tint);
                }
                let hovered = ui.rect_contains_pointer(ui.max_rect());
                let reserve = if hovered { 18.0 } else { 0.0 };
                ui.allocate_ui(Vec2::new(ui.available_width() - reserve, 16.0), |ui| {
                    Line::new()
                        .value(&s.title)
                        .tint(if on { READOUT } else { VALUE })
                        .size(11.5)
                        .gap(6.0)
                        .note(format!("{} {}", s.agent, word))
                        .size(10.5)
                        .elided(ui)
                });
                if hovered {
                    let x = ui
                        .add(
                            egui::Label::new(egui::RichText::new("×").size(14.0).color(LEGEND))
                                .sense(Sense::click()),
                        )
                        .on_hover_text("close and delete this chat");
                    if x.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    close = x.clicked();
                }
            })
            .response
            .interact(Sense::click());
        if close {
            self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
        }
        let row = resp.rect;
        let x = row.left() + TREE_INDENT + TREE_X;
        let stroke = Stroke::new(1.0, ETCH);
        let elbow = row.center().y;
        let reach = if last { elbow } else { row.bottom() + ui.spacing().item_spacing.y };
        ui.painter().vline(x, top - ui.spacing().item_spacing.y..=reach, stroke);
        ui.painter().hline(x..=row.left() + TREE_INDENT * 2.0 - 2.0, elbow, stroke);
        if self.grid_now().slots() > 1 && self.tab == Tab::Chat && self.shown().contains(&s.id) {
            let tint = if on { READOUT } else { LEGEND };
            ui.painter().rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(row.right() - 2.0, row.top() + 1.0),
                    egui::pos2(row.right(), row.bottom() - 1.0),
                ),
                0.0,
                tint,
            );
        }
        if resp.clicked() && !close {
            self.open_session(s.id, s.project_id);
        }
        resp.context_menu(|ui| {
            if s.status.busy() && ui.add(Key::new("stop").outline(FAULT).min_width(150.0)).clicked()
            {
                if let Some(c) = &mut self.client {
                    c.interrupt(s.id);
                }
                ui.close();
            }
            if ui.add(Key::new("close chat").outline(FAULT).min_width(150.0)).clicked() {
                self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
                ui.close();
            }
        });
    }

    pub fn open_session(&mut self, sid: SessionId, project: Id) {
        self.session = Some(sid);
        self.project = Some(project);
        self.tab = Tab::Chat;
        self.fresh = None;
        self.panes.entry(sid).or_default().follow = true;
        // The chat replaces the one in the focused tile. One already on screen is only focused.
        let shown = self.grid_now().slots();
        let focus = self.focus_slot();
        self.focus_at = place(&mut self.slots, shown, focus, sid);
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
        let working = mine.iter().any(|s| s.status.busy());
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
                        let tint = if s.status.busy() { TRACE } else { LEGEND };
                        if chip(ui, &s.agent, tint, s.status.busy())
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

    fn chat(&mut self, ui: &mut Ui) {
        if self.grid_now().slots() == 1 {
            self.focused = true;
            self.compact = false;
            let tile = self.tiles()[0];
            self.in_pane(tile, |app| app.tile(ui, tile));
            return;
        }
        let (cols, rows) = self.grid_now().dims();
        let area = ui.available_rect_before_wrap();
        let w = (area.width() - TILE_GAP * (cols - 1) as f32) / cols as f32;
        let h = (area.height() - TILE_GAP * (rows - 1) as f32) / rows as f32;
        let rects: Vec<egui::Rect> = (0..cols * rows)
            .map(|i| {
                let (col, row) = ((i % cols) as f32, (i / cols) as f32);
                egui::Rect::from_min_size(
                    area.min + Vec2::new(col * (w + TILE_GAP), row * (h + TILE_GAP)),
                    Vec2::new(w, h),
                )
            })
            .collect();
        // Pressing anywhere in a tile focuses it.
        if ui.input(|i| i.pointer.any_pressed())
            && let Some(at) = rects.iter().position(|r| ui.rect_contains_pointer(*r))
            && at != self.focus_slot()
        {
            self.focus_tile(at);
        }
        let tiles = self.tiles();
        for (i, (rect, tile)) in rects.into_iter().zip(tiles).enumerate() {
            let focused = i == self.focus_slot();
            let edge = if focused { READOUT.gamma_multiply(0.75) } else { ETCH };
            ui.painter().rect_stroke(rect, 2.0, Stroke::new(1.0, edge), egui::StrokeKind::Inside);
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(("tile", i, tile))
                    .max_rect(rect.shrink(8.0))
                    .layout(Layout::top_down(Align::Min)),
            );
            child.set_clip_rect(rect.shrink(1.0).intersect(ui.clip_rect()));
            self.focused = focused;
            self.compact = true;
            self.in_pane(tile, |app| app.tile(&mut child, tile));
            self.compact = false;
            self.focused = true;
        }
        ui.allocate_rect(area, Sense::hover());
    }

    /// Runs `draw` with the draft and scroll state of the chat in this tile, so each tile
    /// keeps what was typed into it.
    fn in_pane<R>(&mut self, tile: Tile, draw: impl FnOnce(&mut Self) -> R) -> R {
        let pane = match tile {
            Tile::Chat(s) => self.panes.remove(&s).unwrap_or_default(),
            Tile::New => std::mem::take(&mut self.new_pane),
            Tile::Empty => Pane::default(),
        };
        let draft = std::mem::replace(&mut self.draft, pane.draft);
        let follow = std::mem::replace(&mut self.follow, pane.follow);
        let before = self.drawing.replace(tile);
        let out = draw(self);
        self.drawing = before;
        let pane = Pane {
            draft: std::mem::replace(&mut self.draft, draft),
            follow: std::mem::replace(&mut self.follow, follow),
        };
        match tile {
            Tile::Chat(s) => {
                self.panes.insert(s, pane);
            }
            Tile::New => self.new_pane = pane,
            Tile::Empty => {}
        }
        out
    }

    fn tile(&mut self, ui: &mut Ui, tile: Tile) {
        match tile {
            Tile::New => self.blank_chat(ui),
            Tile::Empty => {
                ui.add_space(12.0);
                hint(ui, "click a chat in the list to fill the focused tile");
            }
            Tile::Chat(sid) => match self.session_by_id(sid).cloned() {
                Some(s) => self.session_view(ui, s),
                None if self.compact => hint(ui, "loading"),
                None => self.blank_chat(ui),
            },
        }
    }

    fn blank_chat(&mut self, ui: &mut Ui) {
        let Some(o) = self.overview.clone() else {
            return;
        };
        let kept = self.fresh.as_ref().map(|f| f.project).filter(|_| self.compact);
        let Some(project) = kept.or(self.project).or(o.projects.first().map(|p| p.id)) else {
            section(ui, "no projects yet", "", |ui| {
                note(
                    ui,
                    "Add a git repository on the left to start chatting with an agent in it.",
                    LEGEND,
                );
            });
            return;
        };
        let fresh =
            self.fresh.get_or_insert_with(|| Fresh::new(project, PlaceChoice::Main, String::new()));
        if fresh.project != project {
            *fresh = Fresh::new(project, PlaceChoice::Main, String::new());
        }
        if fresh.agent.is_empty() {
            fresh.agent = {
                let last = o.sessions.iter().max_by_key(|s| s.updated_at).map(|s| s.agent.clone());
                last.filter(|a| o.agents.iter().any(|x| &x.name == a && x.available))
                    .or_else(|| o.agents.iter().find(|a| a.available).map(|a| a.name.clone()))
                    .unwrap_or_default()
            };
        }
        let trees: Vec<Worktree> =
            o.worktrees.iter().filter(|w| w.project_id == project && !w.missing).cloned().collect();
        let name =
            o.projects.iter().find(|p| p.id == project).map(|p| p.name.clone()).unwrap_or_default();
        if matches!(fresh.place, PlaceChoice::Existing(id) if !trees.iter().any(|w| w.id == id)) {
            fresh.place = PlaceChoice::Main;
        }
        let here = match fresh.place {
            PlaceChoice::New => None,
            PlaceChoice::Existing(id) => trees.iter().find(|w| w.id == id),
            PlaceChoice::Main => trees.iter().find(|w| w.main),
        }
        .map(|w| w.label())
        .unwrap_or_else(|| "a new worktree".to_string());
        ui.add_space(ui.available_height() * if self.compact { 0.06 } else { 0.18 });
        ui.vertical_centered(|ui| {
            ui.set_max_width(640.0);
            ui.label(
                egui::RichText::new(format!("{name} on {here}").to_uppercase())
                    .font(legend_font(15.0))
                    .color(VALUE)
                    .extra_letter_spacing(2.0),
            );
            ui.add_space(4.0);
            hint(
                ui,
                "Ask anything. The agent works in this worktree and can read and file issues.",
            );
            ui.add_space(14.0);
            let fresh = self.fresh.as_mut().expect("fresh chat");
            let mut row = |ui: &mut Ui| {
                ui.allocate_ui(Vec2::new(180.0, KEY_H), |ui| {
                    if choice(
                        ui,
                        "fresh-agent",
                        &mut fresh.agent,
                        o.agents.iter().map(|a| {
                            let label = if a.available {
                                a.label.clone()
                            } else {
                                format!("{} (not ready)", a.label)
                            };
                            (a.name.clone(), label)
                        }),
                    ) {
                        fresh.model.clear();
                    }
                });
                let mut opts: Vec<(PlaceChoice, String)> = Vec::new();
                for w in &trees {
                    let label = if w.main { format!("{} (main)", w.label()) } else { w.label() };
                    opts.push((
                        if w.main { PlaceChoice::Main } else { PlaceChoice::Existing(w.id) },
                        label,
                    ));
                }
                opts.push((PlaceChoice::New, "+ new worktree".into()));
                ui.allocate_ui(Vec2::new(200.0, KEY_H), |ui| {
                    choice(ui, "fresh-tree", &mut fresh.place, opts);
                });
                if fresh.place == PlaceChoice::New {
                    ui.allocate_ui(Vec2::new(160.0, KEY_H), |ui| {
                        field(ui, &mut fresh.branch, "branch, e.g. fix-login");
                    });
                }
                let default = o
                    .agents
                    .iter()
                    .find(|a| a.name == fresh.agent)
                    .map(|a| a.about.clone())
                    .unwrap_or_default();
                let hint_text = match fresh.agent.as_str() {
                    "foreman" => format!("model, {}", short_about(&default)),
                    _ => "model, the agent's default if empty".to_string(),
                };
                let list = self.agent_models.get(&fresh.agent);
                model_input(ui, "fresh-model", &mut fresh.model, &hint_text, list);
            };
            if self.compact {
                ui.horizontal_wrapped(&mut row);
            } else {
                ui.horizontal(&mut row);
            }
        });
        ui.add_space(10.0);
        let agent_ready = o
            .agents
            .iter()
            .any(|a| self.fresh.as_ref().is_some_and(|f| f.agent == a.name) && a.available);
        let needs_branch = self
            .fresh
            .as_ref()
            .is_some_and(|f| f.place == PlaceChoice::New && f.branch.trim().is_empty());
        let hint_text = if !agent_ready {
            "that agent is not ready on the server"
        } else if needs_branch {
            "name the new worktree's branch first"
        } else {
            "what should it do"
        };
        let sent = self.composer(ui, agent_ready && !needs_branch, false, hint_text);
        if let (Some((text, attachments)), Some(f)) = (sent, self.fresh.clone()) {
            let place = match f.place {
                PlaceChoice::Main => Place::Main,
                PlaceChoice::Existing(id) => Place::Existing { worktree_id: id },
                PlaceChoice::New => Place::New { branch: f.branch.trim().to_string() },
            };
            let new = NewSession {
                project_id: f.project,
                agent: f.agent.clone(),
                place,
                title: None,
                model: Some(f.model.trim().to_string()).filter(|m| !m.is_empty()),
                issue_id: None,
                prompt: Some(text).filter(|t| !t.is_empty()),
                attachments,
            };
            if let Some(c) = &mut self.client {
                c.add_session(&new);
            }
            self.awaiting = Some(f.project);
        }
    }

    fn slash_menu(&mut self, ui: &mut Ui, field: egui::Rect) {
        let typed = self.draft.trim_start();
        if !typed.starts_with('/') || typed.contains(char::is_whitespace) {
            return;
        }
        let typed = typed.trim_start_matches('/').to_string();
        let matches: Vec<&foreman_core::Slash> =
            foreman_core::SLASH.iter().filter(|c| c.name.starts_with(&typed)).collect();
        if matches.is_empty() {
            return;
        }
        let height = matches.len() as f32 * 24.0 + 10.0;
        let at = egui::pos2(field.left(), field.top() - height - 4.0);
        let mut picked = None;
        egui::Area::new(ui.id().with("slash-menu"))
            .order(egui::Order::Foreground)
            .fixed_pos(at)
            .show(ui.ctx(), |ui| {
                egui::Frame::NONE
                    .fill(PANEL)
                    .stroke(Stroke::new(1.0, ETCH))
                    .corner_radius(2)
                    .inner_margin(Margin::symmetric(8, 5))
                    .show(ui, |ui| {
                        ui.set_width(field.width() - 16.0);
                        for c in matches {
                            let r = ui
                                .horizontal(|ui| {
                                    ui.set_min_height(20.0);
                                    Line::new()
                                        .set(format!("/{}", c.name))
                                        .size(12.0)
                                        .gap(6.0)
                                        .value(c.args)
                                        .size(11.0)
                                        .tint(LEGEND)
                                        .gap(12.0)
                                        .note(c.about)
                                        .size(11.0)
                                        .elided(ui);
                                })
                                .response
                                .interact(Sense::click());
                            if r.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            if r.clicked() {
                                picked = Some(c);
                            }
                        }
                    });
            });
        if let Some(c) = picked {
            self.draft = format!("/{}{}", c.name, if c.args.is_empty() { "" } else { " " });
        }
    }

    /// Files dropped on the window go to the composer when it is on screen.
    fn take_dropped(&mut self, ctx: &egui::Context) {
        if self.tab != Tab::Chat || self.settings.is_some() {
            return;
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        for f in dropped {
            let name = f
                .path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| f.name.clone());
            let tx = self.tray.sender();
            match (f.bytes, f.path) {
                (Some(bytes), _) => {
                    crate::attach::arrive(tx, ctx.clone(), name, bytes.to_vec());
                }
                #[cfg(not(target_arch = "wasm32"))]
                (None, Some(path)) => {
                    let ctx = ctx.clone();
                    crate::spawn_blocking(move || match std::fs::read(&path) {
                        Ok(data) => crate::attach::arrive(tx, ctx, name, data),
                        Err(e) => {
                            let _ = tx.send(crate::attach::Arrived::Failed(format!("{name}: {e}")));
                        }
                    });
                }
                _ => {}
            }
        }
    }

    fn attachments_row(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut drop_at = None;
        ui.horizontal_wrapped(|ui| {
            for (i, f) in self.tray.files.iter().enumerate() {
                let chip = ui.horizontal(|ui| {
                    if let Some(tex) = &f.preview {
                        let s = tex.size_vec2();
                        let h = 28.0;
                        ui.add(egui::Image::new((tex.id(), Vec2::new(s.x * h / s.y.max(1.0), h))));
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "{} {}",
                            f.name,
                            crate::attach::size(f.size as u64)
                        ))
                        .font(mono(11.0))
                        .color(VALUE),
                    );
                    let x = ui.small_button("x");
                    x.widget_info(|| {
                        egui::WidgetInfo::labeled(
                            egui::WidgetType::Button,
                            true,
                            format!("remove {}", f.name),
                        )
                    });
                    x.clicked()
                });
                if chip.inner {
                    drop_at = Some(i);
                }
                ui.add_space(8.0);
            }
        });
        if let Some(i) = drop_at {
            self.tray.files.remove(i);
        }
        let _ = ctx;
    }

    /// Ctrl+V with a picture on the clipboard: egui only passes text pastes on, so the key is
    /// watched here and the clipboard read for an image off the UI thread.
    pub fn watch_paste(&mut self, ctx: &egui::Context, raw: &egui::RawInput) {
        if self.tab != Tab::Chat || self.settings.is_some() {
            return;
        }
        let pasted = raw.events.iter().any(|e| match e {
            egui::Event::Key { key: egui::Key::V, pressed: false, modifiers, .. } => {
                modifiers.command
            }
            egui::Event::Key { key: egui::Key::Paste, pressed: false, .. } => true,
            _ => false,
        });
        if pasted {
            crate::attach::paste(self.tray.sender(), ctx.clone());
        }
    }

    fn composer(
        &mut self,
        ui: &mut Ui,
        ready: bool,
        busy: bool,
        hint_text: &str,
    ) -> Option<(String, Vec<foreman_core::Upload>)> {
        // Files go to the focused tile only: the tray is shared, and it is the tile you are using.
        let files = self.focused && !self.tray.files.is_empty();
        if files {
            self.attachments_row(ui);
            ui.add_space(4.0);
        }
        let compact = self.compact;
        let edit = prose(ui, &mut self.draft, hint_text, if compact { 2 } else { 3 });
        if self.refocus.is_some() && self.refocus == self.drawing {
            self.refocus = None;
            edit.request_focus();
        }
        self.slash_menu(ui, edit.rect);
        let enter = edit.has_focus()
            && ui.input(|i| {
                i.key_pressed(egui::Key::Enter) && (i.modifiers.command || !i.modifiers.shift)
            });
        if enter && self.draft.ends_with('\n') && !ui.input(|i| i.modifiers.command) {
            let trimmed = self.draft.trim_end_matches('\n').to_string();
            self.draft = trimmed;
        }
        let mut out = None;
        ui.horizontal(|ui| {
            let tip = match (compact, busy) {
                (true, true) => "enter steers, / for commands",
                (true, false) => "enter sends, / for commands",
                (false, true) => {
                    "enter steers the agent, shift+enter for a new line, / for commands, drop or paste files"
                }
                (false, false) => {
                    "enter sends, shift+enter for a new line, / for commands, drop or paste files"
                }
            };
            hint(ui, tip);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let sending = self.focused && !self.tray.files.is_empty();
                let can = ready && (!self.draft.trim().is_empty() || sending);
                let label = if busy { "steer" } else { "send" };
                let width = if compact { 64.0 } else { 100.0 };
                if (ui.add(Key::new(label).solid(READOUT).enabled(can).min_width(width)).clicked()
                    || (enter && can))
                    && can
                {
                    let files = if self.focused { self.tray.take() } else { Vec::new() };
                    out = Some((self.draft.trim().to_string(), files));
                    self.draft.clear();
                    self.follow = true;
                }
                if !compact || self.focused {
                    self.speak_key(ui);
                }
                if crate::attach::can_pick() && !compact {
                    let key = ui.add(Key::new("attach").outline(VALUE));
                    key.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "attach")
                    });
                    if key.clicked() {
                        crate::attach::pick(self.tray.sender(), ui.ctx().clone());
                    }
                }
            });
        });
        out
    }

    /// The key that speaks into the composer: press to start the microphone, press again to stop
    /// and have what was said written into the draft.
    fn speak_key(&mut self, ui: &mut Ui) {
        if cfg!(any(target_arch = "wasm32", target_os = "android")) {
            return;
        }
        let ctx = ui.ctx().clone();
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        let (listening, reading) = (self.voice.listening(), self.voice.reading());
        let label = if listening {
            format!("stop {:.0}s", self.voice.seconds())
        } else if reading {
            let w = self.voice.status.clone();
            if w.is_empty() { "reading".to_string() } else { w }
        } else {
            "speak".to_string()
        };
        let key = if listening {
            Key::new(&label).solid(FAULT)
        } else {
            Key::new(&label).outline(if reading { LEGEND } else { VALUE }).enabled(!reading)
        };
        let key = ui.add(key);
        key.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "speak"));
        if listening {
            // A meter beside the key, so a muted microphone shows before the words do not.
            let level = self.voice.level().sqrt().clamp(0.0, 1.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(36.0, 6.0), Sense::hover());
            ui.painter().rect_filled(rect, 2.0, WELL);
            let mut on = rect;
            on.set_width(rect.width() * level);
            ui.painter().rect_filled(on, 2.0, OK);
            key.clone().on_hover_text(format!("listening on {}", self.voice.device()));
        } else if !reading {
            key.clone()
                .on_hover_text("speak instead of typing; the words are added to the message");
        }
        if key.clicked() {
            if listening {
                self.voice.finish(&self.speech, wake);
            } else if let Some(p) = self.speech.problem() {
                self.notice = Some((false, p.into()));
            } else if let Err(e) = self.voice.start(wake) {
                self.notice = Some((false, e));
            } else {
                self.speaking_in = self.drawing;
            }
        }
        // Escape on a listening composer throws the take away instead of reading it.
        if listening && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.voice.cancel();
        }
    }

    /// The header of a chat in a small tile: its state, its name, and the keys that matter.
    fn tile_header(&mut self, ui: &mut Ui, s: &Session, tree: Option<&Worktree>) {
        ui.horizontal(|ui| {
            let (_, word) = status_look(s.status);
            lamp(ui, word, s.status != SessionStatus::Stopped, s.status == SessionStatus::Failed);
            ui.add_space(4.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(Key::new("close").outline(FAULT))
                    .on_hover_text("stop it and delete this chat")
                    .clicked()
                {
                    self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
                }
                if s.status.busy()
                    && ui.add(Key::new("stop").outline(FAULT)).clicked()
                    && let Some(c) = &mut self.client
                {
                    c.interrupt(s.id);
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let place = tree.map(|w| format!(" {}", w.label())).unwrap_or_default();
                    Line::new()
                        .value(&s.title)
                        .size(11.5)
                        .gap(6.0)
                        .note(format!("{}{place}", s.agent))
                        .size(10.5)
                        .elided(ui)
                        .on_hover_text(format!(
                            "{}\n{} {}\n{} in, {} out",
                            s.title,
                            s.agent,
                            s.model.as_deref().unwrap_or_default(),
                            tokens(s.usage.input_tokens + s.usage.cache_read_tokens),
                            tokens(s.usage.output_tokens)
                        ));
                });
            });
        });
    }

    fn session_view(&mut self, ui: &mut Ui, s: Session) {
        let tree = self.worktree(s.worktree_id).cloned();
        if self.compact {
            self.tile_header(ui, &s, tree.as_ref());
        } else {
            self.full_header(ui, &s, tree.as_ref());
        }
        self.session_body(ui, s, tree);
    }

    fn full_header(&mut self, ui: &mut Ui, s: &Session, tree: Option<&Worktree>) {
        ui.horizontal(|ui| {
            let (tint, word) = status_look(s.status);
            let _ = tint;
            lamp(ui, word, s.status != SessionStatus::Stopped, s.status == SessionStatus::Failed);
            ui.add_space(6.0);
            let mut line = Line::new().legend("agent").gap(6.0).value(&s.agent).size(11.0);
            if let Some(m) = &s.model {
                line = line.gap(14.0).legend("model").gap(6.0).set(m).size(11.0);
            }
            if let Some(w) = tree {
                line = line.gap(14.0).legend("worktree").gap(6.0).value(w.label()).size(11.0);
            }
            if let Some(i) = s.issue_id {
                line = line.gap(14.0).legend("issue").gap(6.0).value(format!("#{i}")).size(11.0);
            }
            line.show(ui);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(Key::new("close").outline(FAULT))
                    .on_hover_text("stop it and delete this chat")
                    .clicked()
                {
                    self.confirm = Some(Confirm::Session(s.id, s.title.clone()));
                }
                if ui
                    .add(Key::new("model").outline(VALUE))
                    .on_hover_text("switch this chat to another model: low, medium, high or an id")
                    .clicked()
                {
                    self.draft = "/model ".into();
                }
                if toggle(ui, "thinking", self.thinking).clicked() {
                    self.thinking = !self.thinking;
                }
                if s.status.busy()
                    && ui.add(Key::new("stop").outline(FAULT)).clicked()
                    && let Some(c) = &mut self.client
                {
                    c.interrupt(s.id);
                }
                let mut usage = Line::new()
                    .measured(format!(
                        "{} in",
                        tokens(s.usage.input_tokens + s.usage.cache_read_tokens)
                    ))
                    .size(11.0)
                    .gap(8.0)
                    .measured(format!("{} out", tokens(s.usage.output_tokens)))
                    .size(11.0);
                if s.usage.cost_usd > 0.0 {
                    usage = usage.gap(8.0).measured(format!("${:.2}", s.usage.cost_usd)).size(11.0);
                }
                usage.show(ui);
            });
        });
    }

    fn session_body(&mut self, ui: &mut Ui, s: Session, tree: Option<Worktree>) {
        ui.add_space(6.0);

        // The composer, and the row of attachments above it when there are any.
        let files = self.focused && !self.tray.files.is_empty();
        let input_h = if self.compact { 84.0 } else { 100.0 } + if files { 40.0 } else { 0.0 };
        let avail = (ui.available_height() - input_h).max(120.0);
        let t = self.transcripts.entry(s.id).or_default();
        let messages = t.messages.clone();
        // The transcript gets a rect of its own and is clipped to it: something that cannot
        // wrap (a long link, a table) widens the ui it is drawn in, and in a tile that would
        // push the composer and the tile's edge along with it.
        let rect = egui::Rect::from_min_size(
            ui.cursor().min,
            Vec2::new(ui.available_width(), avail.min(ui.available_height())),
        );
        let mut inner = ui
            .new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)));
        inner.set_clip_rect(rect.intersect(ui.clip_rect()));
        let scroll = egui::ScrollArea::vertical()
            .max_height(rect.height())
            .auto_shrink([false, false])
            .stick_to_bottom(self.follow)
            .show(&mut inner, |ui| {
                ui.set_max_width(ui.available_width());
                if messages.is_empty() {
                    hint(ui, "no messages yet");
                }
                for m in &messages {
                    // Each entry gets a ui of the column's width, clipped to it, so one with
                    // something that cannot wrap does not widen the entries after it.
                    let column = egui::Rect::from_min_size(
                        ui.cursor().min,
                        Vec2::new(ui.available_width(), f32::INFINITY),
                    );
                    let mut one = ui.new_child(
                        egui::UiBuilder::new()
                            .max_rect(column)
                            .layout(Layout::top_down(Align::Min)),
                    );
                    one.set_clip_rect(
                        egui::Rect::from_min_max(
                            column.min,
                            egui::pos2(column.right(), f32::INFINITY),
                        )
                        .intersect(ui.clip_rect()),
                    );
                    self.entry(&mut one, m, s.id);
                    let used = one.min_rect();
                    ui.allocate_rect(
                        egui::Rect::from_min_max(
                            column.min,
                            egui::pos2(column.right(), used.bottom().max(column.top())),
                        ),
                        Sense::hover(),
                    );
                    ui.add_space(6.0);
                }
                if s.status == SessionStatus::Running {
                    hint(ui, "working");
                }
            });
        ui.allocate_rect(rect, Sense::hover());
        let at_bottom =
            scroll.state.offset.y + scroll.inner_rect.height() >= scroll.content_size.y - 4.0;
        self.follow = at_bottom;

        ui.add_space(6.0);
        let missing = tree.as_ref().is_some_and(|w| w.missing);
        let hint_text = if missing {
            "this worktree is gone, the session can only be read"
        } else if s.status == SessionStatus::Waiting {
            "answer the question above, or say something else"
        } else if s.status.busy() {
            "steer the agent while it works"
        } else {
            "tell the agent what to do next"
        };
        if let Some((text, files)) = self.composer(ui, !missing, s.status.busy(), hint_text) {
            let slash = files.is_empty().then(|| foreman_core::parse_slash(&text)).flatten();
            match slash.map(|(c, _)| c.name) {
                Some("new") => {
                    let tree = self.worktree(s.worktree_id).cloned();
                    self.new_chat(tree.as_ref());
                    if let Some(f) = &mut self.fresh {
                        f.agent = s.agent.clone();
                    }
                }
                Some("close") => self.confirm = Some(Confirm::Session(s.id, s.title.clone())),
                _ => {
                    if let Some(c) = &mut self.client {
                        c.send_with(s.id, &text, files);
                    }
                }
            }
        }
    }

    /// A card of one or more questions. A lone single-choice question is answered by
    /// clicking its option; anything else is filled in and sent with one button.
    fn questions(
        &mut self,
        ui: &mut Ui,
        session: SessionId,
        id: &str,
        qs: &[Question],
        done: Option<&[String]>,
    ) {
        let rail = if done.is_some() { ETCH } else { READOUT };
        let quick = qs.len() == 1 && !qs[0].multiple && !qs[0].options.is_empty();
        let legend = if qs.len() == 1 { "question" } else { "questions" };
        card(
            ui,
            Some(rail),
            |ui| {
                Line::new().legend(legend).show(ui);
            },
            |ui| {
                let mut slots = self.drafts.remove(id).unwrap_or_default();
                slots.resize(qs.len(), Slot::default());
                let mut sent: Option<Vec<String>> = None;
                let mut enter = false;
                for (n, q) in qs.iter().enumerate() {
                    if n > 0 {
                        ui.add_space(10.0);
                    }
                    if qs.len() > 1 {
                        Line::new()
                            .legend(&format!("{} of {}", n + 1, qs.len()))
                            .size(10.0)
                            .show(ui);
                    }
                    let salt = id.len() * 7919 + session.0.as_u128() as usize + n * 104_729;
                    markdown::show(ui, &mut self.markdown, &q.text, salt);
                    if let Some(a) = done {
                        let said = a.get(n).map(String::as_str).unwrap_or_default();
                        Line::new().legend("you said").gap(8.0).set(said).size(12.0).wrapped(ui);
                        continue;
                    }
                    let slot = &mut slots[n];
                    for (i, o) in q.options.iter().enumerate() {
                        let on = slot.picked.contains(&i);
                        if !option_row(ui, o, q.multiple, on, (id, n, i)).clicked() {
                            continue;
                        }
                        if quick {
                            sent = Some(vec![o.clone()]);
                        } else if q.multiple {
                            if on {
                                slot.picked.retain(|p| *p != i);
                            } else {
                                slot.picked.push(i);
                                slot.picked.sort_unstable();
                            }
                        } else {
                            slot.picked = if on { vec![] } else { vec![i] };
                            slot.typed.clear();
                        }
                    }
                    let hint =
                        if q.options.is_empty() { "type an answer" } else { "or type an answer" };
                    let r = if quick {
                        let ready = !slot.typed.trim().is_empty();
                        let mut go = false;
                        let r = field_then(ui, &mut slot.typed, hint, 90.0, |ui| {
                            go = ui.add(Key::new("answer").solid(READOUT).enabled(ready)).clicked();
                        });
                        if go && ready {
                            sent = Some(vec![slot.typed.trim().to_string()]);
                        }
                        r
                    } else {
                        field(ui, &mut slot.typed, hint)
                    };
                    if r.changed() && !q.multiple && !slot.typed.trim().is_empty() {
                        slot.picked.clear();
                    }
                    enter |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                }
                if done.is_none() && !quick {
                    let replies: Vec<String> =
                        qs.iter().zip(&slots).map(|(q, s)| s.reply(q)).collect();
                    let ready = replies.iter().all(|r| !r.is_empty());
                    ui.add_space(8.0);
                    let label = if qs.len() > 1 { "send answers" } else { "answer" };
                    let key = ui.add(Key::new(label).solid(READOUT).enabled(ready));
                    key.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, ready, label)
                    });
                    let go = key.clicked();
                    if (go || enter) && ready {
                        sent = Some(replies);
                    }
                } else if enter && quick {
                    let t = slots[0].typed.trim();
                    if !t.is_empty() {
                        sent = Some(vec![t.to_string()]);
                    }
                }
                match (sent, &mut self.client) {
                    (Some(a), Some(c)) => c.answer(session, id, &a),
                    _ => {
                        self.drafts.insert(id.to_string(), slots);
                    }
                }
            },
        );
    }

    fn entry(&mut self, ui: &mut Ui, m: &Message, session: SessionId) {
        match &m.entry {
            Entry::User { text, attachments, queued } => {
                ui.add_space(6.0);
                let (rail, fill) =
                    if *queued { (LEGEND, egui::Color32::TRANSPARENT) } else { (READOUT, PANEL) };
                rail_block(ui, rail, fill, |ui| {
                    if *queued {
                        Line::new()
                            .legend("you")
                            .size(10.0)
                            .gap(8.0)
                            .note("queued: the agent has not read this yet")
                            .size(10.0)
                            .show(ui);
                    } else {
                        Line::new()
                            .legend("you")
                            .size(10.0)
                            .gap(8.0)
                            .note(ago(m.created_at))
                            .size(10.0)
                            .show(ui);
                    }
                    ui.add_space(2.0);
                    if !text.is_empty() {
                        markdown::show(ui, &mut self.markdown, text, m.id as usize);
                    }
                    if !attachments.is_empty() {
                        ui.add_space(4.0);
                        self.attached(ui, session, attachments);
                    }
                });
                ui.add_space(4.0);
            }
            Entry::Assistant { text } => {
                egui::Frame::NONE
                    .inner_margin(Margin { left: 4, right: 4, top: 2, bottom: 2 })
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        markdown::show(ui, &mut self.markdown, text, m.id as usize);
                    });
            }
            Entry::Thinking { text } => {
                if !self.thinking {
                    return;
                }
                rail_block(ui, egui::Color32::TRANSPARENT, egui::Color32::TRANSPARENT, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(text.trim()).italics().size(12.0).color(LEGEND),
                        )
                        .wrap(),
                    );
                });
            }
            Entry::Tool { name, input, output, error, .. } => {
                self.tool_row(ui, m.id, name, input, output.as_deref(), *error)
            }
            Entry::Question { id, question, answer } => {
                let done = answer.as_ref().map(|a| [a.clone()]);
                self.questions(
                    ui,
                    session,
                    id,
                    std::slice::from_ref(question),
                    done.as_ref().map(|d| &d[..]),
                )
            }
            Entry::Questions { id, questions, answers } => {
                self.questions(ui, session, id, questions, answers.as_deref())
            }
            Entry::Error { text } => {
                rail_block(ui, FAULT, FAULT.gamma_multiply(0.08), |ui| {
                    Line::new().legend("error").tint(FAULT).size(10.0).show(ui);
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).font(mono(11.5)).color(FAULT))
                            .wrap(),
                    );
                });
            }
            Entry::Notice { text } => {
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    Line::new().note(text.clone()).size(11.0).wrapped(ui);
                });
            }
            Entry::Summary { text } if text.is_empty() => {
                heading(ui, "cleared: the agent starts afresh here");
            }
            Entry::Summary { text } => {
                heading(ui, "compacted: the agent carries on from this summary");
                rail_block(ui, OK, PANEL, |ui| {
                    markdown::show(ui, &mut self.markdown, text, m.id as usize);
                });
                ui.add_space(4.0);
            }
        }
    }

    /// The files a message carried: pictures as thumbnails, anything else as a named chip.
    fn attached(&mut self, ui: &mut Ui, session: SessionId, files: &[foreman_core::Attachment]) {
        let client = self.client.as_ref();
        let ctx = ui.ctx().clone();
        ui.horizontal_wrapped(|ui| {
            for a in files {
                let label = format!("{} {}", a.name, crate::attach::size(a.size));
                if a.is_image()
                    && let Some(c) = client
                {
                    let thumb = self.tray.thumb(session, a, |tx| {
                        let (key, ctx) = ((session, a.id), ctx.clone());
                        c.attachment(session, a.id, move |bytes| {
                            let img = bytes.as_deref().and_then(crate::attach::decode);
                            let _ = tx.send((key, img));
                            ctx.request_repaint();
                        });
                    });
                    match thumb {
                        crate::attach::Thumb::Ready(tex) => {
                            let s = tex.size_vec2();
                            let h = s.y.min(160.0);
                            ui.add(egui::Image::new((
                                tex.id(),
                                Vec2::new(s.x * h / s.y.max(1.0), h),
                            )))
                            .on_hover_text(&label);
                            continue;
                        }
                        crate::attach::Thumb::Loading => {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                            spinner(ui, r.center(), 8.0, LEGEND);
                        }
                        crate::attach::Thumb::Failed => {}
                    }
                }
                chip(ui, &label, LEGEND, false);
            }
        });
    }

    fn tool_row(
        &mut self,
        ui: &mut Ui,
        id: Id,
        name: &str,
        input: &serde_json::Value,
        output: Option<&str>,
        error: bool,
    ) {
        let (tint, state) = match (output, error) {
            (None, _) => (TRACE, "running"),
            (Some(_), true) => (FAULT, "failed"),
            (Some(_), false) => (LEGEND, ""),
        };
        let open_id = egui::Id::new(("tool-open", id));
        let mut open = ui.ctx().data(|d| d.get_temp::<bool>(open_id)).unwrap_or(error);
        let body = tool_body(name, input);
        let rail = if error {
            FAULT
        } else if output.is_none() {
            TRACE
        } else {
            ETCH
        };
        rail_block(ui, rail, egui::Color32::TRANSPARENT, |ui| {
            let head = ui
                .horizontal(|ui| {
                    let mark = if output.is_none() {
                        "◌"
                    } else if open {
                        "▾"
                    } else {
                        "▸"
                    };
                    ui.label(egui::RichText::new(mark).font(mono(11.0)).color(tint));
                    let mut line = Line::new().legend(name).size(10.0).tint(tint).gap(8.0);
                    line = line.value(tool_summary(name, input)).size(11.0).tint(VALUE);
                    if !state.is_empty() {
                        line = line.gap(10.0).legend(state).size(9.5).tint(tint);
                    }
                    line.elided(ui);
                })
                .response
                .interact(Sense::click());
            if head.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if head.clicked() {
                open = !open;
            }
            if !open {
                if let Some(out) = output.filter(|o| !o.trim().is_empty()) {
                    let first = out.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
                    let more = out.lines().count().saturating_sub(1);
                    let tail = if more > 0 { format!("   +{more} lines") } else { String::new() };
                    ui.horizontal(|ui| {
                        ui.add_space(16.0);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("{}{tail}", first.trim()))
                                    .font(mono(10.5))
                                    .color(LEGEND.gamma_multiply(0.8)),
                            )
                            .truncate(),
                        );
                    });
                }
                return;
            }
            if let Some(b) = &body {
                ui.add_space(2.0);
                code_block(ui, b, VALUE, ("tool-in", id));
            }
            if let Some(out) = output {
                ui.add_space(4.0);
                let text = if out.trim().is_empty() { "no output" } else { out };
                code_block(ui, text, if error { FAULT } else { LEGEND }, ("tool-out", id));
            }
        });
        ui.ctx().data_mut(|d| d.insert_temp(open_id, open));
    }

    fn modals(&mut self, ctx: &egui::Context) {
        self.add_project_modal(ctx);
        self.file_issue_modal(ctx);
        self.start_modal(ctx);
        self.worktree_modal(ctx);
        self.confirm_modal(ctx);
        self.settings_modal(ctx);
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
                row_help(ui, "new folder", "Make the folder if it is missing and run git init in it, so a brand new project can start here.", |ui| {
                    ui.checkbox(&mut a.create, "create it");
                });
            });
            footer(ui, |ui| {
                if ui.add(Key::new(if a.create { "create" } else { "add" }).solid(READOUT).enabled(!a.path.trim().is_empty()).min_width(100.0)).clicked() {
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
                create: a.create,
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
        let agent_models = self.agent_models.clone();
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
                    if choice(ui, "agent", &mut s.agent, agents.iter().map(|a| {
                        let label = if a.available { a.label.clone() } else { format!("{} (not installed)", a.label) };
                        (a.name.clone(), label)
                    })) {
                        s.model.clear();
                    }
                });
                row_help(ui, "model", "Passed to the agent as is. Empty uses its default.", |ui| {
                    let list = agent_models.get(&s.agent);
                    model_input(ui, "start-model", &mut s.model, "default", list);
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
                    here && o.sessions.iter().any(|x| x.worktree_id == w.id && x.status.busy())
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
                attachments: Vec::new(),
            };
            if let Some(c) = &mut self.client {
                match s.issue {
                    Some(iid) => c.start_issue(iid, &new),
                    None => {
                        c.add_session(&new);
                        self.awaiting = Some(new.project_id);
                    }
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
                format!("Close {name}?"),
                "The agent is stopped if it is working and the conversation is deleted. The worktree and its changes stay.",
                "close and delete",
            ),
            Confirm::Issue(_, name) => (
                format!("Delete {name}?"),
                "Its comments go with it. Sessions that worked on it are kept.",
                "delete",
            ),
        };
        let mut done = false;
        let mut gone = None;
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
                                Confirm::Session(id, _) => {
                                    client.delete_session(*id);
                                    gone = Some(*id);
                                }
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
        if let Some(id) = gone {
            self.forget(id);
        }
        if done || m.should_close() {
            self.confirm = None;
        }
    }

    /// Escape stops the agent working in the open chat. Dialogs, pickers and menus take the
    /// key first to close themselves, so this only sees an Escape nothing else wanted.
    fn escape_stops_agent(&mut self, ctx: &egui::Context) {
        if egui::Popup::is_any_open(ctx) {
            return;
        }
        let Some(s) = self.current_session().filter(|s| s.status.busy()).map(|s| s.id) else {
            return;
        };
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            && let Some(c) = &mut self.client
        {
            c.interrupt(s);
        }
    }

    pub fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.tick(&ctx);
        let narrow = ctx.content_rect().width() < 720.0;
        self.narrow = narrow;

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
                Tab::Chat => self.chat(ui),
            });
        });
        self.modals(&ctx);
        self.escape_stops_agent(&ctx);
        self.markdown.sweep();
        if let Some((_, at)) = &self.offline {
            ctx.request_repaint_after(
                RETRY.saturating_sub(at.elapsed()) + Duration::from_millis(50),
            );
        }
        if self.tab == Tab::Chat
            && self
                .shown()
                .into_iter()
                .any(|id| self.session_by_id(id).is_some_and(|s| s.status.busy()))
        {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }
}

/// A key for one grid layout, drawn as the tiles it makes.
fn grid_button(ui: &mut Ui, grid: Grid, on: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(26.0, 22.0), Sense::click());
    let resp = resp.on_hover_text(grid.about());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::Button,
            true,
            on,
            format!("layout {}", grid.key()),
        )
    });
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let tint = if on {
        READOUT
    } else if resp.hovered() {
        VALUE
    } else {
        LEGEND
    };
    let p = ui.painter();
    p.rect_filled(rect, 2.0, if on { tint.gamma_multiply(0.18) } else { WELL });
    p.rect_stroke(
        rect,
        2.0,
        Stroke::new(1.0, if on || resp.hovered() { tint } else { ETCH }),
        egui::StrokeKind::Inside,
    );
    let (cols, rows) = grid.dims();
    let art = rect.shrink2(Vec2::new(6.0, 5.0));
    let gap = 1.5;
    let (w, h) = (
        (art.width() - gap * (cols - 1) as f32) / cols as f32,
        (art.height() - gap * (rows - 1) as f32) / rows as f32,
    );
    for r in 0..rows {
        for c in 0..cols {
            let at = art.min + Vec2::new(c as f32 * (w + gap), r as f32 * (h + gap));
            p.rect_filled(egui::Rect::from_min_size(at, Vec2::new(w, h)), 0.5, tint);
        }
    }
    resp
}

/// A small spinning arc, for something that is working. Keeps the frame loop going while visible.
/// The colour and word a session status is shown with.
fn status_look(status: SessionStatus) -> (egui::Color32, &'static str) {
    match status {
        SessionStatus::Running => (TRACE, "working"),
        SessionStatus::Waiting => (READOUT, "needs you"),
        SessionStatus::Failed => (FAULT, "failed"),
        SessionStatus::Stopped => (LEGEND, "stopped"),
        SessionStatus::Idle => (OK, "idle"),
    }
}

/// How many sessions are in each status, the ones that want attention first.
/// Statuses nobody is in are left out.
pub fn status_counts(sessions: &[Session]) -> Vec<(SessionStatus, usize)> {
    [
        SessionStatus::Waiting,
        SessionStatus::Failed,
        SessionStatus::Running,
        SessionStatus::Idle,
        SessionStatus::Stopped,
    ]
    .into_iter()
    .map(|st| (st, sessions.iter().filter(|s| s.status == st).count()))
    .filter(|(_, n)| *n > 0)
    .collect()
}

/// A chip per status across every project, so the whole fleet reads at a glance.
fn status_badge(ui: &mut Ui, sessions: &[Session]) {
    let counts = status_counts(sessions);
    if counts.is_empty() {
        chip(ui, "no agents", LEGEND, false);
        return;
    }
    // Right to left, so add them reversed to read left to right.
    for (status, n) in counts.into_iter().rev() {
        let (tint, word) = status_look(status);
        chip(ui, &format!("{n} {word}"), tint, status != SessionStatus::Stopped)
            .on_hover_text(format!("{n} agent chat{} {word}", if n == 1 { "" } else { "s" }));
    }
}

fn spinner(ui: &Ui, center: egui::Pos2, radius: f32, tint: egui::Color32) {
    let t = ui.input(|i| i.time) as f32;
    let start = t * std::f32::consts::TAU * 1.2;
    let sweep = std::f32::consts::TAU * 0.7;
    let steps = 14;
    let points: Vec<egui::Pos2> = (0..=steps)
        .map(|i| {
            let a = start + sweep * i as f32 / steps as f32;
            center + Vec2::new(a.cos(), a.sin()) * radius
        })
        .collect();
    let p = ui.painter();
    p.circle_stroke(center, radius, Stroke::new(1.0, tint.gamma_multiply(0.2)));
    p.add(egui::Shape::line(points, Stroke::new(1.6, tint)));
    ui.ctx().request_repaint_after(Duration::from_millis(33));
}

/// Scan the installed fonts off the UI thread; the window draws with egui's own until it is done.
fn scan_fonts(ctx: &egui::Context) -> Option<std::sync::mpsc::Receiver<Installed>> {
    if cfg!(any(target_arch = "wasm32", target_os = "android")) {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = ctx.clone();
    crate::spawn_blocking(move || {
        let _ = tx.send(Installed::load());
        ctx.request_repaint();
    });
    Some(rx)
}

fn server_section(ui: &mut Ui, st: &mut Settings, has_local: bool) {
    section(ui, "server", "where foreman runs its agents", |ui| {
        if has_local {
            segments(
                ui,
                &mut st.place,
                &[(Where::Local, "this machine", None), (Where::Remote, "remote", None)],
            );
            ui.add_space(8.0);
        }
        match st.place {
            Where::Local => note(
                ui,
                "Foreman starts its server in the background on this machine and keeps it running after the window closes, so agents carry on working.",
                LEGEND,
            ),
            Where::Remote => {
                row_help(
                    ui,
                    "address",
                    "Where the server listens, through whatever tunnel or DNAT you put in front of it.",
                    |ui| {
                        field(ui, &mut st.url, "https://foreman.example.com");
                    },
                );
                row_help(ui, "token", "Printed by `foreman token` on that machine.", |ui| {
                    secret(ui, &mut st.token);
                });
            }
        }
    });
}

fn speech_section(ui: &mut Ui, sp: &mut crate::voice::Settings) {
    use crate::voice::{LOCAL_MODELS, Mode, Voice};
    section(ui, "voice input", "kept on this device", |ui| {
        let mut options = vec![(Mode::Server, "speech server", None)];
        if cfg!(feature = "local-speech") {
            options.insert(0, (Mode::Local, "this machine", None));
        }
        segments(ui, &mut sp.mode, &options);
        ui.add_space(8.0);
        match sp.mode {
            Mode::Local => {
                row_help(
                    ui,
                    "model",
                    "Whisper, run by foreman on this machine. Downloaded the first time you speak.",
                    |ui| {
                        choice(
                            ui,
                            "speech-model",
                            &mut sp.model,
                            LOCAL_MODELS.iter().map(|m| {
                                let have = if Voice::installed(m.0) { ", downloaded" } else { "" };
                                (m.0.to_string(), format!("{}, {} MB{have}", m.2, m.3))
                            }),
                        );
                    },
                );
            }
            Mode::Server => {
                row_help(
                    ui,
                    "address",
                    "Any server with an OpenAI style /v1/audio/transcriptions: OpenAI, Groq, faster-whisper-server, whisper.cpp.",
                    |ui| {
                        field(ui, &mut sp.url, "http://localhost:8000/v1");
                    },
                );
                row_help(
                    ui,
                    "key",
                    "Sent as a bearer token. Leave empty if the server needs none.",
                    |ui| {
                        secret(ui, &mut sp.key);
                    },
                );
                row_help(ui, "model", "Sent as the model name. Empty sends whisper-1.", |ui| {
                    field(ui, &mut sp.server_model, "whisper-1");
                });
            }
        }
        if let Some(p) = sp.problem() {
            status(ui, false, p);
        }
    });
    ui.add_space(8.0);
    hint(
        ui,
        "The speak key beside send records from the default microphone until you press it again, then adds what you said to the message. Nothing is sent until you send it.",
    );
}

fn appearance_section(ui: &mut Ui, face: &mut Typeface, installed: &Installed, scanning: bool) {
    section(ui, "appearance", "kept on this device", |ui| {
        if installed.can_choose() {
            row_help(
                ui,
                "font",
                "Any font installed here. Empty uses the system's sans-serif.",
                |ui| {
                    font_input(ui, "font-family", &mut face.family, &installed.all);
                },
            );
            row_help(
                ui,
                "code font",
                "Used for code, tool output and figures. Empty uses the system's monospace.",
                |ui| {
                    font_input(ui, "font-mono", &mut face.mono, &installed.mono);
                },
            );
        } else if scanning {
            hint(ui, "looking for installed fonts");
        } else {
            hint(ui, "This platform does not list its fonts, so the text keeps the built in ones.");
        }
        row_help(ui, "text size", "Scales every piece of text in the window.", |ui| {
            ui.add(
                egui::DragValue::new(&mut face.size)
                    .range(fonts::MIN..=fonts::MAX)
                    .speed(0.1)
                    .fixed_decimals(1)
                    .suffix(" pt"),
            );
        });
    });
}

fn font_input(ui: &mut Ui, id: &str, value: &mut String, list: &[String]) {
    let w = ui.available_width();
    ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
        ModelPicker::new(id, list).noun("font").allow_empty("system default").show(ui, value);
    });
}

/// The model input: a dropdown over the server's list when there is one, else a plain field.
fn model_input(
    ui: &mut Ui,
    id: &str,
    value: &mut String,
    hint_text: &str,
    list: Option<&Result<Vec<String>, String>>,
) {
    match list {
        Some(Ok(l)) if !l.is_empty() => {
            let w = ui.available_width();
            ui.allocate_ui(Vec2::new(w, KEY_H), |ui| {
                ModelPicker::new(id, l).allow_empty(hint_text).show(ui, value);
            });
        }
        _ => {
            field(ui, value, hint_text);
        }
    }
}

fn short_about(about: &str) -> String {
    match about.split_once(" at ") {
        Some((model, _)) => format!("default {model}"),
        None => about.to_string(),
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

/// One choice on its own row, with a tick box for several picks or a dot for one.
fn option_row(
    ui: &mut Ui,
    label: &str,
    multiple: bool,
    on: bool,
    salt: impl std::hash::Hash + std::fmt::Debug,
) -> egui::Response {
    let (pad, mark) = (8.0, 22.0);
    let tint = if on { READOUT } else { VALUE };
    let width = ui.available_width();
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = ui.painter().layout(label.to_string(), font, tint, width - pad * 2.0 - mark);
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(width, galley.size().y + 10.0), Sense::hover());
    let resp = ui.interact(rect, ui.id().with(salt), Sense::click());
    resp.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Button, true, on, label.to_string())
    });
    if ui.is_rect_visible(rect) {
        let lit = on || resp.hovered();
        let fill = if on {
            READOUT.gamma_multiply(0.18)
        } else if resp.hovered() {
            ETCH.gamma_multiply(0.6)
        } else {
            WELL
        };
        let p = ui.painter();
        p.rect_filled(rect, 2.0, fill);
        p.rect_stroke(
            rect,
            2.0,
            Stroke::new(1.0, if lit { tint } else { ETCH }),
            egui::StrokeKind::Inside,
        );
        let edge = Stroke::new(1.0, if lit { tint } else { LEGEND });
        let at = egui::pos2(rect.left() + pad + 4.0, rect.center().y);
        if multiple {
            let tick = egui::Rect::from_center_size(at, Vec2::splat(9.0));
            p.rect_stroke(tick, 1.0, edge, egui::StrokeKind::Inside);
            if on {
                p.rect_filled(tick.shrink(2.0), 0.0, tint);
            }
        } else {
            p.circle_stroke(at, 4.5, edge);
            if on {
                p.circle_filled(at, 2.5, tint);
            }
        }
        let top = egui::pos2(rect.left() + pad + mark, rect.top() + 5.0);
        p.galley(top, galley, tint);
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

fn rail_block(ui: &mut Ui, rail: egui::Color32, fill: egui::Color32, body: impl FnOnce(&mut Ui)) {
    let r = egui::Frame::NONE
        .fill(fill)
        .inner_margin(Margin { left: 12, right: 8, top: 4, bottom: 4 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui);
        });
    let rect = r.response.rect;
    ui.painter().rect_filled(
        egui::Rect::from_min_max(rect.left_top(), egui::pos2(rect.left() + 3.0, rect.bottom())),
        0.0,
        rail,
    );
}

fn code_block(
    ui: &mut Ui,
    text: &str,
    colour: egui::Color32,
    salt: impl std::hash::Hash + std::fmt::Debug,
) {
    egui::Frame::NONE.fill(CHASSIS).corner_radius(2).inner_margin(Margin::symmetric(8, 5)).show(
        ui,
        |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt(salt)
                .max_height(320.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(text.trim_end()).font(mono(11.0)).color(colour),
                        )
                        .wrap(),
                    );
                });
        },
    );
}

fn tool_body(name: &str, input: &serde_json::Value) -> Option<String> {
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str());
    if let Some(c) = pick("command") {
        return (c.lines().count() > 1 || c.len() > 120).then(|| format!("$ {c}"));
    }
    match name {
        "todo_write" => {
            let items = input.get("items")?.as_array()?;
            let lines: Vec<String> = items
                .iter()
                .filter_map(|i| {
                    let text = i.get("text").or_else(|| i.get("content"))?.as_str()?;
                    let mark = match i.get("status").and_then(|s| s.as_str()) {
                        Some("done" | "completed") => "[x]",
                        Some("in_progress" | "doing") => "[~]",
                        _ => "[ ]",
                    };
                    Some(format!("{mark} {text}"))
                })
                .collect();
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
        "edit_file" | "edit" => {
            let old = pick("old").or(pick("oldText"))?;
            let new = pick("new").or(pick("newText")).unwrap_or_default();
            let mut out = String::new();
            for l in old.lines() {
                out.push_str(&format!("- {l}\n"));
            }
            for l in new.lines() {
                out.push_str(&format!("+ {l}\n"));
            }
            Some(out)
        }
        "write_file" | "write" => {
            pick("content").map(|c| c.lines().take(40).collect::<Vec<_>>().join("\n"))
        }
        _ if input.is_object() && input.as_object().is_some_and(|o| o.len() > 1) => {
            serde_json::to_string_pretty(input).ok()
        }
        _ => None,
    }
}

fn tool_summary(name: &str, input: &serde_json::Value) -> String {
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    if name == "todo_write"
        && let Some(items) = input.get("items").and_then(|i| i.as_array())
    {
        let done = items
            .iter()
            .filter(|i| {
                matches!(i.get("status").and_then(|s| s.as_str()), Some("done" | "completed"))
            })
            .count();
        return format!("{done} of {} done", items.len());
    }
    let text = pick("command")
        .or_else(|| pick("path"))
        .or_else(|| pick("file_path"))
        .or_else(|| pick("pattern"))
        .or_else(|| pick("query"))
        .or_else(|| pick("url"))
        .or_else(|| pick("handle"))
        .or_else(|| pick("message"))
        .or_else(|| pick("topic"))
        .unwrap_or_else(|| if input.is_null() { String::new() } else { input.to_string() });
    let line = text.lines().next().unwrap_or_default().to_string();
    let _ = name;
    line.chars().take(160).collect()
}

impl eframe::App for App {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.watch_paste(ctx, raw);
    }

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
    fn a_checklist_is_drawn_as_a_list_with_its_progress() {
        let input = serde_json::json!({"items": [
            {"text": "read", "status": "done"},
            {"text": "write", "status": "in_progress"},
            {"text": "test"},
        ]});
        assert_eq!(tool_body("todo_write", &input).unwrap(), "[x] read\n[~] write\n[ ] test");
        assert_eq!(tool_summary("todo_write", &input), "1 of 3 done");
    }

    #[test]
    fn status_counts_skip_empty_statuses_and_lead_with_attention() {
        let s = |status| Session {
            id: SessionId::default(),
            project_id: 1,
            worktree_id: 1,
            agent: "a".into(),
            title: String::new(),
            cwd: String::new(),
            model: None,
            issue_id: None,
            status,
            agent_session: None,
            usage: Default::default(),
            created_at: 0,
            updated_at: 0,
        };
        let all = [
            s(SessionStatus::Idle),
            s(SessionStatus::Running),
            s(SessionStatus::Running),
            s(SessionStatus::Waiting),
        ];
        assert_eq!(
            status_counts(&all),
            vec![
                (SessionStatus::Waiting, 1),
                (SessionStatus::Running, 2),
                (SessionStatus::Idle, 1)
            ]
        );
        assert!(status_counts(&[]).is_empty());
    }

    fn ids(n: usize) -> Vec<SessionId> {
        (0..n).map(|i| format!("00000000-0000-0000-0000-{:012}", i + 1).parse().unwrap()).collect()
    }

    #[test]
    fn a_chat_replaces_the_focused_tile() {
        let id = ids(6);
        let mut slots = [Some(id[0]), Some(id[1]), Some(id[2]), Some(id[3])];
        let at = place(&mut slots, 4, 1, id[4]);
        assert_eq!(at, 1);
        assert_eq!(slots, [Some(id[0]), Some(id[4]), Some(id[2]), Some(id[3])]);
    }

    #[test]
    fn a_chat_already_on_screen_only_takes_focus() {
        let id = ids(5);
        let mut slots = [Some(id[0]), Some(id[1]), Some(id[2]), Some(id[3])];
        let at = place(&mut slots, 4, 0, id[2]);
        assert_eq!(at, 2);
        assert_eq!(slots, [Some(id[0]), Some(id[1]), Some(id[2]), Some(id[3])]);
    }

    #[test]
    fn a_chat_in_a_hidden_tile_moves_instead_of_showing_twice() {
        let id = ids(5);
        let mut slots = [Some(id[0]), Some(id[1]), None, Some(id[3])];
        let at = place(&mut slots, 2, 0, id[3]);
        assert_eq!(at, 0);
        assert_eq!(slots, [Some(id[3]), Some(id[1]), None, None]);
    }

    #[test]
    fn grids_know_their_size() {
        let sizes: Vec<usize> = Grid::ALL.iter().map(|g| g.slots()).collect();
        assert_eq!(sizes, vec![1, 2, 2, 4]);
        for g in Grid::ALL {
            assert_eq!(Grid::from_key(g.key()), Some(g));
        }
    }

    #[test]
    fn speech_lands_in_the_draft_of_the_tile_that_asked() {
        let mut app = App::new(&egui::Context::default(), None, None);
        let (a, b) = (
            "00000000-0000-0000-0000-000000000001".parse::<SessionId>().unwrap(),
            "00000000-0000-0000-0000-000000000002".parse::<SessionId>().unwrap(),
        );
        app.panes.entry(a).or_default().draft = "first".into();
        app.speaking_in = Some(Tile::Chat(b));
        app.add_speech(" hello there ");
        assert_eq!(app.draft_of(b), "hello there");
        assert_eq!(app.draft_of(a), "first");
        assert_eq!(app.refocus, Some(Tile::Chat(b)));
    }

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
