use agent_native::{Cli, Decoder, Event, Launch, Turn, content_text, parse};
use agentbench_core::Usage;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonCommand {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    pub program: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub model_args: Vec<String>,
    #[serde(default = "yes")]
    pub prompt_on_stdin: bool,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub input_tokens: Option<String>,
    #[serde(default)]
    pub output_tokens: Option<String>,
    #[serde(default)]
    pub plain: bool,
}

fn yes() -> bool {
    true
}

fn fill(template: &[String], key: &str, value: &str) -> Vec<String> {
    template.iter().map(|a| a.replace(key, value)).collect()
}

impl JsonCommand {
    fn has_prompt_placeholder(&self) -> bool {
        self.args.iter().any(|a| a.contains("{prompt}"))
    }
}

#[agent_native::async_trait]
impl Cli for JsonCommand {
    fn name(&self) -> &str {
        &self.name
    }

    fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    fn program(&self) -> &Path {
        &self.program
    }

    fn resumable(&self) -> bool {
        !self.resume_args.is_empty() && self.session.is_some()
    }

    fn launch(&self, turn: &Turn, _brief: &str) -> Launch {
        let mut args = fill(&self.args, "{prompt}", turn.prompt);
        if let Some(id) = turn.resume.filter(|_| self.resumable()) {
            args.extend(fill(&self.resume_args, "{session}", id));
        }
        if let Some(model) = turn.model {
            args.extend(fill(&self.model_args, "{model}", model));
        }
        let stdin = (self.prompt_on_stdin && !self.has_prompt_placeholder())
            .then(|| turn.prompt.to_string());
        Launch { program: self.program.clone(), args, stdin, env: vec![] }
    }

    fn decoder(&self) -> Box<dyn Decoder> {
        Box::new(PointerDecoder { spec: self.clone(), texted: false })
    }
}

struct PointerDecoder {
    spec: JsonCommand,
    texted: bool,
}

fn at<'a>(v: &'a Value, pointer: &Option<String>) -> Option<&'a Value> {
    pointer.as_deref().and_then(|p| v.pointer(p)).filter(|x| !x.is_null())
}

impl PointerDecoder {
    fn text(&mut self, text: &str) -> Vec<Event> {
        if text.trim().is_empty() {
            return vec![];
        }
        let delta = if self.texted { format!("\n{text}") } else { text.to_string() };
        self.texted = true;
        vec![Event::Text { delta }]
    }
}

fn refusal(v: &Value) -> Option<String> {
    let kind = v.get("type").and_then(Value::as_str);
    let reason = v
        .pointer("/message/stop_reason")
        .or_else(|| v.get("stop_reason"))
        .and_then(Value::as_str)
        .filter(|r| matches!(*r, "refusal" | "content_filter"));
    if let Some(reason) = reason.filter(|_| kind != Some("result")) {
        let said = v.get("message").map(|m| content_text(m.get("content").unwrap_or(m)));
        let said = said.filter(|t| !t.trim().is_empty()).unwrap_or_default();
        return Some(format!("the model refused to answer ({reason}): {}", said.trim()));
    }
    if kind != Some("result") {
        return None;
    }
    let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or_default();
    let failed = v.get("is_error").and_then(Value::as_bool).unwrap_or(false)
        || subtype.starts_with("error");
    if !failed && reason.is_none() {
        return None;
    }
    let mut said = v.get("result").map(content_text).unwrap_or_default();
    if said.trim().is_empty() {
        said = v
            .get("errors")
            .and_then(Value::as_array)
            .map(|e| e.iter().map(content_text).collect::<Vec<_>>().join("; "))
            .unwrap_or_default();
    }
    let why = reason.unwrap_or(subtype);
    let said = if said.trim().is_empty() { "the agent gave no reason" } else { said.trim() };
    Some(if why.is_empty() { format!("the agent stopped with an error: {said}") } else {
        format!("the agent stopped ({why}): {said}")
    })
}

impl Decoder for PointerDecoder {
    fn line(&mut self, line: &str) -> Vec<Event> {
        let Some(v) = parse(line) else {
            return if self.spec.plain { self.text(line.trim_end()) } else { vec![] };
        };
        let mut out = Vec::new();
        if let Some(id) = at(&v, &self.spec.session).and_then(Value::as_str) {
            out.push(Event::Started { agent_session: id.to_string() });
        }
        if let Some(t) = at(&v, &self.spec.text) {
            out.extend(self.text(&content_text(t)));
        }
        if let Some(e) = at(&v, &self.spec.error) {
            out.push(Event::Error(content_text(e)));
        }
        if let Some(e) = refusal(&v) {
            out.push(Event::Error(e));
        }
        let input = at(&v, &self.spec.input_tokens).and_then(Value::as_u64);
        let output = at(&v, &self.spec.output_tokens).and_then(Value::as_u64);
        if input.is_some() || output.is_some() {
            out.push(Event::Usage(Usage {
                input_tokens: input.unwrap_or(0),
                output_tokens: output.unwrap_or(0),
                turns: 1,
                ..Default::default()
            }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_native::decode_all;

    fn spec() -> JsonCommand {
        toml_like(serde_json::json!({
            "name": "aider",
            "program": "aider",
            "args": ["--json"],
            "resume_args": ["--session", "{session}"],
            "model_args": ["--model", "{model}"],
            "session": "/session",
            "text": "/message/content",
            "error": "/error",
            "input_tokens": "/usage/in",
            "output_tokens": "/usage/out"
        }))
    }

    fn toml_like(v: Value) -> JsonCommand {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn fields_are_read_by_pointer() {
        let events = decode_all(
            &spec(),
            &[
                r#"{"session":"s9"}"#,
                r#"{"message":{"content":"hello"}}"#,
                r#"{"message":{"content":[{"text":"again"}]}}"#,
                r#"{"usage":{"in":4,"out":2}}"#,
                r#"{"error":"boom"}"#,
                "not json",
            ],
        );
        assert_eq!(
            events,
            vec![
                Event::Started { agent_session: "s9".into() },
                Event::Text { delta: "hello".into() },
                Event::Text { delta: "\nagain".into() },
                Event::Usage(Usage {
                    input_tokens: 4,
                    output_tokens: 2,
                    turns: 1,
                    ..Default::default()
                }),
                Event::Error("boom".into()),
            ]
        );
    }

    #[test]
    fn a_claude_refusal_and_error_result_become_errors() {
        let events = decode_all(
            &spec(),
            &[
                r#"{"type":"assistant","message":{"stop_reason":"refusal","content":[{"type":"text","text":"I cannot assist with that."}]}}"#,
                r#"{"type":"result","subtype":"success","is_error":false,"result":"fine"}"#,
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"Usage policy violation"}"#,
            ],
        );
        let events: Vec<_> = events.into_iter().filter(|e| matches!(e, Event::Error(_))).collect();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(&events[0], Event::Error(e) if e.contains("refusal") && e.contains("I cannot assist with that.")));
        assert!(matches!(&events[1], Event::Error(e) if e.contains("error_during_execution") && e.contains("Usage policy violation")));
    }

    #[test]
    fn plain_mode_keeps_non_json_lines_as_text() {
        let mut s = spec();
        s.plain = true;
        let events = decode_all(&s, &["just words"]);
        assert_eq!(events, vec![Event::Text { delta: "just words".into() }]);
    }

    #[test]
    fn the_prompt_goes_on_stdin_unless_an_argument_takes_it() {
        let c = agent_native::context();
        let turn = Turn {
            files: &[],
            prompt: "do it",
            cwd: Path::new("/tmp"),
            model: Some("m"),
            effort: None,
            resume: Some("s9"),
            history: &[],
            context: &c,
        };
        let l = spec().launch(&turn, "");
        assert_eq!(l.args, ["--json", "--session", "s9", "--model", "m"]);
        assert_eq!(l.stdin.as_deref(), Some("do it"));

        let mut inline = spec();
        inline.args = vec!["--message".into(), "{prompt}".into()];
        let l = inline.launch(&turn, "");
        assert_eq!(l.args[..2], ["--message", "do it"]);
        assert_eq!(l.stdin, None);
    }

    #[test]
    fn it_only_resumes_when_it_can_find_a_session() {
        let mut s = spec();
        s.session = None;
        assert!(!s.resumable());
        let c = agent_native::context();
        let turn = Turn {
            files: &[],
            prompt: "x",
            cwd: Path::new("/"),
            model: None,
            effort: None,
            resume: Some("s9"),
            history: &[],
            context: &c,
        };
        assert_eq!(s.launch(&turn, "").args, ["--json"]);
    }
}
