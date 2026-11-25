use crate::{Id, IssueState, SessionId};
use serde::{Deserialize, Serialize};

pub const LOG_CLIP: usize = 600;
pub const LOG_DAYS_SETTING: &str = "supervisor_log_days";
pub const LOG_DAYS_DEFAULT: i64 = 90;
pub const LOG_PAGE: usize = 50;
pub const LOG_PAGE_MAX: usize = 500;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LogItem {
    pub id: Id,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub asker: String,
    #[serde(default)]
    pub asker_title: String,
    #[serde(default)]
    pub session: Option<SessionId>,
    #[serde(default)]
    pub supervisor: Option<SessionId>,
    #[serde(default)]
    pub project_id: Option<Id>,
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub issue_id: Option<Id>,
    #[serde(default)]
    pub issue_state: Option<IssueState>,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub arrived_at: i64,
    #[serde(default)]
    pub needs_answer: bool,
    #[serde(default)]
    pub closed_at: Option<i64>,
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub reply: String,
    #[serde(default)]
    pub merge: String,
    #[serde(default)]
    pub clipped: bool,
}

impl LogItem {
    pub fn open(&self) -> bool {
        self.closed_at.is_none()
    }

    pub fn kind_label(&self) -> &str {
        kind_label(&self.kind)
    }

    pub fn who(&self) -> String {
        match self.asker.as_str() {
            "user" => "you".to_string(),
            "autopilot" => "autopilot".to_string(),
            "" => self.kind_label().to_string(),
            id => {
                let short: String = id.chars().take(8).collect();
                if self.asker_title.trim().is_empty() {
                    format!("session {short}")
                } else {
                    format!("{} ({short})", self.asker_title.trim())
                }
            }
        }
    }

    pub fn answer(&self) -> String {
        if self.open() {
            return String::new();
        }
        let mut parts: Vec<String> = Vec::new();
        let action = action_label(&self.action, &self.detail);
        if !action.is_empty() {
            parts.push(action);
        }
        let said = if self.outcome.trim().is_empty() { self.reply.trim() } else { self.outcome.trim() };
        if !said.is_empty() {
            parts.push(said.to_string());
        }
        if !self.merge.trim().is_empty() {
            parts.push(self.merge.trim().to_string());
        }
        if let (Some(i), Some(state)) = (self.issue_id, self.issue_state) {
            parts.push(format!("#{i} is {}", state.as_str()));
        }
        parts.join(". ")
    }

    pub fn clip(mut self) -> LogItem {
        let (q, a) = clip_chars(&self.question, LOG_CLIP);
        let (o, b) = clip_chars(&self.outcome, LOG_CLIP);
        let (r, c) = clip_chars(&self.reply, LOG_CLIP);
        self.question = q;
        self.outcome = o;
        self.reply = r;
        self.clipped = a || b || c;
        self
    }
}

pub fn kind_label(kind: &str) -> &str {
    match kind {
        "user" => "user message",
        "question" => "worker question",
        "judge" => "judge request",
        "check_in" => "check in",
        "watch" => "watch",
        "process" => "process report",
        "groom" => "groom",
        "judge_issues" => "judge issues",
        "main_dirty" => "dirty checkout",
        "session" => "session message",
        "autopilot" => "autopilot",
        "watchdog" => "watchdog",
        "add_project" => "add project",
        other => other,
    }
}

pub fn action_label(action: &str, detail: &str) -> String {
    let base = match action {
        "" => return String::new(),
        "replied" => "replied",
        "answer_session" => "answered",
        "escalate_question" => "put to the user",
        "judge_session" => "judged",
        "check_in" => "checked in",
        "file_issue" => "filed",
        "start_session" => "started session",
        "stale" => "left open, gave up",
        "superseded" => "superseded",
        "settled" => "settled",
        "no_action_needed" => "no action needed",
        "groom_done" => "groomed",
        "judge_issue" => "judged the issues",
        "main_checkout" => "fixed the checkout",
        "read_session" | "unwatch_session" => "read",
        other => other,
    };
    if detail.trim().is_empty() { base.to_string() } else { format!("{base} {}", detail.trim()) }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SupervisorLog {
    #[serde(default)]
    pub items: Vec<LogItem>,
    #[serde(default)]
    pub open: usize,
    #[serde(default)]
    pub next_before: Option<Id>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_answer: Option<bool>,
}

impl LogQuery {
    pub fn to_query(&self) -> String {
        let mut parts = Vec::new();
        let mut add = |k: &str, v: String| parts.push(format!("{k}={}", encode(&v)));
        if let Some(v) = &self.kind {
            add("kind", v.clone());
        }
        if let Some(v) = &self.project {
            add("project", v.clone());
        }
        if let Some(v) = &self.state {
            add("state", v.clone());
        }
        if let Some(v) = self.since {
            add("since", v.to_string());
        }
        if let Some(v) = self.until {
            add("until", v.to_string());
        }
        if let Some(v) = &self.q {
            add("q", v.clone());
        }
        if let Some(v) = self.limit {
            add("limit", v.to_string());
        }
        if let Some(v) = self.before {
            add("before", v.to_string());
        }
        if let Some(v) = self.needs_answer {
            add("needs_answer", v.to_string());
        }
        parts.join("&")
    }
}

fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSettings {
    #[serde(default)]
    pub days: i64,
}

pub fn clip_chars(text: &str, n: usize) -> (String, bool) {
    match text.char_indices().nth(n) {
        Some((at, _)) => (format!("{}...", &text[..at]), true),
        None => (text.to_string(), false),
    }
}

pub fn first_sentence(text: &str) -> String {
    let text = text.trim();
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let mut end = line.len();
    let bytes: Vec<(usize, char)> = line.char_indices().collect();
    for (n, (at, c)) in bytes.iter().enumerate() {
        if matches!(c, '.' | '!' | '?') && bytes.get(n + 1).is_none_or(|(_, next)| next.is_whitespace()) {
            end = at + c.len_utf8();
            break;
        }
    }
    clip_chars(&line[..end], LOG_CLIP).0
}

pub fn parse_since(text: &str, now: i64) -> Option<i64> {
    let text = text.trim();
    if let Ok(ms) = text.parse::<i64>() {
        return Some(ms);
    }
    let (num, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    let ms = match unit {
        "m" | "min" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 7 * 86_400_000,
        _ => return None,
    };
    Some(now - n * ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_sentence_stops_at_the_first_full_stop() {
        assert_eq!(first_sentence("Merged it. Then I closed the session."), "Merged it.");
        assert_eq!(first_sentence("\n\nversion 1.2 ships today\nmore"), "version 1.2 ships today");
        assert_eq!(first_sentence("Done!"), "Done!");
    }

    #[test]
    fn since_reads_relative_spans_and_plain_times() {
        assert_eq!(parse_since("1d", 100_000_000), Some(100_000_000 - 86_400_000));
        assert_eq!(parse_since("2h", 10_000_000), Some(10_000_000 - 7_200_000));
        assert_eq!(parse_since("1234", 0), Some(1234));
        assert_eq!(parse_since("soon", 0), None);
    }

    #[test]
    fn a_closed_judge_item_reads_as_verdict_reason_merge_and_issue_state() {
        let item = LogItem {
            kind: "judge".into(),
            closed_at: Some(5),
            action: "judge_session".into(),
            detail: "concluded_pass".into(),
            outcome: "tests ran".into(),
            merge: "merged issue-3 into the main branch".into(),
            issue_id: Some(3),
            issue_state: Some(IssueState::Done),
            ..Default::default()
        };
        assert_eq!(item.answer(), "judged concluded_pass. tests ran. merged issue-3 into the main branch. #3 is done");
    }

    #[test]
    fn a_long_question_is_clipped_and_marked() {
        let item = LogItem { question: "x".repeat(700), ..Default::default() }.clip();
        assert!(item.clipped);
        assert_eq!(item.question.chars().count(), LOG_CLIP + 3);
    }

    #[test]
    fn the_query_string_encodes_text() {
        let q = LogQuery { q: Some("a b&c".into()), state: Some("open".into()), ..Default::default() };
        assert_eq!(q.to_query(), "state=open&q=a%20b%26c");
    }
}
