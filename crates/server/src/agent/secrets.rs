use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const OPEN: &str = "{{secret:";
const CLOSE: &str = "}}";

#[derive(Clone, Default)]
pub struct Secrets(Arc<Mutex<HashMap<(String, String), String>>>);

pub fn placeholder(name: &str) -> String {
    format!("{OPEN}{name}{CLOSE}")
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

impl Secrets {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), String>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn has(&self, session: &str, name: &str) -> bool {
        self.lock().contains_key(&(session.to_string(), name.to_string()))
    }

    pub fn set(&self, session: &str, name: &str, value: &str) {
        self.lock().insert((session.to_string(), name.to_string()), value.to_string());
    }

    pub fn forget(&self, session: &str) {
        self.lock().retain(|(s, _), _| s != session);
    }

    pub fn names(&self, session: &str) -> Vec<String> {
        let mut names: Vec<String> =
            self.lock().keys().filter(|(s, _)| s == session).map(|(_, n)| n.clone()).collect();
        names.sort();
        names
    }

    pub fn fill(&self, session: &str, args: &Value) -> Result<Value, String> {
        match args {
            Value::String(text) => self.fill_text(session, text).map(Value::String),
            Value::Array(items) => items
                .iter()
                .map(|v| self.fill(session, v))
                .collect::<Result<_, _>>()
                .map(Value::Array),
            Value::Object(map) => map
                .iter()
                .map(|(k, v)| self.fill(session, v).map(|v| (k.clone(), v)))
                .collect::<Result<_, _>>()
                .map(Value::Object),
            other => Ok(other.clone()),
        }
    }

    fn fill_text(&self, session: &str, text: &str) -> Result<String, String> {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(OPEN) {
            let after = &rest[start + OPEN.len()..];
            let Some(end) = after.find(CLOSE) else { break };
            let name = &after[..end];
            if !valid_name(name) {
                out.push_str(&rest[..start + OPEN.len()]);
                rest = after;
                continue;
            }
            let value =
                self.lock().get(&(session.to_string(), name.to_string())).cloned().ok_or_else(
                    || format!("no secret called {name:?} in this chat; call request_secret first"),
                )?;
            out.push_str(&rest[..start]);
            out.push_str(&value);
            rest = &after[end + CLOSE.len()..];
        }
        out.push_str(rest);
        Ok(out)
    }

    pub fn scrub(&self, session: &str, text: &str) -> String {
        let mut forms: Vec<(String, String)> = Vec::new();
        for ((s, name), value) in self.lock().iter() {
            if s != session || value.is_empty() {
                continue;
            }
            for form in spellings(value) {
                forms.push((form, placeholder(name)));
            }
        }
        forms.sort_by_key(|(f, _)| std::cmp::Reverse(f.len()));
        let mut out = text.to_string();
        for (form, mask) in forms {
            out = out.replace(&form, &mask);
        }
        out
    }
}

fn percent(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn spellings(value: &str) -> Vec<String> {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let backwards: String = value.chars().rev().collect();
    let mut all = vec![
        value.to_string(),
        backwards.clone(),
        percent(value),
        percent(value).to_lowercase(),
        hex(value.as_bytes()),
        hex(value.as_bytes()).to_uppercase(),
        hex(backwards.as_bytes()),
        value.replace('\\', "\\\\"),
        value.replace('\'', "'\\''"),
    ];
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        all.push(engine.encode(value.as_bytes()));
        all.push(engine.encode(backwards.as_bytes()));
    }
    let bytes = value.as_bytes();
    for skip in 1..3.min(bytes.len()) {
        let whole = bytes[skip..].len() / 3 * 3;
        if whole >= 6 {
            all.push(STANDARD_NO_PAD.encode(&bytes[skip..skip + whole]));
        }
    }
    all.retain(|s| s.len() >= 3);
    all.sort();
    all.dedup();
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_placeholder_is_filled_in_any_argument_and_the_value_scrubbed_from_output() {
        let s = Secrets::default();
        s.set("a", "API_KEY", "hunter2");
        let filled = s
            .fill("a", &json!({"command": "curl -H 'x: {{secret:API_KEY}}' h", "n": [1, "{{secret:API_KEY}}"]}))
            .unwrap();
        assert_eq!(filled["command"], "curl -H 'x: hunter2' h");
        assert_eq!(filled["n"][1], "hunter2");
        assert_eq!(s.scrub("a", "sent hunter2 ok"), "sent {{secret:API_KEY}} ok");
        assert_eq!(s.scrub("b", "sent hunter2 ok"), "sent hunter2 ok");
    }

    #[test]
    fn common_encodings_of_the_value_are_scrubbed_too() {
        let s = Secrets::default();
        s.set("a", "K", "p@ss w0rd!");
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode("p@ss w0rd!");
        assert_eq!(s.scrub("a", &format!("out: {b64}")), "out: {{secret:K}}");
        let hex: String = "p@ss w0rd!".bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(s.scrub("a", &hex), "{{secret:K}}");
        assert_eq!(s.scrub("a", "p%40ss%20w0rd%21"), "{{secret:K}}");
        assert_eq!(s.scrub("a", "!dr0w ss@p"), "{{secret:K}}");
        let basic = base64::engine::general_purpose::STANDARD.encode("user:p@ss w0rd!");
        assert!(!s.scrub("a", &format!("Authorization: Basic {basic}")).contains(&basic[8..]));
    }

    #[test]
    fn an_unknown_secret_is_refused_and_other_chats_cannot_use_it() {
        let s = Secrets::default();
        s.set("a", "K", "v");
        assert!(s.fill("b", &json!("{{secret:K}}")).unwrap_err().contains("request_secret"));
        assert_eq!(
            s.fill("a", &json!("{{ not a secret }} {{secret:bad name}}")).unwrap(),
            json!("{{ not a secret }} {{secret:bad name}}")
        );
        s.forget("a");
        assert!(!s.has("a", "K"));
    }
}
