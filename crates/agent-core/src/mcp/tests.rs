use super::*;
use std::sync::mpsc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FAKE: &str = include_str!("fake_server.py");

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    mcp: Arc<Mcp>,
}

fn rig(servers: Value) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    std::fs::write(dir.path().join("fake.py"), FAKE).unwrap();
    let text = serde_json::to_string(&json!({ "mcpServers": servers }))
        .unwrap()
        .replace("FAKE_PY", &dir.path().join("fake.py").display().to_string());
    let global = dir.path().join("mcp.json");
    std::fs::write(&global, text).unwrap();
    let settings = Settings {
        files: vec![global],
        project_files: vec![".mcp.json".into()],
        client: "test".into(),
        tool_dirs: Vec::new(),
    };
    Rig { _dir: dir, root, mcp: Arc::new(Mcp::new(settings)) }
}

fn fake() -> Value {
    json!({ "fake": { "command": "python3", "args": ["FAKE_PY"], "env": { "FAKE_MCP_VAR": "set" } } })
}

impl Rig {
    async fn run(&self, args: Value) -> Result<Answer, String> {
        self.mcp.run(&args, &self.root, &CancellationToken::new()).await
    }
}

#[tokio::test]
async fn a_stdio_server_starts_when_first_used_and_its_tools_are_listed_and_called() {
    let r = rig(fake());
    let list = r.run(json!({})).await.unwrap().text;
    assert!(list.contains("- fake (stdio: python3 "), "{list}");
    assert!(list.contains("not started"), "{list}");
    assert!(r.mcp.running().is_empty(), "listing the servers starts none");

    let tools = r.run(json!({ "server": "fake" })).await.unwrap().text;
    assert!(tools.starts_with("fake (fake 1.2) has 6 tools:"), "{tools}");
    assert!(tools.contains("- echo(text: string, loud?: boolean): Say back the text"), "{tools}");
    assert!(tools.contains("    text: What to say"), "{tools}");
    assert!(tools.contains("- env():"), "tools from the second page: {tools}");
    assert!(tools.contains("The server says:\nUse echo to test."), "{tools}");
    assert_eq!(r.mcp.running(), [("fake".to_string(), r.root.clone())]);
    assert!(r.run(json!({})).await.unwrap().text.contains("running, 6 tools"));

    let said =
        r.run(json!({ "server": "fake", "tool": "echo", "args": { "text": "hello" } })).await;
    let root = client::root_uri(&r.root).unwrap();
    assert_eq!(said.unwrap().text, format!("hello from {root}"));

    let env = r.run(json!({ "server": "fake", "tool": "env", "args": "{}" })).await.unwrap();
    assert_eq!(env.text, format!("set in {}", r.root.display()));
}

#[tokio::test]
async fn a_picture_comes_back_to_be_attached_and_a_tool_error_is_an_error() {
    let r = rig(fake());
    let shot = r.run(json!({ "server": "fake", "tool": "shot" })).await.unwrap();
    assert_eq!(shot.text, "here it is\n[a image/png picture, 1 KB, attached]");
    assert_eq!(shot.images, [Image { mime: "image/png".into(), data: "iVBORw0KGgo=".into() }]);
    assert!(
        shot.images[0].part()["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    let failed = r.run(json!({ "server": "fake", "tool": "fail" })).await;
    assert_eq!(failed, Err("the board is missing".to_string()));
    let bad = r.run(json!({ "server": "fake", "tool": "echo", "args": [1] })).await;
    assert!(bad.unwrap_err().contains("must be an object"));
}

#[tokio::test]
async fn names_that_do_not_exist_list_what_does() {
    let r = rig(fake());
    let e = r.run(json!({ "server": "nope" })).await.unwrap_err();
    assert_eq!(e, "there is no MCP server called \"nope\"; the servers are: fake");
    let e = r.run(json!({ "server": "fake", "tool": "nope" })).await.unwrap_err();
    assert!(e.starts_with("fake has no tool called \"nope\"; its tools are: echo, shot"), "{e}");
    let schema = r.run(json!({ "server": "FAKE", "tool": "echo", "schema": true })).await.unwrap();
    assert!(schema.text.contains("\"required\": [\n    \"text\"\n  ]"), "{}", schema.text);
}

#[tokio::test]
async fn a_server_that_dies_mid_call_says_why_and_is_started_again_next_time() {
    let r = rig(fake());
    let e = r.run(json!({ "server": "fake", "tool": "die" })).await.unwrap_err();
    assert!(e.contains("fake stopped before it answered"), "{e}");
    assert!(e.contains("fake: out of memory"), "{e}");
    let again =
        r.run(json!({ "server": "fake", "tool": "echo", "args": { "text": "back" } })).await;
    assert!(again.unwrap().text.starts_with("back from "));
}

#[tokio::test]
async fn stopping_the_chat_ends_a_call_that_is_still_running() {
    let r = rig(fake());
    r.run(json!({ "server": "fake" })).await.unwrap();
    let stop = CancellationToken::new();
    let later = stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        later.cancel();
    });
    let started = Instant::now();
    let got = r.mcp.run(&json!({ "server": "fake", "tool": "slow" }), &r.root, &stop).await;
    assert_eq!(got, Err("stopped".to_string()));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_server_that_cannot_start_is_reported_in_the_list() {
    let r = rig(json!({ "gone": { "command": "egui-bench-no-such-mcp-server" } }));
    let e = r.run(json!({ "server": "gone" })).await.unwrap_err();
    assert!(
        e.contains("could not start gone: `egui-bench-no-such-mcp-server` is not on PATH"),
        "{e}"
    );
    let list = r.run(json!({})).await.unwrap().text;
    assert!(list.contains("failed last time: could not start gone"), "{list}");
}

#[tokio::test]
async fn the_projects_own_file_adds_servers_and_the_section_names_them() {
    let r = rig(fake());
    assert_eq!(
        r.mcp.section(&r.root).unwrap(),
        "MCP servers are set up for you: fake. Reach their tools through the mcp tool: call it with a server to list \
         that server's tools, then with server, tool and args to use one."
    );
    std::fs::write(
        r.root.join(".mcp.json"),
        r#"{"mcpServers": {"site": {"url": "http://127.0.0.1:9/mcp"}}}"#,
    )
    .unwrap();
    let list = r.run(json!({})).await.unwrap().text;
    assert!(list.contains("- site (http: http://127.0.0.1:9/mcp): not started"), "{list}");
    let empty = rig(json!({}));
    assert!(empty.mcp.section(&empty.root).is_none());
    assert!(empty.run(json!({})).await.unwrap().text.starts_with("No MCP servers are set up."));
}

struct Seen {
    headers: String,
    body: Value,
}

async fn http_server(sse: bool) -> (String, mpsc::Receiver<Seen>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let (seen, asked) = mpsc::channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    if s.read(&mut byte).await.unwrap_or(0) != 1 {
                        return;
                    }
                    buf.push(byte[0]);
                }
                let headers = String::from_utf8_lossy(&buf).to_lowercase();
                let len = headers
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("content-length:").and_then(|v| v.trim().parse().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                let _ = s.read_exact(&mut body).await;
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = body["method"].as_str().unwrap_or("").to_string();
                let id = body["id"].clone();
                let _ = seen.send(Seen { headers: headers.clone(), body: body.clone() });
                let result = match method.as_str() {
                    "initialize" => json!({
                        "protocolVersion": "2025-03-26",
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "web", "title": "Web docs", "version": "1" },
                    }),
                    "tools/list" => json!({ "tools": [{
                        "name": "search", "description": "Search the docs.",
                        "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } }, "required": ["q"] },
                    }] }),
                    "tools/call" => json!({
                        "content": [],
                        "structuredContent": { "hits": [body["params"]["arguments"]["q"]] },
                    }),
                    _ => Value::Null,
                };
                let reply = if id.is_null() {
                    "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else {
                    let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                    let (ct, text) = if sse {
                        let note = json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": {} });
                        (
                            "text/event-stream",
                            format!(
                                "event: message\ndata: {note}\n\nevent: message\ndata: {msg}\n\n"
                            ),
                        )
                    } else {
                        ("application/json", msg.to_string())
                    };
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nMcp-Session-Id: abc123\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                        text.len()
                    )
                };
                let _ = s.write_all(reply.as_bytes()).await;
            });
        }
    });
    (url, asked)
}

async fn over_http(sse: bool) {
    let (url, asked) = http_server(sse).await;
    let r = rig(json!({ "web": { "url": url, "headers": { "X-Key": "k1" } } }));
    let tools = r.run(json!({ "server": "web" })).await.unwrap().text;
    assert_eq!(tools, "web (Web docs 1) has 1 tools:\n- search(q: string): Search the docs.");
    let found =
        r.run(json!({ "server": "web", "tool": "search", "args": { "q": "bun" } })).await.unwrap();
    assert_eq!(found.text, "{\n  \"hits\": [\n    \"bun\"\n  ]\n}");
    let seen: Vec<Seen> = asked.try_iter().collect();
    let methods: Vec<&str> = seen.iter().map(|s| s.body["method"].as_str().unwrap_or("")).collect();
    assert_eq!(methods, ["initialize", "notifications/initialized", "tools/list", "tools/call"]);
    assert!(seen.iter().all(|s| s.headers.contains("x-key: k1")));
    assert!(seen[0].headers.contains("accept: application/json, text/event-stream"));
    assert!(!seen[0].headers.contains("mcp-session-id"));
    for later in &seen[1..] {
        assert!(later.headers.contains("mcp-session-id: abc123"), "{}", later.headers);
        assert!(later.headers.contains("mcp-protocol-version: 2025-03-26"), "{}", later.headers);
    }
}

#[tokio::test]
async fn a_streamable_http_server_answering_in_json_keeps_its_session() {
    over_http(false).await;
}

#[tokio::test]
async fn a_streamable_http_server_answering_in_an_event_stream_keeps_its_session() {
    over_http(true).await;
}

#[test]
fn every_kind_of_content_is_put_into_words() {
    let got = render(&json!({ "content": [
        { "type": "text", "text": "one" },
        { "type": "audio", "mimeType": "audio/wav", "data": "AAAA" },
        { "type": "resource", "resource": { "uri": "file:///a.txt", "text": "inside" } },
        { "type": "resource", "resource": { "uri": "file:///b.bin", "blob": "AAAA", "mimeType": "application/zip" } },
        { "type": "resource_link", "uri": "file:///c", "name": "c" },
    ] }))
    .unwrap();
    assert_eq!(
        got.text,
        "one\n[audio/wav audio left out]\nfile:///a.txt:\ninside\n[file:///b.bin: application/zip left out]\n[link: file:///c c]"
    );
    assert_eq!(render(&json!({ "content": [] })).unwrap().text, "(the tool answered with nothing)");
}

#[test]
fn argument_types_read_the_way_they_are_written() {
    let t = client::Tool {
        name: "set".into(),
        about: "Set things.".into(),
        schema: json!({ "type": "object", "required": ["mode"], "properties": {
            "either": { "anyOf": [{ "type": "number" }, { "type": "null" }] },
            "ids": { "type": "array", "items": { "type": "integer" } },
            "maybe": { "type": ["string", "null"] },
            "mode": { "enum": ["a", "b"] },
        } }),
    };
    assert_eq!(
        describe(&t),
        "- set(mode: \"a\"|\"b\", either?: number, ids?: integer[], maybe?: string): Set things."
    );
}

#[test]
fn the_summary_says_which_server_and_tool() {
    assert_eq!(summary(&json!({})).unwrap(), "servers");
    assert_eq!(summary(&json!({ "server": "ha" })).unwrap(), "ha: tools");
    assert_eq!(summary(&json!({ "server": "ha", "tool": "light" })).unwrap(), "ha light");
}

#[test]
fn a_server_with_many_tools_is_listed_a_line_a_tool_within_the_budget() {
    let tools: Vec<client::Tool> = (0..137)
        .map(|i| client::Tool {
            name: format!("tool_{i}"),
            about: format!("Tool number {i}. {}", "It does a great deal of things. ".repeat(12)),
            schema: json!({ "type": "object", "required": ["a"], "properties": {
                "a": { "type": "string", "description": "The first argument, explained at length.".repeat(4) },
                "b": { "type": "integer" },
            } }),
        })
        .collect();
    let out = listing_of("kicad", "kicad-mcp-server 1.0.0", "", &tools);
    assert!(out.len() <= LISTING_CHARS, "{} chars", out.len());
    assert!(
        out.starts_with(
            "kicad (kicad-mcp-server 1.0.0) has 137 tools, too many to describe in full"
        ),
        "{out}"
    );
    assert!(out.contains("\n- tool_0(a: string, b?: integer): Tool number 0\n"), "{out}");
    assert!(out.contains("- tool_136("), "every tool is named");
    assert!(!out.contains("explained at length"));
    let few = listing_of("fake", "", "", &tools[..2]);
    assert!(few.contains("explained at length"), "a short list is described in full");
}
