use super::{Cli, Decoder, Event, Launch, Turn, content_text, parse, str_at, u64_at};
use agentbench_core::Usage;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct Pi {
    program: PathBuf,
}

impl Pi {
    pub fn new(program: impl Into<PathBuf>) -> Pi {
        Pi { program: program.into() }
    }
}

#[super::async_trait]
impl Cli for Pi {
    fn name(&self) -> &str {
        "pi"
    }

    fn label(&self) -> &str {
        "Pi"
    }

    fn program(&self) -> &Path {
        &self.program
    }

    fn resumable(&self) -> bool {
        true
    }

    async fn models(&self) -> Result<Vec<String>, String> {
        let text = super::process::listing(&self.program, &["--list-models"]).await?;
        Ok(pi_models(&text))
    }

    fn launch(&self, turn: &Turn, brief: &str) -> Launch {
        let mut args = vec!["--mode".to_string(), "json".to_string()];
        let session =
            turn.resume.map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        args.extend(["--session-id".to_string(), session]);
        args.extend(["--append-system-prompt".to_string(), brief.to_string()]);
        if let Some(model) = turn.model {
            args.extend(["--model".to_string(), model.to_string()]);
        }
        Launch {
            program: self.program.clone(),
            args,
            stdin: Some(turn.prompt.to_string()),
            env: vec![],
        }
    }

    fn decoder(&self) -> Box<dyn Decoder> {
        Box::new(PiDecoder::default())
    }
}

/// `pi --list-models` prints a table of `provider model ...`; pi takes `provider/model`.
fn pi_models(text: &str) -> Vec<String> {
    let mut out: Vec<String> = text
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("provider"))
        .skip(1)
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            Some(format!("{}/{}", w.next()?, w.next()?))
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

#[derive(Default)]
struct PiDecoder {
    tools: HashMap<String, String>,
    error: Option<String>,
    texted: bool,
    thought: bool,
}

impl Decoder for PiDecoder {
    fn line(&mut self, line: &str) -> Vec<Event> {
        let Some(v) = parse(line) else {
            return vec![];
        };
        match str_at(&v, "type").unwrap_or_default() {
            "session" => str_at(&v, "id")
                .map(|id| vec![Event::Started { agent_session: id.to_string() }])
                .unwrap_or_default(),
            "message_start" => {
                let assistant =
                    v.get("message").and_then(|m| str_at(m, "role")) == Some("assistant");
                if assistant && self.texted {
                    self.texted = false;
                    return vec![Event::Text { delta: "\n\n".into() }];
                }
                vec![]
            }
            "message_update" => {
                let Some(update) = v.get("assistantMessageEvent") else {
                    return vec![];
                };
                if str_at(update, "type") == Some("thinking_delta") {
                    self.thought = true;
                    let delta = str_at(update, "delta").unwrap_or_default();
                    return match delta.is_empty() {
                        true => vec![],
                        false => vec![Event::Thinking { text: delta.to_string() }],
                    };
                }
                if str_at(update, "type") != Some("text_delta") {
                    return vec![];
                }
                let delta = str_at(update, "delta").unwrap_or_default();
                if delta.is_empty() || (!self.texted && delta.trim().is_empty()) {
                    return vec![];
                }
                let delta = if self.texted { delta } else { delta.trim_start() };
                self.texted = true;
                vec![Event::Text { delta: delta.to_string() }]
            }
            "message_end" => self.message_end(&v),
            "tool_execution_start" => {
                let id = str_at(&v, "toolCallId").unwrap_or_default().to_string();
                let name = str_at(&v, "toolName").unwrap_or("tool").to_string();
                self.tools.insert(id.clone(), name.clone());
                let input = v.get("args").cloned().unwrap_or(Value::Null);
                vec![Event::ToolStart { call_id: id, name, input }]
            }
            "tool_execution_end" => {
                let id = str_at(&v, "toolCallId").unwrap_or_default().to_string();
                let name = self
                    .tools
                    .remove(&id)
                    .or_else(|| str_at(&v, "toolName").map(str::to_string))
                    .unwrap_or("tool".into());
                let output = v
                    .get("result")
                    .map(|r| content_text(r.get("content").unwrap_or(r)))
                    .unwrap_or_default();
                let error = v.get("isError").and_then(Value::as_bool).unwrap_or(false);
                vec![Event::ToolEnd { call_id: id, name, output, error }]
            }
            "auto_retry_start" => {
                self.error = None;
                vec![Event::Notice(format!(
                    "retrying: {}",
                    str_at(&v, "errorMessage").unwrap_or("provider error")
                ))]
            }
            "auto_retry_end" if v.get("success").and_then(Value::as_bool) == Some(false) => {
                self.error =
                    Some(str_at(&v, "finalError").unwrap_or("gave up retrying").to_string());
                vec![]
            }
            "compaction_end" if v.get("result").is_some_and(|r| !r.is_null()) => {
                vec![Event::Notice("context compacted".into())]
            }
            "agent_settled" => {
                let aborted = v.get("aborted").and_then(Value::as_bool).unwrap_or(false);
                let mut out = Vec::new();
                if let Some(e) = self.error.take().filter(|_| !aborted) {
                    out.push(Event::Error(e));
                }
                out.push(Event::Done);
                out
            }
            _ => vec![],
        }
    }
}

impl PiDecoder {
    fn message_end(&mut self, v: &Value) -> Vec<Event> {
        let Some(m) = v.get("message").filter(|m| str_at(m, "role") == Some("assistant")) else {
            return vec![];
        };
        let mut out = Vec::new();
        let streamed = std::mem::take(&mut self.thought);
        if let Some(blocks) = m.get("content").and_then(Value::as_array).filter(|_| !streamed) {
            let thinking: Vec<&str> = blocks
                .iter()
                .filter(|b| str_at(b, "type") == Some("thinking"))
                .filter_map(|b| str_at(b, "thinking"))
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect();
            if !thinking.is_empty() {
                out.push(Event::Thinking { text: thinking.join("\n") });
            }
        }
        if let Some(u) = m.get("usage") {
            out.push(Event::Usage(Usage {
                input_tokens: u64_at(u, "input"),
                output_tokens: u64_at(u, "output"),
                cache_read_tokens: u64_at(u, "cacheRead"),
                cost_usd: u
                    .get("cost")
                    .and_then(|c| c.get("total"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0),
                turns: 1,
                context: match u64_at(u, "totalTokens") {
                    0 => u64_at(u, "input") + u64_at(u, "cacheRead") + u64_at(u, "cacheWrite"),
                    n => n.saturating_sub(u64_at(u, "output")),
                },
                model: str_at(m, "model").unwrap_or_default().to_string(),
                host: str_at(m, "provider").unwrap_or_default().to_string(),
                ..Usage::default()
            }));
        }
        if str_at(m, "stopReason") == Some("error") {
            self.error = Some(str_at(m, "errorMessage").unwrap_or("provider error").to_string());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::decode_all;

    #[test]
    fn pi_models_are_provider_slash_model() {
        let text = "provider   model   context\nanthropic  claude-a  1M\nopenai  gpt-x  200K\nanthropic  claude-a  1M\n";
        assert_eq!(pi_models(text), ["anthropic/claude-a", "openai/gpt-x"]);
        assert!(pi_models("no table here").is_empty());
    }

    const RUN: &[&str] = &[
        r#"{"type":"session","version":3,"id":"s-1","cwd":"/tmp"}"#,
        r#"{"type":"agent_start"}"#,
        r#"{"type":"message_start","message":{"role":"user","content":"go"}}"#,
        r#"{"type":"message_start","message":{"role":"assistant","content":[]}}"#,
        r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"hmm"}}"#,
        r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"I'll run it.\n"},{"type":"toolCall","id":"c1"}],"usage":{"input":10,"output":3,"cacheRead":4,"cost":{"total":0.25}},"stopReason":"toolUse"}}"#,
        r#"{"type":"tool_execution_start","toolCallId":"c1","toolName":"bash","args":{"command":"echo hi"}}"#,
        r#"{"type":"tool_execution_end","toolCallId":"c1","toolName":"bash","result":{"content":[{"type":"text","text":"hi"}]},"isError":false}"#,
        r#"{"type":"message_start","message":{"role":"assistant","content":[]}}"#,
        r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"\n\n"}}"#,
        r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"po"}}"#,
        r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"ng"}}"#,
        r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"pong"}],"usage":{"input":20,"output":1},"stopReason":"stop"}}"#,
        r#"{"type":"agent_settled","aborted":false}"#,
    ];

    #[test]
    fn a_tool_run_decodes_in_order() {
        let events = decode_all(&Pi::new("pi"), RUN);
        assert_eq!(events[0], Event::Started { agent_session: "s-1".into() });
        assert_eq!(
            events[1],
            Event::Thinking { text: "hmm".into() },
            "thinking streams as it comes"
        );
        assert!(
            matches!(&events[2], Event::Usage(u) if u.input_tokens == 10 && u.cost_usd == 0.25)
        );
        assert!(matches!(&events[3], Event::ToolStart { name, .. } if name == "bash"));
        assert_eq!(
            events[4],
            Event::ToolEnd {
                call_id: "c1".into(),
                name: "bash".into(),
                output: "hi".into(),
                error: false
            }
        );
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Event::Text { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "pong");
        assert_eq!(events.last(), Some(&Event::Done));
    }

    #[test]
    fn unstreamed_thinking_comes_whole_at_the_end_of_the_message() {
        let events = decode_all(
            &Pi::new("pi"),
            &[
                r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan it\n"}]}}"#,
            ],
        );
        assert_eq!(events, vec![Event::Thinking { text: "plan it".into() }]);
    }

    #[test]
    fn a_second_reply_is_a_new_paragraph() {
        let events = decode_all(
            &Pi::new("pi"),
            &[
                r#"{"type":"message_start","message":{"role":"assistant"}}"#,
                r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"one"}}"#,
                r#"{"type":"message_start","message":{"role":"assistant"}}"#,
                r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"two"}}"#,
            ],
        );
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Event::Text { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "one\n\ntwo");
    }

    #[test]
    fn a_provider_error_is_reported_at_settle_unless_aborted() {
        let err = r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"529"}}"#;
        let events =
            decode_all(&Pi::new("pi"), &[err, r#"{"type":"agent_settled","aborted":false}"#]);
        assert!(events.contains(&Event::Error("529".into())));
        let events =
            decode_all(&Pi::new("pi"), &[err, r#"{"type":"agent_settled","aborted":true}"#]);
        assert!(!events.iter().any(|e| matches!(e, Event::Error(_))));
    }

    #[test]
    fn a_retry_clears_the_pending_error() {
        let events = decode_all(
            &Pi::new("pi"),
            &[
                r#"{"type":"message_end","message":{"role":"assistant","stopReason":"error","errorMessage":"529"}}"#,
                r#"{"type":"auto_retry_start","attempt":1,"errorMessage":"529"}"#,
                r#"{"type":"agent_settled","aborted":false}"#,
            ],
        );
        assert!(!events.iter().any(|e| matches!(e, Event::Error(_))));
    }

    #[test]
    fn launch_reuses_the_session_to_resume() {
        let pi = Pi::new("pi");
        let c = crate::agent::context();
        let turn = |prompt, model, resume| Turn {
            files: &[],
            prompt,
            cwd: Path::new("/tmp"),
            model,
            effort: None,
            resume,
            history: &[],
            context: &c,
        };
        let fresh = pi.launch(&turn("hi", Some("m"), None), "BRIEF");
        assert_eq!(fresh.args[..3], ["--mode", "json", "--session-id"]);
        assert_eq!(fresh.args[4..], ["--append-system-prompt", "BRIEF", "--model", "m"]);
        assert_eq!(fresh.stdin.as_deref(), Some("hi"));
        let again = pi.launch(&turn("more", None, Some("s-1")), "BRIEF");
        assert_eq!(
            again.args,
            ["--mode", "json", "--session-id", "s-1", "--append-system-prompt", "BRIEF"]
        );
    }
}
