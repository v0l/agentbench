//! A small, language-agnostic client for language servers (rust-analyzer, gopls, clangd,
//! typescript-language-server, pyright...), spoken to over stdio.
//!
//! It only does what the code tools need: start a server and wait until it has indexed, keep the
//! files it is asked about in sync with the disk, and ask for symbols, references and diagnostics.
//! Nothing in here knows about a particular language.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use url::Url;

/// Files kept open in a server at once; the oldest are closed past this.
const MAX_DOCS: usize = 400;
/// How often a request the server gave up on half way ("content modified") is sent again.
const RETRIES: usize = 4;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A line and a column, both from 0, as the protocol counts them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub start: Pos,
    pub end: Pos,
}

impl Span {
    pub fn contains(&self, p: Pos) -> bool {
        self.start <= p && p <= self.end
    }
}

pub fn pos_of(v: &Value) -> Pos {
    Pos {
        line: v.get("line").and_then(Value::as_u64).unwrap_or(0) as u32,
        col: v.get("character").and_then(Value::as_u64).unwrap_or(0) as u32,
    }
}

pub fn span_of(v: &Value) -> Span {
    Span { start: pos_of(&v["start"]), end: pos_of(&v["end"]) }
}

/// One named thing in a file: a function, a struct, an enum, a variant, a module...
/// A file's symbols are kept flat, in the order they appear, with `depth` saying how deeply each
/// is nested, so the children of symbol `i` are the entries after it that are deeper.
#[derive(Clone, Debug, PartialEq)]
pub struct Sym {
    pub name: String,
    pub kind: u32,
    pub detail: String,
    /// The whole item, attributes and body included.
    pub range: Span,
    /// Where its name is.
    pub sel: Pos,
    pub parent: Option<usize>,
    pub depth: usize,
}

/// Flatten what `textDocument/documentSymbol` answered, whichever of its two shapes it used.
pub fn parse_symbols(v: &Value) -> Vec<Sym> {
    fn walk(list: &[Value], parent: Option<usize>, depth: usize, out: &mut Vec<Sym>) {
        for item in list {
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            let kind = item.get("kind").and_then(Value::as_u64).unwrap_or(0) as u32;
            let detail = item.get("detail").and_then(Value::as_str).unwrap_or_default().to_string();
            // DocumentSymbol has `range`; the older SymbolInformation has `location.range`.
            let range = item
                .get("range")
                .or_else(|| item.pointer("/location/range"))
                .map(span_of)
                .unwrap_or_default();
            let sel = item.get("selectionRange").map(|r| span_of(r).start).unwrap_or(range.start);
            let at = out.len();
            out.push(Sym { name, kind, detail, range, sel, parent, depth });
            if let Some(children) = item.get("children").and_then(Value::as_array) {
                walk(children, Some(at), depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(list) = v.as_array() {
        walk(list, None, 0, &mut out);
    }
    out
}

/// The indices of the symbols nested inside symbol `i`, however deep.
pub fn inside(syms: &[Sym], i: usize) -> std::ops::Range<usize> {
    let depth = syms[i].depth;
    let end = syms[i + 1..].iter().position(|s| s.depth <= depth).map_or(syms.len(), |n| i + 1 + n);
    i + 1..end
}

/// A problem a server reported.
#[derive(Clone, Debug, PartialEq)]
pub struct Diag {
    pub pos: Pos,
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub code: String,
    pub source: String,
    pub message: String,
}

pub fn parse_diags(list: &[Value]) -> Vec<Diag> {
    list.iter()
        .map(|d| Diag {
            pos: d.get("range").map(|r| span_of(r).start).unwrap_or_default(),
            severity: d.get("severity").and_then(Value::as_u64).unwrap_or(1) as u8,
            code: match d.get("code") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            },
            source: d.get("source").and_then(Value::as_str).unwrap_or_default().to_string(),
            message: d.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
        })
        .collect()
}

#[derive(Default)]
struct State {
    /// Work-done progress tokens that have begun and not ended.
    progress: HashSet<String>,
    /// What rust-analyzer's `experimental/serverStatus` last said; other servers never say.
    quiescent: Option<bool>,
    /// The last time a server sent progress or status.
    changed: Option<Instant>,
    /// What the server last published, by file, and when.
    published: HashMap<String, (Instant, Vec<Value>)>,
    dead: bool,
}

struct Doc {
    version: i64,
    text: String,
    language: String,
    symbols: Option<Arc<Vec<Sym>>>,
}

#[derive(Default)]
struct Docs {
    map: HashMap<PathBuf, Doc>,
    /// Oldest first.
    order: Vec<PathBuf>,
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, (i64, String)>>>>>;
type Writer = Arc<tokio::sync::Mutex<ChildStdin>>;

pub struct Client {
    pub name: String,
    root: PathBuf,
    writer: Writer,
    next: AtomicI64,
    pending: Pending,
    state: Arc<Mutex<State>>,
    caps: Value,
    started: Instant,
    docs: Mutex<Docs>,
    _child: Mutex<Child>,
}

async fn send(writer: &Writer, msg: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    let mut w = writer.lock().await;
    let head = format!("Content-Length: {}\r\n\r\n", body.len());
    let sent = async {
        w.write_all(head.as_bytes()).await?;
        w.write_all(&body).await?;
        w.flush().await
    };
    sent.await.map_err(|e| format!("the language server is gone ({e})"))
}

async fn read_message(r: &mut BufReader<ChildStdout>) -> Option<Value> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            if len.is_some() {
                break;
            }
            continue;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().ok();
        }
    }
    let mut buf = vec![0u8; len?];
    r.read_exact(&mut buf).await.ok()?;
    serde_json::from_slice(&buf).ok()
}

fn token_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// What to answer a request the server makes of the client. None of them need a real answer; a
/// server only wants to hear that its progress bars and registrations were accepted.
fn answer(method: &str, params: &Value) -> Value {
    match method {
        "workspace/configuration" => {
            let n = params.get("items").and_then(Value::as_array).map_or(0, Vec::len);
            Value::Array(vec![Value::Null; n])
        }
        _ => Value::Null,
    }
}

async fn pump(
    mut out: BufReader<ChildStdout>,
    writer: Writer,
    pending: Pending,
    state: Arc<Mutex<State>>,
) {
    while let Some(msg) = read_message(&mut out).await {
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").cloned().filter(|i| !i.is_null());
        match (method, id) {
            (Some(method), Some(id)) => {
                let result = answer(method, &msg["params"]);
                let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                let _ = send(&writer, &reply).await;
            }
            (Some(method), None) => {
                let p = &msg["params"];
                let mut s = lock(&state);
                match method {
                    "$/progress" => {
                        let token = token_of(&p["token"]);
                        match p.pointer("/value/kind").and_then(Value::as_str) {
                            Some("begin") => {
                                s.progress.insert(token);
                            }
                            Some("end") => {
                                s.progress.remove(&token);
                            }
                            _ => {}
                        }
                        s.changed = Some(Instant::now());
                    }
                    "experimental/serverStatus" => {
                        s.quiescent = p.get("quiescent").and_then(Value::as_bool);
                        s.changed = Some(Instant::now());
                    }
                    "textDocument/publishDiagnostics" => {
                        if let Some(uri) = p.get("uri").and_then(Value::as_str) {
                            let list = p
                                .get("diagnostics")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            s.published.insert(uri.to_string(), (Instant::now(), list));
                        }
                    }
                    _ => {}
                }
            }
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { continue };
                let tx = lock(&pending).remove(&id);
                if let Some(tx) = tx {
                    let result = match msg.get("error") {
                        Some(e) => Err((
                            e.get("code").and_then(Value::as_i64).unwrap_or(0),
                            e.get("message").and_then(Value::as_str).unwrap_or("error").to_string(),
                        )),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(result);
                }
            }
            (None, None) => {}
        }
    }
    // The server is gone: whoever is waiting for an answer is let go.
    lock(&state).dead = true;
    lock(&pending).clear();
}

pub fn uri_of(path: &Path) -> Result<String, String> {
    Url::from_file_path(path)
        .map(String::from)
        .map_err(|_| format!("{} is not an absolute path", path.display()))
}

pub fn path_of(uri: &str) -> Option<PathBuf> {
    Url::parse(uri).ok()?.to_file_path().ok()
}

impl Client {
    /// Start `program` with `args` in `root` and finish the handshake.
    pub async fn start(
        name: &str,
        program: &Path,
        args: &[String],
        root: &Path,
        options: Option<Value>,
        limit: Duration,
    ) -> Result<Client, String> {
        let mut child = Command::new(program)
            .args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("{}: {e}", program.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let writer: Writer = Arc::new(tokio::sync::Mutex::new(stdin));
        let pending: Pending = Default::default();
        let state: Arc<Mutex<State>> = Default::default();
        tokio::spawn(pump(BufReader::new(stdout), writer.clone(), pending.clone(), state.clone()));

        let mut client = Client {
            name: name.to_string(),
            root: root.to_path_buf(),
            writer,
            next: AtomicI64::new(1),
            pending,
            state,
            caps: Value::Null,
            started: Instant::now(),
            docs: Mutex::new(Docs::default()),
            _child: Mutex::new(child),
        };
        let uri = uri_of(root)?.to_string();
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": { "name": "agentbench" },
            "rootUri": uri,
            "workspaceFolders": [{ "uri": uri, "name": root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() }],
            "initializationOptions": options.unwrap_or(Value::Null),
            "capabilities": {
                "general": { "positionEncodings": ["utf-16"] },
                "window": { "workDoneProgress": true },
                "workspace": { "configuration": true, "workspaceFolders": true },
                "experimental": { "serverStatusNotification": true },
                "textDocument": {
                    "synchronization": { "didSave": false },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "references": {},
                    "publishDiagnostics": { "versionSupport": true },
                    "diagnostic": { "dynamicRegistration": false },
                },
            },
        });
        let never = CancellationToken::new();
        let init = client.request("initialize", params, limit, &never).await?;
        client.caps = init.get("capabilities").cloned().unwrap_or(Value::Null);
        client.notify("initialized", json!({})).await?;
        Ok(client)
    }

    pub fn alive(&self) -> bool {
        !lock(&self.state).dead
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        send(&self.writer, &json!({ "jsonrpc": "2.0", "method": method, "params": params })).await
    }

    async fn once(
        &self,
        method: &str,
        params: &Value,
        limit: Duration,
        stop: &CancellationToken,
    ) -> Result<Value, (i64, String)> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = send(&self.writer, &msg).await {
            lock(&self.pending).remove(&id);
            return Err((0, e));
        }
        let outcome = tokio::select! {
            r = rx => r.map_err(|_| (0, "the language server stopped".to_string())),
            _ = tokio::time::sleep(limit) => Err((0, format!("the language server did not answer {method} in {}s", limit.as_secs()))),
            _ = stop.cancelled() => Err((0, "stopped".to_string())),
        };
        match outcome {
            Ok(r) => r,
            Err(e) => {
                lock(&self.pending).remove(&id);
                let _ = self.notify("$/cancelRequest", json!({ "id": id })).await;
                Err(e)
            }
        }
    }

    /// Ask the server and wait for the answer. A request the server dropped because the code
    /// changed under it is asked again.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
        stop: &CancellationToken,
    ) -> Result<Value, String> {
        let mut last = String::new();
        for attempt in 0..RETRIES {
            match self.once(method, &params, limit, stop).await {
                Ok(v) => return Ok(v),
                // ContentModified and ServerCancelled: the answer would have been stale
                Err((-32801 | -32802, e)) => {
                    last = e;
                    tokio::time::sleep(Duration::from_millis(150 * (attempt as u64 + 1))).await;
                }
                Err((_, e)) => return Err(e),
            }
        }
        Err(last)
    }

    /// Whether the server is still reading the project, or has stopped.
    pub fn health(&self) -> agentbench_core::LspHealth {
        use agentbench_core::LspHealth;
        if !self.alive() {
            LspHealth::Failed
        } else if self.is_ready() {
            LspHealth::Ready
        } else {
            LspHealth::Indexing
        }
    }

    fn is_ready(&self) -> bool {
        let s = lock(&self.state);
        if s.dead {
            return false;
        }
        match s.quiescent {
            Some(q) => q,
            // A server that does not say: quiet when nothing is in progress and nothing has
            // changed for a moment.
            None => {
                let since = s.changed.unwrap_or(self.started).elapsed();
                s.progress.is_empty() && since > Duration::from_millis(800)
            }
        }
    }

    /// Wait until the server has indexed the project, up to `limit`. False when it had not by then.
    pub async fn ready(&self, limit: Duration, stop: &CancellationToken) -> bool {
        let end = Instant::now() + limit;
        loop {
            if self.is_ready() {
                return true;
            }
            if !self.alive() || Instant::now() >= end {
                return false;
            }
            tokio::select! {
                _ = stop.cancelled() => return false,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }

    /// Make the server's copy of `path` match the disk, opening it when it is new. Answers its
    /// address, and whether the server was told of a change.
    pub async fn sync(&self, path: &Path, language: &str) -> Result<(String, bool), String> {
        let bytes = tokio::fs::read(path).await.map_err(|e| format!("{}: {e}", path.display()))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let uri = uri_of(path)?;
        let mut close: Vec<PathBuf> = Vec::new();
        let msg = {
            let mut docs = lock(&self.docs);
            match docs.map.get_mut(path) {
                Some(d) if d.text == text => None,
                Some(d) => {
                    d.version += 1;
                    d.text = text.clone();
                    d.symbols = None;
                    Some((
                        "textDocument/didChange",
                        json!({
                            "textDocument": { "uri": uri, "version": d.version },
                            "contentChanges": [{ "text": text }],
                        }),
                    ))
                }
                None => {
                    docs.map.insert(
                        path.to_path_buf(),
                        Doc {
                            version: 1,
                            text: text.clone(),
                            language: language.into(),
                            symbols: None,
                        },
                    );
                    docs.order.push(path.to_path_buf());
                    while docs.order.len() > MAX_DOCS {
                        let old = docs.order.remove(0);
                        docs.map.remove(&old);
                        close.push(old);
                    }
                    Some((
                        "textDocument/didOpen",
                        json!({ "textDocument": { "uri": uri, "languageId": language, "version": 1, "text": text } }),
                    ))
                }
            }
        };
        for old in close {
            if let Ok(u) = uri_of(&old) {
                let _ = self
                    .notify("textDocument/didClose", json!({ "textDocument": { "uri": u } }))
                    .await;
            }
        }
        let changed = msg.is_some();
        if let Some((method, params)) = msg {
            self.notify(method, params).await?;
        }
        Ok((uri, changed))
    }

    /// Bring every file the server has open up to date with the disk, for edits made by anything
    /// but the code tools (a formatter, a build script, `bash`).
    pub async fn refresh(&self) {
        let open: Vec<(PathBuf, String)> =
            lock(&self.docs).map.iter().map(|(p, d)| (p.clone(), d.language.clone())).collect();
        for (path, language) in open {
            if path.exists() {
                let _ = self.sync(&path, &language).await;
            }
        }
    }

    /// The symbols in a file, as the server finds them.
    pub async fn symbols(
        &self,
        path: &Path,
        language: &str,
        stop: &CancellationToken,
    ) -> Result<Arc<Vec<Sym>>, String> {
        let (uri, _) = self.sync(path, language).await?;
        let (version, cached) = {
            let docs = lock(&self.docs);
            let d = docs.map.get(path).ok_or("the file was closed")?;
            (d.version, d.symbols.clone())
        };
        if let Some(s) = cached {
            return Ok(s);
        }
        let v = self
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri } }),
                Duration::from_secs(30),
                stop,
            )
            .await?;
        let syms = Arc::new(parse_symbols(&v));
        if let Some(d) = lock(&self.docs).map.get_mut(path)
            && d.version == version
        {
            d.symbols = Some(syms.clone());
        }
        Ok(syms)
    }

    /// Where `query` is defined across the project, as the server's symbol index has it:
    /// name, kind, file and the position of the name.
    pub async fn find(&self, query: &str, stop: &CancellationToken) -> Result<Vec<Found>, String> {
        let v = self
            .request("workspace/symbol", json!({ "query": query }), Duration::from_secs(30), stop)
            .await?;
        Ok(v.as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|s| {
                        let loc = s.get("location")?;
                        Some(Found {
                            name: s.get("name")?.as_str()?.to_string(),
                            kind: s.get("kind").and_then(Value::as_u64).unwrap_or(0) as u32,
                            container: s
                                .get("containerName")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            path: path_of(loc.get("uri")?.as_str()?)?,
                            at: loc.get("range").map(|r| span_of(r).start),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Every place the thing named at `at` is used, not counting where it is defined.
    pub async fn references(
        &self,
        path: &Path,
        language: &str,
        at: Pos,
        stop: &CancellationToken,
    ) -> Result<Vec<(PathBuf, Pos)>, String> {
        let (uri, _) = self.sync(path, language).await?;
        let v = self
            .request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": uri },
                    "position": { "line": at.line, "character": at.col },
                    "context": { "includeDeclaration": false },
                }),
                Duration::from_secs(60),
                stop,
            )
            .await?;
        Ok(v.as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|l| {
                        Some((path_of(l.get("uri")?.as_str()?)?, span_of(l.get("range")?).start))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// What the server has to say about a file as it is on disk now.
    pub async fn diagnostics(
        &self,
        path: &Path,
        language: &str,
        stop: &CancellationToken,
    ) -> Result<Vec<Diag>, String> {
        let before = Instant::now();
        let (uri, changed) = self.sync(path, language).await?;
        if self.caps.get("diagnosticProvider").is_some_and(|p| !p.is_null()) {
            let v = self
                .request(
                    "textDocument/diagnostic",
                    json!({ "textDocument": { "uri": uri } }),
                    Duration::from_secs(30),
                    stop,
                )
                .await?;
            return Ok(parse_diags(
                v.get("items").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default(),
            ));
        }
        // No pull: use what the server pushes. After a change, wait for the push that follows it.
        let key = uri.to_string();
        let end = Instant::now() + Duration::from_secs(4);
        loop {
            let got = lock(&self.state).published.get(&key).cloned();
            match got {
                Some((at, list)) if !changed || at >= before => return Ok(parse_diags(&list)),
                _ if Instant::now() >= end => {
                    return Ok(parse_diags(
                        &lock(&self.state)
                            .published
                            .get(&key)
                            .map(|p| p.1.clone())
                            .unwrap_or_default(),
                    ));
                }
                _ => {}
            }
            tokio::select! {
                _ = stop.cancelled() => return Err("stopped".into()),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }
        }
    }
}

/// One hit of a symbol search.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub name: String,
    pub kind: u32,
    pub container: String,
    pub path: PathBuf,
    pub at: Option<Pos>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_symbols_are_flattened_in_order_with_their_depth() {
        let v = json!([
            { "name": "Mode", "kind": 10, "range": {"start": {"line": 23, "character": 0}, "end": {"line": 28, "character": 1}},
              "selectionRange": {"start": {"line": 24, "character": 5}, "end": {"line": 24, "character": 9}},
              "children": [
                { "name": "Content", "kind": 22, "range": {"start": {"line": 25, "character": 4}, "end": {"line": 25, "character": 11}},
                  "selectionRange": {"start": {"line": 25, "character": 4}, "end": {"line": 25, "character": 11}} },
              ] },
            { "name": "search", "kind": 12, "detail": "fn()", "range": {"start": {"line": 30, "character": 0}, "end": {"line": 40, "character": 1}},
              "selectionRange": {"start": {"line": 30, "character": 7}, "end": {"line": 30, "character": 13}} },
        ]);
        let syms = parse_symbols(&v);
        let names: Vec<_> = syms.iter().map(|s| (s.name.as_str(), s.depth, s.parent)).collect();
        assert_eq!(names, [("Mode", 0, None), ("Content", 1, Some(0)), ("search", 0, None)]);
        assert_eq!(syms[0].sel, Pos { line: 24, col: 5 });
        assert_eq!(inside(&syms, 0), 1..2);
        assert_eq!(inside(&syms, 1), 2..2);
        assert_eq!(inside(&syms, 2), 3..3);
        assert!(syms[0].range.contains(Pos { line: 26, col: 0 }));
        assert!(!syms[0].range.contains(Pos { line: 29, col: 0 }));
    }

    #[test]
    fn the_flat_symbol_shape_is_read_too() {
        let v = json!([{ "name": "f", "kind": 12,
            "location": { "uri": "file:///a.rs", "range": {"start": {"line": 3, "character": 0}, "end": {"line": 5, "character": 1}} } }]);
        let syms = parse_symbols(&v);
        assert_eq!(syms.len(), 1);
        assert_eq!(syms[0].range.start.line, 3);
        assert_eq!(syms[0].sel, syms[0].range.start);
        assert!(parse_symbols(&Value::Null).is_empty());
    }

    #[test]
    fn diagnostics_are_read_with_numeric_or_text_codes() {
        let list = [
            json!({"range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 3}}, "severity": 1, "code": "E0425", "message": "cannot find value"}),
            json!({"range": {"start": {"line": 4, "character": 0}, "end": {"line": 4, "character": 1}}, "code": 2322, "message": "mismatch", "source": "ts"}),
        ];
        let d = parse_diags(&list);
        assert_eq!((d[0].severity, d[0].code.as_str(), d[0].pos.line), (1, "E0425", 1));
        assert_eq!((d[1].severity, d[1].code.as_str(), d[1].source.as_str()), (1, "2322", "ts"));
    }

    #[test]
    fn file_addresses_round_trip() {
        let p = Path::new("/tmp/some dir/a.rs");
        let u = uri_of(p).unwrap();
        assert_eq!(path_of(&u).unwrap(), p);
        assert!(uri_of(Path::new("relative.rs")).is_err());
    }

    #[test]
    fn requests_from_the_server_are_accepted() {
        assert_eq!(
            answer("workspace/configuration", &json!({"items": [{}, {}]})),
            json!([null, null])
        );
        assert_eq!(answer("window/workDoneProgress/create", &json!({})), Value::Null);
    }
}
