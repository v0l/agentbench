use crate::client::{Client, Reply, Server};
use crate::fonts::{self, Installed, Typeface};
use crate::form::{field, field_then, prose, prose_id, secret};
use crate::picker::ModelPicker;
use crate::scan::Scan;
use agentbench_core::{
    Comment, Entry, Id, Issue, IssuePatch, IssueState, Lsp, LspHealth, Message, NewComment,
    NewIssue, NewProject, NewSession, NewWorktree, Overview, Place, Question, Session, SessionId,
    SessionStatus, Worktree,
};
use egui::{Align, Layout, Margin, Sense, Stroke, Ui, Vec2};
use egui_bench::key::Key;
use egui_bench::markdown;
use egui_bench::prelude::*;
use std::collections::{HashMap, HashSet};
use web_time::{Duration, Instant};

mod chat;
mod composer;
mod header;
mod modals;
mod settings;
mod side;
mod transcript;
mod widgets;

use self::settings::{model_input, pairing_of};
pub use self::widgets::status_counts;
use self::widgets::*;

const SIDE_W: f32 = 320.0;
const RETRY: Duration = Duration::from_secs(3);
const TREE_INDENT: f32 = 16.0;
const TREE_X: f32 = 5.0;
/// The most tiles any grid has.
const SLOTS: usize = 6;
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
    /// Six chats, three across and two down.
    Six,
}

impl Grid {
    pub const ALL: [Grid; 5] = [Grid::Single, Grid::Columns, Grid::Rows, Grid::Quad, Grid::Six];

    /// Columns and rows.
    pub fn dims(self) -> (usize, usize) {
        match self {
            Grid::Single => (1, 1),
            Grid::Columns => (2, 1),
            Grid::Rows => (1, 2),
            Grid::Quad => (2, 2),
            Grid::Six => (3, 2),
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
            Grid::Six => "3x2",
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
            Grid::Six => "six chats, three across and two down",
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

/// The pages of the settings dialog.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Speech,
    Agent,
    Titles,
}

struct Settings {
    page: Page,
    speech: crate::voice::Settings,
    place: Where,
    url: String,
    token: String,
    /// The address a phone reaches this server at, for the pairing code.
    external: String,
    /// Whether the model's thinking is shown in chats.
    thinking: bool,
    /// Whether a phone tells you in the background when a chat needs you or finishes.
    alerts: bool,
    /// Set by the scan key, for the window to open the camera.
    scan: bool,
    /// The token of the server this window is connected to, which the code carries.
    pair_token: String,
    /// The address of the connected server, used for the code when no other is given.
    pair_url: String,
    face: Typeface,
    agent_error: Option<String>,
    asking_agent: bool,
    model: agentbench_core::ModelSettings,
    loaded: bool,
    models: Option<Result<Vec<String>, String>>,
    asking: bool,
    title: agentbench_core::TitleSettings,
    title_loaded: bool,
    title_models: Option<Result<Vec<String>, String>>,
    title_asking: bool,
    /// Where the server runs speech: `None` until it answers, `Some(None)` when it cannot say.
    audio: Option<Option<agentbench_core::AudioStatus>>,
    /// The voices the server reads aloud in: `None` until it answers.
    voices: Option<Option<Vec<(String, String)>>>,
    /// The models the server reads speech with: `None` until it answers.
    speech_models: Option<Option<Vec<(String, String)>>>,
}

struct TerminalView {
    session: SessionId,
    screen: Option<agentbench_core::Screen>,
    asked: Option<web_time::Instant>,
    size: (u16, u16),
    jobs: Vec<agentbench_core::Job>,
    jobs_asked: Option<web_time::Instant>,
    job: Option<String>,
}

impl TerminalView {
    fn new(session: SessionId) -> TerminalView {
        TerminalView {
            session,
            screen: None,
            asked: None,
            size: (0, 0),
            jobs: Vec::new(),
            jobs_asked: None,
            job: None,
        }
    }
}

fn terminal_bytes(events: &[egui::Event]) -> String {
    let mut out = String::new();
    for e in events {
        match e {
            egui::Event::Text(t) | egui::Event::Paste(t) => {
                out.push_str(&t.replace("\r\n", "\r").replace('\n', "\r"))
            }
            egui::Event::Copy => out.push('\x03'),
            egui::Event::Cut => out.push('\x18'),
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                use egui::Key as K;
                if modifiers.ctrl && !modifiers.shift {
                    if let Some(c) = key.name().chars().next().filter(|_| key.name().len() == 1) {
                        let c = c.to_ascii_lowercase();
                        if c.is_ascii_lowercase() && !matches!(c, 'c' | 'x' | 'v') {
                            out.push((c as u8 - b'a' + 1) as char);
                        }
                    }
                    continue;
                }
                out.push_str(match key {
                    K::Enter => "\r",
                    K::Backspace => "\x7f",
                    K::Tab if modifiers.shift => "\x1b[Z",
                    K::Tab => "\t",
                    K::Escape => "\x1b",
                    K::ArrowUp => "\x1b[A",
                    K::ArrowDown => "\x1b[B",
                    K::ArrowRight => "\x1b[C",
                    K::ArrowLeft => "\x1b[D",
                    K::Home => "\x1b[H",
                    K::End => "\x1b[F",
                    K::Delete => "\x1b[3~",
                    K::PageUp => "\x1b[5~",
                    K::PageDown => "\x1b[6~",
                    _ => "",
                });
            }
            _ => {}
        }
    }
    out
}

const CONSOLE_EVERY: Duration = Duration::from_secs(1);
const TERMINAL_EVERY: Duration = Duration::from_millis(150);

fn job_state(j: &agentbench_core::Job) -> String {
    match (j.running, j.exit) {
        (true, _) => "running".into(),
        (false, Some(c)) => format!("exited {c}"),
        (false, None) => "ended".into(),
    }
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                Some('[') => {
                    for n in chars.by_ref() {
                        if ('@'..='~').contains(&n) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(n) = chars.next() {
                        if n == '\x07' || (n == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => out.push('\n'),
            c => out.push(c),
        }
    }
    out
}

enum Confirm {
    Project(Id, String),
    Worktree(Id, String, bool),
    Session(SessionId, String),
    Issue(Id, String),
}

#[derive(Default)]
struct Transcript {
    rows: Vec<Message>,
    messages: Vec<Message>,
    last: Id,
    loading: bool,
    /// How tall each message was when last laid out, so the ones scrolled out of sight are not
    /// laid out again, only given their room. Thrown away when the width or the look changes.
    heights: Heights,
}

/// Heights of transcript entries. `of` holds what was measured for the current width and look.
/// When those change, the old heights (scaled to the new width) become `guess`: good enough to
/// leave room for entries out of sight, so a resize lays out only what is on screen.
#[derive(Default)]
struct Heights {
    key: (u32, u32, u32),
    width: f32,
    of: HashMap<Id, f32>,
    guess: HashMap<Id, f32>,
}

impl Heights {
    /// Move to a new width and look: measured heights turn into guesses.
    fn keep_for(&mut self, key: (u32, u32, u32), width: f32) {
        if self.key == key {
            return;
        }
        // Text wraps about in proportion to the width; a different text size or font changes
        // heights in ways not worth guessing, but a stale guess is still better than none.
        let scale = if self.width > 0.0 && width > 0.0 { self.width / width } else { 1.0 };
        let old = std::mem::take(&mut self.of);
        for h in self.guess.values_mut() {
            *h *= scale;
        }
        for (id, h) in old {
            self.guess.insert(id, h * scale);
        }
        self.key = key;
        self.width = width;
    }

    /// The height of an entry, measured if possible, else guessed.
    fn known(&self, id: Id) -> Option<f32> {
        self.of.get(&id).or_else(|| self.guess.get(&id)).copied()
    }

    fn measured(&mut self, id: Id, h: f32) {
        self.of.insert(id, h);
        self.guess.remove(&id);
    }

    fn forget(&mut self, id: Id) {
        self.of.remove(&id);
        self.guess.remove(&id);
    }
}

/// How far past the visible part of the transcript entries are still laid out, so a scroll
/// of a few lines never shows a gap.
const OVERSCAN: f32 = 300.0;

pub struct App {
    client: Option<Client>,
    face: Typeface,
    installed: std::sync::Arc<Installed>,
    font_scan: Option<std::sync::mpsc::Receiver<Installed>>,
    font_load: Option<std::sync::mpsc::Receiver<egui::FontDefinitions>>,
    local: Option<std::sync::Arc<LocalServer>>,
    remote: bool,
    /// The address a phone reaches the server at, for the pairing code. Kept on this device.
    external: String,
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
    /// Whether the project list is open over a narrow window, where it has no panel of its own.
    drawer: bool,
    /// The last link the app was opened with, so it is paired with once.
    last_link: Option<String>,
    /// A chat a tapped notification asked for, shown once the overview is loaded.
    open_pending: Option<SessionId>,
    /// The camera, while a pairing code is being scanned.
    scanner: Option<crate::scan::Scanner>,
    scan_view: Option<egui::TextureHandle>,
    /// How tall the on screen keyboard is, which the window is not resized for.
    keyboard: f32,
    link_checked: Instant,
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
    terminal: Option<TerminalView>,
    settings: Option<Settings>,
    save_requested: bool,
    tray: crate::attach::Tray,
    voice: crate::voice::Voice,
    speech: crate::voice::Settings,
    /// Reading a reply aloud, and the message being read.
    reader: crate::voice::Reader,
    reading: Option<Id>,
    /// Models on the server the native agent is saved to use. None until asked.
    agent_models: HashMap<String, Result<Vec<String>, String>>,
    asking_models: HashSet<String>,
    markdown: markdown::Cache,
    follow: bool,
    jump: Option<Tile>,
    awaiting: Option<Id>,
    thinking: bool,
    /// How many times the fonts were swapped, so measured transcript heights are dropped.
    fonts_loaded: u32,
    /// Whether a phone tells you in the background when a chat needs you or finishes.
    alerts: bool,
    /// Put the cursor back in this tile's composer: set when speech lands in its draft.
    refocus: Option<Tile>,
    /// How tall each tile's composer was when last drawn, so the transcript leaves it room.
    composer_h: HashMap<Option<Tile>, f32>,
    /// The tile being drawn, so a key pressed in it knows whose draft it works on.
    drawing: Option<Tile>,
    /// The tile whose speak key started the take in progress; the words go to its draft.
    speaking_in: Option<Tile>,
    /// The words the voice has put at the end of that draft so far, so the next reading can
    /// replace them rather than add to them.
    spoken: String,
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
            external: storage.and_then(|s| s.get_string("external")).unwrap_or_default(),
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
            drawer: false,
            last_link: None,
            open_pending: None,
            scanner: None,
            scan_view: None,
            keyboard: 0.0,
            link_checked: Instant::now() - Duration::from_secs(5),
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
            terminal: None,
            settings: None,
            save_requested: false,
            tray: crate::attach::Tray::default(),
            voice: Default::default(),
            reader: Default::default(),
            reading: None,
            speech: crate::voice::Settings::load(storage),
            agent_models: HashMap::new(),
            asking_models: HashSet::new(),
            markdown: markdown::Cache::default(),
            follow: true,
            jump: None,
            awaiting: None,
            refocus: None,
            composer_h: HashMap::new(),
            drawing: None,
            speaking_in: None,
            spoken: String::new(),
            fonts_loaded: 0,
            thinking: storage.and_then(|s| s.get_string("thinking")).as_deref() != Some("false"),
            alerts: storage.and_then(|s| s.get_string("alerts")).as_deref() != Some("false"),
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
        storage.set_string("external", self.external.clone());
        storage.set_string("thinking", self.thinking.to_string());
        storage.set_string("alerts", self.alerts.to_string());
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

    /// Tell the background service which server to watch, and whether to.
    fn sync_alerts(&self) {
        if let Some(c) = &self.client {
            crate::notify::watch(&c.server.url, &c.server.token, self.alerts);
        }
    }

    fn connect_to(&mut self, ctx: &egui::Context, server: Server) {
        crate::notify::watch(&server.url, &server.token, self.alerts);
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

    /// Whether a project is the built-in home folder, which has no git repo behind it.
    fn is_home(&self, project: Id) -> bool {
        self.overview.as_ref().is_some_and(|o| o.projects.iter().any(|p| p.id == project && p.home))
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

    /// Connect to the server a pairing link names, and keep it for next time.
    pub fn pair(&mut self, ctx: &egui::Context, with: agentbench_core::pairing::Pairing) {
        self.settings = None;
        self.remote = true;
        self.connect_to(
            ctx,
            Server { url: with.url.trim().to_string(), token: with.token.trim().to_string() },
        );
        self.notice = Some((true, format!("paired with {}", agentbench_core::host_of(&with.url))));
    }

    /// Reads the link the app was opened with, when it is new, and pairs with it.
    fn open_launch_link(&mut self, ctx: &egui::Context) {
        let Some(text) = crate::system::launch_link() else {
            return;
        };
        if self.last_link.as_deref() == Some(text.as_str()) {
            return;
        }
        self.last_link = Some(text.clone());
        if let Some(p) = agentbench_core::pairing::Pairing::parse(&text) {
            self.pair(ctx, p);
        } else if let Some(id) = chat_in_link(&text) {
            // A tapped notification: show that chat once the overview has it.
            self.open_pending = Some(id);
        }
    }

    /// Opens the chat a notification named, when the server's chats are known.
    fn open_pending_chat(&mut self) {
        let Some(id) = self.open_pending else {
            return;
        };
        let Some(o) = &self.overview else {
            return;
        };
        let found = o.sessions.iter().find(|s| s.id == id).map(|s| s.project_id);
        self.open_pending = None;
        if let Some(project) = found {
            self.open_session(id, project);
        }
    }

    /// On a phone the app is drawn under the status bar and the gesture bar, and the window is not
    /// resized for the keyboard. Tell egui how much of the screen those take, so panels and
    /// dialogs stay clear of them.
    fn keep_clear_of_the_system(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        if !cfg!(target_os = "android") {
            return;
        }
        self.keyboard = crate::system::keyboard_height(ctx);
        let bottom = self.keyboard.max(crate::system::bottom_inset(ctx));
        raw.safe_area_insets = Some(egui::SafeAreaInsets(egui::epaint::MarginF32 {
            left: 0.0,
            right: 0.0,
            top: crate::system::top_inset(ctx),
            bottom,
        }));
        if self.keyboard > 0.0 || ctx.egui_wants_keyboard_input() {
            // The keyboard slides in over several frames, and nothing else asks for them.
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Whether the project list is open over a narrow window.
    pub fn drawer_open(&self) -> bool {
        self.drawer
    }

    /// The device the server says its speech runs on, once the settings dialog has asked.
    /// The speech models the settings dialog was told about, once the server has answered.
    pub fn settings_speech_models(&self) -> Option<Option<Vec<(String, String)>>> {
        self.settings.as_ref()?.speech_models.clone()
    }

    /// The voices the settings dialog was told about, once the server has answered.
    pub fn settings_voices(&self) -> Option<Option<Vec<(String, String)>>> {
        self.settings.as_ref()?.voices.clone()
    }

    pub fn settings_audio_device(&self) -> Option<String> {
        self.settings.as_ref()?.audio.clone()?.map(|a| a.device)
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

    /// Show a page of the settings dialog: general, speech, agent or titles.
    pub fn settings_page(&mut self, page: &str) {
        if let Some(st) = &mut self.settings {
            st.page = match page {
                "speech" => Page::Speech,
                "agent" => Page::Agent,
                "titles" => Page::Titles,
                _ => Page::General,
            };
        }
    }

    /// Pick the model voice input runs locally.
    /// Read speech on the agentbench server, as a phone does.
    pub fn settings_use_server_speech(&mut self) {
        if let Some(st) = &mut self.settings {
            st.speech.mode = crate::voice::Mode::Agentbench;
        }
    }

    pub fn settings_set_speech_model(&mut self, id: &str) {
        if let Some(st) = &mut self.settings {
            st.speech.model = id.into();
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

    pub fn settings_set_external(&mut self, external: &str) {
        if let Some(st) = &mut self.settings {
            st.external = external.into();
        }
    }

    /// The link the settings' pairing code holds, if it shows one.
    pub fn settings_pairing(&self) -> Option<agentbench_core::pairing::Pairing> {
        self.settings.as_ref().and_then(pairing_of)
    }

    pub fn settings_set_text_size(&mut self, size: f32) {
        if let Some(st) = &mut self.settings {
            st.face.size = size;
        }
    }

    /// The draft of the tile whose speak key was pressed. The draft on screen belongs to
    /// whichever tile is being drawn, and none is when speech arrives.
    fn speech_draft(&mut self) -> (Tile, &mut String) {
        let tile = self.speaking_in.unwrap_or(Tile::New);
        let draft = match tile {
            Tile::Chat(s) => &mut self.panes.entry(s).or_default().draft,
            Tile::New | Tile::Empty => &mut self.new_pane.draft,
        };
        (tile, draft)
    }

    /// Put what was said at the end of the draft, in place of what an earlier reading of the same
    /// take put there. `last` ends the take.
    pub fn add_speech(&mut self, text: &str, last: bool) {
        let spoken = std::mem::take(&mut self.spoken);
        let (tile, draft) = self.speech_draft();
        if !spoken.is_empty() && draft.ends_with(&spoken) {
            draft.truncate(draft.len() - spoken.len());
        }
        let before = draft.len();
        crate::voice::append(draft, text);
        let added = draft[before..].to_string();
        self.refocus = Some(tile);
        if last {
            self.speaking_in = None;
        } else {
            self.spoken = added;
        }
    }

    /// Take back what a take wrote into the draft, because it came to nothing.
    fn drop_speech(&mut self) {
        let spoken = std::mem::take(&mut self.spoken);
        if !spoken.is_empty() {
            let (_, draft) = self.speech_draft();
            if draft.ends_with(&spoken) {
                draft.truncate(draft.len() - spoken.len());
            }
        }
        self.speaking_in = None;
    }

    /// Whether `speak` is under way, for tests.
    pub fn speaking(&self) -> bool {
        self.voice.busy()
    }

    /// The speech settings as the voice should use them now: the agentbench server option becomes
    /// the address and token of the server this window is connected to.
    fn speech_now(&self) -> crate::voice::Settings {
        let server = self.client.as_ref().map(|c| (c.server.url.as_str(), c.server.token.as_str()));
        self.speech.resolved(server)
    }

    /// Read a reply aloud with the connected server's voice, or stop if that reply is being read.
    pub(super) fn toggle_reading(&mut self, ctx: &egui::Context, id: Id, text: &str) {
        if self.reading == Some(id) && self.reader.busy() {
            self.reader.stop();
            self.reading = None;
            return;
        }
        let Some(c) = &self.client else {
            self.notice =
                Some((false, "not connected to an agentbench server to read aloud".into()));
            return;
        };
        let from = crate::voice::Source {
            url: c.server.base(),
            token: c.server.token.clone(),
            voice: self.speech.reply_voice.clone(),
        };
        let ctx = ctx.clone();
        match self.reader.start(text, from, move || ctx.request_repaint()) {
            Ok(()) => self.reading = Some(id),
            Err(e) => {
                self.reading = None;
                self.notice = Some((false, e));
            }
        }
    }

    /// Whether the reply with this message id is being read aloud.
    pub(super) fn is_reading(&self, id: Id) -> bool {
        self.reading == Some(id) && self.reader.busy()
    }

    /// The hotkey: start listening in the tile in use, or finish the take now.
    fn toggle_speech(&mut self, ctx: &egui::Context) {
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        if self.voice.listening() {
            self.voice.finish(&self.speech_now(), wake);
            return;
        }
        let tile = match self.session {
            Some(s) => Tile::Chat(s),
            None if self.fresh.is_some() => Tile::New,
            None => return,
        };
        self.begin_speech(tile, wake);
    }

    fn begin_speech(&mut self, tile: Tile, wake: impl Fn() + Send + Sync + 'static) {
        if self.voice.busy() {
            return;
        }
        if let Some(p) = self.speech.problem() {
            self.notice = Some((false, p.into()));
        } else if self.speech.mode == crate::voice::Mode::Agentbench && self.client.is_none() {
            self.notice =
                Some((false, "not connected to an agentbench server to read speech".into()));
        } else if let Err(e) = self.voice.start(wake) {
            self.notice = Some((false, e));
        } else {
            self.speaking_in = Some(tile);
            self.spoken.clear();
        }
    }

    /// The draft of the chat `session`, as the tile shows it.
    pub fn draft_of(&self, session: SessionId) -> &str {
        self.panes.get(&session).map_or("", |p| p.draft.as_str())
    }

    fn restore_queued(&mut self, session: SessionId, queued: &[String]) {
        if queued.is_empty() {
            return;
        }
        let pane = self.panes.entry(session).or_default();
        pane.draft = agentbench_core::restore_queued(&pane.draft, queued);
        self.refocus = Some(Tile::Chat(session));
    }

    pub fn follows(&self, session: SessionId) -> bool {
        self.panes.get(&session).is_none_or(|p| p.follow)
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
                    self.project =
                        o.projects.iter().find(|p| !p.home).or(o.projects.first()).map(|p| p.id);
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
                    for i in 0..agentbench_core::SHOWN_PER_ROW {
                        t.heights.forget(m.id * agentbench_core::SHOWN_PER_ROW + i);
                    }
                    match t.rows.iter_mut().rev().find(|x| x.id == m.id) {
                        Some(x) => *x = m,
                        None => t.rows.push(m),
                    }
                }
                t.messages = agentbench_core::shown(&t.rows);
            }
            Reply::Comments(iid, c) => {
                self.comments.insert(iid, c);
            }
            Reply::Interrupted(sid, queued) => {
                self.notice = Some((true, "stopping".into()));
                self.restore_queued(sid, &queued);
            }
            Reply::Screen(sid, screen) => {
                if let Some(t) = self.terminal.as_mut().filter(|t| t.session == sid) {
                    t.screen = Some(screen);
                    t.asked = None;
                }
            }
            Reply::Jobs(sid, jobs) => {
                if let Some(t) = self.terminal.as_mut().filter(|t| t.session == sid) {
                    t.jobs = jobs;
                    t.jobs_asked = None;
                }
            }
            Reply::Model(m) => {
                if let Some(st) = &mut self.settings {
                    if st.loaded {
                        self.settings = None;
                        self.notice =
                            Some((true, format!("native agent uses {} at {}", m.model, m.url)));
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
                self.agent_models.remove("native");
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
            Reply::SpeechModels(m) => {
                if let Some(st) = &mut self.settings {
                    st.speech_models = Some(m);
                }
            }
            Reply::Voices(v) => {
                if let Some(st) = &mut self.settings {
                    st.voices = Some(v);
                }
            }
            Reply::Audio(a) => {
                if let Some(st) = &mut self.settings {
                    st.audio = Some(a);
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
        let tail = t.rows.iter().rev().take(3).map(|m| m.id).min().unwrap_or(0);
        let after = if t.rows.is_empty() { 0 } else { tail.saturating_sub(1) };
        if let Some(c) = &mut self.client {
            c.messages(sid, after);
        }
    }

    fn tick(&mut self, ctx: &egui::Context) {
        self.poll_starting(ctx);
        if cfg!(target_os = "android") && self.link_checked.elapsed() > Duration::from_secs(1) {
            self.link_checked = Instant::now();
            self.open_launch_link(ctx);
            self.open_pending_chat();
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        if let Some(found) = self.font_scan.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.font_scan = None;
            self.installed = std::sync::Arc::new(found);
            self.load_fonts(ctx);
        }
        if let Some(defs) = self.font_load.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.font_load = None;
            ctx.set_fonts(defs);
            self.fonts_loaded += 1;
        }
        if let Some(problem) = self.tray.poll(ctx) {
            self.notice = Some((false, problem));
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::M)) {
            self.toggle_speech(ctx);
        }
        let wake = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        for e in self.reader.tick() {
            self.notice = Some((false, e));
        }
        if !self.reader.busy() {
            self.reading = None;
        }
        let speech = self.speech_now();
        self.voice.tick(&speech, wake);
        for said in self.voice.poll() {
            match said {
                crate::voice::Said::Partial(text) => self.add_speech(&text, false),
                crate::voice::Said::Final(text) if text.trim().is_empty() => {
                    self.drop_speech();
                    self.notice = Some((false, "no speech was heard".into()));
                }
                crate::voice::Said::Final(text) => self.add_speech(&text, true),
                crate::voice::Said::Failed(e) => {
                    self.drop_speech();
                    self.notice = Some((false, e));
                }
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

    pub fn frame(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.tick(&ctx);
        let narrow = ctx.content_rect().width() < 720.0;
        self.narrow = narrow;

        // The root panels cover the whole screen, so they keep clear of the system's bars and the
        // keyboard themselves; dialogs already stay inside `content_rect`.
        let (above, below) = (
            ctx.content_rect().top() - ctx.viewport_rect().top(),
            ctx.viewport_rect().bottom() - ctx.content_rect().bottom(),
        );
        if below > 0.0 {
            egui::Panel::bottom("system-bottom")
                .exact_size(below)
                .resizable(false)
                .frame(egui::Frame::NONE.fill(CHASSIS))
                .show(ui, |_| {});
        }
        egui::Panel::top("header")
            .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(Margin {
                left: 12,
                right: 12,
                top: 8 + above as i8,
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
        if narrow {
            self.drawer_panel(&ctx);
        } else {
            self.drawer = false;
            egui::Panel::left("side")
                .exact_size(SIDE_W)
                .resizable(false)
                .frame(egui::Frame::NONE.fill(CHASSIS).inner_margin(8))
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        self.side(ui);
                    });
                });
        }
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
        // Entries scrolled out of sight are not drawn, so they do not touch their markdown; keep
        // it until the cache has grown well past what a screen needs.
        if self.markdown.len() > 400 {
            self.markdown.sweep();
        }
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

impl eframe::App for App {
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.keep_clear_of_the_system(ctx, raw);
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
    use super::transcript::{tool_body, tool_summary};
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
    fn claudes_checklist_is_drawn_too() {
        let input = serde_json::json!({"todos": [
            {"content": "read", "status": "completed"},
            {"content": "write", "status": "pending"},
        ]});
        assert_eq!(tool_body("TodoWrite", &input).unwrap(), "[x] read\n[ ] write");
        assert_eq!(tool_summary("TodoWrite", &input), "1 of 2 done");
    }

    #[test]
    fn the_pinned_plan_is_the_newest_checklist() {
        let call = |id: i64, name: &str, input: serde_json::Value| Message {
            id,
            session_id: SessionId(Default::default()),
            entry: Entry::Tool {
                call_id: format!("c{id}"),
                name: name.into(),
                input,
                output: None,
                error: false,
            },
            created_at: 0,
        };
        let old = serde_json::json!({"items": [{"text": "old", "status": "done"}]});
        let new = serde_json::json!({"items": [{"text": "new", "status": "in_progress"}]});
        let msgs = vec![
            call(1, "todo_write", old),
            call(2, "bash", serde_json::json!({"command": "ls"})),
            call(3, "todo_write", new),
            call(4, "bash", serde_json::json!({"command": "ls"})),
        ];
        let plan = super::transcript::latest_plan(&msgs).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].1, "new");
        assert!(super::transcript::latest_plan(&msgs[1..2]).is_none());
    }

    #[test]
    fn only_a_phone_gets_the_short_status_tags() {
        use super::composer::status_form;
        assert_eq!(
            status_form(false, false),
            (false, false),
            "a wide window: everything, in words"
        );
        assert_eq!(status_form(true, false), (true, false), "a tile: less, still in words");
        assert_eq!(status_form(false, true), (true, true), "a phone: less, and short");
    }

    #[test]
    fn a_notification_link_names_its_chat() {
        let id = "ab0e0099-33d5-494c-bc8c-cbdc8ea44989";
        let got = chat_in_link(&format!("agentbench://open?session={id}&t=1"));
        assert_eq!(got.map(|i| i.to_string()).as_deref(), Some(id));
        assert!(chat_in_link("agentbench://pair?url=x&token=y").is_none());
        assert!(chat_in_link("agentbench://open?session=nope").is_none());
    }

    #[test]
    fn language_servers_are_listed_once_each_in_their_worst_state() {
        use super::composer::lsp_summary;
        let l = |name: &str, dir: &str, health, detail: &str| Lsp {
            name: name.into(),
            dir: dir.into(),
            health,
            detail: detail.into(),
        };
        let all = [
            l("rust", ".", LspHealth::Ready, ""),
            l("typescript", "web", LspHealth::Ready, ""),
            l("typescript", "docs", LspHealth::Indexing, ""),
            l("go", ".", LspHealth::Missing, "go install gopls"),
        ];
        let sum = lsp_summary(&all);
        let names: Vec<_> = sum.iter().map(|(n, h, _)| (n.as_str(), *h)).collect();
        assert_eq!(
            names,
            [
                ("rust", LspHealth::Ready),
                ("typescript", LspHealth::Indexing),
                ("go", LspHealth::Missing)
            ]
        );
        assert_eq!(sum[0].2, "lsp rust: ready");
        assert_eq!(sum[1].2, "lsp typescript in web: ready\nlsp typescript in docs: indexing");
        assert_eq!(sum[2].2, "lsp go: missing: go install gopls");
        assert!(lsp_summary(&[]).is_empty());
    }

    #[test]
    fn a_usage_stored_before_language_servers_still_reads() {
        let old = r#"{"input_tokens":1,"output_tokens":2,"cache_read_tokens":0,"cost_usd":0.0,"turns":1}"#;
        let u: agentbench_core::Usage = serde_json::from_str(old).unwrap();
        assert!(u.lsp.is_empty());
        let mut total = u.clone();
        total.add(&agentbench_core::Usage {
            lsp: vec![Lsp { name: "rust".into(), health: LspHealth::Ready, ..Lsp::default() }],
            ..Default::default()
        });
        assert_eq!(total.lsp.len(), 1);
        // a reading that says nothing about servers leaves the last one
        total.add(&agentbench_core::Usage { context: 5, ..Default::default() });
        assert_eq!(total.lsp.len(), 1);
        let back: agentbench_core::Usage =
            serde_json::from_str(&serde_json::to_string(&total).unwrap()).unwrap();
        assert_eq!(back.lsp[0].health, LspHealth::Ready);
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
            effort: None,
            issue_id: None,
            status,
            agent_session: None,
            usage: Default::default(),
            created_at: 0,
            updated_at: 0,
            queued: Vec::new(),
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
        assert_eq!(sizes, vec![1, 2, 2, 4, 6]);
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
        app.add_speech(" hello there ", true);
        assert_eq!(app.draft_of(b), "hello there");
        assert_eq!(app.draft_of(a), "first");
        assert_eq!(app.refocus, Some(Tile::Chat(b)));
    }

    #[test]
    fn stopping_puts_the_queued_messages_back_in_the_draft() {
        let mut app = App::new(&egui::Context::default(), None, None);
        let a = "00000000-0000-0000-0000-000000000001".parse::<SessionId>().unwrap();
        app.panes.entry(a).or_default().draft = "half typed".into();
        app.handle(Reply::Interrupted(a, vec!["first".into(), "second".into()]));
        assert_eq!(app.draft_of(a), "first\nsecond\nhalf typed");
        assert_eq!(app.refocus, Some(Tile::Chat(a)));
    }

    #[test]
    fn a_job_tab_can_be_clicked() {
        let ctx = egui::Context::default();
        let rect = std::cell::Cell::new(egui::Rect::NOTHING);
        let clicked = std::cell::Cell::new(0);
        let frame = |ctx: &egui::Context, events: Vec<egui::Event>| {
            let input = egui::RawInput { events, ..Default::default() };
            ctx.run_ui(input, |ui| {
                let r = widgets::tab_chip(ui, "job-2 running", OK, false);
                rect.set(r.rect);
                clicked.set(clicked.get() + r.clicked() as u32);
            });
        };
        frame(&ctx, vec![]);
        let at = rect.get().center();
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(&ctx, vec![egui::Event::PointerMoved(at)]);
        frame(&ctx, vec![button(true)]);
        frame(&ctx, vec![button(false)]);
        assert_eq!(clicked.get(), 1);
    }

    #[test]
    fn terminal_keys_become_what_a_shell_expects() {
        use egui::{Event, Key, Modifiers};
        let key = |key, modifiers| Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        assert_eq!(
            terminal_bytes(&[Event::Text("ls".into()), key(Key::Enter, Modifiers::NONE)]),
            "ls\r"
        );
        assert_eq!(terminal_bytes(&[Event::Paste("a\nb".into())]), "a\rb");
        assert_eq!(terminal_bytes(&[key(Key::ArrowUp, Modifiers::NONE)]), "\x1b[A");
        assert_eq!(terminal_bytes(&[key(Key::Escape, Modifiers::NONE)]), "\x1b");
        assert_eq!(terminal_bytes(&[key(Key::D, Modifiers::CTRL)]), "\x04");
        assert_eq!(terminal_bytes(&[Event::Copy]), "\x03");
    }

    #[test]
    fn job_text_loses_colour_codes_and_carriage_returns() {
        assert_eq!(
            strip_ansi("\x1b[32mready\x1b[0m in 3ms\r\nnext\rtick"),
            "ready in 3ms\nnext\ntick"
        );
        assert_eq!(strip_ansi("\x1b]0;title\x07plain"), "plain");
    }

    #[test]
    fn the_terminal_keeps_the_jobs_of_its_own_chat_only() {
        let mut app = App::new(&egui::Context::default(), None, None);
        let a = "00000000-0000-0000-0000-000000000001".parse::<SessionId>().unwrap();
        let b = "00000000-0000-0000-0000-000000000002".parse::<SessionId>().unwrap();
        let job =
            agentbench_core::Job { handle: "job-1".into(), running: true, ..Default::default() };
        app.handle(Reply::Jobs(a, vec![job.clone()]));
        assert!(app.terminal.is_none());
        app.terminal = Some(TerminalView::new(a));
        app.handle(Reply::Jobs(b, vec![job.clone()]));
        assert!(app.terminal.as_ref().unwrap().jobs.is_empty());
        app.handle(Reply::Jobs(a, vec![job]));
        assert_eq!(app.terminal.as_ref().unwrap().jobs.len(), 1);
    }

    #[test]
    fn each_reading_of_a_take_replaces_the_one_before() {
        let mut app = App::new(&egui::Context::default(), None, None);
        let a = "00000000-0000-0000-0000-000000000001".parse::<SessionId>().unwrap();
        app.panes.entry(a).or_default().draft = "fix".into();
        app.speaking_in = Some(Tile::Chat(a));
        app.add_speech("the bug", false);
        assert_eq!(app.draft_of(a), "fix the bug");
        app.add_speech("the bug in the parser", false);
        assert_eq!(app.draft_of(a), "fix the bug in the parser");
        app.add_speech("the bug in the parser please", true);
        assert_eq!(app.draft_of(a), "fix the bug in the parser please");
        app.speaking_in = Some(Tile::Chat(a));
        app.add_speech("again", false);
        app.drop_speech();
        assert_eq!(app.draft_of(a), "fix the bug in the parser please");
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
