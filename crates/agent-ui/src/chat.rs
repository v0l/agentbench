use super::config::Config;
use super::grants::{self, Grants};
use super::workspace::Workspace;
use super::{Backend, CancellationToken, Cx, Desk, Services, local, skillset, tools};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

const PATIENCE: Duration = Duration::from_secs(600);
pub const ASK_USER: &str = "ask_user";

fn ask_spec() -> tools::Spec {
    let one = json!({
        "question": tools::string("The question, one or two sentences"),
        "options": { "type": "array", "items": { "type": "string" }, "description": "Choices to pick from" },
        "multiple": tools::boolean("The user may pick more than one option"),
    });
    let mut all = one.clone();
    all["questions"] = json!({
        "type": "array",
        "description": "Several questions to ask together instead of question",
        "items": { "type": "object", "properties": one, "required": ["question"] },
    });
    tools::Spec::new(
        ASK_USER,
        "Ask the user a question and wait for the answer. Give options when the answer is one of a few choices, \
         and set multiple when they may pick more than one. To ask several things at once, pass questions. \
         Use it when you cannot go on without a decision, not for things you can find out yourself.",
        tools::object(all, &[]),
    )
}

fn offered(backend: &dyn Backend) -> Value {
    let mut list = tools::list(backend);
    let have = |l: &Value| {
        l.as_array().is_some_and(|a| a.iter().any(|t| t["function"]["name"] == ASK_USER))
    };
    if !have(&list)
        && !backend.leave_out().contains(&ASK_USER)
        && let Some(a) = list.as_array_mut()
    {
        let s = ask_spec();
        a.push(json!({
            "type": "function",
            "function": { "name": s.name, "description": s.about, "parameters": s.schema },
        }));
    }
    list
}

pub fn questions_of(args: &Value) -> Vec<Asking> {
    let one = |q: &Value| {
        let text = q.get("question").or_else(|| q.get("text"))?.as_str()?.trim();
        (!text.is_empty()).then(|| Asking {
            text: text.to_string(),
            options: q
                .get("options")
                .and_then(Value::as_array)
                .map(|o| o.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
            multiple: q.get("multiple").and_then(Value::as_bool).unwrap_or(false),
        })
    };
    match args.get("questions").and_then(Value::as_array) {
        Some(qs) if !qs.is_empty() => qs.iter().filter_map(one).collect(),
        _ => one(args).into_iter().collect(),
    }
}

pub struct Attached {
    pub name: String,
    pub mime: String,
    pub data: Arc<Vec<u8>>,
}

impl Attached {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    fn part(&self) -> Value {
        use base64::Engine;
        if self.is_image() {
            let b64 = base64::engine::general_purpose::STANDARD.encode(self.data.as_slice());
            return json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{b64}", self.mime) },
            });
        }
        let text = String::from_utf8_lossy(&self.data);
        json!({ "type": "text", "text": format!("{}:\n```\n{}\n```", self.name, text.trim_end()) })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asking {
    pub text: String,
    pub options: Vec<String>,
    pub multiple: bool,
}

pub enum Turn {
    You { text: String, files: Vec<Attached> },
    Said(String),
    Thinking(String),
    Did { name: String, args: String, answer: Option<Result<String, String>> },
    Asked { id: u64, questions: Vec<Asking>, answers: Option<Vec<String>> },
    Fault(String),
}

enum Update {
    Delta(String),
    Thinking(String),
    Said,
    Calling { name: String, args: String },
    Answered(Result<String, String>),
    Asking { questions: Vec<Asking>, reply: tokio::sync::oneshot::Sender<Vec<String>> },
    Fault(String),
    Done,
}

pub struct Chat {
    pub backend: Arc<dyn Backend>,
    pub grants: Grants,
    pub config: Config,
    pub turns: Vec<Turn>,
    pub draft: String,
    pub files: Vec<Attached>,
    pub services: Arc<Services>,
    history: Vec<Value>,
    remembered: usize,
    updates: Option<Receiver<Update>>,
    stop: CancellationToken,
    speaking: bool,
    thinking: bool,
    asking: Vec<(u64, tokio::sync::oneshot::Sender<Vec<String>>)>,
    asked: u64,
}

impl Chat {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self {
            config: Config::load(backend.config_dir().as_deref()),
            grants: Grants::default(),
            backend,
            turns: Vec::new(),
            draft: String::new(),
            files: Vec::new(),
            services: Arc::new(Services::default()),
            history: Vec::new(),
            remembered: 0,
            updates: None,
            stop: CancellationToken::new(),
            speaking: false,
            thinking: false,
            asking: Vec::new(),
            asked: 0,
        }
    }

    pub fn with_services(mut self, services: Services) -> Self {
        self.services = Arc::new(services);
        self
    }

    pub fn workspace_path(&self) -> std::path::PathBuf {
        self.config.workspace_path(self.backend.app())
    }

    pub fn busy(&self) -> bool {
        self.updates.is_some()
    }

    pub fn clear(&mut self) {
        for grant in self.grants.list() {
            self.grants.revoke(&grant.path);
        }
        self.turns.clear();
        self.history.clear();
        self.remembered = 0;
        let (services, owner) =
            (self.services.clone(), self.workspace_path().display().to_string());
        super::runtime().spawn(async move { services.jobs.kill_all(&owner).await });
    }

    pub fn interrupt(&self) {
        self.stop.cancel();
    }

    pub fn ask(&mut self, text: &str, desk: Desk) {
        self.ask_with(text, Vec::new(), desk);
    }

    pub fn answer(&mut self, id: u64, answers: Vec<String>) {
        if let Some(at) = self.asking.iter().position(|(i, _)| *i == id) {
            let (_, reply) = self.asking.remove(at);
            let _ = reply.send(answers.clone());
        }
        for t in &mut self.turns {
            if let Turn::Asked { id: i, answers: a, .. } = t
                && *i == id
            {
                *a = Some(answers.clone());
            }
        }
    }

    pub fn ask_with(&mut self, text: &str, files: Vec<Attached>, desk: Desk) {
        let text = text.trim();
        if (text.is_empty() && files.is_empty()) || self.busy() {
            return;
        }
        let content = if files.is_empty() {
            json!(text)
        } else {
            let mut parts = vec![json!({ "type": "text", "text": text })];
            parts.extend(files.iter().map(Attached::part));
            Value::Array(parts)
        };
        self.turns.push(Turn::You { text: text.to_string(), files });
        if let Some(fault) = self.config.fault() {
            self.turns.push(Turn::Fault(format!("{fault}: set one under Model")));
            self.remembered = self.turns.len();
            return;
        }
        self.history.push(json!({ "role": "user", "content": content }));
        let opened = Workspace::open(&self.workspace_path()).and_then(|w| {
            grants::vet(
                &w.root().display().to_string(),
                true,
                self.backend.config_dir().as_deref(),
                w.root(),
            )
            .map(|_| w)
        });
        let workspace = match opened {
            Ok(w) => w.with_grants(self.grants.clone()),
            Err(e) => {
                self.turns.push(Turn::Fault(format!("the workspace: {e}")));
                self.remembered = self.turns.len();
                self.history.pop();
                return;
            }
        };
        let (out, updates) = channel();
        self.updates = Some(updates);
        self.speaking = false;
        self.thinking = false;
        self.stop = CancellationToken::new();
        let worker = Worker {
            backend: self.backend.clone(),
            config: self.config.clone(),
            cx: Cx { workspace, desk, stop: self.stop.clone(), services: self.services.clone() },
            history: self.history.clone(),
            out,
        };
        super::runtime().spawn(worker.run());
    }

    pub fn poll(&mut self) {
        let Some(rx) = self.updates.as_ref() else { return };
        let mut done = false;
        for u in rx.try_iter().collect::<Vec<_>>() {
            match u {
                Update::Delta(text) => {
                    self.thinking = false;
                    if self.speaking
                        && let Some(Turn::Said(s)) = self.turns.last_mut()
                    {
                        s.push_str(&text);
                        continue;
                    }
                    let text = text.trim_start();
                    if text.is_empty() {
                        continue;
                    }
                    self.turns.push(Turn::Said(text.to_string()));
                    self.speaking = true;
                }
                Update::Thinking(text) => {
                    if self.thinking
                        && let Some(Turn::Thinking(s)) = self.turns.last_mut()
                    {
                        s.push_str(&text);
                        continue;
                    }
                    let text = text.trim_start();
                    if text.is_empty() {
                        continue;
                    }
                    self.speaking = false;
                    self.thinking = true;
                    self.turns.push(Turn::Thinking(text.to_string()));
                }
                Update::Asking { questions, reply } => {
                    self.speaking = false;
                    self.thinking = false;
                    self.asked += 1;
                    self.asking.push((self.asked, reply));
                    self.turns.push(Turn::Asked { id: self.asked, questions, answers: None });
                }
                Update::Said => {
                    self.speaking = false;
                    self.thinking = false;
                    if let Some(Turn::Said(s)) = self.turns.last_mut() {
                        s.truncate(s.trim_end().len());
                    }
                }
                Update::Calling { name, args } => {
                    self.speaking = false;
                    self.thinking = false;
                    self.turns.push(Turn::Did { name, args, answer: None });
                }
                Update::Answered(r) => {
                    if let Some(Turn::Did { answer, .. }) = self
                        .turns
                        .iter_mut()
                        .rev()
                        .find(|t| matches!(t, Turn::Did { answer: None, .. }))
                    {
                        *answer = Some(r);
                    }
                }
                Update::Fault(e) => {
                    self.speaking = false;
                    self.turns.push(Turn::Fault(e));
                }
                Update::Done => done = true,
            }
        }
        if done {
            self.updates = None;
            self.speaking = false;
            self.thinking = false;
            self.asking.clear();
            self.remember();
        }
    }

    fn remember(&mut self) {
        for t in &self.turns[self.remembered..] {
            let content = match t {
                Turn::Said(s) => s.clone(),
                Turn::Did { name, args, answer: Some(Ok(_)) } => {
                    format!("called {name}({args}) and read the answer")
                }
                Turn::Did { name, args, answer: Some(Err(e)) } => {
                    format!("called {name}({args}): {e}")
                }
                _ => continue,
            };
            self.history.push(json!({ "role": "assistant", "content": content }));
        }
        self.remembered = self.turns.len();
    }
}

struct Worker {
    backend: Arc<dyn Backend>,
    config: Config,
    cx: Cx,
    history: Vec<Value>,
    out: Sender<Update>,
}

#[derive(Default)]
struct Completion {
    text: String,
    calls: Vec<Call>,
}

#[derive(Default, Clone)]
struct Call {
    id: String,
    name: String,
    args: String,
}

impl Worker {
    fn say(&self, u: Update) {
        let _ = self.out.send(u);
        self.cx.desk.ring();
    }

    async fn run(mut self) {
        if let Err(e) = self.exchange().await {
            self.say(Update::Fault(e));
        }
        self.say(Update::Done);
    }

    async fn exchange(&mut self) -> Result<(), String> {
        let tools = offered(self.backend.as_ref());
        let steps = self.config.steps.max(1);
        for _ in 0..steps {
            if self.cx.stop.is_cancelled() {
                return Err("stopped".into());
            }
            let reply = tokio::select! {
                r = self.complete(&tools) => r?,
                _ = self.cx.stop.cancelled() => return Err("stopped".into()),
            };
            let mut message = json!({ "role": "assistant", "content": reply.text });
            if !reply.calls.is_empty() {
                message["tool_calls"] = Value::Array(
                    reply
                        .calls
                        .iter()
                        .map(|c| {
                            json!({
                                "id": c.id,
                                "type": "function",
                                "function": { "name": c.name, "arguments": c.args },
                            })
                        })
                        .collect(),
                );
            }
            self.history.push(message);
            if reply.calls.is_empty() {
                self.say(Update::Said);
                return Ok(());
            }
            for call in reply.calls {
                let answer = self.run_tool(&call).await;
                let content = match &answer {
                    Ok(v) => v.clone(),
                    Err(e) => json!({ "error": e }).to_string(),
                };
                self.history.push(json!({
                    "role": "tool",
                    "tool_call_id": call.id,
                    "content": content,
                }));
                self.say(Update::Answered(answer));
            }
        }
        Err(format!("stopped after {steps} tool calls"))
    }

    async fn run_tool(&self, call: &Call) -> Result<String, String> {
        self.say(Update::Calling { name: call.name.clone(), args: call.args.clone() });
        let args: Value = if call.args.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&call.args).map_err(|e| format!("bad arguments: {e}"))?
        };
        if call.name == ASK_USER && !self.backend.specs().iter().any(|s| s.name == ASK_USER) {
            return self.ask_user(&args).await;
        }
        local::run(self.backend.as_ref(), &self.cx, &call.name, &args)
            .await
            .unwrap_or_else(|| Err(format!("no tool called {}", call.name)))
    }

    async fn ask_user(&self, args: &Value) -> Result<String, String> {
        let questions = questions_of(args);
        if questions.is_empty() {
            return Err("missing `question`".into());
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        self.say(Update::Asking { questions: questions.clone(), reply });
        let got = tokio::select! {
            a = answer => a.map_err(|_| "the question was dropped".to_string())?,
            _ = self.cx.stop.cancelled() => return Err("stopped".into()),
        };
        Ok(questions
            .iter()
            .zip(&got)
            .map(|(q, a)| format!("{}\n{a}", q.text))
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    async fn complete(&self, tools: &Value) -> Result<Completion, String> {
        let mut messages = vec![json!({ "role": "system", "content": self.brief() })];
        messages.extend(self.history.iter().cloned());
        let mut body = json!({
            "model": self.config.model,
            "messages": messages,
            "tools": tools,
            "stream": true,
        });
        self.config.sampling.over(self.backend.sampling()).add_to(&mut body);
        if let Some(kwargs) = self.config.chat_template_kwargs() {
            body["chat_template_kwargs"] = kwargs;
        }
        let mut req = super::http::client()
            .post(self.config.endpoint())
            .header("content-type", "application/json")
            .header("user-agent", self.backend.user_agent())
            .timeout(PATIENCE)
            .body(body.to_string());
        if !self.config.key.trim().is_empty() {
            req = req.bearer_auth(self.config.key.trim());
        }
        let resp = req.send().await.map_err(|e| super::http::describe(&e))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("{status}: {}", text.trim()));
        }
        let mut reply = Completion::default();
        let mut pending = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| super::http::describe(&e))?;
            pending.extend_from_slice(&chunk);
            while let Some(at) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=at).collect();
                self.take_line(&String::from_utf8_lossy(&line), &mut reply);
            }
        }
        if !pending.is_empty() {
            self.take_line(&String::from_utf8_lossy(&pending), &mut reply);
        }
        Ok(reply)
    }

    fn take_line(&self, line: &str, reply: &mut Completion) {
        let Some(data) = line.trim_end().strip_prefix("data:") else { return };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        if let Ok(v) = serde_json::from_str::<Value>(data) {
            self.take_delta(&v, reply);
        }
    }

    fn take_delta(&self, v: &Value, reply: &mut Completion) {
        let Some(delta) = v.pointer("/choices/0/delta") else { return };
        for key in ["reasoning_content", "reasoning"] {
            if let Some(t) = delta.get(key).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                self.say(Update::Thinking(t.to_string()));
            }
        }
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            reply.text.push_str(text);
            self.say(Update::Delta(text.to_string()));
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

    fn brief(&self) -> String {
        let root = self.cx.workspace.root();
        let skills = skillset::discover(&skillset::roots(root), &self.backend.preferred_skills());
        let base = self.backend.brief(root, &skills);
        match self.config.brief.trim() {
            "" => base,
            extra => format!("{base}\n\n{extra}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::toolkit;
    use super::*;
    use std::io::{Read, Write};

    pub struct Test(pub toolkit::Toolkit);

    impl Test {
        pub fn new() -> Self {
            Self(toolkit::Toolkit::new().with(toolkit::Tool::host(
                "list_things",
                "List the things the host has, with their state.",
                tools::object(json!({}), &[]),
            )))
        }
    }

    #[super::super::async_trait]
    impl Backend for Test {
        fn app(&self) -> &'static str {
            "egui-bench-test"
        }

        fn sampling(&self) -> super::super::config::Sampling {
            super::super::config::Sampling {
                temperature: Some(1.0),
                top_k: Some(40),
                ..Default::default()
            }
        }

        fn brief(&self, workspace: &std::path::Path, skills: &[skillset::Skill]) -> String {
            format!("test brief {} {}", workspace.display(), skills.len())
        }

        fn welcome(&self) -> super::super::Welcome {
            super::super::Welcome {
                kicker: String::new(),
                title: String::new(),
                text: String::new(),
                placeholder: String::new(),
            }
        }

        fn toolkit(&self) -> &toolkit::Toolkit {
            &self.0
        }
    }

    fn chat() -> Chat {
        Chat::new(Arc::new(Test::new()))
    }

    fn worker(
        url: String,
    ) -> (Worker, Receiver<Update>, std::sync::mpsc::Receiver<super::super::Ask>) {
        let (out, updates) = channel();
        let (desk, asks) = crate::desk(&egui::Context::default());
        let dir = std::env::temp_dir().join(format!("bench-chat-ws-{}", std::process::id()));
        let w = Worker {
            backend: Arc::new(Test::new()),
            config: Config { url, model: "stub".into(), ..Config::default() },
            cx: Cx::new(Workspace::open(&dir).unwrap(), desk),
            history: vec![json!({ "role": "user", "content": "what things are there" })],
            out,
        };
        (w, updates, asks)
    }

    fn stub_server(replies: Vec<Vec<Value>>) -> (String, Receiver<Value>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!("http://{}/v1", listener.local_addr().expect("an address"));
        let (seen, asked) = channel();
        std::thread::spawn(move || {
            let mut left = replies.into_iter();
            if let Some(Ok(mut s)) = listener.incoming().next() {
                loop {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        match s.read(&mut byte) {
                            Ok(1) => head.push(byte[0]),
                            _ => return,
                        }
                    }
                    let text = String::from_utf8_lossy(&head).to_string();
                    let len = text
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let mut body = vec![0u8; len];
                    if s.read_exact(&mut body).is_err() {
                        return;
                    }
                    let _ = seen.send(serde_json::from_slice(&body).unwrap_or(Value::Null));
                    let Some(events) = left.next() else { return };
                    let mut sse = String::new();
                    for e in events {
                        sse.push_str(&format!("data: {e}\n\n"));
                    }
                    sse.push_str("data: [DONE]\n\n");
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                        sse.len()
                    );
                    if s.write_all(head.as_bytes()).is_err() || s.write_all(sse.as_bytes()).is_err()
                    {
                        return;
                    }
                    let _ = s.flush();
                }
            }
        });
        (url, asked)
    }

    #[test]
    fn the_thinking_choice_is_sent_as_a_template_kwarg() {
        for (thinking, expect) in [(None, None), (Some(false), Some(false)), (Some(true), Some(true))] {
            let (url, seen) = stub_server(vec![vec![json!({"choices":[{"delta":{"content":"hi"}}]})]]);
            let (mut w, _updates, _asks) = worker(url);
            w.config.thinking = thinking;
            super::super::runtime().block_on(w.run());
            let body = seen.try_iter().next().expect("a request");
            let sent = body.get("chat_template_kwargs").map(|k| k["enable_thinking"].as_bool().unwrap());
            assert_eq!(sent, expect);
        }
    }

    #[test]
    fn a_streamed_tool_call_is_reassembled() {
        let (w, _updates, _asks) = worker(String::new());
        let mut reply = Completion::default();
        let chunks = [
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"re"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"move","arguments":"{\"thing\":"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a\"}"}}]}}]}),
            json!({"choices":[{"delta":{"content":"moving"}}]}),
        ];
        for c in chunks {
            w.take_delta(&c, &mut reply);
        }
        assert_eq!(reply.text, "moving");
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.calls[0].name, "remove");
        assert_eq!(reply.calls[0].args, "{\"thing\":\"a\"}");
    }

    #[test]
    fn two_calls_in_one_reply_keep_their_own_arguments() {
        let (w, _updates, _asks) = worker(String::new());
        let mut reply = Completion::default();
        w.take_delta(
            &json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"a","function":{"name":"list_things","arguments":"{}"}},
                {"index":1,"id":"b","function":{"name":"remove","arguments":"{\"thing\":\"a\"}"}}
            ]}}]}),
            &mut reply,
        );
        assert_eq!(reply.calls.len(), 2);
        assert_eq!(reply.calls[0].name, "list_things");
        assert_eq!(reply.calls[1].args, "{\"thing\":\"a\"}");
    }

    #[test]
    fn blank_lines_round_a_reply_are_not_drawn() {
        let (tx, rx) = channel();
        let mut chat = Chat { updates: Some(rx), ..chat() };
        for d in ["\n\n", "Done", " it.", "\n\n"] {
            let _ = tx.send(Update::Delta(d.to_string()));
        }
        let _ = tx.send(Update::Said);
        chat.poll();
        assert_eq!(chat.turns.iter().filter(|t| matches!(t, Turn::Said(_))).count(), 1);
        let Some(Turn::Said(said)) = chat.turns.last() else { panic!("no reply") };
        assert_eq!(said, "Done it.");
    }

    #[test]
    fn what_was_remembered_is_not_remembered_again() {
        let (tx, rx) = channel();
        let mut chat = Chat { updates: Some(rx), ..chat() };
        chat.history.push(json!({ "role": "user", "content": "one" }));
        chat.turns.push(Turn::You { text: "one".into(), files: Vec::new() });
        let _ = tx.send(Update::Delta("first".into()));
        let _ = tx.send(Update::Said);
        let _ = tx.send(Update::Done);
        chat.poll();
        let (tx, rx) = channel();
        chat.updates = Some(rx);
        chat.history.push(json!({ "role": "user", "content": "two" }));
        chat.turns.push(Turn::You { text: "two".into(), files: Vec::new() });
        let _ = tx.send(Update::Delta("second".into()));
        let _ = tx.send(Update::Said);
        let _ = tx.send(Update::Done);
        chat.poll();
        let said: Vec<&str> = chat.history.iter().filter_map(|m| m["content"].as_str()).collect();
        assert_eq!(said, ["one", "first", "two", "second"]);
    }

    #[test]
    fn sampling_reaches_the_request_with_the_backend_filling_gaps() {
        let (url, asked) = stub_server(vec![vec![json!({"choices":[{"delta":{"content":"ok"}}]})]]);
        let (mut w, _updates, _asks) = worker(url);
        w.config.sampling.temperature = Some(0.3);
        super::super::runtime().block_on(w.run());
        let sent = asked.recv().expect("a request");
        assert_eq!(sent["temperature"], 0.3);
        assert_eq!(sent["top_k"], 40);
        assert!(sent.get("top_p").is_none());
    }

    #[test]
    fn a_tool_call_reaches_the_window_and_its_answer_goes_back() {
        let (url, asked) = stub_server(vec![
            vec![json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"c1","function":{"name":"list_things","arguments":"{}"}}
            ]}}]})],
            vec![
                json!({"choices":[{"delta":{"content":"two things "}}]}),
                json!({"choices":[{"delta":{"content":"found"}}]}),
            ],
        ]);
        let (w, updates, asks) = worker(url);
        let window = std::thread::spawn(move || {
            let ask = asks.recv().expect("one tool call");
            assert!(ask.name == "list_things");
            let _ = ask.reply.send(Ok(json!({ "things": [{ "name": "a" }] })));
        });
        super::super::runtime().block_on(w.run());
        window.join().expect("the window answered");

        let mut said = String::new();
        let mut called = Vec::new();
        let mut faults = Vec::new();
        for u in updates.try_iter() {
            match u {
                Update::Delta(t) => said.push_str(&t),
                Update::Calling { name, .. } => called.push(name),
                Update::Fault(e) => faults.push(e),
                _ => {}
            }
        }
        assert!(faults.is_empty(), "{faults:?}");
        assert_eq!(called, ["list_things"]);
        assert_eq!(said, "two things found");

        let first = asked.recv().expect("a first request");
        assert_eq!(first["stream"], true);
        let sandboxed = super::super::tools::NEEDS_SANDBOX.len();
        let builtins = super::super::local::NAMES.len() - sandboxed;
        assert_eq!(
            first["tools"].as_array().map(Vec::len),
            Some(builtins + 2 + if super::super::sandbox::supported() { sandboxed } else { 0 })
        );
        assert_eq!(first["messages"][0]["role"], "system");
        let second = asked.recv().expect("a second request");
        let msgs = second["messages"].as_array().expect("messages");
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2]["tool_calls"][0]["function"]["name"], "list_things");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[3]["tool_call_id"], "c1");
        assert_eq!(msgs[3]["content"], "{\"things\":[{\"name\":\"a\"}]}");
    }

    #[test]
    fn a_server_that_refuses_is_reported() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!("http://{}/v1", listener.local_addr().expect("an address"));
        std::thread::spawn(move || {
            if let Some(Ok(mut s)) = listener.incoming().next() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let body = "{\"error\":\"no such model\"}";
                let _ = s.write_all(
                    format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        let (w, updates, _asks) = worker(url);
        super::super::runtime().block_on(w.run());
        let faults: Vec<String> = updates
            .try_iter()
            .filter_map(|u| match u {
                Update::Fault(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(faults.len(), 1);
        assert!(faults[0].contains("404"), "{}", faults[0]);
        assert!(faults[0].contains("no such model"), "{}", faults[0]);
    }

    #[test]
    fn a_workspace_that_holds_credentials_is_refused() {
        let mut c = chat();
        c.config.model = "m".into();
        c.config.workspace = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
        if c.config.workspace.is_empty() {
            return;
        }
        let (desk, _asks) = crate::desk(&egui::Context::default());
        c.ask("hello", desk);
        assert!(!c.busy(), "nothing was started");
        let Some(Turn::Fault(why)) = c.turns.last() else {
            panic!("no fault: {:?}", c.turns.len())
        };
        assert!(why.contains("workspace") && why.contains("home folder"), "{why}");
    }

    #[test]
    fn a_stopped_chat_ends_without_waiting_for_the_server() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let url = format!("http://{}/v1", listener.local_addr().expect("an address"));
        let hold = std::thread::spawn(move || {
            let held = listener.incoming().next();
            std::thread::sleep(Duration::from_secs(3));
            drop(held);
        });
        let (w, updates, _asks) = worker(url);
        let stop = w.cx.stop.clone();
        let started = std::time::Instant::now();
        super::super::runtime().spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            stop.cancel();
        });
        super::super::runtime().block_on(w.run());
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
        let faults: Vec<String> = updates
            .try_iter()
            .filter_map(|u| match u {
                Update::Fault(e) => Some(e),
                _ => None,
            })
            .collect();
        assert_eq!(faults, ["stopped"]);
        let _ = hold.join();
    }

    #[test]
    fn reasoning_streams_into_a_thinking_turn_before_the_reply() {
        let (tx, rx) = channel();
        let mut chat = Chat { updates: Some(rx), ..chat() };
        for t in ["The user wants ", "a list."] {
            let _ = tx.send(Update::Thinking(t.into()));
        }
        let _ = tx.send(Update::Delta("Here it is.".into()));
        let _ = tx.send(Update::Said);
        chat.poll();
        assert!(matches!(&chat.turns[0], Turn::Thinking(t) if t == "The user wants a list."));
        assert!(matches!(&chat.turns[1], Turn::Said(t) if t == "Here it is."));
    }

    #[test]
    fn a_reasoning_delta_becomes_a_thinking_update() {
        let (w, updates, _asks) = worker(String::new());
        let mut reply = Completion::default();
        w.take_delta(&json!({"choices":[{"delta":{"reasoning_content":"hmm"}}]}), &mut reply);
        assert!(reply.text.is_empty());
        assert!(updates.try_iter().any(|u| matches!(u, Update::Thinking(t) if t == "hmm")));
    }

    #[test]
    fn questions_are_read_one_or_several() {
        let one = questions_of(&json!({"question": "Which?", "options": ["a", "b"]}));
        assert_eq!(one, vec![Asking { text: "Which?".into(), options: vec!["a".into(), "b".into()], multiple: false }]);
        let many = questions_of(&json!({"questions": [{"question": "x", "multiple": true}, {"question": "y"}]}));
        assert_eq!(many.len(), 2);
        assert!(many[0].multiple);
        assert!(questions_of(&json!({})).is_empty());
    }

    #[test]
    fn an_answer_goes_back_to_the_waiting_question() {
        let (tx, rx) = channel();
        let mut chat = Chat { updates: Some(rx), ..chat() };
        let (reply, answer) = tokio::sync::oneshot::channel();
        let _ = tx.send(Update::Asking { questions: questions_of(&json!({"question": "Go?"})), reply });
        chat.poll();
        let Some(Turn::Asked { id, answers: None, .. }) = chat.turns.last() else { panic!("no question") };
        let id = *id;
        chat.answer(id, vec!["yes".into()]);
        assert_eq!(answer.blocking_recv().unwrap(), vec!["yes".to_string()]);
        assert!(matches!(chat.turns.last(), Some(Turn::Asked { answers: Some(a), .. }) if a == &["yes".to_string()]));
    }

    #[test]
    fn a_picture_goes_to_the_model_as_an_image_part() {
        let mut chat = chat();
        chat.config.model.clear();
        let png = Attached { name: "a.png".into(), mime: "image/png".into(), data: Arc::new(vec![1, 2, 3]) };
        let part = png.part();
        assert_eq!(part["type"], "image_url");
        assert!(part["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,AQID"));
        let (desk, _asks) = crate::desk(&egui::Context::default());
        chat.ask_with("", vec![png], desk);
        assert!(matches!(&chat.turns[0], Turn::You { files, .. } if files.len() == 1));
    }

    #[test]
    fn a_chat_with_no_model_says_so() {
        let mut chat = Chat { config: Config::default(), ..chat() };
        chat.config.model.clear();
        let (desk, _asks) = crate::desk(&egui::Context::default());
        chat.ask("what things are there", desk);
        assert_eq!(chat.turns.len(), 2);
        assert!(matches!(chat.turns[1], Turn::Fault(_)));
        assert!(!chat.busy());
    }
}
