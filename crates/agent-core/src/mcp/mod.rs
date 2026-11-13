pub mod client;
pub mod config;

use super::CancellationToken;
use super::tools::{Spec, boolean, object, string};
use base64::Engine;
use client::Client;
use config::Server;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

pub const NAME: &str = "mcp";
const IDLE: Duration = Duration::from_secs(30 * 60);
const START: Duration = Duration::from_secs(120);
const LIST: Duration = Duration::from_secs(60);
const CALL: Duration = Duration::from_secs(300);
const ABOUT_CHARS: usize = 400;
const PARAM_CHARS: usize = 160;
const SHORT_CHARS: usize = 120;
pub const LISTING_CHARS: usize = 24_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub files: Vec<PathBuf>,
    pub project_files: Vec<String>,
    pub client: String,
    pub tool_dirs: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            files: dirs::home_dir()
                .map(|h| h.join(".agents").join("mcp.json"))
                .into_iter()
                .collect(),
            project_files: Vec::new(),
            client: "egui_bench".into(),
            tool_dirs: tool_dirs(),
        }
    }
}

fn tool_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let mut dirs: Vec<PathBuf> =
        [".cargo/bin", ".local/bin", ".bun/bin", ".deno/bin", "go/bin", ".npm-global/bin"]
            .iter()
            .map(|d| home.join(d))
            .collect();
    dirs.extend(["/usr/local/bin", "/opt/homebrew/bin"].map(PathBuf::from));
    if let Ok(versions) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut found: Vec<PathBuf> = versions.flatten().map(|v| v.path().join("bin")).collect();
        found.sort();
        found.reverse();
        dirs.extend(found);
    }
    dirs
}

#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub mime: String,
    pub data: String,
}

impl Image {
    pub fn part(&self) -> Value {
        json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{};base64,{}", self.mime, self.data) },
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answer {
    pub text: String,
    pub images: Vec<Image>,
}

struct Live {
    server: Server,
    client: tokio::sync::OnceCell<Result<Arc<Client>, String>>,
    used: Mutex<Instant>,
}

#[derive(Default)]
pub struct Mcp {
    pub settings: Settings,
    live: Mutex<HashMap<(String, PathBuf), Arc<Live>>>,
    failures: Mutex<HashMap<(String, PathBuf), String>>,
    reaper: AtomicBool,
}

pub fn spec() -> Spec {
    Spec::new(
        NAME,
        "Use the tools of the MCP servers the user set up. With no arguments it lists the servers. With `server` it \
         starts that server if it is not running and lists its tools with their arguments. With `server`, `tool` and \
         `args` it calls the tool and gives back what it answered. List a server's tools before calling one, and pass \
         `args` as an object matching the tool's arguments.",
        object(
            json!({
                "server": string("The server, as the list names it"),
                "tool": string("The tool to call on that server"),
                "args": { "type": "object", "description": "The tool's arguments, as an object" },
                "schema": boolean("Show the tool's full input schema instead of calling it"),
            }),
            &[],
        ),
    )
}

pub fn summary(args: &Value) -> Option<String> {
    let field = |k: &str| args.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    Some(match (field("server"), field("tool")) {
        (Some(s), Some(t)) => format!("{s} {t}"),
        (Some(s), None) => format!("{s}: tools"),
        _ => "servers".into(),
    })
}

fn clip(text: &str, max: usize) -> String {
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one.char_indices().nth(max) {
        Some((i, _)) => format!("{}...", one[..i].trim_end()),
        None => one,
    }
}

fn type_of(schema: &Value) -> String {
    if let Some(choices) = schema.get("enum").and_then(Value::as_array) {
        let words: Vec<String> = choices.iter().take(8).map(|c| c.to_string()).collect();
        let more = if choices.len() > 8 { "|..." } else { "" };
        return format!("{}{more}", words.join("|"));
    }
    match schema.get("type") {
        Some(Value::String(t)) if t == "array" => {
            let inner = schema.get("items").map(type_of).unwrap_or_else(|| "any".into());
            format!("{inner}[]")
        }
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(ts)) => ts
            .iter()
            .filter_map(Value::as_str)
            .filter(|t| *t != "null")
            .collect::<Vec<_>>()
            .join("|"),
        _ => {
            for key in ["anyOf", "oneOf"] {
                if let Some(list) = schema.get(key).and_then(Value::as_array) {
                    let types: Vec<String> =
                        list.iter().map(type_of).filter(|t| t != "null" && t != "any").collect();
                    if !types.is_empty() {
                        return types.join("|");
                    }
                }
            }
            "any".into()
        }
    }
}

fn arguments(tool: &client::Tool) -> Vec<(&String, &Value, bool)> {
    let required: Vec<&str> = tool
        .schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let props = tool.schema.get("properties").and_then(Value::as_object);
    let mut out: Vec<(&String, &Value, bool)> = props
        .into_iter()
        .flatten()
        .map(|(name, p)| (name, p, required.contains(&name.as_str())))
        .collect();
    out.sort_by_key(|(_, _, needed)| !needed);
    out
}

fn signature(tool: &client::Tool) -> String {
    let params: Vec<String> = arguments(tool)
        .into_iter()
        .map(|(name, p, needed)| format!("{name}{}: {}", if needed { "" } else { "?" }, type_of(p)))
        .collect();
    format!("{}({})", tool.name, params.join(", "))
}

pub fn describe(tool: &client::Tool) -> String {
    let mut out = format!("- {}", signature(tool));
    if !tool.about.is_empty() {
        out.push_str(&format!(": {}", clip(&tool.about, ABOUT_CHARS)));
    }
    for (name, p, _) in arguments(tool) {
        if let Some(d) =
            p.get("description").and_then(Value::as_str).filter(|d| !d.trim().is_empty())
        {
            out.push_str(&format!("\n    {name}: {}", clip(d, PARAM_CHARS)));
        }
    }
    out
}

fn size(bytes: usize) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

pub fn render(result: &Value) -> Result<Answer, String> {
    let mut answer = Answer::default();
    let mut lines: Vec<String> = Vec::new();
    for part in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        let field = |k: &str| part.get(k).and_then(Value::as_str).unwrap_or("");
        match field("type") {
            "text" => lines.push(field("text").to_string()),
            "image" => {
                let mime = match field("mimeType") {
                    "" => "image/png",
                    m => m,
                };
                let data = field("data").trim().to_string();
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .map(|b| b.len())
                    .unwrap_or(data.len() * 3 / 4);
                lines.push(format!("[a {mime} picture, {}, attached]", size(bytes)));
                answer.images.push(Image { mime: mime.to_string(), data });
            }
            "audio" => lines.push(format!("[{} audio left out]", field("mimeType"))),
            "resource" => {
                let r = &part["resource"];
                let uri = r.get("uri").and_then(Value::as_str).unwrap_or("");
                match r.get("text").and_then(Value::as_str) {
                    Some(text) => lines.push(format!("{uri}:\n{text}")),
                    None => lines.push(format!(
                        "[{uri}: {} left out]",
                        r.get("mimeType").and_then(Value::as_str).unwrap_or("binary data")
                    )),
                }
            }
            "resource_link" => {
                let name = field("name");
                lines.push(
                    format!("[link: {} {}]", field("uri"), name).trim_end_matches(' ').to_string(),
                )
            }
            other => lines.push(format!("[{other} content left out]")),
        }
    }
    let structured = result.get("structuredContent").filter(|v| !v.is_null());
    if lines.iter().all(|l| l.trim().is_empty())
        && let Some(s) = structured
    {
        lines = vec![serde_json::to_string_pretty(s).unwrap_or_default()];
    }
    answer.text = super::spawn::clip(lines.join("\n"));
    if answer.text.trim().is_empty() {
        answer.text = "(the tool answered with nothing)".into();
    }
    if result.get("isError").and_then(Value::as_bool).unwrap_or(false) {
        return Err(answer.text);
    }
    Ok(answer)
}

impl Mcp {
    pub fn new(settings: Settings) -> Mcp {
        Mcp { settings, ..Mcp::default() }
    }

    pub fn files(&self, root: &Path) -> Vec<PathBuf> {
        let mut files = self.settings.files.clone();
        files.extend(self.settings.project_files.iter().map(|f| root.join(f)));
        files
    }

    pub fn servers(&self, root: &Path) -> (Vec<Server>, Vec<String>) {
        config::load(&self.files(root))
    }

    pub fn configured(&self, root: &Path) -> bool {
        !self.servers(root).0.is_empty()
    }

    pub fn section(&self, root: &Path) -> Option<String> {
        let (servers, _) = self.servers(root);
        if servers.is_empty() {
            return None;
        }
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        Some(format!(
            "MCP servers are set up for you: {}. Reach their tools through the mcp tool: call it with a server to list \
             that server's tools, then with server, tool and args to use one.",
            names.join(", ")
        ))
    }

    pub fn running(&self) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = lock(&self.live)
            .iter()
            .filter(|(_, l)| matches!(l.client.get(), Some(Ok(c)) if c.alive()))
            .map(|(k, _)| k.clone())
            .collect();
        out.sort();
        out
    }

    pub fn stop_all(&self) {
        lock(&self.live).clear();
    }

    fn evict(&self) {
        lock(&self.live).retain(|_, l| lock(&l.used).elapsed() < IDLE);
    }

    fn reap(self: &Arc<Self>) {
        if self.reaper.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak: Weak<Mcp> = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                match weak.upgrade() {
                    Some(m) => m.evict(),
                    None => break,
                }
            }
        });
    }

    async fn client(self: &Arc<Self>, server: &Server, root: &Path) -> Result<Arc<Client>, String> {
        self.reap();
        let key = (server.name.clone(), root.to_path_buf());
        for _ in 0..2 {
            self.evict();
            let live = {
                let mut all = lock(&self.live);
                if all.get(&key).is_some_and(|l| l.server != *server) {
                    all.remove(&key);
                }
                all.entry(key.clone())
                    .or_insert_with(|| {
                        Arc::new(Live {
                            server: server.clone(),
                            client: Default::default(),
                            used: Mutex::new(Instant::now()),
                        })
                    })
                    .clone()
            };
            *lock(&live.used) = Instant::now();
            let started = live
                .client
                .get_or_init(|| async {
                    Client::start(
                        server,
                        root,
                        &self.settings.tool_dirs,
                        &self.settings.client,
                        START,
                    )
                    .await
                    .map(Arc::new)
                })
                .await;
            match started {
                Ok(c) if c.alive() => {
                    lock(&self.failures).remove(&key);
                    return Ok(c.clone());
                }
                Ok(_) => {
                    lock(&self.live).remove(&key);
                }
                Err(e) => {
                    let e = format!("could not start {}: {e}", server.name);
                    lock(&self.live).remove(&key);
                    lock(&self.failures).insert(key, e.clone());
                    return Err(e);
                }
            }
        }
        Err(format!("{} keeps stopping as soon as it starts", server.name))
    }

    fn listing(&self, root: &Path, servers: &[Server], problems: &[String]) -> String {
        if servers.is_empty() {
            let files: Vec<String> =
                self.files(root).iter().map(|f| f.display().to_string()).collect();
            let mut out = format!(
                "No MCP servers are set up. They are read from {} in the common format: \
                 {{\"mcpServers\": {{\"name\": {{\"command\": \"...\", \"args\": [...]}}}}}} or {{\"url\": \"...\"}}.",
                files.join(" and ")
            );
            for p in problems {
                out.push_str(&format!("\n- {p}"));
            }
            return out;
        }
        let mut out = String::from("MCP servers:");
        for s in servers {
            let key = (s.name.clone(), root.to_path_buf());
            let live = lock(&self.live).get(&key).cloned();
            let state = match live.as_ref().and_then(|l| l.client.get()) {
                Some(Ok(c)) if c.alive() => match c.cached_tools() {
                    Some(n) => format!("running, {n} tools"),
                    None => "running".into(),
                },
                _ => match lock(&self.failures).get(&key) {
                    Some(e) => format!("failed last time: {e}"),
                    None if live.is_some() => "starting".into(),
                    None => "not started".into(),
                },
            };
            let kind = match s.kind() {
                Ok(config::Kind::Http) => "http",
                Ok(config::Kind::Stdio) => "stdio",
                Err(_) => "broken",
            };
            out.push_str(&format!("\n- {} ({kind}: {}): {state}", s.name, s.shown()));
        }
        for p in problems {
            out.push_str(&format!("\n- problem: {p}"));
        }
        out.push_str("\nCall mcp with a server to start it and list its tools.");
        out
    }

    pub async fn run(
        self: &Arc<Self>,
        args: &Value,
        root: &Path,
        stop: &CancellationToken,
    ) -> Result<Answer, String> {
        let text =
            |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
        let (servers, problems) = self.servers(root);
        let Some(name) = text("server") else {
            return Ok(Answer {
                text: self.listing(root, &servers, &problems),
                images: Vec::new(),
            });
        };
        let server = servers
            .iter()
            .find(|s| s.name == name)
            .or_else(|| servers.iter().find(|s| s.name.eq_ignore_ascii_case(name)));
        let Some(server) = server else {
            let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
            return Err(format!(
                "there is no MCP server called {name:?}; the servers are: {}",
                if names.is_empty() { "none".to_string() } else { names.join(", ") }
            ));
        };
        server.kind()?;
        let client = tokio::select! {
            c = self.client(server, root) => c?,
            _ = stop.cancelled() => return Err("stopped".into()),
        };
        let tools = client.tools(LIST, stop).await?;
        let Some(tool) = text("tool") else {
            return Ok(Answer { text: tool_listing(&client, &tools), images: Vec::new() });
        };
        let Some(found) = tools.iter().find(|t| t.name == tool) else {
            let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            return Err(format!(
                "{} has no tool called {tool:?}; its tools are: {}",
                server.name,
                names.join(", ")
            ));
        };
        if args.get("schema").and_then(Value::as_bool).unwrap_or(false) {
            let schema = serde_json::to_string_pretty(&found.schema).unwrap_or_default();
            return Ok(Answer {
                text: format!("{}\n\ninput schema:\n{schema}", describe(found)),
                images: Vec::new(),
            });
        }
        let call_args = match args.get("args").or_else(|| args.get("arguments")) {
            None | Some(Value::Null) => json!({}),
            Some(Value::String(s)) if s.trim().is_empty() => json!({}),
            Some(Value::String(s)) => serde_json::from_str(s)
                .map_err(|e| format!("`args` must be an object of the tool's arguments ({e})"))?,
            Some(v) => v.clone(),
        };
        if !call_args.is_object() {
            return Err("`args` must be an object of the tool's arguments".into());
        }
        let limit = server.timeout.map_or(CALL, Duration::from_secs);
        let result = client.call(tool, call_args, limit, stop).await;
        let result = match result {
            Err(e) if !client.alive() => {
                let said = client.stderr();
                Err(if said.is_empty() { e } else { format!("{e}\n{} said:\n{said}", server.name) })
            }
            other => other,
        };
        render(&result?)
    }

    pub async fn call(
        self: &Arc<Self>,
        args: &Value,
        root: &Path,
        stop: &CancellationToken,
    ) -> Result<String, String> {
        let answer = self.run(args, root, stop).await?;
        if answer.images.is_empty() {
            return Ok(answer.text);
        }
        Ok(format!("{}\n({} pictures could not be shown here)", answer.text, answer.images.len()))
    }
}

fn tool_listing(client: &Client, tools: &[client::Tool]) -> String {
    listing_of(&client.name, &client.title, &client.instructions, tools)
}

fn first_sentence(text: &str) -> String {
    let one = clip(text, SHORT_CHARS);
    match one.split_once(". ") {
        Some((head, _)) => head.to_string(),
        None => one.trim_end_matches('.').to_string(),
    }
}

pub fn listing_of(name: &str, title: &str, instructions: &str, tools: &[client::Tool]) -> String {
    let title = match title {
        "" => name.to_string(),
        t => format!("{name} ({t})"),
    };
    let said = match instructions {
        "" => String::new(),
        text => format!("\n\nThe server says:\n{}", clip(text, 1500)),
    };
    let full: Vec<String> = tools.iter().map(describe).collect();
    let length: usize = full.iter().map(|l| l.len() + 1).sum();
    if length + said.len() + title.len() + 40 <= LISTING_CHARS {
        return format!("{title} has {} tools:\n{}{said}", tools.len(), full.join("\n"));
    }
    let mut out = format!(
        "{title} has {} tools, too many to describe in full; call mcp with server, tool and schema: true to see one's arguments:",
        tools.len()
    );
    for t in tools {
        let line = match first_sentence(&t.about).as_str() {
            "" => format!("\n- {}", signature(t)),
            about => format!("\n- {}: {about}", signature(t)),
        };
        if out.len() + line.len() + said.len() > LISTING_CHARS {
            let rest: Vec<&str> =
                tools.iter().skip_while(|o| o.name != t.name).map(|o| o.name.as_str()).collect();
            out.push_str(&format!("\n- and {}", rest.join(", ")));
            break;
        }
        out.push_str(&line);
    }
    let room = LISTING_CHARS.saturating_sub(out.len());
    out.push_str(&said.chars().take(room).collect::<String>());
    out
}

pub fn tool(mcp: Arc<Mcp>) -> super::toolkit::Tool {
    let s = spec();
    super::toolkit::Tool::new(&s.name, &s.about, s.schema, move |args, cx| {
        let mcp = mcp.clone();
        async move { mcp.call(&args, cx.workspace.root(), &cx.stop).await }
    })
    .summarised_by(summary)
}

#[cfg(test)]
mod tests;
