use super::editor::Editor;
use agentbench_core::{
    AgentInfo, Entry, Id, Message, Project, Question, SLASH, Session, SessionId, SessionStatus,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};

pub const FLASH_FOR: Duration = Duration::from_secs(6);
pub const QUIT_WITHIN: Duration = Duration::from_secs(2);
const WHEEL: usize = 3;

pub struct Command {
    pub name: &'static str,
    pub args: &'static str,
    pub about: &'static str,
}

const CLIENT: [Command; 4] = [
    Command { name: "resume", args: "", about: "pick an earlier chat to carry on" },
    Command { name: "agent", args: "<name>", about: "use another agent for the next new chat" },
    Command { name: "help", args: "", about: "keys and commands" },
    Command { name: "quit", args: "", about: "leave; the agent keeps working" },
];

pub fn commands() -> Vec<Command> {
    let mut all: Vec<Command> =
        SLASH.iter().map(|s| Command { name: s.name, args: s.args, about: s.about }).collect();
    all.extend(CLIENT);
    all
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    Info,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    Quit,
    Send(String),
    Answer { question: String, answers: Vec<String> },
    Interrupt,
    Open(SessionId),
    Delete(SessionId),
    Sessions,
    Refresh,
}

pub enum Reply {
    Opened(Session, Vec<Message>),
    Refreshed(Session, Vec<Message>),
    Created(Session),
    Interrupted(SessionId, Vec<String>),
    Sessions(Vec<Session>),
    Changed(Result<Id, String>),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Overlay {
    None,
    Sessions { items: Vec<Session>, at: usize },
    Help,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pick {
    pub question_id: String,
    pub index: usize,
    pub at: usize,
    pub checked: Vec<bool>,
    pub answers: Vec<String>,
    pub typing: bool,
}

pub struct Pending {
    pub id: String,
    pub questions: Vec<Question>,
}

pub struct App {
    pub project: Project,
    pub agents: Vec<AgentInfo>,
    pub agent: String,
    pub model: Option<String>,
    pub session: Option<Session>,
    pub messages: Vec<Message>,
    pub editor: Editor,
    pub history: Vec<String>,
    history_at: Option<usize>,
    draft: String,
    pub scroll: usize,
    pub expanded: bool,
    pub overlay: Overlay,
    pub pick: Pick,
    pub menu_at: usize,
    pub flash: Option<(Tone, String, Instant)>,
    pub online: bool,
    pub frame: usize,
    pub quit_armed: Option<Instant>,
    pub dirty: bool,
    pub lines_stale: bool,
    pub opening: bool,
}

impl App {
    pub fn new(project: Project, agents: Vec<AgentInfo>, agent: String) -> App {
        App {
            project,
            agents,
            agent,
            model: None,
            session: None,
            messages: Vec::new(),
            editor: Editor::with_width(80),
            history: Vec::new(),
            history_at: None,
            draft: String::new(),
            scroll: 0,
            expanded: false,
            overlay: Overlay::None,
            pick: Pick::default(),
            menu_at: 0,
            flash: None,
            online: true,
            frame: 0,
            quit_armed: None,
            dirty: true,
            lines_stale: true,
            opening: false,
        }
    }

    pub fn busy(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.status.busy())
    }

    pub fn say(&mut self, tone: Tone, text: impl Into<String>) {
        self.flash = Some((tone, text.into(), Instant::now()));
        self.dirty = true;
    }

    pub fn tick(&mut self, now: Instant) {
        if self.busy() {
            self.frame = self.frame.wrapping_add(1);
            self.dirty = true;
        }
        if self.flash.as_ref().is_some_and(|(_, _, at)| now.duration_since(*at) > FLASH_FOR) {
            self.flash = None;
            self.dirty = true;
        }
        if self.quit_armed.is_some_and(|at| now.duration_since(at) > QUIT_WITHIN) {
            self.quit_armed = None;
            self.dirty = true;
        }
    }

    pub fn new_chat(&mut self) {
        self.session = None;
        self.messages.clear();
        self.scroll = 0;
        self.pick = Pick::default();
        self.model = None;
        self.lines_stale = true;
        self.dirty = true;
    }

    pub fn open(&mut self, session: Session, messages: Vec<Message>) {
        self.agent = session.agent.clone();
        self.session = Some(session);
        self.messages = messages;
        self.scroll = 0;
        self.pick = Pick::default();
        self.lines_stale = true;
        self.dirty = true;
        for m in &agentbench_core::shown(&self.messages) {
            if let Entry::User { text, .. } = &m.entry
                && self.history.last() != Some(text)
            {
                self.history.push(text.clone());
            }
        }
    }

    pub fn merge(&mut self, session: Session, incoming: Vec<Message>) {
        if self.session.as_ref().map(|s| s.id) != Some(session.id) {
            return;
        }
        self.session = Some(session);
        for m in incoming {
            match self.messages.iter_mut().rev().find(|x| x.id == m.id) {
                Some(x) => *x = m,
                None => self.messages.push(m),
            }
        }
        self.messages.sort_by_key(|m| m.id);
        self.lines_stale = true;
        self.dirty = true;
    }

    pub fn refetch_after(&self) -> Id {
        let tail = self.messages.iter().rev().nth(3).map(|m| m.id).unwrap_or(0);
        let open = self
            .messages
            .iter()
            .filter(|m| match &m.entry {
                Entry::Tool { output, .. } => output.is_none(),
                Entry::Question { answer, .. } => answer.is_none(),
                Entry::Questions { answers, .. } => answers.is_none(),
                Entry::Wire { messages } => {
                    messages.iter().any(|s| s.message["role"] == "assistant")
                }
                _ => false,
            })
            .map(|m| m.id)
            .min();
        open.map_or(tail, |o| o.min(tail)).saturating_sub(1)
    }

    pub fn reply(&mut self, reply: Reply) -> Option<Cmd> {
        self.dirty = true;
        match reply {
            Reply::Opened(session, messages) => {
                self.opening = false;
                self.overlay = Overlay::None;
                self.open(session, messages);
            }
            Reply::Refreshed(session, messages) => self.merge(session, messages),
            Reply::Created(session) => {
                self.session = Some(session);
                self.model = None;
                self.lines_stale = true;
                return Some(Cmd::Refresh);
            }
            Reply::Interrupted(id, queued) => {
                if self.session.as_ref().is_some_and(|s| s.id == id) && !queued.is_empty() {
                    let text = agentbench_core::restore_queued(self.editor.text(), &queued);
                    self.editor.set(&text);
                    self.history_at = None;
                }
            }
            Reply::Sessions(all) => {
                let mut items: Vec<Session> =
                    all.into_iter().filter(|s| s.project_id == self.project.id).collect();
                items.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
                if matches!(self.overlay, Overlay::Sessions { .. }) {
                    self.overlay = Overlay::Sessions { items, at: 0 };
                }
            }
            Reply::Changed(Ok(_)) => self.online = true,
            Reply::Changed(Err(_)) => self.online = false,
            Reply::Failed(e) => {
                self.opening = false;
                self.say(Tone::Error, e);
            }
        }
        None
    }

    pub fn pending(&self) -> Option<Pending> {
        if !self.session.as_ref().is_some_and(|s| s.status == SessionStatus::Waiting) {
            return None;
        }
        self.messages.iter().rev().find_map(|m| match &m.entry {
            Entry::Question { id, question, answer: None } => {
                Some(Pending { id: id.clone(), questions: vec![question.clone()] })
            }
            Entry::Questions { id, questions, answers: None } => {
                Some(Pending { id: id.clone(), questions: questions.clone() })
            }
            _ => None,
        })
    }

    fn sync_pick(&mut self, id: &str) {
        if self.pick.question_id != id {
            self.pick = Pick { question_id: id.to_string(), ..Pick::default() };
        }
    }

    pub fn choosing(&self) -> Option<(Pending, Pick)> {
        let pending = self.pending()?;
        let mut pick = self.pick.clone();
        if pick.question_id != pending.id {
            pick = Pick { question_id: pending.id.clone(), ..Pick::default() };
        }
        let q = pending.questions.get(pick.index)?;
        if q.options.is_empty() || pick.typing {
            return None;
        }
        Some((pending, pick))
    }

    pub fn slash_menu(&self) -> Vec<Command> {
        let text = self.editor.text();
        if !text.starts_with('/') || text.contains(char::is_whitespace) {
            return Vec::new();
        }
        let typed = &text[1..];
        commands().into_iter().filter(|c| c.name.starts_with(typed)).collect()
    }

    pub fn mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollUp => self.scroll_by(WHEEL as isize),
            MouseEventKind::ScrollDown => self.scroll_by(-(WHEEL as isize)),
            _ => {}
        }
    }

    pub fn scroll_by(&mut self, by: isize) {
        self.scroll = self.scroll.saturating_add_signed(by);
        self.dirty = true;
    }

    pub fn paste(&mut self, text: &str) {
        if self.choosing().is_some() {
            return;
        }
        self.editor.insert_str(text);
        self.dirty = true;
    }

    pub fn key(&mut self, k: KeyEvent, page: usize) -> Option<Cmd> {
        if k.kind == KeyEventKind::Release {
            return None;
        }
        self.dirty = true;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        if k.code != KeyCode::Char('c') || !ctrl {
            self.quit_armed = None;
        }
        if self.overlay != Overlay::None {
            return self.overlay_key(k);
        }
        match (k.code, ctrl) {
            (KeyCode::Char('c'), true) => return self.interrupt_or_quit(),
            (KeyCode::Char('d'), true) if self.editor.is_empty() => return Some(Cmd::Quit),
            (KeyCode::Char('o'), true) => {
                self.expanded = !self.expanded;
                self.lines_stale = true;
                return None;
            }
            (KeyCode::PageUp, _) => {
                self.scroll_by(page as isize);
                return None;
            }
            (KeyCode::PageDown, _) => {
                self.scroll_by(-(page as isize));
                return None;
            }
            (KeyCode::End, true) => {
                self.scroll = 0;
                return None;
            }
            (KeyCode::Home, true) => {
                self.scroll = usize::MAX / 2;
                return None;
            }
            _ => {}
        }
        if k.code == KeyCode::Esc {
            return self.escape();
        }
        if let Some((pending, pick)) = self.choosing() {
            return self.choose(pending, pick, k);
        }
        let menu = self.slash_menu();
        if !menu.is_empty() {
            match k.code {
                KeyCode::Up => {
                    self.menu_at = self.menu_at.min(menu.len() - 1).saturating_sub(1);
                    return None;
                }
                KeyCode::Down => {
                    self.menu_at = (self.menu_at + 1).min(menu.len() - 1);
                    return None;
                }
                KeyCode::Tab => {
                    self.complete(&menu);
                    return None;
                }
                KeyCode::Enter if !shift && !alt => {
                    let at = self.menu_at.min(menu.len() - 1);
                    let typed = self.editor.text()[1..].to_string();
                    if menu[at].name != typed {
                        self.complete(&menu);
                        return None;
                    }
                }
                _ => {}
            }
        }
        match k.code {
            KeyCode::Enter if shift || alt => self.editor.insert('\n'),
            KeyCode::Enter => {
                if self.editor.text().ends_with('\\') {
                    self.editor.backspace();
                    self.editor.insert('\n');
                } else {
                    return self.submit();
                }
            }
            KeyCode::Char('j') if ctrl => self.editor.insert('\n'),
            KeyCode::Char('a') if ctrl => self.editor.line_start(),
            KeyCode::Char('e') if ctrl => self.editor.line_end(),
            KeyCode::Char('b') if ctrl => self.editor.left(),
            KeyCode::Char('f') if ctrl => self.editor.right(),
            KeyCode::Char('k') if ctrl => self.editor.kill_to_end(),
            KeyCode::Char('u') if ctrl => self.editor.kill_to_start(),
            KeyCode::Char('w') if ctrl => self.editor.delete_word_back(),
            KeyCode::Char('b') if alt => self.editor.word_left(),
            KeyCode::Char('f') if alt => self.editor.word_right(),
            KeyCode::Backspace if alt || ctrl => self.editor.delete_word_back(),
            KeyCode::Left if alt || ctrl => self.editor.word_left(),
            KeyCode::Right if alt || ctrl => self.editor.word_right(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Home => self.editor.line_start(),
            KeyCode::End => self.editor.line_end(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Up => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Tab => self.editor.insert_str("    "),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.editor.insert(c);
                self.menu_at = 0;
                self.history_at = None;
            }
            _ => {}
        }
        None
    }

    fn complete(&mut self, menu: &[Command]) {
        let at = self.menu_at.min(menu.len() - 1);
        let c = &menu[at];
        let tail = if c.args.is_empty() { "" } else { " " };
        self.editor.set(&format!("/{}{tail}", c.name));
        self.menu_at = 0;
    }

    fn interrupt_or_quit(&mut self) -> Option<Cmd> {
        if !self.editor.is_empty() {
            self.editor.clear();
            self.history_at = None;
            return None;
        }
        if self.busy() {
            return Some(Cmd::Interrupt);
        }
        if self.quit_armed.is_some() {
            return Some(Cmd::Quit);
        }
        self.quit_armed = Some(Instant::now());
        self.say(Tone::Info, "press ctrl+c again to leave");
        None
    }

    fn escape(&mut self) -> Option<Cmd> {
        if self.pick.typing {
            self.pick.typing = false;
            return None;
        }
        if !self.slash_menu().is_empty() {
            self.editor.clear();
            return None;
        }
        if self.busy() {
            return Some(Cmd::Interrupt);
        }
        if self.scroll > 0 {
            self.scroll = 0;
        }
        None
    }

    fn up(&mut self) {
        if self.editor.up() {
            return;
        }
        let next = match self.history_at {
            None if self.history.is_empty() => return,
            None => {
                self.draft = self.editor.text().to_string();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_at = Some(next);
        let text = self.history[next].clone();
        self.editor.set(&text);
    }

    fn down(&mut self) {
        if self.editor.down() {
            return;
        }
        match self.history_at {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.history_at = Some(i + 1);
                let text = self.history[i + 1].clone();
                self.editor.set(&text);
            }
            Some(_) => {
                self.history_at = None;
                let draft = std::mem::take(&mut self.draft);
                self.editor.set(&draft);
            }
        }
    }

    fn overlay_key(&mut self, k: KeyEvent) -> Option<Cmd> {
        match &mut self.overlay {
            Overlay::Help => {
                self.overlay = Overlay::None;
                None
            }
            Overlay::Sessions { items, at } => match k.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.overlay = Overlay::None;
                    None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    *at = at.saturating_sub(1);
                    None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    *at = (*at + 1).min(items.len().saturating_sub(1));
                    None
                }
                KeyCode::Enter => {
                    let id = items.get(*at).map(|s| s.id)?;
                    self.opening = true;
                    Some(Cmd::Open(id))
                }
                _ => None,
            },
            Overlay::None => None,
        }
    }

    fn choose(&mut self, pending: Pending, mut pick: Pick, k: KeyEvent) -> Option<Cmd> {
        let q = pending.questions[pick.index].clone();
        let rows = q.options.len() + 1;
        if pick.checked.len() != q.options.len() {
            pick.checked = vec![false; q.options.len()];
        }
        let mut done: Option<String> = None;
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => pick.at = pick.at.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => pick.at = (pick.at + 1).min(rows - 1),
            KeyCode::Char(' ') if q.multiple && pick.at < q.options.len() => {
                pick.checked[pick.at] = !pick.checked[pick.at];
            }
            KeyCode::Char('o') | KeyCode::Tab => pick.typing = true,
            KeyCode::Char(c @ '1'..='9') => {
                let n = c as usize - '1' as usize;
                if n < q.options.len() {
                    pick.at = n;
                    if q.multiple {
                        pick.checked[n] = !pick.checked[n];
                    } else {
                        done = Some(q.options[n].clone());
                    }
                }
            }
            KeyCode::Enter => {
                if pick.at == q.options.len() {
                    pick.typing = true;
                } else if q.multiple {
                    let mut chosen: Vec<&str> = q
                        .options
                        .iter()
                        .zip(&pick.checked)
                        .filter(|(_, on)| **on)
                        .map(|(o, _)| o.as_str())
                        .collect();
                    if chosen.is_empty() {
                        chosen.push(&q.options[pick.at]);
                    }
                    done = Some(chosen.join(", "));
                } else {
                    done = Some(q.options[pick.at].clone());
                }
            }
            _ => {}
        }
        self.pick = pick;
        self.sync_pick(&pending.id);
        done.and_then(|answer| self.record(pending, answer))
    }

    fn record(&mut self, pending: Pending, answer: String) -> Option<Cmd> {
        self.sync_pick(&pending.id);
        self.pick.answers.push(answer);
        self.pick.index += 1;
        self.pick.at = 0;
        self.pick.typing = false;
        self.pick.checked.clear();
        if self.pick.index < pending.questions.len() {
            return None;
        }
        let answers = std::mem::take(&mut self.pick.answers);
        self.pick.index = 0;
        Some(Cmd::Answer { question: pending.id, answers })
    }

    fn submit(&mut self) -> Option<Cmd> {
        let text = self.editor.text().trim().to_string();
        if text.is_empty() {
            return None;
        }
        if let Some(pending) = self.pending() {
            self.sync_pick(&pending.id);
            let q = &pending.questions[self.pick.index.min(pending.questions.len() - 1)];
            if q.options.is_empty() || self.pick.typing {
                self.editor.clear();
                return self.record(pending, text);
            }
        }
        self.editor.clear();
        self.history_at = None;
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.scroll = 0;
        if let Some(rest) = text.strip_prefix('/') {
            let (name, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if let Some(cmd) = self.slash(name, arg.trim(), &text) {
                return cmd;
            }
        }
        Some(Cmd::Send(text))
    }

    fn slash(&mut self, name: &str, arg: &str, text: &str) -> Option<Option<Cmd>> {
        if !commands().iter().any(|c| c.name == name) {
            return None;
        }
        Some(match name {
            "quit" => Some(Cmd::Quit),
            "help" => {
                self.overlay = Overlay::Help;
                None
            }
            "new" => {
                self.new_chat();
                self.say(Tone::Info, "new chat: the first message starts it");
                None
            }
            "resume" => {
                self.overlay = Overlay::Sessions { items: Vec::new(), at: 0 };
                Some(Cmd::Sessions)
            }
            "close" => match self.session.take() {
                Some(s) => {
                    self.new_chat();
                    Some(Cmd::Delete(s.id))
                }
                None => None,
            },
            "agent" => {
                self.pick_agent(arg);
                None
            }
            "model" if self.session.is_none() => {
                self.model = Some(arg.to_string()).filter(|m| !m.is_empty());
                let shown = self.model.clone().unwrap_or_else(|| "the default model".into());
                self.say(Tone::Info, format!("the next chat uses {shown}"));
                None
            }
            _ if self.session.is_none() => {
                self.say(Tone::Error, format!("/{name} needs a chat, send a message first"));
                None
            }
            "stop" => Some(Cmd::Interrupt),
            _ => Some(Cmd::Send(text.to_string())),
        })
    }

    fn pick_agent(&mut self, name: &str) {
        let ready: Vec<&AgentInfo> = self.agents.iter().filter(|a| a.available).collect();
        if name.is_empty() {
            let names: Vec<&str> = ready.iter().map(|a| a.name.as_str()).collect();
            self.say(Tone::Info, format!("agents: {} (now {})", names.join(", "), self.agent));
            return;
        }
        match ready.iter().find(|a| a.name == name) {
            Some(a) => {
                self.agent = a.name.clone();
                let label = a.label.clone();
                self.say(Tone::Info, format!("the next new chat uses {label}"));
            }
            None => self.say(Tone::Error, format!("no ready agent named {name}")),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use agentbench_core::Usage;
    use crossterm::event::KeyEventState;

    pub fn project() -> Project {
        Project {
            id: 1,
            name: "chats".into(),
            path: "/home/me".into(),
            branch: None,
            remote: None,
            created_at: 0,
            home: true,
        }
    }

    pub fn agent(name: &str, available: bool) -> AgentInfo {
        AgentInfo {
            name: name.into(),
            label: name.to_uppercase(),
            available,
            resumable: true,
            about: String::new(),
        }
    }

    pub fn session(status: SessionStatus) -> Session {
        Session {
            id: SessionId(uuid::Uuid::from_u128(7)),
            project_id: 1,
            worktree_id: 1,
            agent: "native".into(),
            title: "a chat".into(),
            cwd: "/home/me".into(),
            model: None,
            effort: None,
            issue_id: None,
            status,
            agent_session: None,
            usage: Usage::default(),
            created_at: 0,
            updated_at: 0,
            queued: Vec::new(),
        }
    }

    pub fn msg(id: Id, entry: Entry) -> Message {
        Message { id, session_id: SessionId(uuid::Uuid::from_u128(7)), entry, created_at: 0 }
    }

    pub fn app() -> App {
        App::new(
            project(),
            vec![agent("native", true), agent("claude", true), agent("pi", false)],
            "native".into(),
        )
    }

    pub fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    pub fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent { modifiers, ..press(code) }
    }

    pub fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.key(press(KeyCode::Char(c)), 10);
        }
    }

    pub fn ask(options: &[&str], multiple: bool) -> Question {
        Question {
            text: "which?".into(),
            options: options.iter().map(|s| s.to_string()).collect(),
            multiple,
        }
    }

    fn waiting(app: &mut App, entry: Entry) {
        app.open(session(SessionStatus::Waiting), vec![msg(1, entry)]);
    }

    #[test]
    fn enter_sends_the_text_and_clears_the_composer() {
        let mut a = app();
        type_text(&mut a, "hello there");
        assert_eq!(a.key(press(KeyCode::Enter), 10), Some(Cmd::Send("hello there".into())));
        assert!(a.editor.is_empty());
    }

    #[test]
    fn an_empty_composer_sends_nothing() {
        let mut a = app();
        type_text(&mut a, "   ");
        assert_eq!(a.key(press(KeyCode::Enter), 10), None);
    }

    #[test]
    fn alt_enter_ctrl_j_and_a_trailing_backslash_make_a_newline() {
        let mut a = app();
        type_text(&mut a, "a");
        a.key(with(KeyCode::Enter, KeyModifiers::ALT), 10);
        type_text(&mut a, "b");
        a.key(with(KeyCode::Char('j'), KeyModifiers::CONTROL), 10);
        type_text(&mut a, "c\\");
        assert_eq!(a.key(press(KeyCode::Enter), 10), None);
        type_text(&mut a, "d");
        assert_eq!(a.editor.text(), "a\nb\nc\nd");
        assert_eq!(a.key(press(KeyCode::Enter), 10), Some(Cmd::Send("a\nb\nc\nd".into())));
    }

    #[test]
    fn up_recalls_what_was_sent_and_down_restores_the_draft() {
        let mut a = app();
        type_text(&mut a, "first");
        a.key(press(KeyCode::Enter), 10);
        type_text(&mut a, "second");
        a.key(press(KeyCode::Enter), 10);
        type_text(&mut a, "draft");
        a.key(press(KeyCode::Up), 10);
        assert_eq!(a.editor.text(), "second");
        a.key(press(KeyCode::Up), 10);
        assert_eq!(a.editor.text(), "first");
        a.key(press(KeyCode::Up), 10);
        assert_eq!(a.editor.text(), "first");
        a.key(press(KeyCode::Down), 10);
        a.key(press(KeyCode::Down), 10);
        assert_eq!(a.editor.text(), "draft");
    }

    #[test]
    fn ctrl_c_clears_then_stops_then_leaves_on_the_second_press() {
        let mut a = app();
        let c = || with(KeyCode::Char('c'), KeyModifiers::CONTROL);
        type_text(&mut a, "oops");
        assert_eq!(a.key(c(), 10), None);
        assert!(a.editor.is_empty());
        assert_eq!(a.key(c(), 10), None);
        assert_eq!(a.key(c(), 10), Some(Cmd::Quit));
        a.open(session(SessionStatus::Running), vec![]);
        assert_eq!(a.key(c(), 10), Some(Cmd::Interrupt));
    }

    #[test]
    fn another_key_disarms_the_second_ctrl_c() {
        let mut a = app();
        let c = || with(KeyCode::Char('c'), KeyModifiers::CONTROL);
        a.key(c(), 10);
        a.key(press(KeyCode::Char('x')), 10);
        a.editor.clear();
        assert_eq!(a.key(c(), 10), None);
    }

    #[test]
    fn escape_interrupts_a_busy_agent_only() {
        let mut a = app();
        assert_eq!(a.key(press(KeyCode::Esc), 10), None);
        a.open(session(SessionStatus::Running), vec![]);
        assert_eq!(a.key(press(KeyCode::Esc), 10), Some(Cmd::Interrupt));
    }

    #[test]
    fn stopping_puts_the_queued_messages_back_in_the_composer() {
        let mut a = app();
        let s = session(SessionStatus::Running);
        let id = s.id;
        a.open(s, vec![]);
        type_text(&mut a, "half");
        a.reply(Reply::Interrupted(id, vec!["first".into(), "second".into()]));
        assert_eq!(a.editor.text(), "first\nsecond\nhalf");
        a.reply(Reply::Interrupted(SessionId(uuid::Uuid::from_u128(9)), vec!["other".into()]));
        assert_eq!(a.editor.text(), "first\nsecond\nhalf");
    }

    #[test]
    fn ctrl_d_leaves_only_from_an_empty_composer() {
        let mut a = app();
        type_text(&mut a, "x");
        a.key(with(KeyCode::Char('d'), KeyModifiers::CONTROL), 10);
        assert_eq!(a.editor.text(), "x");
        a.editor.clear();
        assert_eq!(a.key(with(KeyCode::Char('d'), KeyModifiers::CONTROL), 10), Some(Cmd::Quit));
    }

    #[test]
    fn paging_scrolls_and_end_returns_to_the_bottom() {
        let mut a = app();
        a.key(press(KeyCode::PageUp), 12);
        assert_eq!(a.scroll, 12);
        a.key(press(KeyCode::PageDown), 5);
        assert_eq!(a.scroll, 7);
        a.key(press(KeyCode::PageDown), 50);
        assert_eq!(a.scroll, 0);
        a.key(press(KeyCode::PageUp), 12);
        a.key(with(KeyCode::End, KeyModifiers::CONTROL), 12);
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn the_slash_menu_filters_and_tab_completes() {
        let mut a = app();
        type_text(&mut a, "/mo");
        let names: Vec<&str> = a.slash_menu().iter().map(|c| c.name).collect();
        assert_eq!(names, ["model"]);
        a.key(press(KeyCode::Tab), 10);
        assert_eq!(a.editor.text(), "/model ");
        assert!(a.slash_menu().is_empty());
    }

    #[test]
    fn enter_on_a_partial_command_completes_instead_of_sending() {
        let mut a = app();
        type_text(&mut a, "/he");
        assert_eq!(a.key(press(KeyCode::Enter), 10), None);
        assert_eq!(a.editor.text(), "/help");
        assert_eq!(a.key(press(KeyCode::Enter), 10), None);
        assert_eq!(a.overlay, Overlay::Help);
    }

    #[test]
    fn server_commands_go_to_the_server_once_there_is_a_chat() {
        let mut a = app();
        type_text(&mut a, "/compact");
        assert_eq!(a.key(press(KeyCode::Enter), 10), None);
        assert!(matches!(a.flash, Some((Tone::Error, _, _))));
        a.open(session(SessionStatus::Idle), vec![]);
        type_text(&mut a, "/compact");
        assert_eq!(a.key(press(KeyCode::Enter), 10), Some(Cmd::Send("/compact".into())));
    }

    #[test]
    fn new_forgets_the_chat_and_close_deletes_it() {
        let mut a = app();
        let s = session(SessionStatus::Idle);
        a.open(s.clone(), vec![msg(1, Entry::user("hi"))]);
        type_text(&mut a, "/new");
        a.key(press(KeyCode::Enter), 10);
        assert!(a.session.is_none() && a.messages.is_empty());
        a.open(s.clone(), vec![]);
        type_text(&mut a, "/close");
        assert_eq!(a.key(press(KeyCode::Enter), 10), Some(Cmd::Delete(s.id)));
        assert!(a.session.is_none());
    }

    #[test]
    fn model_before_a_chat_is_kept_for_the_chat() {
        let mut a = app();
        type_text(&mut a, "/model high");
        assert_eq!(a.key(press(KeyCode::Enter), 10), None);
        assert_eq!(a.model.as_deref(), Some("high"));
        a.new_chat();
        assert_eq!(a.model, None);
    }

    #[test]
    fn agent_switches_to_a_ready_agent_only() {
        let mut a = app();
        type_text(&mut a, "/agent claude");
        a.key(press(KeyCode::Enter), 10);
        assert_eq!(a.agent, "claude");
        type_text(&mut a, "/agent pi");
        a.key(press(KeyCode::Enter), 10);
        assert_eq!(a.agent, "claude");
        assert!(matches!(a.flash, Some((Tone::Error, _, _))));
    }

    #[test]
    fn resume_asks_for_the_sessions_and_enter_opens_the_one_picked() {
        let mut a = app();
        type_text(&mut a, "/resume");
        assert_eq!(a.key(press(KeyCode::Enter), 10), Some(Cmd::Sessions));
        let mut old = session(SessionStatus::Idle);
        old.id = SessionId(uuid::Uuid::from_u128(1));
        old.updated_at = 5;
        let mut new = session(SessionStatus::Idle);
        new.updated_at = 9;
        let mut other = session(SessionStatus::Idle);
        other.project_id = 99;
        a.reply(Reply::Sessions(vec![old.clone(), other, new.clone()]));
        let Overlay::Sessions { items, .. } = &a.overlay else { panic!("no picker") };
        assert_eq!(items.iter().map(|s| s.id).collect::<Vec<_>>(), [new.id, old.id]);
        a.key(press(KeyCode::Down), 10);
        assert_eq!(a.key(press(KeyCode::Enter), 10), Some(Cmd::Open(old.id)));
    }

    #[test]
    fn merging_replaces_changed_entries_and_appends_new_ones() {
        let mut a = app();
        let s = session(SessionStatus::Running);
        a.open(
            s.clone(),
            vec![msg(1, Entry::user("hi")), msg(2, Entry::Assistant { text: "he".into() })],
        );
        a.merge(
            s,
            vec![
                msg(2, Entry::Assistant { text: "hello".into() }),
                msg(3, Entry::Assistant { text: "more".into() }),
            ],
        );
        assert_eq!(a.messages.len(), 3);
        assert_eq!(a.messages[1].entry.text(), "hello");
    }

    #[test]
    fn a_stale_refresh_for_another_chat_is_ignored() {
        let mut a = app();
        a.open(session(SessionStatus::Idle), vec![]);
        let mut other = session(SessionStatus::Idle);
        other.id = SessionId(uuid::Uuid::from_u128(99));
        a.merge(other, vec![msg(1, Entry::user("x"))]);
        assert!(a.messages.is_empty());
    }

    #[test]
    fn refetch_reaches_back_to_the_oldest_open_tool_call() {
        let mut a = app();
        let tool = |output: Option<&str>| Entry::Tool {
            call_id: "c".into(),
            name: "bash".into(),
            input: serde_json::json!({}),
            output: output.map(str::to_string),
            error: false,
        };
        let mut msgs = vec![msg(1, tool(None))];
        for i in 2..=9 {
            msgs.push(msg(i, Entry::Assistant { text: "x".into() }));
        }
        a.open(session(SessionStatus::Running), msgs);
        assert_eq!(a.refetch_after(), 0);
        a.messages[0].entry = tool(Some("done"));
        assert_eq!(a.refetch_after(), 5);
    }

    #[test]
    fn a_single_choice_question_answers_with_enter() {
        let mut a = app();
        let id = "q1".to_string();
        waiting(
            &mut a,
            Entry::Question {
                id: id.clone(),
                question: ask(&["red", "blue"], false),
                answer: None,
            },
        );
        a.key(press(KeyCode::Down), 10);
        assert_eq!(
            a.key(press(KeyCode::Enter), 10),
            Some(Cmd::Answer { question: id, answers: vec!["blue".into()] })
        );
    }

    #[test]
    fn a_number_key_picks_an_option_at_once() {
        let mut a = app();
        waiting(
            &mut a,
            Entry::Question {
                id: "q".into(),
                question: ask(&["red", "blue"], false),
                answer: None,
            },
        );
        assert_eq!(
            a.key(press(KeyCode::Char('1')), 10),
            Some(Cmd::Answer { question: "q".into(), answers: vec!["red".into()] })
        );
    }

    #[test]
    fn a_multiple_choice_question_joins_the_ticked_options() {
        let mut a = app();
        waiting(
            &mut a,
            Entry::Question { id: "q".into(), question: ask(&["a", "b", "c"], true), answer: None },
        );
        a.key(press(KeyCode::Char(' ')), 10);
        a.key(press(KeyCode::Down), 10);
        a.key(press(KeyCode::Down), 10);
        a.key(press(KeyCode::Char(' ')), 10);
        assert_eq!(
            a.key(press(KeyCode::Enter), 10),
            Some(Cmd::Answer { question: "q".into(), answers: vec!["a, c".into()] })
        );
    }

    #[test]
    fn a_set_of_questions_is_answered_in_order_and_sent_together() {
        let mut a = app();
        let qs =
            vec![ask(&["x", "y"], false), Question { text: "name?".into(), ..Default::default() }];
        waiting(&mut a, Entry::Questions { id: "s".into(), questions: qs, answers: None });
        assert_eq!(a.key(press(KeyCode::Char('2')), 10), None);
        type_text(&mut a, "Bob");
        assert_eq!(
            a.key(press(KeyCode::Enter), 10),
            Some(Cmd::Answer { question: "s".into(), answers: vec!["y".into(), "Bob".into()] })
        );
    }

    #[test]
    fn other_lets_a_choice_question_take_typed_text() {
        let mut a = app();
        waiting(
            &mut a,
            Entry::Question { id: "q".into(), question: ask(&["a"], false), answer: None },
        );
        a.key(press(KeyCode::Char('o')), 10);
        assert!(a.choosing().is_none());
        type_text(&mut a, "none of those");
        assert_eq!(
            a.key(press(KeyCode::Enter), 10),
            Some(Cmd::Answer { question: "q".into(), answers: vec!["none of those".into()] })
        );
    }

    #[test]
    fn an_answered_or_idle_question_is_not_pending() {
        let mut a = app();
        a.open(
            session(SessionStatus::Waiting),
            vec![msg(
                1,
                Entry::Question {
                    id: "q".into(),
                    question: ask(&["a"], false),
                    answer: Some("a".into()),
                },
            )],
        );
        assert!(a.pending().is_none());
        a.open(
            session(SessionStatus::Idle),
            vec![msg(
                1,
                Entry::Question { id: "q".into(), question: ask(&["a"], false), answer: None },
            )],
        );
        assert!(a.pending().is_none());
    }

    #[test]
    fn replies_set_state_and_report_failures() {
        let mut a = app();
        a.reply(Reply::Failed("boom".into()));
        assert!(matches!(&a.flash, Some((Tone::Error, t, _)) if t == "boom"));
        a.reply(Reply::Changed(Err("down".into())));
        assert!(!a.online);
        a.reply(Reply::Changed(Ok(3)));
        assert!(a.online);
        assert_eq!(a.reply(Reply::Created(session(SessionStatus::Running))), Some(Cmd::Refresh));
        assert!(a.session.is_some());
    }

    #[test]
    fn flashes_and_the_quit_prompt_expire() {
        let mut a = app();
        a.say(Tone::Info, "hi");
        a.tick(Instant::now() + FLASH_FOR + Duration::from_secs(1));
        assert!(a.flash.is_none());
    }
}
