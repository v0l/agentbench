use crate::autopilot::{self, Autopilot, clip, number};
use crate::runner::Origin;
use crate::store::{Store, StoreError};
use agentbench_core::{Id, Issue, IssueState, Project, Vision, span};

const BACKLOG_SHOWN: usize = 40;
const DONE_SHOWN: usize = 10;
const BODY_CHARS: usize = 200;

pub(crate) fn groom_on(store: &Store) -> bool {
    store.setting(autopilot::GROOM).ok().flatten().as_deref() != Some("0")
}

pub(crate) fn groom_hours(store: &Store) -> u64 {
    number(store, autopilot::GROOM_HOURS, autopilot::DEFAULT_GROOM_HOURS)
}

fn at_key(project: Id) -> String {
    format!("groom_at_{project}")
}

fn seen_key(project: Id) -> String {
    format!("groom_seen_{project}")
}

fn signature(backlog: &[&Issue]) -> String {
    let newest = backlog.iter().map(|i| i.updated_at).max().unwrap_or(0);
    format!("{}:{newest}", backlog.len())
}

fn backlog_of(issues: &[Issue], project: Id) -> Vec<&Issue> {
    let mut backlog: Vec<&Issue> =
        issues.iter().filter(|i| i.project_id == project && i.state == IssueState::Backlog).collect();
    backlog.sort_by_key(|i| (i.created_at, i.id));
    backlog
}

fn remember(store: &Store, project: Id, now: i64, backlog: &[&Issue]) {
    let _ = store.set_setting(&at_key(project), &now.to_string());
    let _ = store.set_setting(&seen_key(project), &signature(backlog));
}

pub fn done(store: &Store, project: &Project, summary: &str, now: i64) -> Result<(), StoreError> {
    let issues = store.issues()?;
    remember(store, project.id, now, &backlog_of(&issues, project.id));
    let line = match summary.trim() {
        "" => format!("groomed {}", project.name),
        s => format!("groomed {}: {}", project.name, clip(s, 300)),
    };
    store.log_autopilot(Some(project.id), None, &line)
}

pub(crate) fn issue_line(i: &Issue, now: i64) -> String {
    let mut line = if i.trust.is_untrusted() {
        format!("- #{} {}", i.id, agentbench_core::untrusted_block(i.external_author.as_deref().unwrap_or("unknown"), &clip(i.title.trim(), 200)))
    } else {
        format!("- #{} {}", i.id, i.title.trim())
    };
    if !i.labels.is_empty() {
        line.push_str(&format!(" [{}]", i.labels.join(", ")));
    }
    line.push_str(&format!(" priority {}", i.priority));
    line.push_str(&format!(", goal {}", i.goal.as_deref().unwrap_or("none")));
    line.push_str(&format!(", age {}", span(now - i.created_at)));
    if !i.depends_on.is_empty() {
        let ids: Vec<String> = i.depends_on.iter().map(|d| format!("#{d}")).collect();
        line.push_str(&format!(", depends_on {}", ids.join(" ")));
    }
    let body = i.body.split_whitespace().collect::<Vec<_>>().join(" ");
    if !body.is_empty() {
        let shown = clip(&body, BODY_CHARS);
        let shown = if i.trust.is_untrusted() {
            agentbench_core::untrusted_block(i.external_author.as_deref().unwrap_or("unknown"), &shown)
        } else {
            shown
        };
        line.push_str(&format!("\n    {shown}"));
    }
    line
}

pub(crate) fn message(
    project: &Project,
    vision: Option<&Vision>,
    backlog: &[&Issue],
    finished: &[&Issue],
    now: i64,
) -> String {
    let mut out = format!(
        "The todo queue of {} is empty, nothing is running on it, and its backlog has {} issue{}. Groom the backlog.\n\n",
        project.name,
        backlog.len(),
        if backlog.len() == 1 { "" } else { "s" }
    );
    match vision {
        Some(v) => {
            out.push_str(&format!("Vision: {}\nActive goals:\n", v.body.summary.trim()));
            let mut any = false;
            for g in v.active_goals() {
                any = true;
                out.push_str(&format!("- {}: {}\n", g.id, g.text.trim()));
            }
            if !any {
                out.push_str("- (none)\n");
            }
        }
        None => out.push_str(
            "This project has no vision. Ask the user for one (tell them in this chat, then use set_vision for what they confirm). Do not promote anything to todo until it exists.\n",
        ),
    }
    out.push_str(&format!("\nBacklog, oldest first ({} shown of {}):\n", backlog.len().min(BACKLOG_SHOWN), backlog.len()));
    for i in backlog.iter().take(BACKLOG_SHOWN) {
        out.push_str(&issue_line(i, now));
        out.push('\n');
    }
    if backlog.len() > BACKLOG_SHOWN {
        out.push_str(&format!("...and {} more, not shown. Groom these first; the rest follow on the next groom.\n", backlog.len() - BACKLOG_SHOWN));
    }
    out.push_str("\nRecently done, for context:\n");
    if finished.is_empty() {
        out.push_str("- (none)\n");
    }
    for i in finished {
        out.push_str(&format!("- #{} {}\n", i.id, i.title.trim()));
    }
    out.push_str(&format!(
        "\nGrooming means:\n\
         - Promote to todo (update_issue state todo) only issues that serve an active goal and are still wanted. Set the goal and priority first.\n\
         - Merge duplicates: comment on the one you keep, close the other as done with a comment saying it is a duplicate.\n\
         - Close stale or superseded issues with a comment giving the reason.\n\
         - Fix labels and depends_on where they are wrong.\n\
         - Never promote an off-vision or goalless issue without the user; ask them instead.\n\
         When finished, call the groom_done tool with project={} and a one-line summary. Leave anything you are unsure about in the backlog.",
        project.name
    ));
    out
}

impl Autopilot {
    pub(crate) fn groom(&self, now: i64) {
        let store = self.store();
        if !groom_on(store) {
            return;
        }
        let Some(sup) = self.runner.supervisor_session() else { return };
        let issues = store.issues().unwrap_or_default();
        let workers = store.workers().unwrap_or_default();
        let window = groom_hours(store) as i64 * 3_600_000;
        for p in store.projects().unwrap_or_default() {
            if p.builtin() || !autopilot::enabled_for(store, p.id) {
                continue;
            }
            if issues.iter().any(|i| i.project_id == p.id && i.state == IssueState::Todo)
                || workers.iter().any(|w| w.project == p.id && autopilot::is_active(&w.state))
            {
                continue;
            }
            let backlog = backlog_of(&issues, p.id);
            if backlog.is_empty() {
                continue;
            }
            let last: i64 = number(store, &at_key(p.id), 0);
            let unchanged = store.setting(&seen_key(p.id)).ok().flatten().as_deref() == Some(signature(&backlog).as_str());
            if last > 0 && (now - last < window || unchanged) {
                continue;
            }
            let mut finished: Vec<&Issue> =
                issues.iter().filter(|i| i.project_id == p.id && i.state == IssueState::Done).collect();
            finished.sort_by_key(|i| std::cmp::Reverse(i.updated_at));
            finished.truncate(DONE_SHOWN);
            let vision = store.vision(p.id).ok().flatten();
            let text = message(&p, vision.as_ref(), &backlog, &finished, now);
            let origin = Origin::named(format!("groom {}", p.name), None);
            match self.runner.tell(&sup, &text, origin) {
                Ok(()) => {
                    remember(store, p.id, now, &backlog);
                    self.log(Some(p.id), None, &format!("groom requested for {}", p.name));
                }
                Err(e) => self.log(Some(p.id), None, &format!("could not ask the supervisor to groom {}: {}", p.name, clip(&e.0, 160))),
            }
        }
    }
}
