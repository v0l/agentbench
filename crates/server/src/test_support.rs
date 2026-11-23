use crate::store::Store;
use agentbench_core::{Entry, Said, Sent, SessionId};
use serde_json::json;

pub fn wire_user_row(text: &str, from: &[&str]) -> Entry {
    let said = Said::Typed { attachments: vec![], from: from.iter().map(|s| s.to_string()).collect() };
    Entry::Wire { messages: vec![Sent { message: json!({"role": "user", "content": text}), said }] }
}

pub fn wire_reply_row(text: &str) -> Entry {
    Entry::Wire { messages: vec![Sent { message: json!({"role": "assistant", "content": text}), said: Said::Plain }] }
}

pub fn wire_user(store: &Store, session: SessionId, text: &str) {
    store.add_message(session, &wire_user_row(text, &[])).unwrap();
}

pub fn wire_reply(store: &Store, session: SessionId, text: &str) {
    store.add_message(session, &wire_reply_row(text)).unwrap();
}
