use super::screenshot;
use egui_bench::agent::grants::Access;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const COLS: usize = 120;
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const OUTPUT_CHARS: usize = 12_000;
const EVAL_CHARS: usize = 8_000;
const IDLE: Duration = Duration::from_secs(20 * 60);
const REPLY_WAIT: Duration = Duration::from_secs(30);
const LAUNCH_WAIT: Duration = Duration::from_secs(20);
const ROW_SLACK: f64 = 9.0;
const PARAGRAPH_GAP: f64 = 36.0;
const EXTRACT: &str = include_str!("browser/extract.js");
const TARGET: &str = include_str!("browser/target.js");
const SELECT: &str = include_str!("browser/select.js");
const FIND: &str = include_str!("browser/find.js");
const WATCH: &str = include_str!("browser/watch.js");
const QUIET: &str = include_str!("browser/quiet.js");
const QUIET_FOR: f64 = 400.0;
const SETTLE_MAX: Duration = Duration::from_secs(10);
const RESTLESS_MAX: Duration = Duration::from_secs(2);
const WAIT_DEFAULT: u64 = 10_000;
const WAIT_MAX: u64 = 60_000;
const DOWNLOAD_MAX: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize, PartialEq)]
pub struct Node {
    pub k: String,
    #[serde(default)]
    pub t: String,
    #[serde(default)]
    pub hint: String,
    #[serde(default)]
    pub lvl: u8,
    #[serde(default)]
    pub on: bool,
    #[serde(default)]
    pub off: bool,
    #[serde(default)]
    pub idx: Option<usize>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct Page {
    pub url: String,
    pub title: String,
    #[serde(rename = "scrollY")]
    pub scroll_y: f64,
    #[serde(rename = "scrollHeight")]
    pub scroll_height: f64,
    pub vw: f64,
    pub vh: f64,
    #[serde(default)]
    pub next: usize,
    pub nodes: Vec<Node>,
}

pub fn parse_page(json: &str) -> Result<Page, String> {
    serde_json::from_str(json).map_err(|e| format!("the page could not be read: {e}"))
}

fn cell(n: &Node) -> String {
    let off = if n.off { " (disabled)" } else { "" };
    let r = n.idx.map(|i| i.to_string()).unwrap_or_default();
    match n.k.as_str() {
        "link" => {
            let t = if n.t.is_empty() { "link" } else { &n.t };
            format!("[{r}]{t}")
        }
        "button" => {
            let t = if n.t.is_empty() { "button" } else { &n.t };
            format!("[{r} {t}{off}]")
        }
        "input" | "textarea" if n.t.is_empty() => format!("[{r}:___ {}{off}]", n.hint),
        "input" | "textarea" => format!("[{r}:{}{off}]", n.t),
        "checkbox" | "radio" => {
            let mark = match (n.k.as_str(), n.on) {
                ("checkbox", true) => "X",
                ("checkbox", false) => " ",
                (_, true) => "\u{25cf}",
                _ => "\u{25cb}",
            };
            let label = if n.t.is_empty() { String::new() } else { format!(" {}", n.t) };
            format!("[{r}:{mark}{off}]{label}")
        }
        "select" => format!("[{r}:\u{25bc} {}{off}]", n.t),
        "file" if n.t.is_empty() => format!("[{r}:file{off}]"),
        "file" => format!("[{r}:file {}{off}]", n.t),
        "heading" => format!("{} {}", "#".repeat(n.lvl.clamp(1, 6) as usize), n.t),
        "img" => format!("[img: {}]", n.t),
        "rule" => "---".to_string(),
        _ => n.t.clone(),
    }
}

fn centre(n: &Node) -> f64 {
    n.y + n.h.min(20.0) / 2.0
}

pub fn render(page: &Page, cols: usize) -> String {
    let mut nodes: Vec<&Node> = page.nodes.iter().collect();
    nodes.sort_by(|a, b| centre(a).total_cmp(&centre(b)).then(a.x.total_cmp(&b.x)));
    let scale = cols as f64 / page.vw.max(1.0);
    let mut rows: Vec<(f64, Vec<&Node>)> = Vec::new();
    for n in nodes {
        match rows.last_mut() {
            Some((y, row)) if centre(n) - *y <= ROW_SLACK => row.push(n),
            _ => rows.push((centre(n), vec![n])),
        }
    }
    let mut lines: Vec<String> = Vec::new();
    let mut last = None;
    for (y, mut row) in rows {
        row.sort_by(|a, b| a.x.total_cmp(&b.x));
        let mut line = String::new();
        let mut width = 0usize;
        for n in row {
            let text = cell(n);
            if text.is_empty() {
                continue;
            }
            let col = (n.x.max(0.0) * scale).round() as usize;
            if col > width {
                line.push_str(&" ".repeat(col - width));
                width = col;
            } else if width > 0 {
                line.push(' ');
                width += 1;
            }
            width += text.chars().count();
            line.push_str(&text);
        }
        if line.is_empty() {
            continue;
        }
        if last.is_some_and(|p| y - p > PARAGRAPH_GAP) {
            lines.push(String::new());
        }
        last = Some(y);
        lines.push(line);
    }
    lines.join("\n")
}

pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    let cut = cut.rsplit_once('\n').map_or(cut.as_str(), |(head, _)| head);
    let rest = text.chars().count() - cut.chars().count();
    format!(
        "{cut}\n[clipped {rest} more characters: scroll down, or use eval to query exactly what you need]"
    )
}

pub fn view(page: &Page, notes: &[String], cols: usize, max: usize) -> String {
    let mut out = String::new();
    for n in notes {
        out.push_str(&format!("[{n}]\n"));
    }
    out.push_str(&format!("url: {}\n", page.url));
    if !page.title.is_empty() {
        out.push_str(&format!("title: {}\n", page.title));
    }
    let end = (page.scroll_y + page.vh).min(page.scroll_height);
    out.push_str(&format!(
        "viewport: {}-{} of {}px{}\n\n",
        page.scroll_y.round(),
        end.round(),
        page.scroll_height.round(),
        if page.scroll_height - end > 20.0 { ", more below" } else { "" }
    ));
    let grid = render(page, cols);
    out.push_str(&clip(if grid.is_empty() { "(nothing visible)" } else { &grid }, max));
    out
}

#[derive(Debug, PartialEq)]
pub enum Incoming {
    Reply { id: u64, result: Result<Value, String> },
    Dialog { session: Option<String>, message: String },
    Event(Value),
    Detached(String),
    Other,
}

pub fn route(text: &str) -> Incoming {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return Incoming::Other };
    if let Some(id) = v.get("id").and_then(Value::as_u64) {
        let result = match v.get("error") {
            Some(e) => Err(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        };
        return Incoming::Reply { id, result };
    }
    if v.get("method").and_then(Value::as_str) == Some("Page.javascriptDialogOpening") {
        let p = &v["params"];
        let kind = p["type"].as_str().unwrap_or("dialog");
        let message = format!("{kind} dialog accepted: {}", p["message"].as_str().unwrap_or(""));
        let session = v.get("sessionId").and_then(Value::as_str).map(str::to_string);
        return Incoming::Dialog { session, message };
    }
    let method = v.get("method").and_then(Value::as_str).unwrap_or("");
    if method == "Target.detachedFromTarget"
        && let Some(s) = v["params"]["sessionId"].as_str()
    {
        return Incoming::Detached(s.to_string());
    }
    if method.starts_with("Browser.download") || method == "Page.fileChooserOpened" {
        return Incoming::Event(json!({ "method": method, "params": v["params"] }));
    }
    Incoming::Other
}

pub struct Key {
    pub key: String,
    pub code: String,
    pub vk: u32,
    pub text: Option<String>,
    pub modifiers: u32,
}

pub fn parse_key(spec: &str) -> Result<Key, String> {
    let mut parts: Vec<&str> = spec.split('+').collect();
    if spec.ends_with("++") {
        parts.truncate(parts.len().saturating_sub(2));
        parts.push("+");
    }
    let name = parts.pop().filter(|s| !s.is_empty()).ok_or("missing `key`")?;
    let mut modifiers = 0;
    for m in parts {
        modifiers |= match m.to_ascii_lowercase().as_str() {
            "alt" => 1,
            "ctrl" | "control" => 2,
            "meta" | "cmd" | "command" => 4,
            "shift" => 8,
            other => return Err(format!("unknown modifier {other}")),
        };
    }
    let named = |key: &str, code: &str, vk: u32, text: Option<&str>| Key {
        key: key.into(),
        code: code.into(),
        vk,
        text: text.map(str::to_string),
        modifiers,
    };
    Ok(match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => named("Enter", "Enter", 13, Some("\r")),
        "tab" => named("Tab", "Tab", 9, None),
        "escape" | "esc" => named("Escape", "Escape", 27, None),
        "backspace" => named("Backspace", "Backspace", 8, None),
        "delete" | "del" => named("Delete", "Delete", 46, None),
        "space" => named(" ", "Space", 32, Some(" ")),
        "arrowup" | "up" => named("ArrowUp", "ArrowUp", 38, None),
        "arrowdown" | "down" => named("ArrowDown", "ArrowDown", 40, None),
        "arrowleft" | "left" => named("ArrowLeft", "ArrowLeft", 37, None),
        "arrowright" | "right" => named("ArrowRight", "ArrowRight", 39, None),
        "pageup" => named("PageUp", "PageUp", 33, None),
        "pagedown" => named("PageDown", "PageDown", 34, None),
        "home" => named("Home", "Home", 36, None),
        "end" => named("End", "End", 35, None),
        _ if name.chars().count() == 1 => {
            let c = name.chars().next().unwrap_or(' ');
            let upper = c.to_ascii_uppercase();
            let code = if c.is_ascii_alphabetic() {
                format!("Key{upper}")
            } else if c.is_ascii_digit() {
                format!("Digit{c}")
            } else {
                String::new()
            };
            let text = (modifiers & 6 == 0).then(|| c.to_string());
            Key { key: c.to_string(), code, vk: upper as u32, text, modifiers }
        }
        _ => return Err(format!("unknown key {name}")),
    })
}

pub fn address(
    ws: &egui_bench::agent::workspace::Workspace,
    target: &str,
) -> Result<String, String> {
    let t = target.trim();
    if t == "about:blank" {
        return Ok(t.to_string());
    }
    let bare = !t.contains("://")
        && !t.contains(char::is_whitespace)
        && !t.starts_with('/')
        && !t.starts_with('.')
        && t.split('/').next().is_some_and(|h| h.contains('.') && !h.ends_with(".html"))
        && !std::path::Path::new(t).exists();
    if bare {
        return Ok(format!("https://{t}"));
    }
    screenshot::address(ws, t)
}

type Pending = Arc<Mutex<HashMap<u64, (Option<String>, oneshot::Sender<Result<Value, String>>)>>>;
const DETACHED: &str = "Session with given id not found";

struct Cdp {
    out: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next: AtomicU64,
    dialogs: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<Value>>>,
    detached: Arc<Mutex<std::collections::HashSet<String>>>,
    closed: Arc<AtomicBool>,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Cdp, String> {
        let (socket, _) =
            tokio_tungstenite::connect_async(url).await.map_err(|e| format!("{url}: {e}"))?;
        let (mut sink, mut stream) = socket.split();
        let (out, mut queue) = mpsc::unbounded_channel::<Message>();
        let pending: Pending = Default::default();
        let dialogs: Arc<Mutex<Vec<String>>> = Default::default();
        let events: Arc<Mutex<Vec<Value>>> = Default::default();
        let detached: Arc<Mutex<std::collections::HashSet<String>>> = Default::default();
        let closed = Arc::new(AtomicBool::new(false));
        tokio::spawn(async move {
            while let Some(m) = queue.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });
        let (p, d, c, o) = (pending.clone(), dialogs.clone(), closed.clone(), out.clone());
        let (e, gone) = (events.clone(), detached.clone());
        tokio::spawn(async move {
            while let Some(Ok(m)) = stream.next().await {
                let Message::Text(text) = m else { continue };
                match route(text.as_str()) {
                    Incoming::Reply { id, result } => {
                        let tx = p.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                        if let Some((_, tx)) = tx {
                            let _ = tx.send(result);
                        }
                    }
                    Incoming::Dialog { session, message } => {
                        d.lock().unwrap_or_else(|e| e.into_inner()).push(message);
                        let mut reply = json!({
                            "id": 0, "method": "Page.handleJavaScriptDialog",
                            "params": { "accept": true },
                        });
                        if let Some(s) = session {
                            reply["sessionId"] = json!(s);
                        }
                        let _ = o.send(Message::text(reply.to_string()));
                    }
                    Incoming::Event(event) => {
                        let mut all = e.lock().unwrap_or_else(|e| e.into_inner());
                        if all.len() < 500 {
                            all.push(event);
                        }
                    }
                    Incoming::Detached(session) => {
                        gone.lock().unwrap_or_else(|e| e.into_inner()).insert(session.clone());
                        let mut all = p.lock().unwrap_or_else(|e| e.into_inner());
                        let waiting: Vec<u64> = all
                            .iter()
                            .filter(|(_, (s, _))| s.as_deref() == Some(session.as_str()))
                            .map(|(id, _)| *id)
                            .collect();
                        for id in waiting {
                            if let Some((_, tx)) = all.remove(&id) {
                                let _ = tx.send(Err(DETACHED.into()));
                            }
                        }
                    }
                    Incoming::Other => {}
                }
            }
            c.store(true, Ordering::SeqCst);
            p.lock().unwrap_or_else(|e| e.into_inner()).clear();
        });
        Ok(Cdp { out, pending, next: AtomicU64::new(1), dialogs, events, detached, closed })
    }

    async fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("the browser closed".into());
        }
        if session
            .is_some_and(|s| self.detached.lock().unwrap_or_else(|e| e.into_inner()).contains(s))
        {
            return Err(DETACHED.into());
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, (session.map(str::to_string), tx));
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.out.send(Message::text(msg.to_string())).map_err(|_| "the browser closed")?;
        match tokio::time::timeout(REPLY_WAIT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("the browser closed".into()),
            Err(_) => {
                self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                Err(format!("{method} got no answer in {}s", REPLY_WAIT.as_secs()))
            }
        }
    }

    fn take_dialogs(&self) -> Vec<String> {
        std::mem::take(&mut *self.dialogs.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn forget_events(&self) {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    async fn event(&self, wait: Duration, want: impl Fn(&Value) -> bool) -> Option<Value> {
        let started = Instant::now();
        loop {
            {
                let mut all = self.events.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(i) = all.iter().position(&want) {
                    return Some(all.remove(i));
                }
            }
            if started.elapsed() >= wait || self.closed.load(Ordering::SeqCst) {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

struct Live {
    cdp: Cdp,
    target: String,
    tab: String,
    refs: AtomicUsize,
    seen: Mutex<String>,
    _child: Option<Child>,
    dir: Option<PathBuf>,
    used: Instant,
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        } else {
            let close = json!({ "id": 0, "method": "Target.closeTarget", "params": { "targetId": self.tab } });
            let _ = self.cdp.out.send(Message::text(close.to_string()));
        }
    }
}

pub const HEADLESS: &str = "headlesschrome";

pub fn target(setting: &str) -> Result<Option<String>, String> {
    let s = setting.trim();
    if s.is_empty() || s.eq_ignore_ascii_case(HEADLESS) || s.eq_ignore_ascii_case("default") {
        return Ok(None);
    }
    endpoint(s).map(Some).ok_or_else(|| format!("target must be {HEADLESS} or ip:port, not {s}"))
}

pub fn endpoint(setting: &str) -> Option<String> {
    let s = setting.trim();
    if s.is_empty() {
        return None;
    }
    if s.starts_with("ws://") || s.starts_with("wss://") {
        return Some(s.to_string());
    }
    let host = s.trim_start_matches("http://").trim_end_matches('/');
    let host = if host.chars().all(|c| c.is_ascii_digit()) {
        format!("127.0.0.1:{host}")
    } else {
        host.to_string()
    };
    Some(format!("ws://{host}/devtools/browser"))
}

async fn attach(url: &str) -> Result<Live, String> {
    let cdp = Cdp::connect(url).await?;
    let made = cdp
        .call(None, "Target.createTarget", json!({ "url": "about:blank", "background": true }))
        .await?;
    let tab = made["targetId"].as_str().ok_or("no browser tab")?.to_string();
    let target = session_for(&cdp, &tab).await?;
    Ok(Live {
        cdp,
        target,
        tab,
        refs: AtomicUsize::new(0),
        seen: Mutex::default(),
        _child: None,
        dir: None,
        used: Instant::now(),
    })
}

async fn session_for(cdp: &Cdp, tab: &str) -> Result<String, String> {
    let attached = cdp
        .call(None, "Target.attachToTarget", json!({ "targetId": tab, "flatten": true }))
        .await?;
    let target = attached["sessionId"].as_str().ok_or("no browser session")?.to_string();
    cdp.call(Some(&target), "Page.enable", json!({})).await?;
    cdp.call(Some(&target), "Page.addScriptToEvaluateOnNewDocument", json!({ "source": WATCH }))
        .await?;
    Ok(target)
}

async fn launch(wanted: Option<&str>) -> Result<Live, String> {
    if let Some(url) = wanted {
        return attach(url).await.map_err(|e| {
            format!("{e}: the browser chosen with set_browser_target is not answering; start it again or pick another target")
        });
    }
    if let Some(url) = std::env::var("AGENTBENCH_BROWSER").ok().and_then(|v| endpoint(&v)) {
        return attach(&url).await;
    }
    start_own().await
}

async fn start_own() -> Result<Live, String> {
    let chrome = screenshot::browser()
        .ok_or("browser needs Chrome, Chromium or Edge, and none was found on the path")?;
    let dir = std::env::temp_dir().join(format!("agentbench-browser-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let result = start(&chrome, &dir).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    result
}

async fn start(chrome: &std::path::Path, dir: &std::path::Path) -> Result<Live, String> {
    let mut child = Command::new(chrome)
        .args([
            "--headless=new",
            "--remote-debugging-port=0",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
            "--hide-scrollbars",
            "--mute-audio",
            "--no-sandbox",
        ])
        .arg(format!("--user-data-dir={}", dir.display()))
        .arg(format!("--window-size={WIDTH},{HEIGHT}"))
        .arg("about:blank")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("{}: {e}", chrome.display()))?;
    let stderr = child.stderr.take().ok_or("no browser output")?;
    let mut lines = BufReader::new(stderr).lines();
    let found = tokio::time::timeout(LAUNCH_WAIT, async {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some((_, url)) = line.split_once("DevTools listening on ") {
                return Some(url.trim().to_string());
            }
        }
        None
    })
    .await
    .map_err(|_| "the browser did not start in time".to_string())?
    .ok_or("the browser exited before it was ready")?;
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    let cdp = Cdp::connect(&found).await?;
    let made = cdp.call(None, "Target.createTarget", json!({ "url": "about:blank" })).await?;
    let tab = made["targetId"].as_str().ok_or("no browser tab")?.to_string();
    let target = session_for(&cdp, &tab).await?;
    cdp.call(
        Some(&target),
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": WIDTH, "height": HEIGHT, "deviceScaleFactor": 1, "mobile": false }),
    )
    .await?;
    Ok(Live {
        cdp,
        target,
        tab,
        refs: AtomicUsize::new(0),
        seen: Mutex::default(),
        _child: Some(child),
        dir: Some(dir.to_path_buf()),
        used: Instant::now(),
    })
}

impl Live {
    async fn reopen(&mut self) -> Result<(), String> {
        let tabs = self.cdp.call(None, "Target.getTargets", json!({})).await?;
        let alive = tabs["targetInfos"]
            .as_array()
            .is_some_and(|all| all.iter().any(|t| t["targetId"] == self.tab.as_str()));
        if alive {
            self.target = session_for(&self.cdp, &self.tab).await?;
            return Ok(());
        }
        let made = self
            .cdp
            .call(
                None,
                "Target.createTarget",
                json!({ "url": "about:blank", "background": self.dir.is_none() }),
            )
            .await?;
        self.tab = made["targetId"].as_str().ok_or("no browser tab")?.to_string();
        self.target = session_for(&self.cdp, &self.tab).await?;
        if self.dir.is_some() {
            self.send(
                "Emulation.setDeviceMetricsOverride",
                json!({ "width": WIDTH, "height": HEIGHT, "deviceScaleFactor": 1, "mobile": false }),
            )
            .await?;
        }
        let seen = self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !seen.is_empty() && seen != "about:blank" {
            self.goto(&seen).await?;
        }
        Ok(())
    }

    async fn send(&self, method: &str, params: Value) -> Result<Value, String> {
        self.cdp.call(Some(&self.target), method, params).await
    }

    async fn eval(&self, expression: &str) -> Result<Value, String> {
        let r = self
            .send(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true,
                }),
            )
            .await?;
        if let Some(e) = r.get("exceptionDetails") {
            let text = e["exception"]["description"]
                .as_str()
                .or_else(|| e["exception"]["value"].as_str())
                .or_else(|| e["text"].as_str())
                .unwrap_or("script error");
            return Err(text.lines().next().unwrap_or(text).to_string());
        }
        Ok(r["result"].get("value").cloned().unwrap_or(Value::Null))
    }

    async fn call_fn(&self, source: &str, args: &[Value]) -> Result<Value, String> {
        let args: Vec<String> = args.iter().map(Value::to_string).collect();
        self.eval(&format!("({source})({})", args.join(","))).await
    }

    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let started = Instant::now();
        let mut calm: Option<Instant> = None;
        while started.elapsed() < SETTLE_MAX {
            let state = match self.eval(QUIET).await {
                Ok(s) => s,
                Err(e) if lost(&e) => return,
                Err(_) => Value::Null,
            };
            if state["ready"] == "complete" && !state["watched"].as_bool().unwrap_or(true) {
                let _ = self.eval(WATCH).await;
            }
            let loaded = state["ready"] == "complete"
                && state["busy"].as_u64() == Some(0)
                && !state["blank"].as_bool().unwrap_or(false);
            calm = if loaded { calm.or(Some(Instant::now())) } else { None };
            let still = state["idle"].as_f64().is_some_and(|ms| ms >= QUIET_FOR);
            if calm.is_some_and(|c| still || c.elapsed() >= RESTLESS_MAX) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn wait(&self, text: Option<&str>, ms: u64) -> Result<(), String> {
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            self.settle().await;
            return Ok(());
        };
        let probe = format!(
            "(document.body ? document.body.innerText : '').toLowerCase().includes({})",
            json!(text.to_lowercase())
        );
        let started = Instant::now();
        loop {
            if self.eval(&probe).await? == Value::Bool(true) {
                self.settle().await;
                return Ok(());
            }
            if started.elapsed() >= Duration::from_millis(ms) {
                return Err(format!("{text:?} did not appear in {:.1}s", ms as f64 / 1000.0));
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    async fn page(&self) -> Result<Page, String> {
        let mut last = String::new();
        for _ in 0..3 {
            let base = self.refs.load(Ordering::SeqCst);
            match self.call_fn(EXTRACT, &[json!(base)]).await {
                Ok(Value::String(s)) => {
                    let page = parse_page(&s)?;
                    self.refs.fetch_max(page.next, Ordering::SeqCst);
                    page.url.clone_into(&mut self.seen.lock().unwrap_or_else(|e| e.into_inner()));
                    return Ok(page);
                }
                Ok(other) => last = format!("unexpected page data: {other}"),
                Err(e) => last = e,
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Err(last)
    }

    async fn grid(&self) -> Result<String, String> {
        let page = self.page().await?;
        Ok(view(&page, &self.cdp.take_dialogs(), COLS, OUTPUT_CHARS))
    }

    async fn locate(&self, args: &Value, label_keys: &[&str]) -> Result<usize, String> {
        let Some(label) = (args.get("ref").is_none())
            .then(|| text_arg(args, label_keys))
            .flatten()
            .filter(|l| !l.trim().is_empty())
        else {
            return reference(args);
        };
        self.page().await?;
        let near = text_arg(args, &["near"]).filter(|n| !n.trim().is_empty());
        let v = self.call_fn(FIND, &[json!(label), json!(near)]).await?;
        match (v["ref"].as_u64(), v["error"].as_str()) {
            (Some(r), _) => Ok(r as usize),
            (None, Some(e)) => Err(e.to_string()),
            (None, None) => Err(format!("nothing on the page is labelled {label:?}")),
        }
    }

    async fn goto(&self, url: &str) -> Result<(), String> {
        let r = self.send("Page.navigate", json!({ "url": url })).await?;
        if let Some(e) = r["errorText"].as_str() {
            return Err(format!("{url}: {e}"));
        }
        self.settle().await;
        Ok(())
    }

    async fn point(&self, r: usize) -> Result<(f64, f64), String> {
        let v = self.call_fn(TARGET, &[json!(r), json!("click")]).await?;
        check_target(&v, r)?;
        Ok((v["x"].as_f64().unwrap_or(0.0), v["y"].as_f64().unwrap_or(0.0)))
    }

    async fn click(&self, r: usize) -> Result<(), String> {
        let (x, y) = self.point(r).await?;
        let mouse = |kind: &str| {
            json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": 1,
                    "buttons": if kind == "mousePressed" { 1 } else { 0 } })
        };
        self.send("Input.dispatchMouseEvent", json!({ "type": "mouseMoved", "x": x, "y": y }))
            .await?;
        self.send("Input.dispatchMouseEvent", mouse("mousePressed")).await?;
        self.send("Input.dispatchMouseEvent", mouse("mouseReleased")).await?;
        self.settle().await;
        Ok(())
    }

    async fn press(&self, spec: &str) -> Result<(), String> {
        let k = parse_key(spec)?;
        let mut down = json!({
            "type": if k.text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": k.key, "code": k.code, "windowsVirtualKeyCode": k.vk,
            "modifiers": k.modifiers,
        });
        if let Some(t) = &k.text {
            down["text"] = json!(t);
        }
        self.send("Input.dispatchKeyEvent", down).await?;
        self.send(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": k.key, "code": k.code,
                    "windowsVirtualKeyCode": k.vk, "modifiers": k.modifiers }),
        )
        .await?;
        self.settle().await;
        Ok(())
    }

    async fn type_into(&self, r: usize, text: &str, submit: bool) -> Result<(), String> {
        let v = self.call_fn(TARGET, &[json!(r), json!("focus")]).await?;
        check_target(&v, r)?;
        if text.is_empty() {
            self.press("Delete").await?;
        } else {
            self.send("Input.insertText", json!({ "text": text })).await?;
        }
        if submit {
            self.press("Enter").await?;
        } else {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        Ok(())
    }

    async fn element(&self, r: usize) -> Result<(String, bool), String> {
        let found = self
            .send(
                "Runtime.evaluate",
                json!({ "expression": format!("(window.__twEls instanceof Map ? window.__twEls.get({r}) : undefined)") }),
            )
            .await?;
        let id = found["result"]["objectId"]
            .as_str()
            .ok_or_else(|| format!("[{r}] is gone from the page; snapshot for fresh refs"))?;
        let kind = self
            .send(
                "Runtime.callFunctionOn",
                json!({ "objectId": id, "returnByValue": true,
                        "functionDeclaration": "function () { return this.isConnected && this.localName === 'input' && this.type === 'file'; }" }),
            )
            .await?;
        Ok((id.to_string(), kind["result"]["value"] == true))
    }

    async fn upload(&self, r: usize, files: &[PathBuf]) -> Result<(), String> {
        let files: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
        let (id, input) = self.element(r).await?;
        if input {
            self.send("DOM.setFileInputFiles", json!({ "files": files, "objectId": id })).await?;
            self.settle().await;
            return Ok(());
        }
        self.cdp.forget_events();
        self.send("Page.setInterceptFileChooserDialog", json!({ "enabled": true })).await?;
        let clicked = self.click(r).await;
        let chooser = match clicked {
            Ok(()) => {
                self.cdp
                    .event(Duration::from_secs(5), |e| e["method"] == "Page.fileChooserOpened")
                    .await
            }
            Err(_) => None,
        };
        let _ = self.send("Page.setInterceptFileChooserDialog", json!({ "enabled": false })).await;
        clicked?;
        let chooser = chooser.ok_or_else(|| {
            format!("[{r}] is not a file field and clicking it opened no file chooser")
        })?;
        let node = chooser["params"]["backendNodeId"].clone();
        if chooser["params"]["mode"] == "selectSingle" && files.len() > 1 {
            return Err("that file chooser takes one file".into());
        }
        self.send("DOM.setFileInputFiles", json!({ "files": files, "backendNodeId": node }))
            .await?;
        self.settle().await;
        Ok(())
    }

    async fn download_click(&self, r: usize, wait: Duration) -> Result<(PathBuf, String), String> {
        let dir =
            std::env::temp_dir().join(format!("agentbench-download-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        self.cdp.forget_events();
        self.cdp
            .call(
                None,
                "Browser.setDownloadBehavior",
                json!({ "behavior": "allowAndName", "downloadPath": dir.display().to_string(), "eventsEnabled": true }),
            )
            .await?;
        let got = self.catch_download(r, wait, &dir).await;
        let _ = self
            .cdp
            .call(None, "Browser.setDownloadBehavior", json!({ "behavior": "default" }))
            .await;
        if got.is_err() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        got
    }

    async fn catch_download(
        &self,
        r: usize,
        wait: Duration,
        dir: &std::path::Path,
    ) -> Result<(PathBuf, String), String> {
        self.click(r).await?;
        let begun =
            self.cdp.event(wait, |e| e["method"] == "Browser.downloadWillBegin").await.ok_or_else(
                || format!("[{r}] started no download in {:.1}s", wait.as_secs_f64()),
            )?;
        let guid = begun["params"]["guid"].as_str().unwrap_or_default().to_string();
        let name = begun["params"]["suggestedFilename"].as_str().unwrap_or("download").to_string();
        let done = self
            .cdp
            .event(DOWNLOAD_MAX, |e| {
                e["method"] == "Browser.downloadProgress"
                    && e["params"]["guid"] == guid.as_str()
                    && e["params"]["state"] != "inProgress"
            })
            .await
            .ok_or_else(|| {
                format!("{name} did not finish downloading in {}s", DOWNLOAD_MAX.as_secs())
            })?;
        if done["params"]["state"] != "completed" {
            return Err(format!("the download of {name} was cancelled"));
        }
        Ok((dir.join(&guid), name))
    }

    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let here = self.eval("location.href").await?;
        let here = here.as_str().unwrap_or("about:blank");
        let full = url::Url::parse(here)
            .and_then(|base| base.join(url))
            .or_else(|_| url::Url::parse(url))
            .map_err(|e| format!("{url}: {e}"))?;
        if !matches!(full.scheme(), "http" | "https") {
            return Err(format!("{full}: only http and https addresses can be downloaded"));
        }
        let cookies = self.send("Network.getCookies", json!({ "urls": [full.as_str()] })).await?;
        let cookie = cookies["cookies"]
            .as_array()
            .map(|all| {
                all.iter()
                    .map(|c| {
                        format!(
                            "{}={}",
                            c["name"].as_str().unwrap_or(""),
                            c["value"].as_str().unwrap_or("")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            })
            .unwrap_or_default();
        let agent = self.eval("navigator.userAgent").await.unwrap_or(Value::Null);
        let mut req = crate::http::client().get(full.as_str()).header("referer", here);
        if !cookie.is_empty() {
            req = req.header("cookie", cookie);
        }
        if let Some(a) = agent.as_str() {
            req = req.header("user-agent", a);
        }
        let reply = tokio::time::timeout(DOWNLOAD_MAX, req.send())
            .await
            .map_err(|_| format!("{full} did not answer in {}s", DOWNLOAD_MAX.as_secs()))?
            .map_err(|e| format!("{full}: {e}"))?;
        if !reply.status().is_success() {
            return Err(format!("{full}: {}", reply.status()));
        }
        let body = tokio::time::timeout(DOWNLOAD_MAX, reply.bytes())
            .await
            .map_err(|_| format!("{full} did not finish in {}s", DOWNLOAD_MAX.as_secs()))?
            .map_err(|e| format!("{full}: {e}"))?;
        Ok(body.to_vec())
    }

    async fn select(&self, r: usize, value: &str) -> Result<(), String> {
        let v = self.call_fn(SELECT, &[json!(r), json!(value)]).await?;
        check_target(&v, r)?;
        self.settle().await;
        Ok(())
    }

    async fn scroll(&self, direction: &str, amount: f64) -> Result<(), String> {
        let page = self.page().await?;
        match direction {
            "top" => {
                self.eval("window.scrollTo(0, 0)").await?;
            }
            "bottom" => {
                self.eval("window.scrollTo(0, document.documentElement.scrollHeight)").await?;
            }
            "down" | "up" => {
                let delta = page.vh * 0.8 * amount * if direction == "up" { -1.0 } else { 1.0 };
                self.send(
                    "Input.dispatchMouseEvent",
                    json!({ "type": "mouseWheel", "x": page.vw / 2.0, "y": page.vh / 2.0,
                            "deltaX": 0, "deltaY": delta }),
                )
                .await?;
            }
            other => {
                return Err(format!(
                    "scroll direction must be down, up, top or bottom, not {other}"
                ));
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(())
    }
}

fn upload_files(
    ws: &egui_bench::agent::workspace::Workspace,
    args: &Value,
) -> Result<Vec<PathBuf>, String> {
    let names: Vec<&str> =
        match args.get("files").or_else(|| args.get("file")).or_else(|| args.get("path")) {
            Some(Value::Array(all)) => all.iter().filter_map(Value::as_str).collect(),
            Some(Value::String(one)) => vec![one.as_str()],
            _ => return Err("missing `files`: the paths of the files to put in the field".into()),
        };
    if names.is_empty() {
        return Err("`files` is empty".into());
    }
    names
        .into_iter()
        .map(|name| {
            let path = ws.resolve_for(name, Access::Read)?;
            if !path.is_file() {
                return Err(format!("{name} is not a file"));
            }
            path.canonicalize().map_err(|e| format!("{name}: {e}"))
        })
        .collect()
}

fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let clean: String =
        base.chars().map(|c| if c.is_control() || c == ':' { '_' } else { c }).collect();
    match clean.trim_matches('.') {
        "" => "download".into(),
        _ => clean,
    }
}

async fn download(
    live: &Live,
    ws: &egui_bench::agent::workspace::Workspace,
    args: &Value,
) -> Result<String, String> {
    let wait = Duration::from_millis(
        args.get("ms")
            .or_else(|| args.get("wait_ms"))
            .and_then(Value::as_u64)
            .unwrap_or(WAIT_DEFAULT)
            .min(WAIT_MAX),
    );
    let (bytes, name) = match (args.get("ref").is_none(), text_arg(args, &["url"])) {
        (true, Some(url)) if text_arg(args, &["label", "text"]).is_none() => {
            let bytes = live.fetch(url).await?;
            let name = url.split(['?', '#']).next().unwrap_or(url);
            (bytes, safe_name(name))
        }
        _ => {
            let r = live.locate(args, &["label", "text"]).await?;
            let (file, name) = live.download_click(r, wait).await?;
            let bytes = std::fs::read(&file).map_err(|e| format!("{name}: {e}"));
            if let Some(dir) = file.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
            (bytes?, safe_name(&name))
        }
    };
    let save = match text_arg(args, &["save_to", "to"]) {
        Some(s) => s.to_string(),
        None => format!("downloads/{name}"),
    };
    let dest = ws.resolve_for(&save, Access::Write)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&dest, &bytes).map_err(|e| format!("{save}: {e}"))?;
    Ok(format!("saved {name} ({} bytes) to {}", bytes.len(), ws.shown(&dest)))
}

fn lost(error: &str) -> bool {
    [
        "Session with given id not found",
        "No target with given id",
        "Target closed",
        "the browser closed",
    ]
    .iter()
    .any(|m| error.contains(m))
}

fn check_target(v: &Value, r: usize) -> Result<(), String> {
    match v.get("error").and_then(Value::as_str) {
        None => Ok(()),
        Some("stale") => Err(format!("[{r}] is gone from the page; snapshot for fresh refs")),
        Some("covered") => Err(format!(
            "[{r}] is covered by {}; close that first",
            v["by"].as_str().unwrap_or("another element")
        )),
        Some(other) => Err(format!("[{r}]: {other}")),
    }
}

pub fn eval_text(value: &Value) -> String {
    let text = match value {
        Value::Null => "null".to_string(),
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    clip(&text, EVAL_CHARS)
}

fn reference(args: &Value) -> Result<usize, String> {
    let v = args.get("ref").ok_or("missing `ref`: the number in [brackets] from the last grid, or `label`: the element's visible text")?;
    let n = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().trim_matches(['[', ']']).parse().ok()))
        .ok_or("`ref` must be a number")?;
    Ok(n as usize)
}

fn text_arg<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args.get(*k).and_then(Value::as_str))
}

pub fn action_name(args: &Value) -> Result<&'static str, String> {
    let name = text_arg(args, &["action"]).ok_or("missing `action`")?;
    Ok(match name.trim().to_ascii_lowercase().as_str() {
        "goto" | "navigate" | "open" | "go" => "goto",
        "snapshot" | "read" | "view" => "snapshot",
        "click" => "click",
        "type" | "fill" => "type",
        "select" => "select",
        "press" | "key" => "press",
        "scroll" => "scroll",
        "wait" | "wait_for" | "sleep" => "wait",
        "upload" | "attach" | "set_files" => "upload",
        "download" | "save" => "download",
        "eval" | "evaluate" | "js" => "eval",
        "connect" => "connect",
        other => {
            return Err(format!(
                "unknown action {other}: use goto, snapshot, click, type, select, press, scroll, wait, upload, download, eval or connect"
            ));
        }
    })
}

type Slot = Arc<tokio::sync::Mutex<Option<Live>>>;

#[derive(Clone, Default)]
pub struct Browsers {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
    targets: Arc<Mutex<HashMap<String, String>>>,
    saved: Option<Arc<PathBuf>>,
}

impl Browsers {
    pub fn saved_in(dir: PathBuf) -> Browsers {
        let file = dir.join("browser-targets.json");
        let targets = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Browsers {
            slots: Default::default(),
            targets: Arc::new(Mutex::new(targets)),
            saved: Some(Arc::new(file)),
        }
    }

    fn wanted(&self, session: &str) -> Option<String> {
        self.targets.lock().unwrap_or_else(|e| e.into_inner()).get(session).cloned()
    }

    fn remember(&self, session: &str, url: Option<&str>) {
        let mut all = self.targets.lock().unwrap_or_else(|e| e.into_inner());
        let changed = match url {
            Some(u) => all.insert(session.to_string(), u.to_string()).as_deref() != Some(u),
            None => all.remove(session).is_some(),
        };
        if let (true, Some(file)) = (changed, &self.saved) {
            if let Some(parent) = file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ =
                std::fs::write(file.as_path(), serde_json::to_string(&*all).unwrap_or_default());
        }
    }

    pub fn close(&self, session: &str) {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).remove(session);
        self.remember(session, None);
    }

    pub fn open(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn slot(&self, session: &str) -> Slot {
        let mut all = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|k, s| {
            k == session
                || s.try_lock()
                    .map_or(true, |l| l.as_ref().is_some_and(|l| l.used.elapsed() < IDLE))
        });
        all.entry(session.to_string()).or_default().clone()
    }

    pub async fn act(
        &self,
        session: &str,
        ws: &egui_bench::agent::workspace::Workspace,
        args: &Value,
        stop: &CancellationToken,
    ) -> Result<String, String> {
        let action = action_name(args)?;
        let url = if action == "goto" {
            let t = text_arg(args, &["url", "path"]).ok_or("missing `url`")?;
            Some(address(ws, t)?)
        } else {
            None
        };
        let slot = self.slot(session);
        let mut guard = slot.lock().await;
        let mut seen = String::new();
        if let Some(l) = guard.as_ref().filter(|l| l.cdp.closed.load(Ordering::SeqCst)) {
            seen = l.seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
            *guard = None;
        }
        if action == "connect" {
            let setting = text_arg(args, &["target", "address"]).unwrap_or("");
            let wanted = target(setting)?;
            *guard = None;
            *guard = Some(tokio::select! {
                l = async { match &wanted { Some(url) => attach(url).await, None => start_own().await } } => l?,
                _ = stop.cancelled() => return Err("stopped".into()),
            });
            self.remember(session, wanted.as_deref());
        }
        let wanted = self.wanted(session);
        if guard.is_none() {
            let live = tokio::select! {
                l = launch(wanted.as_deref()) => l?,
                _ = stop.cancelled() => return Err("stopped".into()),
            };
            if url.is_none() && !seen.is_empty() && seen != "about:blank" {
                let _ = live.goto(&seen).await;
            }
            *guard = Some(live);
        }
        let live = guard.as_mut().ok_or("no browser")?;
        live.used = Instant::now();
        let mut done = tokio::select! {
            r = run(live, ws, action, url.as_deref(), args) => r,
            _ = stop.cancelled() => Err("stopped".into()),
        };
        if done.as_ref().is_err_and(|e| lost(e)) && !stop.is_cancelled() {
            let reopened = if live.cdp.closed.load(Ordering::SeqCst) {
                Err("the browser closed".to_string())
            } else {
                live.reopen().await
            };
            if reopened.is_err() {
                let seen = live.seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
                *guard = None;
                let fresh = launch(wanted.as_deref()).await?;
                if !seen.is_empty() && seen != "about:blank" {
                    let _ = fresh.goto(&seen).await;
                }
                *guard = Some(fresh);
            }
            let live = guard.as_mut().ok_or("no browser")?;
            done = tokio::select! {
                r = run(live, ws, action, url.as_deref(), args) => r,
                _ = stop.cancelled() => Err("stopped".into()),
            };
        }
        if let Some(live) = guard.as_mut() {
            live.used = Instant::now();
            if live.cdp.closed.load(Ordering::SeqCst) {
                *guard = None;
            }
        }
        done
    }
}

async fn run(
    live: &Live,
    ws: &egui_bench::agent::workspace::Workspace,
    action: &str,
    url: Option<&str>,
    args: &Value,
) -> Result<String, String> {
    match action {
        "goto" => live.goto(url.unwrap_or("about:blank")).await?,
        "snapshot" => live.settle().await,
        "wait" => {
            let ms = args
                .get("ms")
                .or_else(|| args.get("wait_ms"))
                .and_then(Value::as_u64)
                .unwrap_or(WAIT_DEFAULT)
                .min(WAIT_MAX);
            live.wait(text_arg(args, &["text", "for"]), ms).await?
        }
        "click" => live.click(live.locate(args, &["label", "text"]).await?).await?,
        "type" => {
            let text = text_arg(args, &["text", "value"]).ok_or("missing `text`")?;
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
            live.type_into(live.locate(args, &["label"]).await?, text, submit).await?
        }
        "select" => {
            let value = text_arg(args, &["value", "text"]).ok_or("missing `value`")?;
            live.select(live.locate(args, &["label"]).await?, value).await?
        }
        "press" => live.press(text_arg(args, &["key"]).ok_or("missing `key`")?).await?,
        "connect" => {}
        "upload" => {
            let files = upload_files(ws, args)?;
            live.upload(live.locate(args, &["label", "text"]).await?, &files).await?
        }
        "download" => return download(live, ws, args).await,
        "scroll" => {
            let direction = text_arg(args, &["direction"]).unwrap_or("down");
            let amount = args.get("amount").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.1, 20.0);
            live.scroll(direction, amount).await?
        }
        _ => {
            let js = text_arg(args, &["js", "expression", "script"]).ok_or("missing `js`")?;
            let value = live.eval(js).await?;
            let mut out = eval_text(&value);
            if args.get("grid").and_then(Value::as_bool).unwrap_or(false) {
                live.settle().await;
                out = format!("{out}\n\n{}", live.grid().await?);
            }
            return Ok(out);
        }
    }
    live.grid().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_bench::agent::workspace::Workspace;

    fn node(k: &str, t: &str, idx: Option<usize>, x: f64, y: f64) -> Node {
        Node {
            k: k.into(),
            t: t.into(),
            hint: String::new(),
            lvl: 0,
            on: false,
            off: false,
            idx,
            x,
            y,
            w: 80.0,
            h: 18.0,
        }
    }

    fn page(nodes: Vec<Node>) -> Page {
        Page {
            url: "http://x/".into(),
            title: "T".into(),
            scroll_y: 0.0,
            scroll_height: 800.0,
            vw: 1200.0,
            vh: 800.0,
            next: 0,
            nodes,
        }
    }

    #[test]
    fn elements_on_one_line_share_a_row_in_x_order() {
        let p = page(vec![
            node("button", "Go", Some(1), 600.0, 102.0),
            node("text", "Name", None, 100.0, 100.0),
            node("input", "Bob", Some(0), 200.0, 98.0),
            node("link", "Home", Some(2), 100.0, 300.0),
        ]);
        let grid = render(&p, 120);
        assert_eq!(grid.lines().count(), 3, "{grid}");
        let first = grid.lines().next().unwrap();
        assert!(first.trim_start().starts_with("Name"), "{first}");
        assert!(first.find("Name") < first.find("[0:Bob]"), "{first}");
        assert!(first.trim_end().ends_with("[1 Go]"), "{first}");
        assert_eq!(grid.lines().nth(1), Some(""));
        assert_eq!(grid.lines().nth(2).unwrap().trim(), "[2]Home");
    }

    #[test]
    fn each_kind_of_control_has_its_own_notation() {
        let mut check = node("checkbox", "", Some(0), 0.0, 0.0);
        check.on = true;
        let mut radio = node("radio", "", Some(1), 0.0, 0.0);
        radio.off = true;
        let mut empty = node("input", "", Some(2), 0.0, 0.0);
        empty.hint = "Email".into();
        let mut head = node("heading", "Sign up", None, 0.0, 0.0);
        head.lvl = 2;
        assert_eq!(cell(&check), "[0:X]");
        assert_eq!(cell(&node("radio", "No", Some(5), 0.0, 0.0)), "[5:\u{25cb}] No");
        assert_eq!(cell(&radio), "[1:\u{25cb} (disabled)]");
        assert_eq!(cell(&empty), "[2:___ Email]");
        assert_eq!(cell(&node("select", "Two", Some(3), 0.0, 0.0)), "[3:\u{25bc} Two]");
        assert_eq!(cell(&node("button", "", Some(4), 0.0, 0.0)), "[4 button]");
        assert_eq!(cell(&head), "## Sign up");
        assert_eq!(cell(&node("img", "logo", None, 0.0, 0.0)), "[img: logo]");
    }

    #[test]
    fn a_page_is_parsed_from_what_the_extractor_returns() {
        let json = r#"{"url":"http://x/","title":"T","scrollY":0,"scrollHeight":900,"vw":1280,"vh":800,
            "nodes":[{"k":"link","t":"Home","idx":0,"x":10,"y":10,"w":40,"h":16}]}"#;
        let p = parse_page(json).unwrap();
        assert_eq!(p.nodes[0].idx, Some(0));
        let text = view(&p, &["alert dialog accepted: hi".into()], 120, 1000);
        assert!(
            text.starts_with("[alert dialog accepted: hi]\nurl: http://x/\ntitle: T\n"),
            "{text}"
        );
        assert!(text.contains("viewport: 0-800 of 900px, more below"), "{text}");
        assert!(text.ends_with("[0]Home"), "{text}");
        assert!(parse_page("nope").is_err());
    }

    #[test]
    fn an_empty_page_says_so() {
        let text = view(&page(vec![]), &[], 120, 1000);
        assert!(text.ends_with("(nothing visible)"), "{text}");
        assert!(!text.contains("more below"));
    }

    #[test]
    fn long_output_is_clipped_at_a_line_and_says_how_much_is_missing() {
        let long = (0..50).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let out = clip(&long, 100);
        assert!(out.starts_with("line 0\n"), "{out}");
        assert!(out.contains("[clipped "), "{out}");
        assert!(!out.contains("line 49"));
        assert_eq!(clip("short", 100), "short");
    }

    #[test]
    fn replies_errors_and_dialogs_are_told_apart() {
        assert_eq!(
            route(r#"{"id":3,"result":{"a":1}}"#),
            Incoming::Reply { id: 3, result: Ok(json!({"a": 1})) }
        );
        assert_eq!(
            route(r#"{"id":4,"error":{"code":-1,"message":"nope"}}"#),
            Incoming::Reply { id: 4, result: Err("nope".into()) }
        );
        assert_eq!(
            route(
                r#"{"method":"Page.javascriptDialogOpening","sessionId":"S","params":{"type":"confirm","message":"sure?"}}"#
            ),
            Incoming::Dialog {
                session: Some("S".into()),
                message: "confirm dialog accepted: sure?".into()
            }
        );
        assert_eq!(route(r#"{"method":"Page.loadEventFired"}"#), Incoming::Other);
        assert_eq!(
            route(r#"{"method":"Target.detachedFromTarget","params":{"sessionId":"S"}}"#),
            Incoming::Detached("S".into())
        );
        assert!(matches!(
            route(r#"{"method":"Browser.downloadWillBegin","params":{"guid":"g"}}"#),
            Incoming::Event(e) if e["params"]["guid"] == "g"
        ));
        assert_eq!(route("garbage"), Incoming::Other);
    }

    #[test]
    fn keys_carry_codes_text_and_modifiers() {
        let enter = parse_key("Enter").unwrap();
        assert_eq!((enter.vk, enter.text.as_deref()), (13, Some("\r")));
        let select_all = parse_key("Ctrl+a").unwrap();
        assert_eq!(
            (select_all.modifiers, select_all.text, select_all.code.as_str()),
            (2, None, "KeyA")
        );
        let plus = parse_key("Shift++").unwrap();
        assert_eq!((plus.key.as_str(), plus.modifiers), ("+", 8));
        assert_eq!(parse_key("x").unwrap().text.as_deref(), Some("x"));
        assert!(parse_key("Bogus").is_err());
        assert!(parse_key("Hyper+a").is_err());
    }

    #[test]
    fn actions_have_aliases_and_unknown_ones_are_refused() {
        assert_eq!(action_name(&json!({"action": "Fill"})), Ok("type"));
        assert_eq!(action_name(&json!({"action": "navigate"})), Ok("goto"));
        assert_eq!(action_name(&json!({"action": "connect"})), Ok("connect"));
        assert!(action_name(&json!({"action": "dance"})).unwrap_err().contains("goto, snapshot"));
        assert!(action_name(&json!({})).is_err());
    }

    #[test]
    fn refs_may_be_numbers_or_bracketed_text() {
        assert_eq!(reference(&json!({"ref": 4})), Ok(4));
        assert_eq!(reference(&json!({"ref": "[12]"})), Ok(12));
        assert!(reference(&json!({})).is_err());
        assert!(reference(&json!({"ref": "x"})).is_err());
    }

    #[test]
    fn bare_domains_get_https_and_files_stay_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = egui_bench::agent::workspace::Workspace::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.html"), "x").unwrap();
        assert_eq!(address(&ws, "example.com/x").unwrap(), "https://example.com/x");
        assert_eq!(address(&ws, "localhost:3000").unwrap(), "http://localhost:3000");
        assert_eq!(address(&ws, "about:blank").unwrap(), "about:blank");
        assert!(address(&ws, "a.html").unwrap().starts_with("file://"));
        assert!(address(&ws, "ftp://x").is_err());
    }

    #[tokio::test]
    async fn connecting_to_a_dead_address_says_so_and_keeps_the_tool_usable() {
        let dir = tempfile::tempdir().unwrap();
        let ws = egui_bench::agent::workspace::Workspace::open(dir.path()).unwrap();
        let stop = CancellationToken::new();
        let browsers = Browsers::default();
        let args = json!({"action": "connect", "target": "127.0.0.1:1"});
        let err = browsers.act("s", &ws, &args, &stop).await.unwrap_err();
        assert!(err.contains("127.0.0.1:1"), "{err}");
    }

    #[test]
    fn the_target_is_headless_chrome_or_an_address() {
        assert_eq!(target("headlesschrome"), Ok(None));
        assert_eq!(target("HeadlessChrome"), Ok(None));
        assert_eq!(target(""), Ok(None));
        assert_eq!(target("10.0.0.5:9222"), Ok(Some("ws://10.0.0.5:9222/devtools/browser".into())));
        assert_eq!(target("9222"), Ok(Some("ws://127.0.0.1:9222/devtools/browser".into())));
    }

    #[test]
    fn the_browser_setting_becomes_a_websocket_address() {
        assert_eq!(endpoint("9222").as_deref(), Some("ws://127.0.0.1:9222/devtools/browser"));
        assert_eq!(
            endpoint("http://127.0.0.1:35215/").as_deref(),
            Some("ws://127.0.0.1:35215/devtools/browser")
        );
        assert_eq!(
            endpoint("ws://h:1/devtools/browser/x").as_deref(),
            Some("ws://h:1/devtools/browser/x")
        );
        assert_eq!(endpoint("  "), None);
    }

    #[test]
    fn eval_results_print_as_plain_text_or_json() {
        assert_eq!(eval_text(&json!("hi")), "hi");
        assert_eq!(eval_text(&Value::Null), "null");
        assert_eq!(eval_text(&json!([1, 2])), "[\n  1,\n  2\n]");
    }

    const FORM: &str = r##"<html><head><title>Form</title></head><body>
<h1>Sign up</h1>
<div style="position:relative;width:200px"><input id=q placeholder="Find"><span style="position:absolute;left:0;top:0;width:24px;height:100%"></span></div>
<label>Name <input id=n placeholder="Your name"></label><br>
<select id=s><option value=one>One</option><option value=two>Two</option></select><br>
<label><input id=c type=checkbox> Agree</label><br>
<button id=open onclick="document.getElementById('m').style.display='block'">Open</button>
<a href="#x" id=behind>Behind link</a>
<div id=m style="display:none;position:fixed;top:0;left:0;width:100%;height:100%;background:#fff;z-index:9">
<h2>Confirm</h2><p>Are you sure?</p>
<button id=yes onclick="document.getElementById('m').style.display='none';document.title='confirmed'">Yes</button>
</div></body></html>"##;

    fn ref_of(grid: &str, needle: &str) -> usize {
        let line = grid
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} in {grid}"));
        let at = line.find(needle).unwrap();
        let open = line[..at].rfind('[').unwrap();
        line[open + 1..].split(|c: char| !c.is_ascii_digit()).next().unwrap().parse().unwrap()
    }

    fn workspace_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(name), body).unwrap();
        }
        let ws = Workspace::open(dir.path()).unwrap();
        (dir, ws)
    }

    async fn act(b: &Browsers, ws: &Workspace, args: Value) -> Result<String, String> {
        b.act("s1", ws, &args, &CancellationToken::new()).await
    }

    const QUESTIONS: &str = r##"<html><body>
<p>Does the game show real-world animals?</p>
<div role=radiogroup>
<div role=radio aria-checked=false tabindex=0>Yes</div>
<div role=radio aria-checked=false tabindex=-1>No</div>
</div>
<p>Is the violence realistic?</p>
<div><span style="position:relative"><input type=radio name=v value=yes style="opacity:0;position:absolute;left:0;width:16px;height:16px;margin:0"><span style="padding-left:20px">Yes</span></span></div>
<div><span style="position:relative"><input type=radio name=v value=no style="opacity:0;position:absolute;left:0;width:16px;height:16px;margin:0"><span style="padding-left:20px">No</span></span></div>
<script>
for (const el of document.querySelectorAll('[role=radio]')) el.addEventListener('click', () => {
  for (const r of el.parentElement.children) { r.setAttribute('aria-checked', r === el); r.tabIndex = r === el ? 0 : -1; }
});
</script>
</body></html>"##;

    const RERENDER: &str = r##"<html><body>
<a href="#keep" id=keep>Keep</a>
<div id=app><button onclick="document.title='started'">Start declaration</button></div>
<script>
function rerender() {
  document.getElementById('app').innerHTML = '<a href="privacy.html">Privacy</a>';
}
</script>
</body></html>"##;

    #[tokio::test]
    async fn a_ref_from_an_older_grid_is_refused_once_its_element_is_gone() {
        if screenshot::browser().is_none() {
            return;
        }
        let (_dir, ws) = workspace_with(&[("r.html", RERENDER), ("privacy.html", "privacy")]);
        let b = Browsers::default();
        let first = act(&b, &ws, json!({"action": "goto", "url": "r.html"})).await.unwrap();
        let start = ref_of(&first, "Start declaration]");
        let keep = ref_of(&first, "]Keep");
        act(&b, &ws, json!({"action": "eval", "js": "rerender()"})).await.unwrap();
        let second = act(&b, &ws, json!({"action": "snapshot"})).await.unwrap();
        assert!(second.contains("Privacy"), "{second}");
        assert_eq!(ref_of(&second, "]Keep"), keep, "{second}");
        assert_ne!(ref_of(&second, "]Privacy"), start, "{second}");
        let err = act(&b, &ws, json!({"action": "click", "ref": start})).await.unwrap_err();
        assert!(err.contains("gone"), "{err}");
        let url = act(&b, &ws, json!({"action": "eval", "js": "location.pathname"})).await.unwrap();
        assert!(url.ends_with("r.html"), "{url}");
        let third = act(&b, &ws, json!({"action": "goto", "url": "r.html"})).await.unwrap();
        assert!(ref_of(&third, "]Keep") > ref_of(&second, "]Privacy"), "{third}");
    }

    async fn serve(routes: Vec<(&'static str, u64, &'static str)>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let cookie = head
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("Cookie: ").or_else(|| l.strip_prefix("cookie: "))
                        })
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let (_, delay, body) =
                        routes.iter().find(|r| r.0 == path).copied().unwrap_or(("", 0, ""));
                    let body = body.replace("{cookie}", &cookie);
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        format!("http://{at}")
    }

    const SPA: &str = r##"<html><body><div id=app></div><script>
setTimeout(() => {
  app.innerHTML = '<p>Loading</p>';
  fetch('/data').then((r) => r.text()).then((t) => { app.innerHTML = '<h1>' + t + '</h1><button onclick="later()">Later</button>'; });
}, 1200);
function later() { setTimeout(() => { document.body.insertAdjacentHTML('beforeend', '<p>Arrived</p>'); }, 2500); }
</script></body></html>"##;

    #[tokio::test]
    async fn a_page_that_renders_late_is_waited_for() {
        if screenshot::browser().is_none() {
            return;
        }
        let base = serve(vec![("/", 0, SPA), ("/data", 1500, "Dashboard")]).await;
        let (_dir, ws) = workspace_with(&[]);
        let b = Browsers::default();
        let grid = act(&b, &ws, json!({"action": "goto", "url": base})).await.unwrap();
        assert!(grid.contains("# Dashboard"), "{grid}");
        let grid = act(&b, &ws, json!({"action": "click", "text": "Later"})).await.unwrap();
        assert!(!grid.contains("Arrived"), "{grid}");
        let grid = act(&b, &ws, json!({"action": "wait", "text": "Arrived"})).await.unwrap();
        assert!(grid.contains("Arrived"), "{grid}");
        let err =
            act(&b, &ws, json!({"action": "wait", "text": "Never", "ms": 600})).await.unwrap_err();
        assert!(err.contains("Never"), "{err}");
    }

    #[tokio::test]
    async fn a_page_that_never_stops_moving_is_not_waited_on_for_long() {
        if screenshot::browser().is_none() {
            return;
        }
        let clock = "<html><body><p id=t>0</p><script>setInterval(() => { t.textContent = Date.now(); }, 50);</script></body></html>";
        let (_dir, ws) = workspace_with(&[("clock.html", clock)]);
        let b = Browsers::default();
        act(&b, &ws, json!({"action": "goto", "url": "clock.html"})).await.unwrap();
        let started = Instant::now();
        act(&b, &ws, json!({"action": "snapshot"})).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_lost_browser_session_is_reopened_on_the_same_page() {
        if screenshot::browser().is_none() {
            return;
        }
        let (_dir, ws) = workspace_with(&[("r.html", RERENDER)]);
        let b = Browsers::default();
        act(&b, &ws, json!({"action": "goto", "url": "r.html"})).await.unwrap();
        {
            let slot = b.slot("s1");
            let guard = slot.lock().await;
            let live = guard.as_ref().unwrap();
            live.cdp
                .call(None, "Target.detachFromTarget", json!({ "sessionId": live.target }))
                .await
                .unwrap();
        }
        let grid = act(&b, &ws, json!({"action": "snapshot"})).await.unwrap();
        assert!(grid.contains("r.html") && grid.contains("Keep"), "{grid}");
        {
            let slot = b.slot("s1");
            let guard = slot.lock().await;
            let live = guard.as_ref().unwrap();
            live.cdp
                .call(None, "Target.closeTarget", json!({ "targetId": live.tab }))
                .await
                .unwrap();
        }
        let grid = act(&b, &ws, json!({"action": "click", "text": "Keep"})).await.unwrap();
        assert!(grid.contains("r.html#keep"), "{grid}");
    }

    #[test]
    fn a_chosen_browser_target_outlives_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let before = Browsers::saved_in(dir.path().to_path_buf());
        before.remember("s1", Some("ws://127.0.0.1:9222/devtools/browser"));
        before.remember("s2", None);
        let after = Browsers::saved_in(dir.path().to_path_buf());
        assert_eq!(after.wanted("s1").as_deref(), Some("ws://127.0.0.1:9222/devtools/browser"));
        assert_eq!(after.wanted("s2"), None);
        after.close("s1");
        assert_eq!(Browsers::saved_in(dir.path().to_path_buf()).wanted("s1"), None);
    }

    const FILES: &str = r##"<html><body>
<input type=file id=f multiple onchange="document.title='got ' + [...this.files].map((f) => f.name + ':' + f.size).join(',')">
<button onclick="document.getElementById('h').click()">Choose screenshot</button>
<input type=file id=h style="display:none" onchange="out.textContent = 'picked ' + this.files[0].name">
<p id=out></p>
<a href="/cert.cer" download>Download certificate</a>
<button onclick="const a = document.createElement('a'); a.href = window.URL.createObjectURL(new Blob(['made here'])); a.download = 'made.txt'; a.click()">Export</button>
<button>Nothing</button>
<script>document.cookie = 'auth=1';</script>
</body></html>"##;

    #[tokio::test]
    async fn files_are_uploaded_and_downloads_are_saved() {
        if screenshot::browser().is_none() {
            return;
        }
        let base = serve(vec![("/", 0, FILES), ("/cert.cer", 0, "cert for {cookie}")]).await;
        let (dir, ws) = workspace_with(&[("shot.png", "12345"), ("two.png", "12")]);
        let b = Browsers::default();
        let grid = act(&b, &ws, json!({"action": "goto", "url": base})).await.unwrap();
        let input = ref_of(&grid, ":file]");
        let grid = act(
            &b,
            &ws,
            json!({"action": "upload", "ref": input, "files": ["shot.png", "two.png"]}),
        )
        .await
        .unwrap();
        assert!(grid.contains("title: got shot.png:5,two.png:2"), "{grid}");
        let grid = act(
            &b,
            &ws,
            json!({"action": "upload", "label": "Choose screenshot", "files": ["shot.png"]}),
        )
        .await
        .unwrap();
        assert!(grid.contains("picked shot.png"), "{grid}");
        let missing =
            act(&b, &ws, json!({"action": "upload", "ref": input, "files": ["nope.png"]}))
                .await
                .unwrap_err();
        assert!(missing.contains("nope.png"), "{missing}");
        let out = act(
            &b,
            &ws,
            json!({"action": "download", "label": "Download certificate", "save_to": "keys/a.cer"}),
        )
        .await
        .unwrap();
        assert!(out.contains("keys/a.cer"), "{out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keys/a.cer")).unwrap(),
            "cert for auth=1"
        );
        let out =
            act(&b, &ws, json!({"action": "download", "url": "cert.cer", "save_to": "b.cer"}))
                .await
                .unwrap();
        assert!(out.contains("b.cer"), "{out}");
        assert_eq!(std::fs::read_to_string(dir.path().join("b.cer")).unwrap(), "cert for auth=1");
        let out = act(&b, &ws, json!({"action": "download", "label": "Export"})).await.unwrap();
        assert!(out.contains("made.txt"), "{out}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("downloads/made.txt")).unwrap(),
            "made here"
        );
        let none = act(&b, &ws, json!({"action": "download", "label": "Nothing", "ms": 1500}))
            .await
            .unwrap_err();
        assert!(none.contains("no download"), "{none}");
    }

    #[tokio::test]
    async fn every_radio_in_a_group_gets_a_ref() {
        if screenshot::browser().is_none() {
            return;
        }
        let (_dir, ws) = workspace_with(&[("q.html", QUESTIONS)]);
        let b = Browsers::default();
        let grid = act(&b, &ws, json!({"action": "goto", "url": "q.html"})).await.unwrap();
        let nos: Vec<&str> = grid.lines().filter(|l| l.contains("No")).collect();
        assert_eq!(nos.len(), 2, "{grid}");
        assert!(nos.iter().all(|l| l.contains(":\u{25cb}]")), "{grid}");
        let aria_no = ref_of(nos[0], ":\u{25cb}] No");
        let grid = act(&b, &ws, json!({"action": "click", "ref": aria_no})).await.unwrap();
        assert!(grid.contains(&format!("[{aria_no}:\u{25cf}] No")), "{grid}");
        let native_no =
            ref_of(grid.lines().filter(|l| l.contains("No")).nth(1).unwrap(), ":\u{25cb}]");
        act(&b, &ws, json!({"action": "click", "ref": native_no})).await.unwrap();
        let picked = act(
            &b,
            &ws,
            json!({"action": "eval", "js": "document.querySelector('input:checked').value"}),
        )
        .await
        .unwrap();
        assert_eq!(picked, "no");
    }

    #[tokio::test]
    async fn a_click_by_label_text_picks_the_answer_near_its_question() {
        if screenshot::browser().is_none() {
            return;
        }
        let (_dir, ws) = workspace_with(&[("q.html", QUESTIONS)]);
        let b = Browsers::default();
        act(&b, &ws, json!({"action": "goto", "url": "q.html"})).await.unwrap();
        let vague = act(&b, &ws, json!({"action": "click", "text": "No"})).await.unwrap_err();
        assert!(vague.contains("2 elements") && vague.contains("near"), "{vague}");
        let grid =
            act(&b, &ws, json!({"action": "click", "text": "No", "near": "violence realistic"}))
                .await
                .unwrap();
        let picked = act(
            &b,
            &ws,
            json!({"action": "eval", "js": "document.querySelector('input:checked').value"}),
        )
        .await
        .unwrap();
        assert_eq!(picked, "no", "{grid}");
        act(&b, &ws, json!({"action": "click", "label": "no", "near": "real-world animals"}))
            .await
            .unwrap();
        let aria = act(&b, &ws, json!({"action": "eval", "js": "document.querySelectorAll('[role=radio]')[1].getAttribute('aria-checked')"}))
            .await
            .unwrap();
        assert_eq!(aria, "true");
        let missing = act(&b, &ws, json!({"action": "click", "text": "Maybe"})).await.unwrap_err();
        assert!(missing.contains("Maybe"), "{missing}");
    }

    #[tokio::test]
    async fn a_form_and_a_modal_are_driven_through_the_grid_when_a_browser_is_installed() {
        if screenshot::browser().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("form.html"), FORM).unwrap();
        let ws = egui_bench::agent::workspace::Workspace::open(dir.path()).unwrap();
        let stop = CancellationToken::new();
        let browsers = Browsers::default();
        let act = |args: Value| {
            let (b, ws, stop) = (browsers.clone(), ws.clone(), stop.clone());
            async move { b.act("s1", &ws, &args, &stop).await }
        };
        let grid = act(json!({"action": "goto", "url": "form.html"})).await.unwrap();
        assert!(grid.contains("title: Form") && grid.contains("# Sign up"), "{grid}");
        assert!(grid.contains(":___ Find]"), "{grid}");
        let name = ref_of(&grid, ":___ Your name");
        let grid = act(json!({"action": "type", "ref": name, "text": "Bob"})).await.unwrap();
        assert!(grid.contains(&format!("[{name}:Bob]")), "{grid}");
        let select = ref_of(&grid, "\u{25bc} One");
        let grid = act(json!({"action": "select", "ref": select, "value": "Two"})).await.unwrap();
        assert!(grid.contains("\u{25bc} Two"), "{grid}");
        let check = ref_of(&grid, ": ] Agree");
        let grid = act(json!({"action": "click", "ref": check})).await.unwrap();
        assert!(grid.contains(&format!("[{check}:X] Agree")), "{grid}");
        let open = ref_of(&grid, "Open]");
        let grid = act(json!({"action": "click", "ref": open})).await.unwrap();
        assert!(grid.contains("## Confirm") && grid.contains("Are you sure?"), "{grid}");
        assert!(!grid.contains("Behind link") && !grid.contains("Sign up"), "{grid}");
        let covered = act(json!({"action": "click", "ref": open})).await.unwrap_err();
        assert!(covered.contains("gone") || covered.contains("covered"), "{covered}");
        let yes = ref_of(&grid, "Yes]");
        let grid = act(json!({"action": "click", "ref": yes})).await.unwrap();
        assert!(grid.contains("title: confirmed") && grid.contains("Behind link"), "{grid}");
        let value = act(json!({"action": "eval", "js": "document.getElementById('n').value"}))
            .await
            .unwrap();
        assert_eq!(value, "Bob");
        let bad = act(json!({"action": "eval", "js": "nope.nope"})).await.unwrap_err();
        assert!(bad.contains("ReferenceError"), "{bad}");
        assert_eq!(browsers.open(), 1);
        browsers.close("s1");
        assert_eq!(browsers.open(), 0);
    }
}
