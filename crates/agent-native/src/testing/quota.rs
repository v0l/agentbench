use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

pub fn ok(body: String) -> Reply {
    Reply { status: 200, headers: vec![], body }
}

pub fn server(
    completions: Vec<Reply>,
    quota: Option<String>,
) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let paths = Arc::new(Mutex::new(Vec::new()));
    let log = paths.clone();
    let queue = Mutex::new(completions.into_iter().collect::<std::collections::VecDeque<_>>());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let path = loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                buf.extend_from_slice(&chunk[..n]);
                if let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..split]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buf.len() >= split + 4 + len {
                        break head.split_whitespace().nth(1).unwrap_or_default().to_string();
                    }
                }
                if n == 0 {
                    break String::new();
                }
            };
            log.lock().unwrap().push(path.clone());
            let reply = if path.split('?').next().unwrap_or_default().ends_with("/quota") {
                match &quota {
                    Some(b) => ok(b.clone()),
                    None => Reply { status: 404, headers: vec![], body: "{}".into() },
                }
            } else {
                match queue.lock().unwrap().pop_front() {
                    Some(r) => r,
                    None => return,
                }
            };
            let kind = if reply.body.starts_with("data:") {
                "text/event-stream"
            } else {
                "application/json"
            };
            let extra: String =
                reply.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
            let _ = write!(
                s,
                "HTTP/1.1 {} X\r\ncontent-type: {kind}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{}",
                reply.status,
                reply.body.len(),
                reply.body
            );
        }
    });
    (url, paths)
}
