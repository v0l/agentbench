use super::{Agent, Context, Event, Io, Turn, brief};
use egui_bench::agent::skillset;
use egui_bench::agent::tools::{self, Spec, object, string};
use egui_bench::agent::workspace::Workspace;
use foreman_core::{Entry, Question, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::BufRead;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const PATIENCE: Duration = Duration::from_secs(600);
const BASH_DEFAULT: u64 = 120;
const BASH_MAX: u64 = 900;
const OUTPUT_MAX: usize = 30_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Model {
    #[serde(default = "default_url")]
    pub url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub key: String,
    #[serde(default = "default_steps")]
    pub steps: usize,
}

fn default_url() -> String {
    egui_bench::agent::config::DEFAULT_URL.to_string()
}

fn default_steps() -> usize {
    60
}

impl Default for Model {
    fn default() -> Self {
        Model {
            url: default_url(),
            model: String::new(),
            key: String::new(),
            steps: default_steps(),
        }
    }
}

impl Model {
    fn endpoint(&self) -> String {
        let base = self.url.trim().trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }
}

#[derive(Clone, Default)]
pub struct Shared(std::sync::Arc<std::sync::RwLock<Model>>);

impl Shared {
    pub fn new(model: Model) -> Shared {
        Shared(std::sync::Arc::new(std::sync::RwLock::new(model)))
    }

    pub fn get(&self) -> Model {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, model: Model) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = model;
    }
}

#[derive(Default)]
pub struct Native {
    pub model: Shared,
}

impl Native {
    pub fn new(model: Model) -> Native {
        Native { model: Shared::new(model) }
    }

    pub fn shared(model: Shared) -> Native {
        Native { model }
    }
}

pub const FOREMAN_TOOLS: [&str; 7] = [
    "list_issues",
    "file_issue",
    "update_issue",
    "comment_on_issue",
    "list_sessions",
    "read_session",
    "ask_user",
];

fn foreman_specs() -> Vec<Spec> {
    let id = |about: &str| json!({ "type": "integer", "description": about });
    vec![
        Spec::new(
            "list_issues",
            "List this project's issues with their state. Pass state to filter: backlog, todo, doing, review or done.",
            object(json!({ "state": string("Only issues in this state") }), &[]),
        ),
        Spec::new(
            "file_issue",
            "File a new issue in this project, for work you found that is outside what you were asked to do.",
            object(
                json!({ "title": string("One line"), "body": string("Details, steps, what done looks like") }),
                &["title"],
            ),
        ),
        Spec::new(
            "update_issue",
            "Change an issue: move it to review when your work on it is ready, or to done once it is merged.",
            object(
                json!({
                    "id": id("The issue number"),
                    "state": string("backlog, todo, doing, review or done"),
                    "title": string("A new title"),
                    "body": string("A new description"),
                }),
                &["id"],
            ),
        ),
        Spec::new(
            "comment_on_issue",
            "Leave a comment on an issue for the user or other agents.",
            object(
                json!({ "id": id("The issue number"), "body": string("The comment") }),
                &["id", "body"],
            ),
        ),
        Spec::new(
            "list_sessions",
            "List the agent sessions in this project: which agent, which worktree, what they are doing.",
            object(json!({}), &[]),
        ),
        Spec::new(
            "read_session",
            "Read the last messages of another session in this project, to see what it did.",
            object(
                json!({ "id": id("The session number"), "last": id("How many messages, default 20") }),
                &["id"],
            ),
        ),
        Spec::new(
            "ask_user",
            "Ask the user a question and wait for the answer. Give options when the answer is one of a few choices. \
             Use it when you cannot go on without a decision, not for things you can find out yourself.",
            object(
                json!({
                    "question": string("The question, one or two sentences"),
                    "options": { "type": "array", "items": { "type": "string" }, "description": "Choices to pick from" },
                }),
                &["question"],
            ),
        ),
    ]
}

fn bash_spec() -> Spec {
    Spec::new(
        "bash",
        "Run a shell command in the worktree and read what it prints: git, cargo, tests, scripts, anything installed. \
         It runs as the user with their environment, in the worktree. Use the file tools for editing.",
        object(
            json!({
                "command": string("The command line, run by bash -c"),
                "timeout_s": { "type": "integer", "description": "Seconds before it is killed, default 120, at most 900" },
            }),
            &["command"],
        ),
    )
}

fn file_specs() -> Vec<Spec> {
    tools::builtin()
        .into_iter()
        .filter(|s| !matches!(s.name.as_str(), "bash" | "request_access" | "workspace"))
        .collect()
}

pub fn specs() -> Vec<Spec> {
    let mut all = file_specs();
    all.push(bash_spec());
    all.extend(foreman_specs());
    all
}

fn tool_list() -> Value {
    Value::Array(
        specs()
            .into_iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": { "name": t.name, "description": t.about, "parameters": t.schema },
                })
            })
            .collect(),
    )
}

fn system(turn: &Turn) -> String {
    let skills = skillset::discover(&skillset::roots(turn.cwd), &[]);
    let mut out = format!(
        "You are a coding agent working in {}. Read before you change, change what was asked, and check your work by building and testing.\n\n",
        turn.cwd.display()
    );
    out.push_str(&brief(turn.context));
    out.push_str(
        "\n\nYour own tools reach the tracker directly: list_issues, file_issue, update_issue, comment_on_issue, \
         list_sessions, read_session. Prefer them to the REST API. ask_user waits for the user's answer.\n\n",
    );
    out.push_str(
        "- Work in the worktree with list_files, read_file, search_files, write_file and edit_file. Paths are relative to it. \
         Read a file before you edit it.\n\
         - bash runs commands in the worktree as the user.\n\
         - Before writing a file format you were not taught, load its skill with load_skill.\n\
         - Write markdown, and put code in fenced blocks.\n",
    );
    if !skills.is_empty() {
        out.push_str("\nSkills you can load:\n");
        out.push_str(&skillset::listing(&skills));
        out.push('\n');
    }
    out
}

fn history(entries: &[Entry]) -> Vec<Value> {
    let mut out = Vec::new();
    for e in entries {
        match e {
            Entry::User { text } => out.push(json!({ "role": "user", "content": text })),
            Entry::Assistant { text } => out.push(json!({ "role": "assistant", "content": text })),
            Entry::Tool { name, input, output, error, .. } => {
                let result = match (output, error) {
                    (Some(o), false) => clip(o, 2000),
                    (Some(o), true) => format!("failed: {}", clip(o, 2000)),
                    (None, _) => "no result".into(),
                };
                out.push(json!({
                    "role": "assistant",
                    "content": format!("called {name}({input}) and got:\n{result}"),
                }));
            }
            Entry::Question { question, answer, .. } => out.push(json!({
                "role": "assistant",
                "content": format!(
                    "asked the user: {} and they answered: {}",
                    question.text,
                    answer.as_deref().unwrap_or("nothing")
                ),
            })),
            Entry::Thinking { .. } | Entry::Error { .. } | Entry::Notice { .. } => {}
        }
    }
    out
}

fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let head = text.floor_char_boundary(max / 2);
    let tail = text.ceil_char_boundary(text.len() - max / 2);
    format!("{}\n... {} bytes left out ...\n{}", &text[..head], tail - head, &text[tail..])
}

#[derive(Default, Clone)]
struct Call {
    id: String,
    name: String,
    args: String,
}

#[derive(Default)]
struct Reply {
    text: String,
    calls: Vec<Call>,
    usage: Option<Usage>,
}

impl Agent for Native {
    fn name(&self) -> &str {
        "foreman"
    }

    fn label(&self) -> &str {
        "Foreman"
    }

    fn available(&self) -> bool {
        !self.model.get().model.trim().is_empty()
    }

    fn about(&self) -> String {
        let m = self.model.get();
        match m.model.trim() {
            "" => "no model set: open settings and pick a server and model".into(),
            name => format!("{name} at {}", m.url),
        }
    }

    fn resumable(&self) -> bool {
        false
    }

    fn steerable(&self) -> bool {
        true
    }

    fn run(&self, turn: &Turn, io: &dyn Io) -> Result<(), String> {
        let mut model = self.model.get();
        if let Some(m) = turn.model.map(str::trim).filter(|m| !m.is_empty()) {
            model.model = m.to_string();
        }
        if model.model.trim().is_empty() {
            return Err("pick a server and model for the foreman agent under settings".into());
        }
        let workspace = Workspace::open(turn.cwd)?;
        let http = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(PATIENCE))
            .timeout_recv_body(Some(PATIENCE))
            .http_status_as_error(false)
            .user_agent(format!("foreman/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        let mut messages = vec![json!({ "role": "system", "content": system(turn) })];
        messages.extend(history(turn.history));
        messages.push(json!({ "role": "user", "content": turn.prompt }));
        let tools = tool_list();
        for _ in 0..model.steps.max(1) {
            if io.stopped() {
                return Err("stopped".into());
            }
            let reply = complete(&model, &http, &messages, &tools, io)?;
            if let Some(u) = reply.usage {
                io.emit(Event::Usage(u));
            }
            let mut message = json!({ "role": "assistant", "content": reply.text });
            if !reply.calls.is_empty() {
                message["tool_calls"] = Value::Array(
                    reply
                        .calls
                        .iter()
                        .map(|c| {
                            json!({ "id": c.id, "type": "function", "function": { "name": c.name, "arguments": c.args } })
                        })
                        .collect(),
                );
            }
            messages.push(message);
            for call in &reply.calls {
                let input: Value = serde_json::from_str(&call.args).unwrap_or(Value::Null);
                io.emit(Event::ToolStart {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    input: input.clone(),
                });
                let result = self.tool(&call.name, &input, &workspace, turn.context, io);
                let (output, error) = match &result {
                    Ok(o) => (o.clone(), false),
                    Err(e) => (e.clone(), true),
                };
                io.emit(Event::ToolEnd {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    output: output.clone(),
                    error,
                });
                let content = if error { json!({ "error": output }).to_string() } else { output };
                messages
                    .push(json!({ "role": "tool", "tool_call_id": call.id, "content": content }));
                if io.stopped() {
                    return Err("stopped".into());
                }
            }
            let mut steered = false;
            while let Some(text) = io.steer() {
                io.emit(Event::Steered { text: text.clone() });
                messages.push(json!({ "role": "user", "content": text }));
                steered = true;
            }
            if reply.calls.is_empty() && !steered {
                io.emit(Event::Done);
                return Ok(());
            }
        }
        Err(format!("stopped after {} steps; send a message to carry on", model.steps))
    }
}

pub fn models(url: &str, key: &str) -> Result<Vec<String>, String> {
    let base = url.trim().trim_end_matches('/').trim_end_matches("/chat/completions");
    if base.is_empty() {
        return Err("the server address is empty".into());
    }
    let http = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut req = http.get(format!("{base}/models"));
    if !key.trim().is_empty() {
        req = req.header("authorization", format!("Bearer {}", key.trim()));
    }
    let resp = req.call().map_err(|e| format!("{base}: {e}"))?;
    let status = resp.status();
    let text = resp.into_body().read_to_string().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{status}: {}", text.trim()));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("not a model list: {e}"))?;
    let mut ids: Vec<String> = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").or_else(|| m.get("name")).and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

fn complete(
    model: &Model,
    http: &ureq::Agent,
    messages: &[Value],
    tools: &Value,
    io: &dyn Io,
) -> Result<Reply, String> {
    let body = json!({
        "model": model.model.trim(),
        "messages": messages,
        "tools": tools,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let mut req = http.post(model.endpoint()).header("content-type", "application/json");
    if !model.key.trim().is_empty() {
        req = req.header("authorization", format!("Bearer {}", model.key.trim()));
    }
    let resp = req.send(body.to_string()).map_err(|e| format!("{}: {e}", model.url))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.into_body().read_to_string().unwrap_or_default();
        return Err(format!("{status}: {}", text.trim()));
    }
    let reader = std::io::BufReader::new(resp.into_body().into_reader());
    read_stream(reader, io)
}

impl Native {
    fn tool(
        &self,
        name: &str,
        args: &Value,
        ws: &Workspace,
        cx: &Context,
        io: &dyn Io,
    ) -> Result<String, String> {
        let text = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        let need = |k: &str| text(k).ok_or_else(|| format!("missing `{k}`"));
        let number = |k: &str| args.get(k).and_then(Value::as_u64).map(|n| n as usize);
        match name {
            "list_files" => ws.list(text("path").as_deref(), number("depth")),
            "read_file" => ws.read(&need("path")?, number("offset"), number("limit")),
            "write_file" => ws.write(&need("path")?, &text("content").unwrap_or_default()),
            "edit_file" => ws.edit(
                &need("path")?,
                &need("old")?,
                &text("new").unwrap_or_default(),
                args.get("all").and_then(Value::as_bool).unwrap_or(false),
            ),
            "search_files" => ws.search(&need("pattern")?, text("path").as_deref()),
            "load_skill" => {
                let skills = skillset::discover(&skillset::roots(ws.root()), &[]);
                skillset::load(&skills, text("name").as_deref(), text("file").as_deref())
            }
            "bash" => bash(
                ws.root(),
                &need("command")?,
                args.get("timeout_s").and_then(Value::as_u64),
                cx,
                io,
            ),
            "ask_user" => {
                let options = args
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|o| o.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                let answer = io.ask(Question { text: need("question")?, options })?;
                Ok(format!("The user answered: {answer}"))
            }
            n if FOREMAN_TOOLS.contains(&n) => io
                .api()
                .call(n, args, cx.session)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()),
            other => Err(format!("no tool called {other}")),
        }
    }
}

fn read_stream(reader: impl BufRead, io: &dyn Io) -> Result<Reply, String> {
    let mut reply = Reply::default();
    for line in reader.lines() {
        if io.stopped() {
            return Err("stopped".into());
        }
        let line = line.map_err(|e| e.to_string())?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        take(&v, &mut reply, io);
    }
    Ok(reply)
}

fn take(v: &Value, reply: &mut Reply, io: &dyn Io) {
    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let cached =
            u.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64).unwrap_or(0);
        reply.usage = Some(Usage {
            input_tokens: n("prompt_tokens").saturating_sub(cached),
            output_tokens: n("completion_tokens"),
            cache_read_tokens: cached,
            cost_usd: 0.0,
            turns: 1,
        });
    }
    let Some(delta) = v.pointer("/choices/0/delta") else { return };
    for key in ["reasoning_content", "reasoning"] {
        if let Some(t) = delta.get(key).and_then(Value::as_str).filter(|t| !t.is_empty()) {
            io.emit(Event::Thinking { text: t.to_string() });
        }
    }
    if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
        reply.text.push_str(text);
        io.emit(Event::Text { delta: text.to_string() });
    }
    let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) else { return };
    for c in calls {
        let i = c.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
        while reply.calls.len() <= i {
            reply.calls.push(Call::default());
        }
        let slot = &mut reply.calls[i];
        if let Some(id) = c.get("id").and_then(Value::as_str) {
            slot.id = id.to_string();
        }
        if let Some(f) = c.get("function") {
            if let Some(n) = f.get("name").and_then(Value::as_str) {
                slot.name.push_str(n);
            }
            if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                slot.args.push_str(a);
            }
        }
    }
}

fn bash(
    cwd: &Path,
    command: &str,
    timeout: Option<u64>,
    cx: &Context,
    io: &dyn Io,
) -> Result<String, String> {
    if command.trim().is_empty() {
        return Err("`command` is empty".into());
    }
    let limit = Duration::from_secs(timeout.unwrap_or(BASH_DEFAULT).clamp(1, BASH_MAX));
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .envs(cx.env())
        .env("NO_COLOR", "1")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| format!("bash: {e}"))?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    let read = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = read(out.map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    let err = read(err.map(|p| Box::new(p) as Box<dyn std::io::Read + Send>));
    let status = loop {
        if let Ok(Some(s)) = child.try_wait() {
            break Some(s);
        }
        if io.stopped() || started.elapsed() >= limit {
            #[cfg(unix)]
            let _ = Command::new("kill").args(["-KILL", &format!("-{}", child.id())]).status();
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    let mut text = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    if !stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&stderr);
    }
    let text = clip(&text, OUTPUT_MAX);
    let took = started.elapsed().as_secs_f32();
    match status {
        Some(s) if s.success() => Ok(format!("{text}\n[exit 0 in {took:.1}s]")),
        Some(s) => Err(format!("{text}\n[exit {} in {took:.1}s]", s.code().unwrap_or(-1))),
        None if io.stopped() => Err("stopped".into()),
        None => {
            Err(format!("{text}\n[killed after {}s: pass a larger timeout_s]", limit.as_secs()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Api, TestIo, context};
    use std::io::{Read, Write};
    use std::sync::Arc;

    struct FakeApi;

    impl Api for FakeApi {
        fn call(
            &self,
            tool: &str,
            args: &Value,
            session: foreman_core::Id,
        ) -> Result<Value, String> {
            Ok(json!({ "tool": tool, "args": args, "session": session }))
        }
    }

    fn stub(replies: Vec<Vec<Value>>) -> (String, std::sync::mpsc::Receiver<Value>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (seen, asked) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (stream, events) in listener.incoming().zip(replies) {
                let Ok(mut s) = stream else { return };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if s.read(&mut byte).unwrap_or(0) != 1 {
                        return;
                    }
                    head.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&head).to_lowercase();
                let len = text
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                let _ = s.read_exact(&mut body);
                let _ = seen.send(serde_json::from_slice(&body).unwrap_or(Value::Null));
                let mut sse = String::new();
                for e in events {
                    sse.push_str(&format!("data: {e}\n\n"));
                }
                sse.push_str("data: [DONE]\n\n");
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                        sse.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (url, asked)
    }

    fn call(id: &str, name: &str, args: Value) -> Value {
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"function":{"name":name,"arguments":args.to_string()}}]}}]})
    }

    fn say(text: &str) -> Value {
        json!({"choices":[{"delta":{"content":text}}]})
    }

    fn run(native: &Native, dir: &Path, prompt: &str, io: &TestIo) -> Result<(), String> {
        let c = context();
        let turn = Turn { prompt, cwd: dir, model: None, resume: None, history: &[], context: &c };
        native.run(&turn, io)
    }

    #[test]
    fn it_edits_files_and_reaches_the_tracker() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();
        let (url, asked) = stub(vec![
            vec![call(
                "c1",
                "edit_file",
                json!({"path": "a.txt", "old": "world", "new": "foreman"}),
            )],
            vec![call("c2", "file_issue", json!({"title": "Follow up"}))],
            vec![
                say("All "),
                say("done."),
                json!({"choices": [], "usage": {"prompt_tokens": 50, "completion_tokens": 4}}),
            ],
        ]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        run(&native, dir.path(), "change world", &io).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "hello foreman\n");
        let events = io.events();
        assert!(events.iter().any(
            |e| matches!(e, Event::ToolEnd { name, error: false, .. } if name == "edit_file")
        ));
        assert!(events.iter().any(|e| matches!(e, Event::ToolEnd { name, output, .. } if name == "file_issue" && output.contains("Follow up"))));
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Event::Text { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "All done.");
        assert!(events.iter().any(|e| matches!(e, Event::Usage(u) if u.input_tokens == 50)));
        assert_eq!(events.last(), Some(&Event::Done));

        let first = asked.recv().unwrap();
        let system = first["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("inside foreman"), "{system}");
        let names: Vec<&str> = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        for n in ["read_file", "edit_file", "bash", "list_issues", "file_issue", "ask_user"] {
            assert!(names.contains(&n), "{n} missing from {names:?}");
        }
        assert!(!names.contains(&"request_access"));
    }

    #[test]
    fn a_steer_lands_between_steps() {
        let dir = tempfile::tempdir().unwrap();
        let (url, asked) =
            stub(vec![vec![call("c1", "list_files", json!({}))], vec![say("ok, stopping")]]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        io.inbox.lock().unwrap().push("actually stop".into());
        run(&native, dir.path(), "look around", &io).unwrap();
        assert!(io.events().contains(&Event::Steered { text: "actually stop".into() }));
        let _ = asked.recv().unwrap();
        let second = asked.recv().unwrap();
        let last = second["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last, json!({"role": "user", "content": "actually stop"}));
    }

    #[test]
    fn ask_user_waits_for_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        let (url, asked) = stub(vec![
            vec![call(
                "c1",
                "ask_user",
                json!({"question": "red or blue?", "options": ["red", "blue"]}),
            )],
            vec![say("blue it is")],
        ]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        io.answers.lock().unwrap().push("blue".into());
        run(&native, dir.path(), "pick", &io).unwrap();
        let _ = asked.recv().unwrap();
        let second = asked.recv().unwrap();
        let tool = second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap()
            .clone();
        assert_eq!(tool["content"], "The user answered: blue");
    }

    #[test]
    fn earlier_turns_are_replayed_from_the_transcript() {
        let entries = vec![
            Entry::User { text: "hi".into() },
            Entry::Thinking { text: "hmm".into() },
            Entry::Tool {
                call_id: "c".into(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
                output: Some("a".into()),
                error: false,
            },
            Entry::Assistant { text: "there is a".into() },
        ];
        let h = history(&entries);
        assert_eq!(h.len(), 3);
        assert_eq!(h[0], json!({"role": "user", "content": "hi"}));
        assert!(h[1]["content"].as_str().unwrap().contains("called bash"));
    }

    #[test]
    fn bash_runs_in_the_worktree_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let io = TestIo::new(Arc::new(FakeApi));
        let c = context();
        let ok = bash(dir.path(), "pwd; echo $FOREMAN_SESSION", None, &c, &io).unwrap();
        assert!(
            ok.contains(&dir.path().canonicalize().unwrap().display().to_string())
                || ok.contains(&dir.path().display().to_string())
        );
        assert!(ok.contains("\n1\n"));
        let bad = bash(dir.path(), "echo oops >&2; exit 3", None, &c, &io).unwrap_err();
        assert!(bad.contains("oops") && bad.contains("exit 3"), "{bad}");
        let slow = bash(dir.path(), "sleep 5", Some(1), &c, &io).unwrap_err();
        assert!(slow.contains("killed after 1s"), "{slow}");
    }

    #[test]
    fn the_model_list_is_read_from_the_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Some(Ok(mut s)) = listener.incoming().next() {
                let mut buf = [0u8; 2048];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                assert!(head.starts_with("get /v1/models"), "{head}");
                assert!(head.contains("authorization: bearer k"), "{head}");
                let body = r#"{"data":[{"id":"b"},{"id":"a"},{"id":"a"}]}"#;
                let _ = s.write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len())
                        .as_bytes(),
                );
            }
        });
        assert_eq!(models(&url, "k").unwrap(), ["a", "b"]);
    }

    #[test]
    fn without_a_model_it_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let io = TestIo::new(Arc::new(FakeApi));
        let e = run(&Native::default(), dir.path(), "x", &io).unwrap_err();
        assert!(e.contains("pick a server and model"), "{e}");
    }
}
