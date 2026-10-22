use super::{Agent, Decoder, Event, Launch, Turn, content_text, parse};
use foreman_core::Usage;
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

impl Agent for JsonCommand {
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

    fn launch(&self, turn: &Turn) -> Launch {
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
    use crate::agent::decode_all;

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
    fn plain_mode_keeps_non_json_lines_as_text() {
        let mut s = spec();
        s.plain = true;
        let events = decode_all(&s, &["just words"]);
        assert_eq!(events, vec![Event::Text { delta: "just words".into() }]);
    }

    #[test]
    fn the_prompt_goes_on_stdin_unless_an_argument_takes_it() {
        let turn =
            Turn { prompt: "do it", cwd: Path::new("/tmp"), model: Some("m"), resume: Some("s9") };
        let l = spec().launch(&turn);
        assert_eq!(l.args, ["--json", "--session", "s9", "--model", "m"]);
        assert_eq!(l.stdin.as_deref(), Some("do it"));

        let mut inline = spec();
        inline.args = vec!["--message".into(), "{prompt}".into()];
        let l = inline.launch(&turn);
        assert_eq!(l.args[..2], ["--message", "do it"]);
        assert_eq!(l.stdin, None);
    }

    #[test]
    fn it_only_resumes_when_it_can_find_a_session() {
        let mut s = spec();
        s.session = None;
        assert!(!s.resumable());
        let l =
            s.launch(&Turn { prompt: "x", cwd: Path::new("/"), model: None, resume: Some("s9") });
        assert_eq!(l.args, ["--json"]);
    }
}
