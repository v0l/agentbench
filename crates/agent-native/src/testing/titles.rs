use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

/// A server that answers each request from `replies` and records the bodies.
pub fn server(replies: Vec<(u16, String)>) -> (String, Arc<Mutex<Vec<Value>>>) {
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
            let kind = if reply.starts_with("data:") {
                "text/event-stream"
            } else {
                "application/json"
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (url, seen)
}

pub fn completion(text: &str) -> String {
    json!({ "choices": [{ "message": { "role": "assistant", "content": text } }] }).to_string()
}

pub fn streamed(delta: Value) -> String {
    format!("data: {}\n\ndata: [DONE]\n\n", json!({ "choices": [{ "delta": delta }] }))
}
