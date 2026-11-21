use base64::Engine;
use std::io::Write;
use std::process::{Command, Stdio};

pub fn osc52(text: &str) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{data}\x07")
}

fn native(text: &str) -> bool {
    let tools: [(&str, &[&str]); 3] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("pbcopy", &[]),
    ];
    for (bin, args) in tools {
        let Ok(mut child) = Command::new(bin)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return true;
        }
    }
    false
}

pub fn copy(text: &str) {
    let remote = std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some();
    if !remote {
        native(text);
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(osc52(text).as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_wraps_the_base64_of_the_text() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }
}
