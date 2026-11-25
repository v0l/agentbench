use crate::store::Store;
use agentbench_core::supervisor_log::{kind_label, parse_since};
use agentbench_core::{Id, LogItem, LogQuery, LogSettings, SupervisorLog};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct LogParams {
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub before: Option<Id>,
    #[serde(default)]
    pub needs_answer: Option<bool>,
}

impl LogParams {
    pub fn query(&self, now: i64) -> Result<LogQuery, String> {
        let time = |v: &Option<String>, name: &str| -> Result<Option<i64>, String> {
            match v.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                None => Ok(None),
                Some(t) => parse_since(t, now).map(Some).ok_or_else(|| format!("`{name}` is a time in ms or a span like 30m, 2h, 1d, 1w")),
            }
        };
        if let Some(s) = self.state.as_deref().filter(|s| !matches!(*s, "open" | "closed" | "all" | "")) {
            return Err(format!("`state` is open, closed or all, not {s}"));
        }
        Ok(LogQuery {
            kind: self.kind.clone(),
            project: self.project.clone(),
            state: self.state.clone(),
            since: time(&self.since, "since")?,
            until: time(&self.until, "until")?,
            q: self.q.clone(),
            limit: self.limit,
            before: self.before,
            needs_answer: self.needs_answer,
        })
    }
}

pub fn list(store: &Store, q: &LogQuery) -> crate::store::Result<SupervisorLog> {
    let mut log = store.log_items(q)?;
    log.items = log.items.into_iter().map(LogItem::clip).collect();
    Ok(log)
}

pub fn settings(store: &Store) -> LogSettings {
    LogSettings { days: crate::ledger::log_days(store) }
}

pub fn save(store: &Store, s: &LogSettings) -> Result<LogSettings, String> {
    if s.days < 0 {
        return Err("`days` is 0 (keep everything) or more".into());
    }
    store.set_setting(agentbench_core::supervisor_log::LOG_DAYS_SETTING, &s.days.to_string()).map_err(|e| e.0)?;
    Ok(settings(store))
}

fn stamp(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02} {}", agentbench_core::clock_hm(ms))
}

fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

fn one_line(text: &str, n: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    agentbench_core::supervisor_log::clip_chars(&flat, n).0
}

fn whereabouts(i: &LogItem) -> String {
    let mut out = String::new();
    if !i.project.is_empty() {
        out.push_str(&format!(" in {}", i.project));
    }
    if let Some(n) = i.issue_id {
        out.push_str(&format!(" #{n}"));
    }
    out
}

pub fn line(i: &LogItem, now: i64) -> String {
    let answer = if i.open() {
        format!("OPEN for {}", agentbench_core::span(now - i.arrived_at))
    } else {
        one_line(&i.answer(), 80)
    };
    format!(
        "{:>5}  {}  {:<15}  {}{}: {}  ->  {}",
        i.id,
        stamp(i.arrived_at),
        kind_label(&i.kind),
        i.who(),
        whereabouts(i),
        one_line(&i.question, 70),
        answer
    )
}

pub fn full(i: &LogItem, now: i64) -> String {
    let mut out = format!("#{} {} from {}{}\narrived {}\n", i.id, kind_label(&i.kind), i.who(), whereabouts(i), stamp(i.arrived_at));
    out.push_str(&format!("\nquestion:\n{}\n", i.question.trim()));
    match i.closed_at {
        None => out.push_str(&format!("\nstill open, for {}\n", agentbench_core::span(now - i.arrived_at))),
        Some(at) => {
            out.push_str(&format!("\nclosed {}: {}\n", stamp(at), agentbench_core::supervisor_log::action_label(&i.action, &i.detail)));
            if !i.outcome.trim().is_empty() {
                out.push_str(&format!("\noutcome:\n{}\n", i.outcome.trim()));
            }
            if !i.reply.trim().is_empty() && i.reply.trim() != i.outcome.trim() {
                out.push_str(&format!("\nreply:\n{}\n", i.reply.trim()));
            }
            if !i.merge.trim().is_empty() {
                out.push_str(&format!("\nmerge: {}\n", i.merge.trim()));
            }
            if let (Some(n), Some(s)) = (i.issue_id, i.issue_state) {
                out.push_str(&format!("issue #{n} is {}\n", s.as_str()));
            }
        }
    }
    out
}

pub fn markdown(store: &Store, q: &LogQuery, now: i64) -> crate::store::Result<String> {
    let mut q = q.clone();
    q.limit = Some(agentbench_core::supervisor_log::LOG_PAGE_MAX);
    let mut items = Vec::new();
    loop {
        let page = store.log_items(&q)?;
        items.extend(page.items);
        match page.next_before {
            Some(b) => q.before = Some(b),
            None => break,
        }
    }
    let range = match (q.since, q.until) {
        (Some(a), Some(b)) => format!("{} to {}", stamp(a), stamp(b)),
        (Some(a), None) => format!("since {}", stamp(a)),
        (None, Some(b)) => format!("until {}", stamp(b)),
        (None, None) => "everything kept".into(),
    };
    let mut out = format!("# Supervisor questions log\n\n{range}, {} items\n", items.len());
    for i in items.iter().rev() {
        out.push_str(&format!("\n## {} {} from {}{}\n\n", stamp(i.arrived_at), kind_label(&i.kind), i.who(), whereabouts(i)));
        for l in i.question.trim().lines() {
            out.push_str(&format!("> {l}\n"));
        }
        out.push('\n');
        if i.open() {
            out.push_str(&format!("Still open, for {}.\n", agentbench_core::span(now - i.arrived_at)));
        } else {
            out.push_str(&format!("{}\n", i.answer()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Closing, NewLogItem};

    fn seeded() -> Store {
        let s = Store::memory().unwrap();
        let a = s.add_log(&NewLogItem { kind: "user", asker: "user", question: "is it shipped", needs_answer: true, ..Default::default() }).unwrap();
        s.close_log(a, &Closing::new("replied").outcome("Yes, at noon.")).unwrap();
        s.add_log(&NewLogItem { kind: "judge", asker: "autopilot", question: "judge the worker", needs_answer: true, ..Default::default() }).unwrap();
        s
    }

    #[test]
    fn the_days_are_ninety_by_default_and_saved() {
        let s = Store::memory().unwrap();
        assert_eq!(settings(&s).days, 90);
        assert_eq!(save(&s, &LogSettings { days: 30 }).unwrap().days, 30);
        assert!(save(&s, &LogSettings { days: -1 }).is_err());
    }

    #[test]
    fn a_line_holds_the_kind_who_question_and_answer() {
        let s = seeded();
        let items = s.log_items(&Default::default()).unwrap().items;
        let now = items[0].arrived_at + 120_000;
        let open = line(&items[0], now);
        assert!(open.contains("judge request") && open.contains("judge the worker") && open.contains("OPEN for 2m"), "{open}");
        let closed = line(&items[1], now);
        assert!(closed.contains("user message") && closed.contains("you: is it shipped") && closed.contains("replied. Yes, at noon."), "{closed}");
    }

    #[test]
    fn the_markdown_export_lists_oldest_first_with_answers() {
        let s = seeded();
        let md = markdown(&s, &Default::default(), crate::store::now()).unwrap();
        assert!(md.starts_with("# Supervisor questions log\n\neverything kept, 2 items"), "{md}");
        let shipped = md.find("> is it shipped").unwrap();
        assert!(shipped < md.find("> judge the worker").unwrap(), "{md}");
        assert!(md.contains("replied. Yes, at noon."), "{md}");
        assert!(md.contains("Still open"), "{md}");
    }

    #[test]
    fn a_date_prints_as_a_calendar_day() {
        assert_eq!(stamp(0), "1970-01-01 00:00 UTC");
        assert_eq!(stamp(1_764_158_400_000), "2025-11-26 12:00 UTC");
    }

    #[test]
    fn params_read_spans_and_refuse_a_bad_state() {
        let p = LogParams { since: Some("1d".into()), ..Default::default() };
        assert_eq!(p.query(86_400_000 * 2).unwrap().since, Some(86_400_000));
        assert!(LogParams { state: Some("maybe".into()), ..Default::default() }.query(0).is_err());
        assert!(LogParams { since: Some("soon".into()), ..Default::default() }.query(0).is_err());
    }
}
