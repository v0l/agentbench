use super::config::{Kind, Server};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub const PROTOCOL: &str = "2025-06-18";
const STDERR_KEPT: usize = 4_000;
const PAGES: usize = 20;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub name: String,
    pub about: String,
    pub schema: Value,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;
type Writer = Arc<tokio::sync::Mutex<ChildStdin>>;

#[derive(Default)]
struct Shared {
    dead: AtomicBool,
    stale: AtomicBool,
    roots: Vec<String>,
}

struct Stdio {
    writer: Writer,
    pending: Pending,
    stderr: Arc<Mutex<String>>,
    child: Mutex<Child>,
}

struct Http {
    url: String,
    headers: Vec<(String, String)>,
    session: Mutex<Option<String>>,
    version: Mutex<Option<String>>,
}

enum Link {
    Stdio(Stdio),
    Http(Http),
}

pub struct Client {
    pub name: String,
    pub title: String,
    pub instructions: String,
    link: Link,
    next: AtomicI64,
    shared: Arc<Shared>,
    tools: Mutex<Option<Arc<Vec<Tool>>>>,
}

fn id_of(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_str()?.trim().parse().ok())
}

fn error_of(e: &Value) -> String {
    let message = e.get("message").and_then(Value::as_str).unwrap_or("error");
    match e.get("code").and_then(Value::as_i64) {
        Some(code) => format!("{message} ({code})"),
        None => message.to_string(),
    }
}

fn outcome(msg: &Value) -> Result<Value, String> {
    match msg.get("error") {
        Some(e) if !e.is_null() => Err(error_of(e)),
        _ => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
    }
}

fn answer(method: &str, roots: &[String]) -> Result<Value, (i64, String)> {
    match method {
        "ping" => Ok(json!({})),
        "roots/list" => Ok(json!({
            "roots": roots.iter().map(|uri| json!({ "uri": uri })).collect::<Vec<_>>(),
        })),
        other => Err((-32601, format!("{other} is not supported by this client"))),
    }
}

fn reply_to(id: &Value, method: &str, roots: &[String]) -> Value {
    match answer(method, roots) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => {
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
        }
    }
}

fn messages(v: Value) -> Vec<Value> {
    match v {
        Value::Array(list) => list,
        one => vec![one],
    }
}

async fn write(writer: &Writer, msg: &Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    line.push(b'\n');
    let mut w = writer.lock().await;
    let sent = async {
        w.write_all(&line).await?;
        w.flush().await
    };
    sent.await.map_err(|e| format!("the server is gone ({e})"))
}

async fn pump(out: ChildStdout, writer: Writer, pending: Pending, shared: Arc<Shared>) {
    let mut lines = BufReader::new(out).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        for msg in messages(v) {
            let method = msg.get("method").and_then(Value::as_str);
            let id = msg.get("id").filter(|i| !i.is_null());
            match (method, id) {
                (Some(method), Some(id)) => {
                    let _ = write(&writer, &reply_to(id, method, &shared.roots)).await;
                }
                (Some(method), None) => notice(method, &shared),
                (None, Some(id)) => {
                    let tx = id_of(id).and_then(|id| lock(&pending).remove(&id));
                    if let Some(tx) = tx {
                        let _ = tx.send(outcome(&msg));
                    }
                }
                (None, None) => {}
            }
        }
    }
    shared.dead.store(true, Ordering::SeqCst);
    lock(&pending).clear();
}

fn notice(method: &str, shared: &Shared) {
    if method == "notifications/tools/list_changed" {
        shared.stale.store(true, Ordering::SeqCst);
    }
}

fn keep_stderr(mut err: tokio::process::ChildStderr, kept: Arc<Mutex<String>>) {
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        while let Ok(n) = err.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let mut k = lock(&kept);
            k.push_str(&String::from_utf8_lossy(&buf[..n]));
            if k.len() > STDERR_KEPT {
                let cut = k.ceil_char_boundary(k.len() - STDERR_KEPT);
                k.drain(..cut);
            }
        }
    });
}

pub fn search_path(extra: &[PathBuf]) -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").iter().flat_map(std::env::split_paths).collect();
    for d in extra {
        if d.is_dir() && !dirs.contains(d) {
            dirs.push(d.clone());
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

fn program(command: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    let cmd = Path::new(command);
    if cmd.components().count() > 1 {
        return cmd.is_file().then(|| cmd.to_path_buf());
    }
    std::env::split_paths(path).map(|d| d.join(cmd)).find(|p| p.is_file())
}

fn spawn(server: &Server, root: &Path, tool_dirs: &[PathBuf]) -> Result<Child, String> {
    let path = search_path(tool_dirs);
    let command = server.command.trim();
    let found = program(command, &path).ok_or_else(|| {
        format!("`{command}` is not on PATH or in the usual tool folders; install it or give its full path")
    })?;
    let cwd = match server.cwd.trim() {
        "" => root.to_path_buf(),
        dir if Path::new(dir).is_absolute() => PathBuf::from(dir),
        dir => root.join(dir),
    };
    let mut cmd = Command::new(found);
    cmd.args(&server.args)
        .current_dir(&cwd)
        .env("PATH", &path)
        .envs(server.env.iter())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd.spawn().map_err(|e| format!("{command}: {e}"))
}

pub fn root_uri(root: &Path) -> Option<String> {
    url::Url::from_directory_path(root).ok().map(String::from)
}

impl Client {
    pub async fn start(
        server: &Server,
        root: &Path,
        tool_dirs: &[PathBuf],
        me: &str,
        limit: Duration,
    ) -> Result<Client, String> {
        let roots: Vec<String> = root_uri(root).into_iter().collect();
        let shared = Arc::new(Shared { roots, ..Shared::default() });
        let link = match server.kind()? {
            Kind::Stdio => {
                let mut child = spawn(server, root, tool_dirs)?;
                let stderr: Arc<Mutex<String>> = Default::default();
                if let Some(err) = child.stderr.take() {
                    keep_stderr(err, stderr.clone());
                }
                let stdin = child.stdin.take().ok_or("no stdin")?;
                let stdout = child.stdout.take().ok_or("no stdout")?;
                let writer: Writer = Arc::new(tokio::sync::Mutex::new(stdin));
                let pending: Pending = Default::default();
                tokio::spawn(pump(stdout, writer.clone(), pending.clone(), shared.clone()));
                Link::Stdio(Stdio { writer, pending, stderr, child: Mutex::new(child) })
            }
            Kind::Http => Link::Http(Http {
                url: server.url.trim().to_string(),
                headers: server.headers(),
                session: Mutex::new(None),
                version: Mutex::new(None),
            }),
        };
        let mut client = Client {
            name: server.name.clone(),
            title: String::new(),
            instructions: String::new(),
            link,
            next: AtomicI64::new(1),
            shared,
            tools: Mutex::new(None),
        };
        let params = json!({
            "protocolVersion": PROTOCOL,
            "capabilities": { "roots": { "listChanged": false } },
            "clientInfo": { "name": me, "version": env!("CARGO_PKG_VERSION") },
        });
        let never = CancellationToken::new();
        let init = match client.request("initialize", params, limit, &never).await {
            Ok(v) => v,
            Err(e) => return Err(client.with_stderr(e).await),
        };
        if let Link::Http(h) = &client.link {
            let version = init.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL);
            *lock(&h.version) = Some(version.to_string());
        }
        let info = init.get("serverInfo");
        let field = |k: &str| info.and_then(|i| i.get(k)).and_then(Value::as_str).unwrap_or("");
        client.title = match (field("title"), field("name"), field("version")) {
            ("", "", _) => String::new(),
            ("", name, v) | (name, _, v) => format!("{name} {v}").trim().to_string(),
        };
        client.instructions =
            init.get("instructions").and_then(Value::as_str).unwrap_or("").trim().to_string();
        client.notify("notifications/initialized", json!({})).await?;
        Ok(client)
    }

    async fn with_stderr(&self, e: String) -> String {
        let Link::Stdio(s) = &self.link else { return e };
        tokio::time::sleep(Duration::from_millis(100)).await;
        let said = lock(&s.stderr).trim().to_string();
        if said.is_empty() { e } else { format!("{e}: {said}") }
    }

    pub fn alive(&self) -> bool {
        !self.shared.dead.load(Ordering::SeqCst)
    }

    pub fn stderr(&self) -> String {
        match &self.link {
            Link::Stdio(s) => lock(&s.stderr).trim().to_string(),
            Link::Http(_) => String::new(),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        match &self.link {
            Link::Stdio(s) => write(&s.writer, &msg).await,
            Link::Http(h) => self.post(h, &msg, None, &CancellationToken::new()).await.map(|_| ()),
        }
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
        stop: &CancellationToken,
    ) -> Result<Value, String> {
        if !self.alive() {
            return Err(format!("{} has stopped", self.name));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let answered = match &self.link {
            Link::Stdio(s) => {
                let (tx, rx) = oneshot::channel();
                lock(&s.pending).insert(id, tx);
                if let Err(e) = write(&s.writer, &msg).await {
                    lock(&s.pending).remove(&id);
                    return Err(e);
                }
                let gone = || format!("{} stopped before it answered", self.name);
                tokio::select! {
                    r = rx => Some(r.unwrap_or_else(|_| Err(gone()))),
                    _ = tokio::time::sleep(limit) => None,
                    _ = stop.cancelled() => {
                        lock(&s.pending).remove(&id);
                        self.cancel(id, "stopped").await;
                        return Err("stopped".into());
                    }
                }
            }
            Link::Http(h) => {
                tokio::select! {
                    r = self.post(h, &msg, Some(id), stop) => {
                        Some(r.and_then(|m| m.map_or_else(|| Err(format!("{} sent no answer", self.name)), |m| outcome(&m))))
                    }
                    _ = tokio::time::sleep(limit) => None,
                }
            }
        };
        match answered {
            Some(r) => r,
            None => {
                if let Link::Stdio(s) = &self.link {
                    lock(&s.pending).remove(&id);
                }
                self.cancel(id, "timed out").await;
                Err(format!("{} did not answer {method} in {}s", self.name, limit.as_secs()))
            }
        }
    }

    async fn cancel(&self, id: i64, reason: &str) {
        let params = json!({ "requestId": id, "reason": reason });
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            self.notify("notifications/cancelled", params),
        )
        .await;
    }

    async fn post(
        &self,
        h: &Http,
        msg: &Value,
        id: Option<i64>,
        stop: &CancellationToken,
    ) -> Result<Option<Value>, String> {
        let mut req = super::super::http::client()
            .post(&h.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(msg.to_string());
        if let Some(s) = lock(&h.session).clone() {
            req = req.header("mcp-session-id", s);
        }
        if let Some(v) = lock(&h.version).clone() {
            req = req.header("mcp-protocol-version", v);
        }
        for (k, v) in &h.headers {
            req = req.header(k, v);
        }
        let resp = tokio::select! {
            r = req.send() => r.map_err(|e| format!("{}: {}", h.url, super::super::http::describe(&e)))?,
            _ = stop.cancelled() => return Err("stopped".into()),
        };
        let status = resp.status();
        if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
            *lock(&h.session) = Some(s.to_string());
        }
        if status.as_u16() == 404 && lock(&h.session).is_some() {
            self.shared.dead.store(true, Ordering::SeqCst);
            return Err(format!("{} ended the session", self.name));
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(format!(
                "{status}: {} wants a sign-in. OAuth is not supported; give it an Authorization header or a bearerToken",
                self.name
            ));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let text: String = text.trim().chars().take(500).collect();
            return Err(format!("{status} from {}: {text}", h.url));
        }
        if status.as_u16() == 202 || id.is_none() {
            return Ok(None);
        }
        let sse = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| c.contains("text/event-stream"));
        if !sse {
            let bytes = resp.bytes().await.map_err(|e| super::super::http::describe(&e))?;
            let v: Value = serde_json::from_slice(&bytes).map_err(|e| {
                format!("{} answered with something that is not JSON: {e}", self.name)
            })?;
            return Ok(messages(v).into_iter().find(|m| m.get("id").and_then(id_of) == id));
        }
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut data = String::new();
        loop {
            let chunk = tokio::select! {
                c = stream.next() => c,
                _ = stop.cancelled() => return Err("stopped".into()),
            };
            let Some(chunk) = chunk else { break };
            buf.extend_from_slice(&chunk.map_err(|e| super::super::http::describe(&e))?);
            while let Some(at) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=at).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    let event = std::mem::take(&mut data);
                    if let Some(found) = self.event(h, &event, id).await {
                        return Ok(Some(found));
                    }
                } else if let Some(d) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(d.strip_prefix(' ').unwrap_or(d));
                }
            }
        }
        Ok(self.event(h, &data, id).await)
    }

    async fn event(&self, h: &Http, data: &str, id: Option<i64>) -> Option<Value> {
        let v: Value = serde_json::from_str(data.trim()).ok()?;
        for msg in messages(v) {
            let method = msg.get("method").and_then(Value::as_str);
            let mid = msg.get("id").filter(|i| !i.is_null());
            match (method, mid) {
                (Some(method), Some(mid)) => {
                    let reply = reply_to(mid, method, &self.shared.roots);
                    let _ = Box::pin(self.post(h, &reply, None, &CancellationToken::new())).await;
                }
                (Some(method), None) => notice(method, &self.shared),
                (None, Some(mid)) if id_of(mid) == id => return Some(msg),
                _ => {}
            }
        }
        None
    }

    pub fn cached_tools(&self) -> Option<usize> {
        lock(&self.tools).as_ref().map(|t| t.len())
    }

    pub async fn tools(
        &self,
        limit: Duration,
        stop: &CancellationToken,
    ) -> Result<Arc<Vec<Tool>>, String> {
        if !self.shared.stale.swap(false, Ordering::SeqCst)
            && let Some(t) = lock(&self.tools).clone()
        {
            return Ok(t);
        }
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let page = self.request("tools/list", params, limit, stop).await?;
            for t in page.get("tools").and_then(Value::as_array).into_iter().flatten() {
                let Some(name) = t.get("name").and_then(Value::as_str) else { continue };
                let title = t.get("title").and_then(Value::as_str).unwrap_or("");
                let about = t.get("description").and_then(Value::as_str).unwrap_or(title);
                out.push(Tool {
                    name: name.to_string(),
                    about: about.trim().to_string(),
                    schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" })),
                });
            }
            cursor = page.get("nextCursor").and_then(Value::as_str).map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        let out = Arc::new(out);
        *lock(&self.tools) = Some(out.clone());
        Ok(out)
    }

    pub async fn call(
        &self,
        tool: &str,
        args: Value,
        limit: Duration,
        stop: &CancellationToken,
    ) -> Result<Value, String> {
        let params = json!({ "name": tool, "arguments": args });
        self.request("tools/call", params, limit, stop).await
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        match &self.link {
            Link::Stdio(s) =>
            {
                #[cfg(unix)]
                if let Some(pid) = lock(&s.child).id() {
                    let _ = std::process::Command::new("kill")
                        .args(["-TERM", "--", &format!("-{pid}")])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                }
            }
            Link::Http(h) => {
                let Some(session) = lock(&h.session).clone() else { return };
                let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
                let mut req =
                    super::super::http::client().delete(&h.url).header("mcp-session-id", session);
                for (k, v) in &h.headers {
                    req = req.header(k, v);
                }
                rt.spawn(async move {
                    let _ = tokio::time::timeout(Duration::from_secs(5), req.send()).await;
                });
            }
        }
    }
}
