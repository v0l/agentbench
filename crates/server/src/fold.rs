use agentbench_core::{Entry, SUMMARY_PREAMBLE};
use serde_json::{Value, json};

pub const SECTION_PREAMBLE: &str =
    "Here is a summary of an earlier part of our conversation. Carry on from it.";

pub const FOLD_PROMPT: &str = "The oldest part of the conversation above is being folded into a summary so the context shrinks; everything after it is kept as it is. \
Summarise only the conversation above, not any earlier summary in it. \
Start with one line, `Topic: ` and three to eight words naming what this part was about. Then the summary as markdown. \
Keep: what the user asked for, decisions made and why, what is done, what is left, the state of any work in progress, \
the results still needed, and what was about to happen next. Be specific with names, ids and paths. No preamble.";

pub const UNNAMED_TOPIC: &str = "earlier messages";

#[derive(Clone, Debug, PartialEq)]
pub struct Section {
    pub text: String,
    pub covers: usize,
}

pub fn sections(entries: &[Entry]) -> Vec<Section> {
    agentbench_core::since_summary(entries)
        .iter()
        .filter_map(|e| match e {
            Entry::Section { text, covers, .. } => {
                Some(Section { text: text.clone(), covers: *covers })
            }
            _ => None,
        })
        .collect()
}

pub fn head_len(messages: &[Value]) -> usize {
    let system = usize::from(messages.first().is_some_and(|m| m["role"] == "system"));
    let carried = messages.get(system).is_some_and(|m| {
        m["role"] == "user"
            && m["content"].as_str().is_some_and(|c| c.starts_with(SUMMARY_PREAMBLE))
    });
    system + usize::from(carried)
}

pub fn apply(raw: &[Value], sections: &[Section]) -> Vec<Value> {
    let head = head_len(raw).min(raw.len());
    let mut at = head;
    let mut out: Vec<Value> = raw[..head].to_vec();
    for section in sections {
        if at + section.covers > raw.len() {
            break;
        }
        at += section.covers;
        out.push(json!({ "role": "user", "content": format!("{SECTION_PREAMBLE}\n\n{}", section.text) }));
    }
    out.extend_from_slice(&raw[at..]);
    out
}

fn is_turn_start(message: &Value) -> bool {
    message["role"] == "user"
}

pub fn plan(raw: &[Value], folded: usize, keep: u64) -> Option<usize> {
    let start = head_len(raw) + folded;
    if start >= raw.len() {
        return None;
    }
    let total = size(&raw[start..]);
    let mut open: std::collections::HashSet<&str> = Default::default();
    let mut best = None;
    for (i, message) in raw.iter().enumerate().skip(start) {
        if i > start && is_turn_start(message) && open.is_empty() {
            if size(&raw[i..]) as u64 * 100 >= keep.saturating_mul(total as u64) {
                best = Some(i - start);
            } else {
                break;
            }
        }
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            if let Some(id) = call["id"].as_str() {
                open.insert(id);
            }
        }
        if message["role"] == "tool"
            && let Some(id) = message["tool_call_id"].as_str()
        {
            open.remove(id);
        }
    }
    best
}

pub fn split_topic(reply: &str) -> (String, String) {
    let reply = reply.trim();
    let (first, rest) = reply.split_once('\n').unwrap_or((reply, ""));
    let named = first
        .trim()
        .trim_start_matches(['*', '#', ' '])
        .strip_prefix("Topic:")
        .map(|t| t.trim().trim_matches('*').trim().to_string())
        .filter(|t| !t.is_empty());
    match named {
        Some(topic) => (topic, rest.trim().to_string()),
        None => (UNNAMED_TOPIC.to_string(), reply.to_string()),
    }
}

pub fn size(messages: &[Value]) -> usize {
    messages.iter().map(|m| m.to_string().len()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Value {
        json!({ "role": "user", "content": text })
    }

    fn said(text: &str) -> Value {
        json!({ "role": "assistant", "content": text })
    }

    fn call(id: &str) -> Value {
        json!({ "role": "assistant", "content": "", "tool_calls": [{
            "id": id, "type": "function", "function": { "name": "read_file", "arguments": "{}" } }] })
    }

    fn result(id: &str, text: &str) -> Value {
        json!({ "role": "tool", "tool_call_id": id, "content": text })
    }

    fn topic(name: &str) -> Vec<Value> {
        let filler = "x".repeat(400);
        vec![
            user(&format!("let us talk about {name}")),
            call(&format!("{name}-1")),
            result(&format!("{name}-1"), &filler),
            said(&format!("{name} looks fine {filler}")),
        ]
    }

    fn long() -> Vec<Value> {
        let mut raw = vec![json!({ "role": "system", "content": "you are the supervisor" })];
        for name in ["login", "billing", "deploys"] {
            raw.extend(topic(name));
        }
        raw
    }

    fn section(text: &str, covers: usize) -> Section {
        Section { text: text.into(), covers }
    }

    #[test]
    fn three_topics_fold_only_the_oldest() {
        let raw = long();
        let covers = plan(&raw, 0, 40).unwrap();
        assert_eq!(covers, 4);
        let after = apply(&raw, &[section("login was fixed", covers)]);
        assert_eq!(after[0], raw[0]);
        assert_eq!(
            after[1],
            user(&format!("{SECTION_PREAMBLE}\n\nlogin was fixed")),
        );
        assert_eq!(after[2..], raw[5..], "the recent messages are untouched");
    }

    #[test]
    fn a_second_fold_appends_and_leaves_the_first_alone() {
        let raw = long();
        let first = plan(&raw, 0, 40).unwrap();
        let once = apply(&raw, &[section("login was fixed", first)]);
        let second = plan(&raw, first, 40).unwrap();
        assert_eq!(second, 4);
        let twice =
            apply(&raw, &[section("login was fixed", first), section("billing too", second)]);
        assert_eq!(twice[..2], once[..2], "the prefix is byte for byte what it was");
        assert_eq!(twice[2], user(&format!("{SECTION_PREAMBLE}\n\nbilling too")));
        assert_eq!(twice[3..], raw[9..]);
    }

    #[test]
    fn a_call_is_never_cut_from_its_result() {
        let filler = "y".repeat(400);
        let raw = vec![
            json!({ "role": "system", "content": "s" }),
            user("start"),
            said(&filler),
            call("c1"),
            user("steer: also check the logs"),
            result("c1", &filler),
            said(&filler),
            user("last"),
            said("ok"),
        ];
        let covers = plan(&raw, 0, 1).unwrap();
        assert_eq!(covers, 6, "cut before the last turn, never at the steer");
        let after = apply(&raw, &[section("s", covers)]);
        assert!(after.iter().all(|m| m["role"] != "tool"));
    }

    #[test]
    fn one_turn_cannot_be_folded() {
        let mut raw = vec![json!({ "role": "system", "content": "s" })];
        raw.extend(topic("login"));
        assert_eq!(plan(&raw, 0, 40), None);
    }

    #[test]
    fn nothing_is_left_to_fold_once_only_the_last_turn_remains() {
        let raw = long();
        let first = plan(&raw, 0, 40).unwrap();
        let second = plan(&raw, first, 40).unwrap();
        assert_eq!(plan(&raw, first + second, 40), None);
    }

    #[test]
    fn an_earlier_full_summary_is_part_of_the_head() {
        let mut raw = vec![
            json!({ "role": "system", "content": "s" }),
            user(&format!("{SUMMARY_PREAMBLE}\n\nwe fixed login")),
        ];
        raw.extend(topic("billing"));
        raw.extend(topic("deploys"));
        assert_eq!(head_len(&raw), 2);
        let covers = plan(&raw, 0, 40).unwrap();
        let after = apply(&raw, &[section("billing", covers)]);
        assert_eq!(after[..2], raw[..2]);
    }

    #[test]
    fn sections_before_the_last_full_summary_are_dropped() {
        let entries = vec![
            Entry::Section { text: "old".into(), covers: 3, topic: "a".into() },
            Entry::Summary { text: "all of it".into() },
            Entry::Section { text: "new".into(), covers: 5, topic: "b".into() },
        ];
        assert_eq!(sections(&entries), vec![section("new", 5)]);
    }

    #[test]
    fn the_topic_is_read_from_the_first_line() {
        assert_eq!(
            split_topic("Topic: login fixes\n\nWe fixed it."),
            ("login fixes".to_string(), "We fixed it.".to_string())
        );
        assert_eq!(
            split_topic("We fixed it."),
            (UNNAMED_TOPIC.to_string(), "We fixed it.".to_string())
        );
    }
}
