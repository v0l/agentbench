use std::time::Duration;

pub const REMIND_AT: [u32; 2] = [15, 30];
pub const ESCALATE_AT: u32 = 45;
pub const THINKING_LIMIT: Duration = Duration::from_secs(600);

const PLANNING_OR_EDITING: [&str; 8] = [
    "todo_write",
    "todowrite",
    "edit_file",
    "write_file",
    "replace_in_files",
    "file_issue",
    "edit",
    "write",
];

pub fn reads_only(tool: &str) -> bool {
    !PLANNING_OR_EDITING.contains(&tool.to_ascii_lowercase().as_str())
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
        for tool in ["todo_write", "edit_file", "write_file", "replace_in_files", "file_issue", "TodoWrite", "Edit"] {
            let mut w = Watchdog::default();
            feed(&mut w, "read_file", 10);
            assert!(feed(&mut w, tool, 1).is_empty());
            assert!(feed(&mut w, "read_file", 60).is_empty(), "{tool}");
        }
    }

    #[test]
    fn bash_and_other_exploring_calls_count_as_reading() {
        let mut w = Watchdog::default();
        assert_eq!(feed(&mut w, "bash", 15), [Nudge::Remind(15)]);
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
