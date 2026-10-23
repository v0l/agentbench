use super::{Cli, Decoder, Event, Launch, Turn, content_text, parse, str_at, u64_at};
use foreman_core::Usage;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Claude {
    program: PathBuf,
}

impl Claude {
    pub fn new(program: impl Into<PathBuf>) -> Claude {
        Claude { program: program.into() }
    }
}

#[super::async_trait]
impl Cli for Claude {
    fn name(&self) -> &str {
        "claude"
    }

    fn label(&self) -> &str {
        "Claude Code"
    }

    fn program(&self) -> &Path {
        &self.program
    }

    fn resumable(&self) -> bool {
        true
    }

    /// Claude Code has no command that lists models, but takes these aliases for the
    /// newest of each family, and any full model name through "custom…".
    async fn models(&self) -> Result<Vec<String>, String> {
        Ok(["default", "opus", "sonnet", "haiku", "opusplan"].map(String::from).to_vec())
    }

    fn launch(&self, turn: &Turn, brief: &str) -> Launch {
        let mut args: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "bypassPermissions",
        ]
        .map(String::from)
        .into();
        args.extend(["--append-system-prompt".to_string(), brief.to_string()]);
        if let Some(model) = turn.model {
            args.extend(["--model".to_string(), model.to_string()]);
        }
        if let Some(id) = turn.resume {
            args.extend(["--resume".to_string(), id.to_string()]);
        }
        Launch {
            program: self.program.clone(),
            args,
            stdin: Some(turn.prompt.to_string()),
            env: vec![],
        }
    }

    fn decoder(&self) -> Box<dyn Decoder> {
        Box::new(ClaudeDecoder::default())
    }
}

#[derive(Default)]
struct ClaudeDecoder {
    tools: HashMap<String, String>,
    texted: bool,
    /// The model the session reported, and what the last request sent it. The result's own
    /// usage adds up every request of the turn, which says nothing about how full the chat is.
    model: String,
    context: u64,
}

impl Decoder for ClaudeDecoder {
    fn line(&mut self, line: &str) -> Vec<Event> {
        let Some(v) = parse(line) else {
            return vec![];
        };
        match str_at(&v, "type").unwrap_or_default() {
            "system" if str_at(&v, "subtype") == Some("init") => {
                let mut out = Vec::new();
                self.model = str_at(&v, "model").unwrap_or_default().to_string();
                if let Some(id) = str_at(&v, "session_id") {
                    out.push(Event::Started { agent_session: id.to_string() });
                }
                if !self.model.is_empty() {
                    out.push(Event::Usage(Usage::serving(&self.model, "", 0)));
                }
                out
            }
            "assistant" => self.assistant(&v),
            "user" => self.tool_results(&v),
            "result" => {
                let mut out = Vec::new();
                if let Some(u) = v.get("usage") {
                    // The window is on the entry for the model that ran.
                    let window = v
                        .get("modelUsage")
                        .and_then(Value::as_object)
                        .and_then(|m| m.get(&self.model).or_else(|| m.values().next()))
                        .map(|m| u64_at(m, "contextWindow"))
                        .unwrap_or(0);
                    out.push(Event::Usage(Usage {
                        context: self.context,
                        window,
                        input_tokens: u64_at(u, "input_tokens")
                            + u64_at(u, "cache_creation_input_tokens"),
                        output_tokens: u64_at(u, "output_tokens"),
                        cache_read_tokens: u64_at(u, "cache_read_input_tokens"),
                        cost_usd: v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
                        turns: 1,
                        ..Usage::default()
                    }));
                }
                if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                    out.push(Event::Error(
                        str_at(&v, "result").unwrap_or("claude failed").to_string(),
                    ));
                }
                out.push(Event::Done);
                out
            }
            _ => vec![],
        }
    }
}

impl ClaudeDecoder {
    fn assistant(&mut self, v: &Value) -> Vec<Event> {
        if v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
            return vec![];
        }
        let Some(blocks) =
            v.get("message").and_then(|m| m.get("content")).and_then(Value::as_array)
        else {
            return vec![];
        };
        if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
            let all = u64_at(u, "input_tokens")
                + u64_at(u, "cache_creation_input_tokens")
                + u64_at(u, "cache_read_input_tokens");
            if all > 0 {
                self.context = all;
            }
        }
        let api_error = v.get("error").is_some_and(|e| !e.is_null());
        let mut out = Vec::new();
        for b in blocks {
            match str_at(b, "type").unwrap_or_default() {
                "text" if !api_error => {
                    let text = str_at(b, "text").unwrap_or_default().trim();
                    if text.is_empty() {
                        continue;
                    }
                    let delta = if self.texted { format!("\n\n{text}") } else { text.to_string() };
                    self.texted = true;
                    out.push(Event::Text { delta });
                }
                "thinking" => {
                    let text = str_at(b, "thinking").unwrap_or_default().trim();
                    if !text.is_empty() {
                        out.push(Event::Thinking { text: text.to_string() });
                    }
                }
                "tool_use" => {
                    let id = str_at(b, "id").unwrap_or_default().to_string();
                    let name = str_at(b, "name").unwrap_or("tool").to_string();
                    self.tools.insert(id.clone(), name.clone());
                    out.push(Event::ToolStart {
                        call_id: id,
                        name,
                        input: b.get("input").cloned().unwrap_or(Value::Null),
                    });
                }
                _ => {}
            }
        }
        out
    }

    fn tool_results(&mut self, v: &Value) -> Vec<Event> {
        let Some(blocks) =
            v.get("message").and_then(|m| m.get("content")).and_then(Value::as_array)
        else {
            return vec![];
        };
        blocks
            .iter()
            .filter(|b| str_at(b, "type") == Some("tool_result"))
            .filter_map(|b| {
                let id = str_at(b, "tool_use_id")?.to_string();
                let name = self.tools.remove(&id)?;
                Some(Event::ToolEnd {
                    call_id: id,
                    name,
                    output: content_text(b.get("content").unwrap_or(&Value::Null)),
                    error: b.get("is_error").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::decode_all;

    #[test]
    fn a_tool_run_decodes_in_order() {
        let events = decode_all(
            &Claude::new("claude"),
            &[
                r#"{"type":"system","subtype":"init","session_id":"abc","tools":[]}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"plan"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]},"parent_tool_use_id":null}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"a.txt","is_error":false}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Done."}]},"parent_tool_use_id":null}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Really."}]},"parent_tool_use_id":null}"#,
                r#"{"type":"result","subtype":"success","is_error":false,"total_cost_usd":0.12,"usage":{"input_tokens":5,"cache_creation_input_tokens":7,"cache_read_input_tokens":3,"output_tokens":9}}"#,
            ],
        );
        assert_eq!(events[0], Event::Started { agent_session: "abc".into() });
        assert_eq!(events[1], Event::Thinking { text: "plan".into() });
        assert!(
            matches!(&events[2], Event::ToolStart { call_id, name, .. } if call_id == "t1" && name == "Bash")
        );
        assert_eq!(
            events[3],
            Event::ToolEnd {
                call_id: "t1".into(),
                name: "Bash".into(),
                output: "a.txt".into(),
                error: false
            }
        );
        assert_eq!(events[4], Event::Text { delta: "Done.".into() });
        assert_eq!(events[5], Event::Text { delta: "\n\nReally.".into() });
        assert!(
            matches!(&events[6], Event::Usage(u) if u.input_tokens == 12 && u.cache_read_tokens == 3 && u.cost_usd == 0.12)
        );
        assert_eq!(events[7], Event::Done);
    }

    #[test]
    fn an_auth_failure_is_an_error_not_a_reply() {
        let events = decode_all(
            &Claude::new("claude"),
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Failed to authenticate"}]},"error":"authentication_failed","parent_tool_use_id":null}"#,
                r#"{"type":"result","subtype":"success","is_error":true,"result":"Failed to authenticate"}"#,
            ],
        );
        assert!(!events.iter().any(|e| matches!(e, Event::Text { .. })));
        assert!(events.contains(&Event::Error("Failed to authenticate".into())));
    }

    #[test]
    fn subagent_chatter_is_not_the_reply() {
        let events = decode_all(
            &Claude::new("claude"),
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"inner"}]},"parent_tool_use_id":"t9"}"#,
            ],
        );
        assert!(events.is_empty());
    }

    #[test]
    fn launch_resumes_by_session_id() {
        let c = crate::agent::context();
        let l = Claude::new("claude").launch(
            &Turn {
                files: &[],
                prompt: "hi",
                cwd: Path::new("/tmp"),
                model: Some("sonnet"),
                resume: Some("abc"),
                history: &[],
                context: &c,
            },
            "BRIEF",
        );
        assert!(l.args.starts_with(&[
            "-p".to_string(),
            "--output-format".into(),
            "stream-json".into()
        ]));
        assert!(l.args.windows(2).any(|w| w == ["--append-system-prompt", "BRIEF"]));
        assert!(l.args.ends_with(&[
            "--model".to_string(),
            "sonnet".into(),
            "--resume".into(),
            "abc".into()
        ]));
    }
}
