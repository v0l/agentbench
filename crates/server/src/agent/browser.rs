use super::screenshot;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
const LOAD_WAIT: Duration = Duration::from_secs(15);
const ROW_SLACK: f64 = 9.0;
const PARAGRAPH_GAP: f64 = 36.0;
const EXTRACT: &str = include_str!("browser/extract.js");
const TARGET: &str = include_str!("browser/target.js");
const SELECT: &str = include_str!("browser/select.js");

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
        "checkbox" => format!("[{r}:{}{off}]", if n.on { "X" } else { " " }),
        "radio" => format!("[{r}:{}{off}]", if n.on { "\u{25cf}" } else { "\u{25cb}" }),
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

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct Cdp {
    out: mpsc::UnboundedSender<Message>,
    pending: Pending,
    next: AtomicU64,
    dialogs: Arc<Mutex<Vec<String>>>,
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
        let closed = Arc::new(AtomicBool::new(false));
        tokio::spawn(async move {
            while let Some(m) = queue.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
        });
        let (p, d, c, o) = (pending.clone(), dialogs.clone(), closed.clone(), out.clone());
        tokio::spawn(async move {
            while let Some(Ok(m)) = stream.next().await {
                let Message::Text(text) = m else { continue };
                match route(text.as_str()) {
                    Incoming::Reply { id, result } => {
                        let tx = p.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
                        if let Some(tx) = tx {
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
                    Incoming::Other => {}
                }
            }
            c.store(true, Ordering::SeqCst);
            p.lock().unwrap_or_else(|e| e.into_inner()).clear();
        });
        Ok(Cdp { out, pending, next: AtomicU64::new(1), dialogs, closed })
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
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
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
}

struct Live {
    cdp: Cdp,
    target: String,
    tab: String,
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
    Ok(Live { cdp, target, tab, _child: None, dir: None, used: Instant::now() })
}

async fn session_for(cdp: &Cdp, tab: &str) -> Result<String, String> {
    let attached = cdp
        .call(None, "Target.attachToTarget", json!({ "targetId": tab, "flatten": true }))
        .await?;
    let target = attached["sessionId"].as_str().ok_or("no browser session")?.to_string();
    cdp.call(Some(&target), "Page.enable", json!({})).await?;
    Ok(target)
}

async fn launch() -> Result<Live, String> {
    if let Some(url) = std::env::var("AGENTBENCH_BROWSER").ok().and_then(|v| endpoint(&v)) {
        return attach(&url).await;
    }
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
        _child: Some(child),
        dir: Some(dir.to_path_buf()),
        used: Instant::now(),
    })
}

impl Live {
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
        while started.elapsed() < LOAD_WAIT {
            if let Ok(Value::String(s)) = self.eval("document.readyState").await {
                if s == "complete" {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tokio::time::sleep(Duration::from_millis(350)).await;
    }

    async fn page(&self) -> Result<Page, String> {
        let mut last = String::new();
        for _ in 0..3 {
            match self.eval(EXTRACT).await {
                Ok(Value::String(s)) => return parse_page(&s),
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
    let v = args.get("ref").ok_or("missing `ref`: the number in [brackets] from the last grid")?;
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
        "eval" | "evaluate" | "js" => "eval",
        other => {
            return Err(format!(
                "unknown action {other}: use goto, snapshot, click, type, select, press, scroll or eval"
            ));
        }
    })
}

#[derive(Clone, Default)]
pub struct Browsers(Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Live>>>>>>);

impl Browsers {
    pub fn close(&self, session: &str) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(session);
    }

    pub fn open(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    fn slot(&self, session: &str) -> Arc<tokio::sync::Mutex<Option<Live>>> {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
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
        if guard.as_ref().is_some_and(|l| l.cdp.closed.load(Ordering::SeqCst)) {
            *guard = None;
        }
        if guard.is_none() {
            *guard = Some(tokio::select! {
                l = launch() => l?,
                _ = stop.cancelled() => return Err("stopped".into()),
            });
        }
        let live = guard.as_mut().ok_or("no browser")?;
        live.used = Instant::now();
        let done = tokio::select! {
            r = run(live, action, url.as_deref(), args) => r,
            _ = stop.cancelled() => Err("stopped".into()),
        };
        live.used = Instant::now();
        if live.cdp.closed.load(Ordering::SeqCst) {
            *guard = None;
        }
        done
    }
}

async fn run(live: &Live, action: &str, url: Option<&str>, args: &Value) -> Result<String, String> {
    match action {
        "goto" => live.goto(url.unwrap_or("about:blank")).await?,
        "snapshot" => {}
        "click" => live.click(reference(args)?).await?,
        "type" => {
            let text = text_arg(args, &["text", "value"]).ok_or("missing `text`")?;
            let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
            live.type_into(reference(args)?, text, submit).await?
        }
        "select" => {
            let value = text_arg(args, &["value", "text"]).ok_or("missing `value`")?;
            live.select(reference(args)?, value).await?
        }
        "press" => live.press(text_arg(args, &["key"]).ok_or("missing `key`")?).await?,
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
