//! Names a chat from what was said in it, with a small, fast model.
//!
//! The model is any OpenAI compatible server (ollama, llama.cpp, vllm). Set it
//! under `[title_model]` in `foreman.toml`; every field left out falls back to
//! the foreman agent's own model, so a bare install titles chats with the model
//! it already has.

use crate::agent::native::Shared;
use foreman_core::Entry;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, RwLock};
use std::time::Duration;

const PATIENCE: Duration = Duration::from_secs(30);
const TITLE_MAX: usize = 60;
const USER_CLIP: usize = 800;
const REPLY_CLIP: usize = 500;
const TRANSCRIPT_MAX: usize = 3500;
const TOKENS: u64 = 400;

const INSTRUCTIONS: &str = "You name chats between a person and a coding agent. \
Read the conversation and reply with a title for it: three to seven words, sentence case, \
saying what the person is trying to get done. No quotes, no markdown, no trailing punctuation. \
Reply with the title and nothing else.";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TitleModel {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub key: String,
}

fn yes() -> bool {
    true
}

impl Default for TitleModel {
    fn default() -> Self {
        TitleModel { enabled: true, url: String::new(), model: String::new(), key: String::new() }
    }
}

/// Where to send a request: the title model, with the foreman model filling in
/// whatever was not set. The key is only borrowed from the foreman model when
/// the server is too, so it never goes to a server it was not given for.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub url: String,
    pub model: String,
    pub key: String,
}

impl Target {
    fn endpoint(&self) -> String {
        let base = self.url.trim().trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }
}

#[derive(Clone)]
pub struct Titles {
    config: Arc<RwLock<TitleModel>>,
    main: Shared,
}

impl TitleModel {
    /// Where requests go: this model, with the foreman model filling in whatever
    /// was not set. The foreman key is only borrowed when the server is borrowed
    /// too, so it never goes to a server it was not given for.
    pub fn resolve(&self, main: &crate::agent::native::Model) -> Option<Target> {
        if !self.enabled {
            return None;
        }
        let own = |s: &str| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        let same_server = own(&self.url).is_none();
        let url = own(&self.url).unwrap_or(main.url.clone());
        let model = own(&self.model).unwrap_or(main.model.clone());
        let key = own(&self.key)
            .or_else(|| same_server.then(|| main.key.trim().to_string()))
            .unwrap_or_default();
        (!model.trim().is_empty() && !url.trim().is_empty()).then_some(Target { url, model, key })
    }
}

impl Titles {
    pub fn new(config: TitleModel, main: Shared) -> Titles {
        Titles { config: Arc::new(RwLock::new(config)), main }
    }

    pub fn config(&self) -> TitleModel {
        self.config.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_config(&self, config: TitleModel) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
    }

    pub fn main(&self) -> crate::agent::native::Model {
        self.main.get()
    }

    pub fn target(&self) -> Option<Target> {
        self.config().resolve(&self.main.get())
    }

    /// A title for these messages, or why there is none.
    pub async fn title(&self, entries: &[Entry]) -> Result<String, String> {
        let target = self.target().ok_or("no title model")?;
        let transcript = transcript(entries);
        if transcript.is_empty() {
            return Err("nothing said yet".into());
        }
        let raw = ask(&target, INSTRUCTIONS, &transcript).await?;
        clean(&raw).ok_or_else(|| format!("the model gave no usable title: {raw:?}"))
    }
}

fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.len() <= max {
        return text.to_string();
    }
    format!("{}…", &text[..text.floor_char_boundary(max)])
}

/// The conversation as plain text: what the person asked and what the agent
/// said back, each clipped, the start of it favoured over the end.
pub fn transcript(entries: &[Entry]) -> String {
    let mut out = String::new();
    let mut replied = false;
    for e in foreman_core::since_summary(entries) {
        let line = match e {
            Entry::User { text } => format!("Person: {}", clip(text, USER_CLIP)),
            Entry::Assistant { text } if !text.trim().is_empty() => {
                replied = true;
                format!("Agent: {}", clip(text, REPLY_CLIP))
            }
            Entry::Summary { text } if !text.trim().is_empty() => {
                format!("Summary so far: {}", clip(text, USER_CLIP))
            }
            _ => continue,
        };
        if out.len() + line.len() > TRANSCRIPT_MAX {
            break;
        }
        out.push_str(&line);
        out.push_str("\n\n");
    }
    if replied { out.trim_end().to_string() } else { String::new() }
}

async fn ask(target: &Target, system: &str, user: &str) -> Result<String, String> {
    let body = |thinking_off: bool| {
        let mut body = json!({
            "model": target.model.trim(),
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user },
            ],
            "temperature": 0.2,
            "max_tokens": TOKENS,
            "stream": false,
        });
        if thinking_off {
            // Servers that do not know this field ignore it; a few reject it,
            // which is why a 400 is retried without it below.
            body["chat_template_kwargs"] = json!({ "enable_thinking": false });
        }
        body
    };
    let send = async |body: Value| -> Result<(u16, String), String> {
        let mut req = crate::http::client().post(target.endpoint()).timeout(PATIENCE).json(&body);
        if !target.key.trim().is_empty() {
            req = req.bearer_auth(target.key.trim());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("{}: {}", target.url, crate::agent::native::describe(&e)))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| e.to_string())?;
        Ok((status, text))
    };
    let (mut status, mut text) = send(body(true)).await?;
    if status == 400 {
        (status, text) = send(body(false)).await?;
    }
    if !(200..300).contains(&status) {
        return Err(format!("{status}: {}", clip(&text, 300)));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("not a completion: {e}"))?;
    v.pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "the reply had no text".to_string())
}

/// A model's answer made into a title: no reasoning block, no label, no quotes,
/// one line, short.
pub fn clean(raw: &str) -> Option<String> {
    let mut text = raw.to_string();
    while let Some(start) = text.find("<think>") {
        match text[start..].find("</think>") {
            Some(end) => text.replace_range(start..start + end + "</think>".len(), ""),
            None => text.truncate(start),
        }
    }
    if let Some(end) = text.find("</think>") {
        text.replace_range(..end + "</think>".len(), "");
    }
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.trim_start_matches('#').trim();
    let line = match line.split_once(':') {
        Some((label, rest)) if label.trim().eq_ignore_ascii_case("title") => rest.trim(),
        _ => line,
    };
    let line = line
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '_' | '“' | '”' | '‘' | '’'))
        .trim()
        .trim_end_matches(['.', '!', ',', ';', ':'])
        .trim();
    let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    Some(foreman_core::short_title(&line, TITLE_MAX))
}

#[cfg(test)]
pub(crate) mod fake {
    use serde_json::{Value, json};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A server that answers each request from `replies` and records the bodies.
    pub(crate) fn server(replies: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, reply) in replies {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let body = loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        if n == 0 {
                            return;
                        }
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..split]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= split + 4 + len {
                        break buf[split + 4..split + 4 + len].to_vec();
                    }
                    if n == 0 {
                        return;
                    }
                };
                log.lock().unwrap().push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (url, seen)
    }

    pub(crate) fn completion(text: &str) -> String {
        json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{completion, server as fake_server};
    use super::*;
    use crate::agent::native::Model;

    #[test]
    fn answers_are_cleaned_into_titles() {
        assert_eq!(clean("Fix the login loop").as_deref(), Some("Fix the login loop"));
        assert_eq!(
            clean("\"Fix the login loop.\"\n\nBecause...").as_deref(),
            Some("Fix the login loop")
        );
        assert_eq!(clean("Title: **Add a title model**").as_deref(), Some("Add a title model"));
        assert_eq!(
            clean("<think>hmm\nlet me see</think>\n\nRename the tab").as_deref(),
            Some("Rename the tab")
        );
        assert_eq!(clean("<think>never closed").as_deref(), None);
        assert_eq!(clean("reasoning here</think>Real title").as_deref(), Some("Real title"));
        assert_eq!(clean("  \n ").as_deref(), None);
        assert_eq!(clean("##  Plan the work  ").as_deref(), Some("Plan the work"));
        let long = clean(&"word ".repeat(40)).unwrap();
        assert!(long.chars().count() <= TITLE_MAX + 1, "{long}");
    }

    fn said(user: &str, agent: &str) -> Vec<Entry> {
        vec![
            Entry::User { text: user.into() },
            Entry::Thinking { text: "hm".into() },
            Entry::Assistant { text: agent.into() },
        ]
    }

    #[test]
    fn the_transcript_is_the_talk_and_waits_for_a_reply() {
        let t = transcript(&said("fix the login loop", "found it in auth.rs"));
        assert_eq!(t, "Person: fix the login loop\n\nAgent: found it in auth.rs");
        let only_user = [Entry::User { text: "hello".into() }];
        assert_eq!(transcript(&only_user), "", "no title until the agent has answered");
        let long = transcript(&said(&"x".repeat(5000), &"y".repeat(5000)));
        assert!(long.len() < TRANSCRIPT_MAX + 100, "{}", long.len());
    }

    fn model(url: &str, name: &str, key: &str) -> Shared {
        Shared::new(Model {
            url: url.into(),
            model: name.into(),
            key: key.into(),
            steps: 1,
            ..Model::default()
        })
    }

    #[test]
    fn the_title_model_falls_back_field_by_field() {
        let main = model("http://big/v1", "big", "main-key");
        let bare = Titles::new(TitleModel::default(), main.clone());
        assert_eq!(
            bare.target().unwrap(),
            Target { url: "http://big/v1".into(), model: "big".into(), key: "main-key".into() }
        );
        let small_model = TitleModel { model: "small".into(), ..TitleModel::default() };
        let t = Titles::new(small_model, main.clone()).target().unwrap();
        assert_eq!(
            (t.url.as_str(), t.model.as_str(), t.key.as_str()),
            ("http://big/v1", "small", "main-key")
        );
        let elsewhere = TitleModel {
            url: "http://small/v1".into(),
            model: "tiny".into(),
            ..TitleModel::default()
        };
        let t = Titles::new(elsewhere, main.clone()).target().unwrap();
        assert_eq!(t.key, "", "the main key is not sent to another server");
        let off = TitleModel { enabled: false, ..TitleModel::default() };
        assert!(Titles::new(off, main).target().is_none());
        assert!(
            Titles::new(TitleModel::default(), model("http://big/v1", "", "")).target().is_none()
        );
    }

    #[test]
    fn the_config_reads_from_toml() {
        let c: crate::Config = toml::from_str(
            "[title_model]\nurl = \"http://localhost:11434/v1\"\nmodel = \"qwen2.5:1.5b\"\n",
        )
        .unwrap();
        assert_eq!(c.title_model.model, "qwen2.5:1.5b");
        assert!(c.title_model.enabled);
        let c: crate::Config = toml::from_str("[title_model]\nenabled = false\n").unwrap();
        assert!(!c.title_model.enabled);
        assert!(toml::from_str::<crate::Config>("").unwrap().title_model.enabled);
    }

    #[tokio::test]
    async fn a_title_comes_from_the_model() {
        let (url, seen) = fake_server(vec![(200, completion("\"Fix the login loop.\""))]);
        let titles = Titles::new(
            TitleModel { url, model: "tiny".into(), ..TitleModel::default() },
            model("http://unused/v1", "big", ""),
        );
        let title = titles.title(&said("fix the login loop", "on it")).await.unwrap();
        assert_eq!(title, "Fix the login loop");
        let sent = seen.lock().unwrap();
        assert_eq!(sent[0]["model"], "tiny");
        assert_eq!(sent[0]["stream"], false);
        assert!(sent[0]["messages"][1]["content"].as_str().unwrap().contains("fix the login loop"));
    }

    #[tokio::test]
    async fn a_server_that_rejects_the_thinking_switch_is_asked_again_without_it() {
        let (url, seen) = fake_server(vec![
            (400, r#"{"error":"unknown field chat_template_kwargs"}"#.into()),
            (200, completion("Plain answer")),
        ]);
        let titles = Titles::new(
            TitleModel { url, model: "tiny".into(), ..TitleModel::default() },
            model("", "", ""),
        );
        assert_eq!(titles.title(&said("hi", "hello")).await.unwrap(), "Plain answer");
        let sent = seen.lock().unwrap();
        assert!(sent[0].get("chat_template_kwargs").is_some());
        assert!(sent[1].get("chat_template_kwargs").is_none());
    }

    #[tokio::test]
    async fn a_failing_server_is_an_error_not_a_title() {
        let (url, _) = fake_server(vec![(500, "boom".into()), (500, "boom".into())]);
        let titles = Titles::new(
            TitleModel { url, model: "tiny".into(), ..TitleModel::default() },
            model("", "", ""),
        );
        assert!(titles.title(&said("hi", "hello")).await.unwrap_err().contains("500"));
    }
}
