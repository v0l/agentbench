use super::{Agent, Context, Event, Io, Turn, async_trait, brief, files, media};
use egui_bench::agent::skillset;
use egui_bench::agent::tools::{self, Spec, integer, object, string};
use egui_bench::agent::workspace::Workspace;
use foreman_core::{Entry, Question, Usage};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// How long the server may take to send the response headers. Servers that queue
/// requests or read a long prompt first can take a while, but not minutes.
const HEADERS: Duration = Duration::from_secs(90);
/// How long the stream may go without sending a byte. A healthy server streams
/// tokens, reasoning or keep-alive comments, so silence this long means the
/// connection is dead and the request is sent again.
const STALL: Duration = Duration::from_secs(45);
const BASH_DEFAULT: u64 = 120;
const BASH_MAX: u64 = 900;
const OUTPUT_MAX: usize = 30_000;
/// How many times a request that stalled, timed out or never connected is sent again.
const RETRIES: u32 = 3;

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
    /// The models a chat can be switched to, by the agent or by the user.
    #[serde(default)]
    pub low: Level,
    #[serde(default)]
    pub medium: Level,
    #[serde(default)]
    pub high: Level,
}

/// One rung of the ladder. A level with no model is not set up. One with no
/// server uses the foreman agent's, key and all; one with its own server never
/// sends it the foreman key.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Level {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub key: String,
}

fn default_url() -> String {
    egui_bench::agent::config::DEFAULT_URL.to_string()
}

pub const DEFAULT_STEPS: usize = 1000;
const OLD_DEFAULT_STEPS: usize = 60;

fn default_steps() -> usize {
    DEFAULT_STEPS
}

impl Default for Model {
    fn default() -> Self {
        Model {
            url: default_url(),
            model: String::new(),
            key: String::new(),
            steps: default_steps(),
            low: Level::default(),
            medium: Level::default(),
            high: Level::default(),
        }
    }
}

impl Model {
    /// The model a level names, or None when `name` is not a level or the
    /// level has no model set.
    pub fn level(&self, name: &str) -> Option<Model> {
        let level = match name.trim().to_lowercase().as_str() {
            "low" => &self.low,
            "medium" => &self.medium,
            "high" => &self.high,
            _ => return None,
        };
        let own = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        let model = own(&level.model)?;
        let mut out = self.clone();
        out.model = model;
        if let Some(url) = own(&level.url) {
            out.url = url;
            out.key = level.key.trim().to_string();
        }
        Some(out)
    }

    /// The model for what a chat has saved: a level, else a model id on the
    /// foreman agent's own server. An unset level is the agent's default.
    pub fn choose(&self, choice: &str) -> Model {
        let choice = choice.trim();
        if choice.is_empty() {
            return self.clone();
        }
        if let Some(m) = self.level(choice) {
            return m;
        }
        if foreman_core::LEVELS.contains(&choice.to_lowercase().as_str()) {
            return self.clone();
        }
        Model { model: choice.to_string(), ..self.clone() }
    }

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
        let mut m = self.0.read().unwrap_or_else(|e| e.into_inner()).clone();
        if m.steps == OLD_DEFAULT_STEPS || m.steps == 0 {
            m.steps = DEFAULT_STEPS;
        }
        m
    }

    pub fn set(&self, model: Model) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = model;
    }
}

#[derive(Default)]
pub struct Native {
    pub model: Shared,
    pub patience: Patience,
}

/// How long the model server may stay silent before a request is given up and sent again.
#[derive(Clone, Copy, Debug)]
pub struct Patience {
    pub headers: Duration,
    pub stall: Duration,
}

impl Default for Patience {
    fn default() -> Self {
        Patience { headers: HEADERS, stall: STALL }
    }
}

impl Native {
    pub fn new(model: Model) -> Native {
        Native { model: Shared::new(model), patience: Patience::default() }
    }

    pub fn shared(model: Shared) -> Native {
        Native { model, patience: Patience::default() }
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
                json!({ "id": string("The session id, as list_sessions gives it"), "last": id("How many messages, default 20") }),
                &["id"],
            ),
        ),
        Spec::new(
            "switch_model",
            "Change which model you run on, from the next step on, and for the rest of this chat. Use `high` for hard \
             reasoning, a tricky bug or a design you keep getting wrong; `low` for mechanical work like renames, \
             formatting and reading files; `medium` for the usual. Only levels the user has set up work.",
            object(
                json!({
                    "level": { "type": "string", "enum": foreman_core::LEVELS, "description": "low, medium or high" },
                    "reason": string("Why, in a few words"),
                }),
                &["level"],
            ),
        ),
        Spec::new(
            "ask_user",
            "Ask the user a question and wait for the answer. Give options when the answer is one of a few choices, \
             and set multiple when they may pick more than one. To ask several things at once, pass questions, a list of \
             {question, options, multiple}; the user answers them together. \
             Use it when you cannot go on without a decision, not for things you can find out yourself.",
            object(
                json!({
                    "question": string("The question, one or two sentences"),
                    "options": { "type": "array", "items": { "type": "string" }, "description": "Choices to pick from" },
                    "multiple": { "type": "boolean", "description": "The user may pick more than one option" },
                    "questions": {
                        "type": "array",
                        "description": "Several questions to ask together instead of question",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": string("The question, one or two sentences"),
                                "options": { "type": "array", "items": { "type": "string" }, "description": "Choices to pick from" },
                                "multiple": { "type": "boolean", "description": "The user may pick more than one option" },
                            },
                            "required": ["question"],
                        },
                    },
                }),
                &[],
            ),
        ),
    ]
}

fn question_of(v: &Value) -> Option<Question> {
    let text = v.get("question").and_then(Value::as_str)?.to_string();
    let options = v
        .get("options")
        .and_then(Value::as_array)
        .map(|o| o.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    let multiple = v.get("multiple").and_then(Value::as_bool).unwrap_or(false);
    Some(Question { text, options, multiple })
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
    let mut specs: Vec<Spec> = tools::builtin()
        .into_iter()
        .filter(|s| {
            !matches!(
                s.name.as_str(),
                "bash"
                    | "request_access"
                    | "workspace"
                    | "edit_file"
                    | "search_files"
                    | "read_file"
            )
        })
        .collect();
    specs.push(Spec::new(
        "read_file",
        "Read a file in the workspace. A text file is read with a line budget, in pages: the answer says \
         which offset to ask for next. A picture (png, jpg, gif, webp, bmp, tiff and more) is shown to you. \
         A sound file (wav, mp3, flac, ogg, m4a and more) is played to you. A video is shown as frames \
         sampled evenly through it; `frames` sets how many, 8 by default.",
        object(
            json!({
                "path": string("A path inside the workspace, relative to it"),
                "offset": integer("The line to start at, from 1 (text files)"),
                "limit": integer("How many lines (text files)"),
                "frames": integer("How many frames to take from a video, 1 to 16"),
            }),
            &["path"],
        ),
    ));
    specs.push(Spec::new(
        "search_files",
        "Search the text files in the worktree for a regular expression, like grep -rn. Case insensitive unless \
         case_sensitive is true. Narrow it with `include` globs such as \"*.rs\" or \"src/**/*.{ts,tsx}\" (a pattern \
         with no slash matches the file name at any depth) and `exclude` globs. .gitignore rules are honoured. Each \
         hit is path:line: text; context lines use a dash instead of a colon. Use output=files to list just the \
         files that match, or output=count for matches per file.",
        object(
            json!({
                "pattern": string("A regular expression, such as \"fn \\\\w+_test\" or \"TODO|FIXME\""),
                "path": string("A folder or file to search in, default the whole worktree"),
                "include": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Only search files matching one of these globs, e.g. [\"*.rs\", \"crates/**/*.toml\"]",
                },
                "exclude": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Skip files matching these globs, e.g. [\"*.lock\", \"tests/**\"]",
                },
                "literal": { "type": "boolean", "description": "Match pattern as plain text, not a regex" },
                "case_sensitive": { "type": "boolean", "description": "Match case exactly (default false)" },
                "context": { "type": "integer", "description": "Lines of context to show round each hit (max 10)" },
                "output": {
                    "type": "string",
                    "enum": ["content", "files", "count"],
                    "description": "content (default) shows matching lines; files lists file names; count gives matches per file",
                },
                "limit": { "type": "integer", "description": "Most matches (or files) to return, default 100" },
            }),
            &["pattern"],
        ),
    ));
    specs.push(Spec::new(
        "edit_file",
        "Replace exact text in a file in the worktree. Each edits[].oldText must match the file and appear once; \
         every edit is matched against the file as it was before this call, so do not overlap them. Put several \
         changes to one file in one call. Read the file first. The answer shows the diff.",
        object(
            json!({
                "path": string("A path in the worktree, relative to it"),
                "edits": {
                    "type": "array",
                    "items": object(
                        json!({
                            "oldText": string("The exact text to replace, with enough lines round it to be unique"),
                            "newText": string("What to put there"),
                        }),
                        &["oldText", "newText"],
                    ),
                },
                "all": { "type": "boolean", "description": "Replace every occurrence of each oldText" },
            }),
            &["path", "edits"],
        ),
    ));
    specs
}

pub fn specs() -> Vec<Spec> {
    let mut all = file_specs();
    all.push(bash_spec());
    all.extend(foreman_specs());
    all
}

/// The levels that have a model, lowest first.
fn levels(base: &Model) -> Vec<&'static str> {
    foreman_core::LEVELS.into_iter().filter(|l| base.level(l).is_some()).collect()
}

fn tool_list(base: &Model) -> Value {
    let any = !levels(base).is_empty();
    Value::Array(
        specs()
            .into_iter()
            .filter(|t| any || t.name != "switch_model")
            .map(|t| {
                json!({
                    "type": "function",
                    "function": { "name": t.name, "description": t.about, "parameters": t.schema },
                })
            })
            .collect(),
    )
}

fn system(turn: &Turn, base: &Model) -> String {
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
    let levels = levels(base);
    if !levels.is_empty() {
        out.push_str(&format!(
            "switch_model moves you to another model for the rest of this chat ({} are set up). Go up when you are stuck \
             or the problem is hard, and down for mechanical work.\n\n",
            levels.join(", ")
        ));
    }
    out.push_str(
        "- Work in the worktree with list_files, read_file, search_files, write_file and edit_file. Paths are relative to it. \
         Read a file before you edit it.\n\
         - read_file also shows you pictures, plays sound files and shows video as frames, so use it to look at screenshots, \
         images, recordings and clips instead of saying you cannot.\n\
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
    for e in foreman_core::since_summary(entries) {
        match e {
            Entry::Summary { text } => out.push(json!({
                "role": "user",
                "content": format!("Here is a summary of our conversation so far. Carry on from it.\n\n{text}"),
            })),
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
            Entry::Questions { questions, answers, .. } => {
                let said = questions
                    .iter()
                    .enumerate()
                    .map(|(i, q)| {
                        let a = answers.as_ref().and_then(|a| a.get(i)).map(String::as_str);
                        format!("{} {}", q.text, a.unwrap_or("nothing"))
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                out.push(json!({
                    "role": "assistant",
                    "content": format!("asked the user several questions and they answered: {said}"),
                }));
            }
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

#[async_trait]
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

    async fn models(&self) -> Result<Vec<String>, String> {
        let m = self.model.get();
        models(&m.url, &m.key).await
    }

    fn steerable(&self) -> bool {
        true
    }

    async fn run(&self, turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
        let base = self.model.get();
        let mut model = base.choose(turn.model.unwrap_or_default());
        if model.model.trim().is_empty() {
            return Err("pick a server and model for the foreman agent under settings".into());
        }
        let workspace = Workspace::open(turn.cwd)?;
        let mut messages = vec![json!({ "role": "system", "content": system(turn, &base) })];
        messages.extend(history(turn.history));
        messages.push(json!({ "role": "user", "content": turn.prompt }));
        let tools = tool_list(&base);
        for _ in 0..model.steps.max(1) {
            if io.stopped() {
                return Err("stopped".into());
            }
            let reply = complete(&model, &messages, &tools, self.patience, io).await?;
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
            let mut attached: Vec<Value> = Vec::new();
            for call in &reply.calls {
                let parsed = files::args(&call.args);
                let input = parsed.clone().unwrap_or(Value::String(call.args.clone()));
                io.emit(Event::ToolStart {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    input: input.clone(),
                });
                let name = files::canonical(&call.name);
                let result = match parsed {
                    Err(e) => Err(e),
                    Ok(args) => match name {
                        "switch_model" => switch(&base, &mut model, &args, turn.context, io),
                        "read_file" => {
                            let found = {
                                let (ws, args) = (workspace.clone(), args.clone());
                                tokio::task::spawn_blocking(move || media::read(&ws, &args))
                                    .await
                                    .map_err(|e| e.to_string())?
                            };
                            match found {
                                Some(found) => found.map(|m| {
                                    attached.extend(m.parts);
                                    m.note
                                }),
                                None => self.tool(name, &args, &workspace, turn.context, io).await,
                            }
                        }
                        _ => self.tool(name, &args, &workspace, turn.context, io).await,
                    },
                };
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
            // A tool result is text only, so what read_file saw goes in the next user message.
            if !attached.is_empty() {
                let mut content = vec![json!({
                    "type": "text",
                    "text": "The media from read_file, as the tool results above described:",
                })];
                content.extend(attached);
                messages.push(json!({ "role": "user", "content": content }));
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
        io.emit(Event::Notice(format!(
            "paused after {} steps, the limit set under agent settings; say continue to carry on",
            model.steps
        )));
        io.emit(Event::Done);
        Ok(())
    }
}

/// Move the rest of this turn, and the chat after it, to another level.
fn switch(
    base: &Model,
    model: &mut Model,
    args: &Value,
    cx: &Context,
    io: &dyn Io,
) -> Result<String, String> {
    let name = args.get("level").and_then(Value::as_str).ok_or("missing `level`")?.trim();
    let set = levels(base);
    if set.is_empty() {
        return Err("no model levels are set up; the user can add them under agent settings".into());
    }
    let Some(next) = base.level(name) else {
        return Err(format!(
            "{name:?} is not available; the levels set up are: {}",
            set.join(", ")
        ));
    };
    let name = name.to_lowercase();
    if next.model == model.model && next.url == model.url {
        return Ok(format!("already on {name} ({})", next.model));
    }
    io.api().call("set_model", &json!({ "model": name }), cx.session)?;
    let reason = args.get("reason").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    io.emit(Event::Notice(format!(
        "switched to {name}: {}{}{}",
        next.model,
        if reason.is_empty() { "" } else { ", " },
        reason
    )));
    let done = format!("now on {name} ({}); the next step runs on it", next.model);
    *model = next;
    Ok(done)
}

pub async fn models(url: &str, key: &str) -> Result<Vec<String>, String> {
    let base = url.trim().trim_end_matches('/').trim_end_matches("/chat/completions");
    if base.is_empty() {
        return Err("the server address is empty".into());
    }
    let mut req =
        crate::http::client().get(format!("{base}/models")).timeout(Duration::from_secs(10));
    if !key.trim().is_empty() {
        req = req.bearer_auth(key.trim());
    }
    let resp = req.send().await.map_err(|e| format!("{base}: {}", describe(&e)))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| describe(&e))?;
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

async fn complete(
    model: &Model,
    messages: &[Value],
    tools: &Value,
    patience: Patience,
    io: &dyn Io,
) -> Result<Reply, String> {
    let body = json!({
        "model": model.model.trim(),
        "messages": messages,
        "tools": tools,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let mut attempt = 0;
    loop {
        let failure = match attempt_once(model, &body, patience, io).await {
            Ok(reply) => return Ok(reply),
            Err(Failure::Fatal(e)) => return Err(e),
            Err(Failure::Transient(e)) => e,
        };
        if io.stopped() {
            return Err("stopped".into());
        }
        if attempt >= RETRIES {
            return Err(format!("{}: {failure}", model.url));
        }
        attempt += 1;
        let wait = backoff(attempt);
        io.emit(Event::Notice(format!(
            "{}: {failure}; retrying in {}s ({attempt}/{RETRIES})",
            model.url,
            wait.as_secs_f32().ceil()
        )));
        tokio::select! {
            _ = io.stop().cancelled() => return Err("stopped".into()),
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

enum Failure {
    /// Worth sending again: the server never answered, went quiet or dropped the line.
    Transient(String),
    /// The server answered with a refusal, or the user stopped the turn.
    Fatal(String),
}

async fn attempt_once(
    model: &Model,
    body: &Value,
    patience: Patience,
    io: &dyn Io,
) -> Result<Reply, Failure> {
    let mut req = crate::http::client().post(model.endpoint()).json(body);
    if !model.key.trim().is_empty() {
        req = req.bearer_auth(model.key.trim());
    }
    let resp = tokio::select! {
        _ = io.stop().cancelled() => return Err(Failure::Fatal("stopped".into())),
        r = tokio::time::timeout(patience.headers, req.send()) => match r {
            Err(_) => {
                let waited = patience.headers.as_secs_f32();
                return Err(Failure::Transient(format!("no answer in {waited}s")));
            }
            Ok(Err(e)) => return Err(Failure::Transient(describe(&e))),
            Ok(Ok(resp)) => resp,
        },
    };
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let e = format!("{status}: {}", text.trim());
        return Err(if status.is_server_error() && status.as_u16() != 500 {
            Failure::Transient(e)
        } else {
            Failure::Fatal(e)
        });
    }
    read_stream(resp.bytes_stream(), patience.stall, io).await
}

/// The cause chain of a request error, which reqwest keeps out of its own message.
pub fn describe(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = cause.source();
    }
    out
}

/// How long to wait before retry number `attempt` (1-based): 2s, 4s, 8s.
fn backoff(attempt: u32) -> Duration {
    if cfg!(test) {
        return Duration::from_millis(5);
    }
    Duration::from_secs(1 << attempt.min(6))
}

impl Native {
    async fn tool(
        &self,
        name: &str,
        args: &Value,
        ws: &Workspace,
        cx: &Context,
        io: &dyn Io,
    ) -> Result<String, String> {
        let number = |keys: &[&str]| files::number(args, keys);
        match name {
            "list_files" | "read_file" | "write_file" | "edit_file" | "search_files"
            | "load_skill" => {
                let (name, args, ws) = (name.to_string(), args.clone(), ws.clone());
                tokio::task::spawn_blocking(move || file_tool(&name, &args, &ws))
                    .await
                    .map_err(|e| e.to_string())?
            }
            "bash" => {
                bash(
                    ws.root(),
                    files::text(args, &["command", "cmd"]).ok_or("missing `command`")?,
                    number(&["timeout_s", "timeout"]).map(|n| n as u64),
                    cx,
                    io,
                )
                .await
            }
            "ask_user" => {
                let asked: Vec<Question> = args
                    .get("questions")
                    .and_then(Value::as_array)
                    .map(|l| l.iter().filter_map(question_of).collect())
                    .unwrap_or_default();
                if asked.is_empty() {
                    let one = question_of(args).ok_or("missing `question`")?;
                    let answer = io.ask(one).await?;
                    return Ok(format!("The user answered: {answer}"));
                }
                let answers = io.ask_many(asked.clone()).await?;
                let lines: Vec<String> = asked
                    .iter()
                    .zip(&answers)
                    .enumerate()
                    .map(|(i, (q, a))| {
                        let a = if a.is_empty() { "(no answer)" } else { a };
                        format!("{}. {} {a}", i + 1, q.text)
                    })
                    .collect();
                Ok(format!("The user answered:\n{}", lines.join("\n")))
            }
            n if FOREMAN_TOOLS.contains(&n) => io
                .api()
                .call(n, args, cx.session)
                .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()),
            other => {
                let names: Vec<String> = specs().into_iter().map(|s| s.name).collect();
                Err(format!(
                    "there is no tool called {other:?}; the tools are: {}",
                    names.join(", ")
                ))
            }
        }
    }
}

/// The tools that touch only the worktree's files. They block on the disk, so they run off
/// the async threads.
fn file_tool(name: &str, args: &Value, ws: &Workspace) -> Result<String, String> {
    let text = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
    let path = || files::text(args, &files::PATH).map(str::to_string);
    let need_path = || path().ok_or_else(|| "missing `path`".to_string());
    let number = |keys: &[&str]| files::number(args, keys);
    match name {
        "list_files" => ws.list(path().as_deref(), number(&["depth"])),
        "read_file" => {
            let offset = number(&["offset", "start_line", "line"]);
            let limit = number(&["limit", "lines", "max_lines"])
                .or_else(|| Some(number(&["end_line"])? + 1 - offset.unwrap_or(1)));
            ws.read(&need_path()?, offset, limit)
        }
        "write_file" => ws.write(
            &need_path()?,
            files::text(args, &["content", "text", "contents"]).unwrap_or_default(),
        ),
        "edit_file" => files::edit(ws, args),
        "search_files" => super::search::search(ws, args),
        "load_skill" => {
            let skills = skillset::discover(&skillset::roots(ws.root()), &[]);
            skillset::load(&skills, text("name").as_deref(), text("file").as_deref())
        }
        other => Err(format!("{other} is not a file tool")),
    }
}

/// Read a server-sent event stream into a reply, giving up when no byte has come for `stall`.
async fn read_stream<S, B, E>(stream: S, stall: Duration, io: &dyn Io) -> Result<Reply, Failure>
where
    S: futures_util::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::fmt::Display,
{
    let mut stream = std::pin::pin!(stream);
    let mut reply = Reply::default();
    let mut pending: Vec<u8> = Vec::new();
    let mut done = false;
    while !done {
        let chunk = tokio::select! {
            _ = io.stop().cancelled() => return Err(Failure::Fatal("stopped".into())),
            next = tokio::time::timeout(stall, stream.next()) => match next {
                Err(_) => return Err(Failure::Transient(stalled(stall, &reply))),
                Ok(None) => break,
                Ok(Some(Err(e))) => return Err(Failure::Transient(format!("the stream broke: {e}"))),
                Ok(Some(Ok(chunk))) => chunk,
            },
        };
        pending.extend_from_slice(chunk.as_ref());
        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            match sse_data(&line) {
                Some("[DONE]") => done = true,
                Some(data) => {
                    if let Ok(v) = serde_json::from_str::<Value>(data) {
                        take(&v, &mut reply, io);
                    }
                }
                None => {}
            }
        }
    }
    if let Some(data) = sse_data(&String::from_utf8_lossy(&pending))
        && let Ok(v) = serde_json::from_str::<Value>(data)
    {
        take(&v, &mut reply, io);
    }
    Ok(reply)
}

fn sse_data(line: &str) -> Option<&str> {
    let data = line.trim_end_matches(['\r', '\n']).strip_prefix("data:")?.trim();
    (!data.is_empty()).then_some(data)
}

fn stalled(stall: Duration, reply: &Reply) -> String {
    let got = if reply.text.is_empty() && reply.calls.is_empty() {
        "before it sent anything".to_string()
    } else {
        "partway through its reply".to_string()
    };
    format!("the server went quiet for {}s {got}", stall.as_secs_f32())
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

async fn bash(
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
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| format!("bash: {e}"))?;
    let read = |pipe: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf).await;
            }
            buf
        })
    };
    let out = read(child.stdout.take().map(|p| Box::new(p) as _));
    let err = read(child.stderr.take().map(|p| Box::new(p) as _));
    let status = tokio::select! {
        s = child.wait() => s.ok(),
        _ = tokio::time::sleep(limit) => None,
        _ = io.stop().cancelled() => None,
    };
    if status.is_none() {
        #[cfg(unix)]
        if let Some(pid) = child.id() {
            super::process::signal_group(pid, "-KILL").await;
        }
        let _ = child.kill().await;
    }
    let mut text = String::from_utf8_lossy(&out.await.unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err.await.unwrap_or_default()).into_owned();
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
            session: foreman_core::SessionId,
        ) -> Result<Value, String> {
            Ok(json!({ "tool": tool, "args": args, "session": session }))
        }
    }

    fn stub(replies: Vec<Vec<Value>>) -> (String, std::sync::mpsc::Receiver<Value>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        stub_on(listener, replies)
    }

    fn stub_on(
        listener: std::net::TcpListener,
        replies: Vec<Vec<Value>>,
    ) -> (String, std::sync::mpsc::Receiver<Value>) {
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

    async fn run(native: &Native, dir: &Path, prompt: &str, io: &TestIo) -> Result<(), String> {
        let c = context();
        let turn = Turn { prompt, cwd: dir, model: None, resume: None, history: &[], context: &c };
        native.run(&turn, io).await
    }

    #[tokio::test]
    async fn it_edits_files_and_reaches_the_tracker() {
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
        run(&native, dir.path(), "change world", &io).await.unwrap();
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

    #[tokio::test]
    async fn a_steer_lands_between_steps() {
        let dir = tempfile::tempdir().unwrap();
        let (url, asked) =
            stub(vec![vec![call("c1", "list_files", json!({}))], vec![say("ok, stopping")]]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        io.inbox.lock().unwrap().push("actually stop".into());
        run(&native, dir.path(), "look around", &io).await.unwrap();
        assert!(io.events().contains(&Event::Steered { text: "actually stop".into() }));
        let _ = asked.recv().unwrap();
        let second = asked.recv().unwrap();
        let last = second["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last, json!({"role": "user", "content": "actually stop"}));
    }

    #[tokio::test]
    async fn ask_user_waits_for_the_answer() {
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
        run(&native, dir.path(), "pick", &io).await.unwrap();
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

    #[tokio::test]
    async fn ask_user_puts_several_questions_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (url, asked) = stub(vec![
            vec![call(
                "c1",
                "ask_user",
                json!({"questions": [
                    {"question": "which licence?", "options": ["MIT", "Apache-2.0"]},
                    {"question": "which targets?", "options": ["linux", "mac"], "multiple": true},
                ]}),
            )],
            vec![say("noted")],
        ]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        io.answers.lock().unwrap().extend(["MIT".to_string(), "linux, mac".to_string()]);
        run(&native, dir.path(), "pick", &io).await.unwrap();
        let first = asked.recv().unwrap();
        let spec = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["function"]["name"] == "ask_user")
            .unwrap()
            .clone();
        assert!(spec["function"]["parameters"]["properties"]["multiple"].is_object());
        assert!(spec["function"]["parameters"]["properties"]["questions"].is_object());
        let second = asked.recv().unwrap();
        let tool = second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap()
            .clone();
        assert_eq!(
            tool["content"],
            "The user answered:\n1. which licence? MIT\n2. which targets? linux, mac"
        );
    }

    #[tokio::test]
    async fn read_file_shows_a_picture_to_the_model() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shot.png"), b"PNGDATA").unwrap();
        std::fs::write(dir.path().join("a.txt"), "plain\n").unwrap();
        let mut second_call = call("c2", "read_file", json!({"path": "a.txt"}));
        second_call["choices"][0]["delta"]["tool_calls"][0]["index"] = json!(1);
        let (url, asked) = stub(vec![
            vec![call("c1", "read_file", json!({"path": "shot.png"})), second_call],
            vec![say("a red box")],
        ]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        run(&native, dir.path(), "look", &io).await.unwrap();
        let _ = asked.recv().unwrap();
        let second = asked.recv().unwrap();
        let messages = second["messages"].as_array().unwrap();
        let tools: Vec<&Value> = messages.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(tools.len(), 2);
        assert!(tools[0]["content"].as_str().unwrap().contains("shot.png: image"));
        assert!(tools[1]["content"].as_str().unwrap().contains("plain"));
        let last = messages.last().unwrap();
        assert_eq!(last["role"], "user");
        let url = last["content"][2]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
        let tool_at = messages.iter().rposition(|m| m["role"] == "tool").unwrap();
        assert!(tool_at < messages.len() - 1, "media must follow every tool result");
    }

    #[test]
    fn a_summary_replaces_what_came_before_it() {
        let entries = vec![
            Entry::User { text: "old".into() },
            Entry::Summary { text: "we fixed login".into() },
            Entry::User { text: "next".into() },
        ];
        let h = history(&entries);
        assert_eq!(h.len(), 2);
        assert!(h[0]["content"].as_str().unwrap().contains("we fixed login"));
        assert_eq!(h[1]["content"], "next");
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

    #[tokio::test]
    async fn bash_runs_in_the_worktree_and_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let io = TestIo::new(Arc::new(FakeApi));
        let c = context();
        let ok = bash(dir.path(), "pwd; echo $FOREMAN_SESSION", None, &c, &io).await.unwrap();
        assert!(
            ok.contains(&dir.path().canonicalize().unwrap().display().to_string())
                || ok.contains(&dir.path().display().to_string())
        );
        assert!(ok.contains(&format!("\n{}\n", crate::agent::test_session())));
        let bad = bash(dir.path(), "echo oops >&2; exit 3", None, &c, &io).await.unwrap_err();
        assert!(bad.contains("oops") && bad.contains("exit 3"), "{bad}");
        let slow = bash(dir.path(), "sleep 5", Some(1), &c, &io).await.unwrap_err();
        assert!(slow.contains("killed after 1s"), "{slow}");
    }

    #[tokio::test]
    async fn the_model_list_is_read_from_the_server() {
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
        assert_eq!(models(&url, "k").await.unwrap(), ["a", "b"]);
    }

    #[tokio::test]
    async fn pi_shaped_calls_and_bad_json_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();
        let (url, asked) = stub(vec![
            vec![call(
                "c1",
                "edit",
                json!({"path": "a.txt", "edits": [{"oldText": "world", "newText": "foreman"}]}),
            )],
            vec![
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c2","function":{"name":"read_file","arguments":"{\"path\": "}}]}}]}),
            ],
            vec![call("c3", "frobnicate", json!({}))],
            vec![say("done")],
        ]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        run(&native, dir.path(), "go", &io).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "hello foreman\n");
        let ends: Vec<(String, bool, String)> = io
            .events()
            .into_iter()
            .filter_map(|e| match e {
                Event::ToolEnd { name, error, output, .. } => Some((name, error, output)),
                _ => None,
            })
            .collect();
        assert!(!ends[0].1, "pi's edit shape works: {}", ends[0].2);
        assert!(ends[0].2.contains("+foreman"), "{}", ends[0].2);
        assert!(ends[1].1 && ends[1].2.contains("not valid JSON"), "{}", ends[1].2);
        assert!(ends[2].1 && ends[2].2.contains("the tools are:"), "{}", ends[2].2);
        let _ = asked.recv();
    }

    /// A server that hangs up on its first `drops` connections, then answers.
    fn flaky(drops: usize, events: Vec<Value>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..drops {
                let Ok((s, _)) = listener.accept() else { return };
                drop(s);
            }
            let _ = tx.send(listener);
        });
        std::thread::spawn(move || {
            if let Ok(listener) = rx.recv() {
                stub_on(listener, vec![events]);
            }
        });
        url
    }

    #[tokio::test]
    async fn a_dropped_connection_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let url = flaky(2, vec![say("hello")]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        run(&native, dir.path(), "go", &io).await.unwrap();
        let events = io.events();
        let notices = events
            .iter()
            .filter(|e| matches!(e, Event::Notice(n) if n.contains("retrying")))
            .count();
        assert_eq!(notices, 2);
        assert!(events.iter().any(|e| matches!(e, Event::Text { delta } if delta == "hello")));
    }

    #[tokio::test]
    async fn retries_give_up_and_report_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let url = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}/v1", l.local_addr().unwrap())
        };
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        let e = run(&native, dir.path(), "go", &io).await.unwrap_err();
        assert!(!e.is_empty());
        let notices = io
            .events()
            .iter()
            .filter(|e| matches!(e, Event::Notice(n) if n.contains("retrying")))
            .count();
        assert_eq!(notices, RETRIES as usize);
    }

    /// A server that, for its first `hangs` connections, sends the headers and the start
    /// of a reply and then goes silent without closing, as a dropped network does. After
    /// that it answers normally.
    fn hangs_midway(hangs: usize, events: Vec<Value>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for _ in 0..hangs {
                let Ok((mut s, _)) = listener.accept() else { return };
                let mut buf = [0u8; 65536];
                let _ = s.read(&mut buf);
                let first = format!("data: {}\n\n", say("thinking about it"));
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{first}\r\n",
                    first.len()
                );
                let _ = s.flush();
                held.push(s);
            }
            let _ = tx.send(listener);
            std::thread::sleep(Duration::from_secs(600));
            drop(held);
        });
        std::thread::spawn(move || {
            if let Ok(listener) = rx.recv() {
                stub_on(listener, vec![events]);
            }
        });
        url
    }

    /// A server that accepts the connection and never answers at all.
    fn never_answers() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for s in listener.incoming().flatten() {
                held.push(s);
            }
        });
        url
    }

    fn impatient(url: String) -> Native {
        Native {
            patience: Patience {
                headers: Duration::from_millis(400),
                stall: Duration::from_millis(400),
            },
            ..Native::new(Model { url, model: "m".into(), ..Model::default() })
        }
    }

    #[tokio::test]
    async fn a_stream_that_goes_silent_is_dropped_and_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let native = impatient(hangs_midway(1, vec![say("hello")]));
        let io = TestIo::new(Arc::new(FakeApi));
        let started = Instant::now();
        run(&native, dir.path(), "go", &io).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
        let events = io.events();
        assert!(
            events.iter().any(|e| matches!(e, Event::Notice(n) if n.contains("went quiet") && n.contains("retrying"))),
            "{events:?}"
        );
        assert!(events.iter().any(|e| matches!(e, Event::Text { delta } if delta == "hello")));
    }

    #[tokio::test]
    async fn a_server_that_never_answers_is_given_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let native = impatient(never_answers());
        let io = TestIo::new(Arc::new(FakeApi));
        let started = Instant::now();
        let e = run(&native, dir.path(), "go", &io).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5), "took {:?}", started.elapsed());
        assert!(e.contains("no answer"), "{e}");
    }

    #[tokio::test]
    async fn stop_ends_a_silent_stream_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let native = Native::new(Model {
            url: hangs_midway(1, vec![]),
            model: "m".into(),
            ..Model::default()
        });
        let io = Arc::new(TestIo::new(Arc::new(FakeApi)));
        let stop = io.stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.cancel();
        });
        let started = Instant::now();
        let e = run(&native, dir.path(), "go", &io).await.unwrap_err();
        assert_eq!(e, "stopped");
        assert!(started.elapsed() < Duration::from_secs(2), "took {:?}", started.elapsed());
    }

    #[test]
    fn the_default_patience_is_well_under_ten_minutes() {
        let p = Patience::default();
        assert!(p.stall <= Duration::from_secs(60), "{p:?}");
        assert!(p.headers <= Duration::from_secs(120), "{p:?}");
    }

    #[tokio::test]
    async fn an_error_status_is_not_retried() {
        let (url, seen) = crate::titles::fake::server(vec![(500, "{}".into())]);
        let dir = tempfile::tempdir().unwrap();
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        let e = run(&native, dir.path(), "go", &io).await.unwrap_err();
        assert!(e.contains("500"), "{e}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn hitting_the_step_limit_pauses_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _asked) = stub(vec![
            vec![call("c1", "list_files", json!({}))],
            vec![call("c2", "list_files", json!({}))],
        ]);
        let native = Native::new(Model { url, model: "m".into(), steps: 2, ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        run(&native, dir.path(), "go", &io).await.unwrap();
        let events = io.events();
        assert!(!events.iter().any(|e| matches!(e, Event::Error(_))));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Notice(n) if n.contains("paused after 2 steps")))
        );
    }

    #[test]
    fn the_old_sixty_step_default_is_raised() {
        let shared = Shared::new(Model { steps: 60, ..Model::default() });
        assert_eq!(shared.get().steps, DEFAULT_STEPS);
        let custom = Shared::new(Model { steps: 250, ..Model::default() });
        assert_eq!(custom.get().steps, 250);
    }

    #[test]
    fn a_level_names_a_model_and_borrows_the_server_and_key_only_when_it_has_none() {
        let m = Model {
            url: "http://main/v1".into(),
            model: "mid".into(),
            key: "secret".into(),
            low: Level { model: "tiny".into(), ..Level::default() },
            high: Level { url: "http://big/v1".into(), model: "huge".into(), key: "other".into() },
            ..Model::default()
        };
        let low = m.level("LOW").unwrap();
        assert_eq!(
            (low.model.as_str(), low.url.as_str(), low.key.as_str()),
            ("tiny", "http://main/v1", "secret")
        );
        let high = m.level("high").unwrap();
        assert_eq!(
            (high.model.as_str(), high.url.as_str(), high.key.as_str()),
            ("huge", "http://big/v1", "other")
        );
        assert!(m.level("medium").is_none());
        assert!(m.level("tiny").is_none());
        // A server of its own without a key does not get the foreman key.
        let bare = Model {
            high: Level { url: "http://big/v1".into(), model: "x".into(), key: String::new() },
            ..m.clone()
        };
        assert_eq!(bare.level("high").unwrap().key, "");
        assert_eq!(levels(&m), ["low", "high"]);
    }

    #[test]
    fn a_chats_saved_model_is_a_level_or_an_id_and_an_unset_level_is_the_default() {
        let m = Model {
            model: "mid".into(),
            low: Level { model: "tiny".into(), ..Level::default() },
            ..Model::default()
        };
        assert_eq!(m.choose("").model, "mid");
        assert_eq!(m.choose("low").model, "tiny");
        assert_eq!(m.choose("medium").model, "mid");
        assert_eq!(m.choose("qwen-big").model, "qwen-big");
    }

    #[tokio::test]
    async fn the_agent_can_switch_to_a_higher_model_on_another_server_mid_turn() {
        let dir = tempfile::tempdir().unwrap();
        let (big_url, big_asked) = stub(vec![vec![say("solved")]]);
        let (url, asked) = stub(vec![vec![call(
            "c1",
            "switch_model",
            json!({"level": "high", "reason": "tricky bug"}),
        )]]);
        let native = Native::new(Model {
            url,
            model: "m".into(),
            high: Level { url: big_url, model: "big".into(), key: String::new() },
            ..Model::default()
        });
        let io = TestIo::new(Arc::new(FakeApi));
        run(&native, dir.path(), "fix it", &io).await.unwrap();
        let first = asked.recv().unwrap();
        assert_eq!(first["model"], "m");
        let names: Vec<&str> = first["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"switch_model"));
        assert!(first["messages"][0]["content"].as_str().unwrap().contains("switch_model"));
        let second = big_asked.recv().unwrap();
        assert_eq!(second["model"], "big");
        let events = io.events();
        assert!(events.iter().any(
            |e| matches!(e, Event::Notice(n) if n.contains("switched to high: big, tricky bug"))
        ));
        assert!(events.iter().any(|e| matches!(e, Event::Text { delta } if delta == "solved")));
    }

    #[tokio::test]
    async fn without_levels_there_is_no_switch_tool_and_asking_for_one_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (url, asked) =
            stub(vec![vec![call("c1", "switch_model", json!({"level": "high"}))], vec![say("ok")]]);
        let native = Native::new(Model { url, model: "m".into(), ..Model::default() });
        let io = TestIo::new(Arc::new(FakeApi));
        run(&native, dir.path(), "x", &io).await.unwrap();
        let first = asked.recv().unwrap();
        assert!(
            first["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t["function"]["name"] != "switch_model")
        );
        let second = asked.recv().unwrap();
        let last = second["messages"].as_array().unwrap().last().unwrap();
        assert!(last["content"].as_str().unwrap().contains("no model levels are set up"), "{last}");
    }

    #[tokio::test]
    async fn a_chat_saved_on_a_level_starts_its_turn_on_that_model() {
        let dir = tempfile::tempdir().unwrap();
        let (url, asked) = stub(vec![vec![say("hi")]]);
        let native = Native::new(Model {
            url,
            model: "m".into(),
            low: Level { model: "tiny".into(), ..Level::default() },
            ..Model::default()
        });
        let io = TestIo::new(Arc::new(FakeApi));
        let c = context();
        let turn = Turn {
            prompt: "x",
            cwd: dir.path(),
            model: Some("low"),
            resume: None,
            history: &[],
            context: &c,
        };
        native.run(&turn, &io).await.unwrap();
        assert_eq!(asked.recv().unwrap()["model"], "tiny");
    }

    #[tokio::test]
    async fn without_a_model_it_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let io = TestIo::new(Arc::new(FakeApi));
        let e = run(&Native::default(), dir.path(), "x", &io).await.unwrap_err();
        assert!(e.contains("pick a server and model"), "{e}");
    }
}
