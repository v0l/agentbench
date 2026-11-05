use std::time::Duration;

pub const REMIND_AT: [u32; 2] = [15, 30];
pub const ESCALATE_AT: u32 = 45;
pub const THINKING_LIMIT: Duration = Duration::from_secs(600);

const READ_ONLY: [&str; 17] = [
    "read_file",
    "search_files",
    "list_files",
    "load_skill",
    "recall",
    "list_issues",
    "get_issue",
    "list_sessions",
    "read_session",
    "code_outline",
    "code_find",
    "code_diagnostics",
    "web_search",
    "web_fetch",
    "user_asks",
    "list_worktrees",
    "session_status",
];

pub fn reads_only(tool: &str) -> bool {
    let lower = tool.to_ascii_lowercase();
    READ_ONLY.contains(&lower.as_str()) || matches!(lower.as_str(), "read" | "grep" | "glob" | "ls")
}

#[derive(Clone, Debug, PartialEq)]
pub enum Nudge {
    Remind(u32),
    Escalate(u32),
}

pub fn reminder(reads: u32) -> String {
    format!(
        "You have read {reads} files and made no edit. Write your implementation steps now with todo_write and start the first one."
    )
}

#[derive(Debug, Default)]
pub struct Watchdog {
    reads: u32,
    reminded: usize,
    escalated: bool,
    planned: bool,
    since_reminder: Duration,
}

impl Watchdog {
    pub fn reads(&self) -> u32 {
        self.reads
    }

    pub fn observe(&mut self, tool: &str, elapsed: Duration) -> Option<Nudge> {
        if self.planned {
            return None;
        }
        if !reads_only(tool) {
            self.planned = true;
            return None;
        }
        self.reads += 1;
        if self.reads >= ESCALATE_AT && !self.escalated {
            self.escalated = true;
            return Some(Nudge::Escalate(self.reads));
        }
        if let Some(at) = REMIND_AT.get(self.reminded)
            && self.reads >= *at
        {
            self.reminded += 1;
            self.since_reminder = elapsed;
            return Some(Nudge::Remind(self.reads));
        }
        if self.reminded == 0 && elapsed >= THINKING_LIMIT {
            self.reminded = 1;
            self.since_reminder = elapsed;
            return Some(Nudge::Remind(self.reads));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(w: &mut Watchdog, tool: &str, n: u32) -> Vec<Nudge> {
        (0..n).filter_map(|_| w.observe(tool, Duration::ZERO)).collect()
    }

    #[test]
    fn reading_without_editing_is_reminded_at_15_and_30_then_escalated_at_45() {
        let mut w = Watchdog::default();
        assert!(feed(&mut w, "read_file", 14).is_empty());
        assert_eq!(feed(&mut w, "search_files", 1), [Nudge::Remind(15)]);
        assert!(feed(&mut w, "code_find", 14).is_empty());
        assert_eq!(feed(&mut w, "read_file", 1), [Nudge::Remind(30)]);
        assert!(feed(&mut w, "read_file", 14).is_empty());
        assert_eq!(feed(&mut w, "read_file", 1), [Nudge::Escalate(45)]);
        assert!(feed(&mut w, "read_file", 20).is_empty());
    }

    #[test]
    fn a_plan_or_an_edit_silences_it() {
        for tool in ["todo_write", "edit_file", "write_file", "file_issue", "bash"] {
            let mut w = Watchdog::default();
            feed(&mut w, "read_file", 10);
            assert!(feed(&mut w, tool, 1).is_empty());
            assert!(feed(&mut w, "read_file", 60).is_empty(), "{tool}");
        }
    }

    #[test]
    fn ten_minutes_in_without_an_edit_reminds_early() {
        let mut w = Watchdog::default();
        assert_eq!(w.observe("read_file", THINKING_LIMIT), Some(Nudge::Remind(1)));
        assert_eq!(w.observe("read_file", THINKING_LIMIT), None);
    }

    #[test]
    fn the_reminder_says_what_to_do() {
        let r = reminder(15);
        assert!(r.contains("read 15 files") && r.contains("todo_write"));
    }
}
