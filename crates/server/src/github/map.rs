use agentbench_core::IssueState;
use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

use crate::store::MirrorIssue;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GhIssue {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub open: bool,
    pub state_reason: Option<String>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub author: String,
    pub url: String,
    pub updated_at: i64,
    pub parent: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mapped {
    Keep(MirrorIssue),
    Remove,
}

impl PartialEq for MirrorIssue {
    fn eq(&self, o: &Self) -> bool {
        self.number == o.number
            && self.title == o.title
            && self.body == o.body
            && self.state == o.state
            && self.priority == o.priority
            && self.labels == o.labels
            && self.author == o.author
            && self.url == o.url
            && self.updated_at == o.updated_at
    }
}

pub fn parse_time(s: &str) -> i64 {
    let b = s.as_bytes();
    if b.len() < 19 {
        return 0;
    }
    let num = |from: usize, to: usize| s.get(from..to).and_then(|p| p.parse::<i64>().ok());
    let (Some(y), Some(m), Some(d), Some(h), Some(mi), Some(se)) =
        (num(0, 4), num(5, 7), num(8, 10), num(11, 13), num(14, 16), num(17, 19))
    else {
        return 0;
    };
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days * 86_400 + h * 3600 + mi * 60 + se) * 1000
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn login(v: &Value, key: &str) -> String {
    v.get(key).and_then(|u| u.get("login")).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn trailing_number(url: &str) -> Option<i64> {
    url.rsplit('/').next().and_then(|n| n.parse().ok())
}

pub fn gh_issue(v: &Value) -> Option<GhIssue> {
    let labels = v
        .get("labels")?
        .as_array()?
        .iter()
        .filter_map(|l| l.as_str().map(str::to_string).or_else(|| l.get("name").and_then(Value::as_str).map(str::to_string)))
        .collect();
    let assignees = v
        .get("assignees")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|u| u.get("login").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default();
    Some(GhIssue {
        number: v.get("number")?.as_i64()?,
        title: text(v, "title"),
        body: text(v, "body"),
        open: v.get("state").and_then(Value::as_str) != Some("closed"),
        state_reason: v.get("state_reason").and_then(Value::as_str).map(str::to_string),
        labels,
        assignees,
        author: login(v, "user"),
        url: text(v, "html_url"),
        updated_at: parse_time(&text(v, "updated_at")),
        parent: v.get("parent_issue_url").and_then(Value::as_str).and_then(trailing_number),
    })
}

fn has_label(labels: &[String], names: &[&str]) -> bool {
    labels.iter().any(|l| names.iter().any(|n| l.eq_ignore_ascii_case(n)))
}

fn priority(labels: &[String]) -> i64 {
    [("priority:high", 3), ("priority:medium", 2), ("priority:low", 1)]
        .iter()
        .find(|(n, _)| has_label(labels, &[n]))
        .map_or(0, |(_, p)| *p)
}

pub fn map_issue(i: &GhIssue, has_open_pr: bool, bot: Option<&str>) -> Mapped {
    let state = if !i.open {
        let dropped = matches!(i.state_reason.as_deref(), Some("not_planned" | "duplicate"))
            || has_label(&i.labels, &["duplicate", "wontfix"]);
        if dropped {
            return Mapped::Remove;
        }
        IssueState::Done
    } else if has_open_pr {
        IssueState::Review
    } else if has_label(&i.labels, &["in progress", "in-progress"])
        || bot.is_some_and(|b| i.assignees.iter().any(|a| a.eq_ignore_ascii_case(b)))
    {
        IssueState::Doing
    } else if has_label(&i.labels, &["ready", "agent:todo"]) {
        IssueState::Todo
    } else {
        IssueState::Backlog
    };
    Mapped::Keep(MirrorIssue {
        number: i.number,
        title: i.title.clone(),
        body: i.body.clone(),
        state,
        priority: priority(&i.labels),
        labels: i.labels.clone(),
        author: i.author.clone(),
        url: i.url.clone(),
        updated_at: i.updated_at,
    })
}

fn dependency_line() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?im)^\s*(?:depends on|blocked by)\b(.*)$").expect("dependency regex"))
}

fn closing_ref() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s*:?\s+#(\d+)").expect("closing regex")
    })
}

fn branch_ref() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"^(?:[A-Za-z]+/)?(?:issue-)?(\d+)(?:[-_/]|$)").expect("branch regex"))
}

fn hash_numbers(line: &str) -> Vec<i64> {
    static R: OnceLock<Regex> = OnceLock::new();
    let r = R.get_or_init(|| Regex::new(r"#(\d+)").expect("number regex"));
    r.captures_iter(line).filter_map(|c| c[1].parse().ok()).collect()
}

pub fn dependencies(body: &str) -> Vec<i64> {
    let mut found = Vec::new();
    for c in dependency_line().captures_iter(body) {
        for n in hash_numbers(&c[1]) {
            if !found.contains(&n) {
                found.push(n);
            }
        }
    }
    found
}

pub fn linked_issues(pr_body: &str, head_ref: &str) -> Vec<i64> {
    let mut found: Vec<i64> =
        closing_ref().captures_iter(pr_body).filter_map(|c| c[1].parse().ok()).collect();
    if let Some(n) = branch_ref().captures(head_ref).and_then(|c| c[1].parse().ok())
        && !found.contains(&n)
    {
        found.push(n);
    }
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(open: bool, reason: Option<&str>, labels: &[&str], assignees: &[&str]) -> GhIssue {
        GhIssue {
            number: 5,
            title: "t".into(),
            open,
            state_reason: reason.map(str::to_string),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            assignees: assignees.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        }
    }

    fn state_of(m: Mapped) -> Option<(IssueState, i64)> {
        match m {
            Mapped::Keep(k) => Some((k.state, k.priority)),
            Mapped::Remove => None,
        }
    }

    #[test]
    fn issues_map_to_states_by_label_assignee_pr_and_closure() {
        use IssueState::*;
        let cases: Vec<(GhIssue, bool, Option<&str>, Option<(IssueState, i64)>)> = vec![
            (issue(true, None, &[], &[]), false, None, Some((Backlog, 0))),
            (issue(true, None, &["bug", "help wanted"], &[]), false, None, Some((Backlog, 0))),
            (issue(true, None, &["ready"], &[]), false, None, Some((Todo, 0))),
            (issue(true, None, &["Agent:Todo"], &[]), false, None, Some((Todo, 0))),
            (issue(true, None, &["in progress"], &[]), false, None, Some((Doing, 0))),
            (issue(true, None, &["ready", "in-progress"], &[]), false, None, Some((Doing, 0))),
            (issue(true, None, &[], &["agentbot"]), false, Some("agentbot"), Some((Doing, 0))),
            (issue(true, None, &[], &["someone"]), false, Some("agentbot"), Some((Backlog, 0))),
            (issue(true, None, &["ready"], &[]), true, None, Some((Review, 0))),
            (issue(true, None, &["in progress"], &["agentbot"]), true, Some("agentbot"), Some((Review, 0))),
            (issue(true, None, &["priority:high"], &[]), false, None, Some((Backlog, 3))),
            (issue(true, None, &["priority:medium", "ready"], &[]), false, None, Some((Todo, 2))),
            (issue(true, None, &["priority:low"], &[]), false, None, Some((Backlog, 1))),
            (issue(false, Some("completed"), &["ready"], &[]), false, None, Some((Done, 0))),
            (issue(false, None, &[], &[]), false, None, Some((Done, 0))),
            (issue(false, Some("not_planned"), &[], &[]), false, None, None),
            (issue(false, Some("duplicate"), &[], &[]), false, None, None),
            (issue(false, Some("completed"), &["duplicate"], &[]), false, None, None),
        ];
        for (i, pr, bot, want) in cases {
            assert_eq!(state_of(map_issue(&i, pr, bot)), want, "{i:?} pr={pr} bot={bot:?}");
        }
    }

    #[test]
    fn labels_are_kept_with_their_github_names() {
        let i = issue(true, None, &["Bug", "priority:high", "good first issue"], &[]);
        let Mapped::Keep(k) = map_issue(&i, false, None) else { panic!() };
        assert_eq!(k.labels, ["Bug", "priority:high", "good first issue"]);
    }

    #[test]
    fn dependencies_come_from_depends_on_and_blocked_by_lines() {
        let cases = [
            ("", vec![]),
            ("Depends on #4", vec![4]),
            ("intro\n\nblocked by #7 and #8\nmore text #9", vec![7, 8]),
            ("  DEPENDS ON #1, #2\nBlocked by #2", vec![1, 2]),
            ("this depends on #3 but is inline", vec![]),
            ("depends on nothing", vec![]),
        ];
        for (body, want) in cases {
            assert_eq!(dependencies(body), want, "{body:?}");
        }
    }

    #[test]
    fn pull_requests_link_by_closing_keyword_and_branch_name() {
        let cases = [
            ("Closes #12", "feature", vec![12]),
            ("fixes #3, resolves #4", "x", vec![3, 4]),
            ("Resolved: #9", "x", vec![9]),
            ("see #5", "x", vec![]),
            ("", "issue-42-add-thing", vec![42]),
            ("", "42-add-thing", vec![42]),
            ("", "fix/17-crash", vec![17]),
            ("closes #42", "issue-42-add-thing", vec![42]),
            ("", "main", vec![]),
            ("", "v2-release", vec![]),
        ];
        for (body, branch, want) in cases {
            assert_eq!(linked_issues(body, branch), want, "{body:?} {branch:?}");
        }
    }

    #[test]
    fn github_times_parse_to_epoch_milliseconds() {
        assert_eq!(parse_time("1970-01-01T00:00:00Z"), 0);
        assert_eq!(parse_time("2024-03-01T12:30:15Z"), 1_709_296_215_000);
        assert_eq!(parse_time("junk"), 0);
    }

    #[test]
    fn a_github_issue_is_read_from_the_api_shape() {
        let v = serde_json::json!({
            "number": 8, "title": "T", "body": null, "state": "open",
            "labels": [{"name": "ready"}], "assignees": [{"login": "bot"}],
            "user": {"login": "alice"}, "html_url": "https://github.com/o/r/issues/8",
            "updated_at": "2024-03-01T12:30:15Z",
            "parent_issue_url": "https://api.github.com/repos/o/r/issues/2"
        });
        let i = gh_issue(&v).unwrap();
        assert_eq!((i.number, i.body.as_str(), i.open, i.parent), (8, "", true, Some(2)));
        assert_eq!(i.labels, ["ready"]);
        assert_eq!(i.assignees, ["bot"]);
        assert_eq!(i.author, "alice");
    }
}
