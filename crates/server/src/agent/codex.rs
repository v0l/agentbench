use super::{Cli, Decoder, Event, Launch, Turn, parse, str_at, u64_at};
use foreman_core::Usage;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub struct Codex {
    program: PathBuf,
}

impl Codex {
    pub fn new(program: impl Into<PathBuf>) -> Codex {
        Codex { program: program.into() }
    }
}

/// `codex debug models` prints `{"models":[{"slug":..,"visibility":"list"},..]}`.
fn codex_models(text: &str) -> Result<Vec<String>, String> {
    let at = text.find('{').ok_or("codex printed no model list")?;
    let v: Value =
        serde_json::from_str(&text[at..]).map_err(|e| format!("not a model list: {e}"))?;
    let mut out: Vec<String> = v
        .get("models")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|m| str_at(m, "visibility").is_none_or(|v| v == "list"))
                .filter_map(|m| str_at(m, "slug").map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out.dedup();
    Ok(out)
}

#[super::async_trait]
impl Cli for Codex {
    fn name(&self) -> &str {
        "codex"
    }

    fn label(&self) -> &str {
        "Codex"
    }

    fn program(&self) -> &Path {
        &self.program
    }

    fn resumable(&self) -> bool {
        true
    }

    async fn models(&self) -> Result<Vec<String>, String> {
        let text = super::process::listing(&self.program, &["debug", "models"]).await?;
        codex_models(&text)
    }

    fn launch(&self, turn: &Turn, brief: &str) -> Launch {
        let mut args = vec!["exec".to_string()];
        if let Some(id) = turn.resume {
            args.extend(["resume".to_string(), id.to_string()]);
        }
        args.extend(["--json", "--skip-git-repo-check", "--full-auto"].map(String::from));
        if let Some(model) = turn.model {
            args.extend(["--model".to_string(), model.to_string()]);
        }
        args.push("-".to_string());
        let stdin = match turn.resume {
            Some(_) => turn.prompt.to_string(),
            None => format!("{brief}\n\n---\n\n{}", turn.prompt),
        };
        Launch { program: self.program.clone(), args, stdin: Some(stdin), env: vec![] }
    }

    fn decoder(&self) -> Box<dyn Decoder> {
        Box::new(CodexDecoder::default())
    }
}

#[derive(Default)]
struct CodexDecoder {
    texted: bool,
}

fn transient(message: &str) -> bool {
    message.starts_with("Reconnecting")
}

impl Decoder for CodexDecoder {
    fn line(&mut self, line: &str) -> Vec<Event> {
        let Some(v) = parse(line) else {
            return vec![];
        };
        match str_at(&v, "type").unwrap_or_default() {
            "thread.started" => str_at(&v, "thread_id")
                .map(|id| vec![Event::Started { agent_session: id.to_string() }])
                .unwrap_or_default(),
            "item.started" => self.started(v.get("item").unwrap_or(&Value::Null)),
            "item.completed" => self.completed(v.get("item").unwrap_or(&Value::Null)),
            "turn.completed" => {
                let u = v.get("usage").cloned().unwrap_or(Value::Null);
                vec![
                    Event::Usage(Usage {
                        input_tokens: u64_at(&u, "input_tokens"),
                        output_tokens: u64_at(&u, "output_tokens"),
                        cache_read_tokens: u64_at(&u, "cached_input_tokens"),
                        cost_usd: 0.0,
                        turns: 1,
                    }),
                    Event::Done,
                ]
            }
            "turn.failed" => {
                let msg =
                    v.get("error").and_then(|e| str_at(e, "message")).unwrap_or("turn failed");
                vec![Event::Error(msg.to_string()), Event::Done]
            }
            "error" => {
                let msg = str_at(&v, "message").unwrap_or("codex error");
                if transient(msg) {
                    vec![Event::Notice(msg.to_string())]
                } else {
                    vec![Event::Error(msg.to_string())]
                }
            }
            _ => vec![],
        }
    }
}

impl CodexDecoder {
    fn started(&mut self, item: &Value) -> Vec<Event> {
        let id = str_at(item, "id").unwrap_or_default().to_string();
        match str_at(item, "type").unwrap_or_default() {
            "command_execution" => vec![Event::ToolStart {
                call_id: id,
                name: "bash".into(),
                input: json!({ "command": str_at(item, "command").unwrap_or_default() }),
            }],
            "mcp_tool_call" => vec![Event::ToolStart {
                call_id: id,
                name: format!(
                    "{}.{}",
                    str_at(item, "server").unwrap_or("mcp"),
                    str_at(item, "tool").unwrap_or("tool")
                ),
                input: item.get("arguments").cloned().unwrap_or(Value::Null),
            }],
            _ => vec![],
        }
    }

    fn completed(&mut self, item: &Value) -> Vec<Event> {
        let id = str_at(item, "id").unwrap_or_default().to_string();
        let failed = str_at(item, "status") == Some("failed");
        match str_at(item, "type").unwrap_or_default() {
            "agent_message" | "assistant_message" => {
                let text = str_at(item, "text").unwrap_or_default().trim();
                if text.is_empty() {
                    return vec![];
                }
                let delta = if self.texted { format!("\n\n{text}") } else { text.to_string() };
                self.texted = true;
                vec![Event::Text { delta }]
            }
            "reasoning" => str_at(item, "text")
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| vec![Event::Thinking { text: t.to_string() }])
                .unwrap_or_default(),
            "command_execution" => {
                let code = item.get("exit_code").and_then(Value::as_i64);
                vec![Event::ToolEnd {
                    call_id: id,
                    name: "bash".into(),
                    output: str_at(item, "aggregated_output").unwrap_or_default().to_string(),
                    error: failed || code.is_some_and(|c| c != 0),
                }]
            }
            "file_change" => {
                let changes: Vec<String> = item
                    .get("changes")
                    .and_then(Value::as_array)
                    .map(|cs| {
                        cs.iter()
                            .map(|c| {
                                format!(
                                    "{} {}",
                                    str_at(c, "kind").unwrap_or("update"),
                                    str_at(c, "path").unwrap_or("?")
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                vec![
                    Event::ToolStart {
                        call_id: id.clone(),
                        name: "edit".into(),
                        input: item.get("changes").cloned().unwrap_or(Value::Null),
                    },
                    Event::ToolEnd {
                        call_id: id,
                        name: "edit".into(),
                        output: changes.join("\n"),
                        error: failed,
                    },
                ]
            }
            "mcp_tool_call" => {
                let error = item.get("error").and_then(|e| str_at(e, "message"));
                let output = match (error, item.get("result")) {
                    (Some(e), _) => e.to_string(),
                    (None, Some(r)) => super::content_text(r.get("content").unwrap_or(r)),
                    (None, None) => String::new(),
                };
                vec![Event::ToolEnd {
                    call_id: id,
                    name: format!(
                        "{}.{}",
                        str_at(item, "server").unwrap_or("mcp"),
                        str_at(item, "tool").unwrap_or("tool")
                    ),
                    output,
                    error: failed || error.is_some(),
                }]
            }
            "web_search" => vec![Event::Notice(format!(
                "searched the web: {}",
                str_at(item, "query").unwrap_or_default()
            ))],
            "error" => vec![Event::Notice(str_at(item, "message").unwrap_or_default().to_string())],
            _ => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::decode_all;

    #[test]
    fn codex_models_skip_the_hidden_ones() {
        let text = r#"{"models":[{"slug":"b","visibility":"list"},{"slug":"a","visibility":"list"},{"slug":"h","visibility":"hide"}]}"#;
        assert_eq!(codex_models(text).unwrap(), ["a", "b"]);
        assert!(codex_models("nope").is_err());
    }

    #[test]
    fn a_tool_run_decodes_in_order() {
        let events = decode_all(
            &Codex::new("codex"),
            &[
                r#"{"type":"thread.started","thread_id":"th-1"}"#,
                r#"{"type":"turn.started"}"#,
                r#"{"type":"error","message":"Reconnecting... 1/5"}"#,
                r#"{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"ls","status":"in_progress"}}"#,
                r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","command":"ls","aggregated_output":"a\n","exit_code":0,"status":"completed"}}"#,
                r#"{"type":"item.completed","item":{"id":"i2","type":"file_change","changes":[{"path":"a.txt","kind":"add"}],"status":"completed"}}"#,
                r#"{"type":"item.completed","item":{"id":"i3","type":"agent_message","text":"done"}}"#,
                r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":7}}"#,
            ],
        );
        assert_eq!(events[0], Event::Started { agent_session: "th-1".into() });
        assert_eq!(events[1], Event::Notice("Reconnecting... 1/5".into()));
        assert!(
            matches!(&events[2], Event::ToolStart { name, input, .. } if name == "bash" && input["command"] == "ls")
        );
        assert_eq!(
            events[3],
            Event::ToolEnd {
                call_id: "i1".into(),
                name: "bash".into(),
                output: "a\n".into(),
                error: false
            }
        );
        assert!(matches!(&events[5], Event::ToolEnd { output, .. } if output == "add a.txt"));
        assert_eq!(events[6], Event::Text { delta: "done".into() });
        assert!(
            matches!(&events[7], Event::Usage(u) if u.input_tokens == 100 && u.cache_read_tokens == 40)
        );
        assert_eq!(events[8], Event::Done);
    }

    #[test]
    fn a_failed_turn_and_a_nonzero_exit_are_errors() {
        let events = decode_all(
            &Codex::new("codex"),
            &[
                r#"{"type":"item.completed","item":{"id":"i1","type":"command_execution","aggregated_output":"","exit_code":2,"status":"completed"}}"#,
                r#"{"type":"error","message":"high demand"}"#,
                r#"{"type":"turn.failed","error":{"message":"high demand"}}"#,
            ],
        );
        assert!(matches!(&events[0], Event::ToolEnd { error: true, .. }));
        assert_eq!(events[1], Event::Error("high demand".into()));
        assert_eq!(&events[2..], &[Event::Error("high demand".into()), Event::Done]);
    }

    #[test]
    fn launch_resumes_a_thread_and_briefs_only_a_new_one() {
        let c = crate::agent::context();
        let turn = |resume| Turn {
            files: &[],
            prompt: "hi",
            cwd: Path::new("/tmp"),
            model: None,
            resume,
            history: &[],
            context: &c,
        };
        let l = Codex::new("codex").launch(&turn(Some("th-1")), "BRIEF");
        assert_eq!(l.args[..3], ["exec", "resume", "th-1"]);
        assert_eq!(l.args.last().map(String::as_str), Some("-"));
        assert_eq!(l.stdin.as_deref(), Some("hi"));
        let fresh = Codex::new("codex").launch(&turn(None), "BRIEF");
        assert!(fresh.stdin.unwrap().starts_with("BRIEF\n"));
    }
}
