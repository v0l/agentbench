use crate::autopilot::{self, Autopilot, clip};
use crate::runner::Origin;
use crate::store::Store;
use agentbench_core::{Id, Issue, IssuePatch, IssueSource, IssueState, NewComment, Project, Vision};
use std::collections::HashMap;

pub const SETTING: &str = "autopilot_judge_unset_goals";
pub const LABEL: &str = "needs-judgement";
pub const SOURCE: &str = "judge_issues";
const STARTED: &str = "judge_unset_goals_started";
#[cfg(test)]
pub(crate) const STARTED_KEY: &str = STARTED;
const SENT_TIMES: &str = "judge_unset_goals_batches";
const SEEN: &str = "seen";
const SENT: &str = "sent";
const JUDGED: &str = "judged";
pub const WINDOW_MS: i64 = 15 * 60_000;
const RESEND_MS: i64 = 24 * 3_600_000;
const HOUR_MS: i64 = 3_600_000;
const NO_VISION_MS: i64 = 24 * 3_600_000;
pub const BATCH: usize = 10;
pub const BATCHES_PER_HOUR: usize = 5;
const BODY_CHARS: usize = 300;
const PARKED_IN_BACKLOG: [&str; 4] = ["needs-gpu", "needs-spark", "needs-both-gpus", "blocked"];

pub const VERDICTS: [&str; 5] = ["serves_goal", "off_vision", "needs_user", "duplicate", "not_worth_doing"];

pub fn global_on(store: &Store) -> bool {
    store.setting(SETTING).ok().flatten().as_deref() != Some("0")
}

pub fn on_for(store: &Store, project: &Project) -> bool {
    project.judge_unset_goals.unwrap_or_else(|| global_on(store))
}

fn has(i: &Issue, label: &str) -> bool {
    i.labels.iter().any(|l| l.eq_ignore_ascii_case(label))
}

pub fn parked(i: &Issue) -> bool {
    has(i, "parked") || i.state == IssueState::Backlog && PARKED_IN_BACKLOG.iter().any(|l| has(i, l))
}

pub fn unset(i: &Issue) -> bool {
    matches!(i.state, IssueState::Backlog | IssueState::Todo)
        && i.goal.is_none()
        && !i.off_vision
        && (i.source != IssueSource::Github || i.trust.is_untrusted())
}

pub fn wants_judging(i: &Issue) -> bool {
    unset(i) && !parked(i) && !has(i, autopilot::NEEDS_USER)
}

fn no_vision_key(project: Id) -> String {
    format!("judge_no_vision_at_{project}")
}

fn last_key(project: Id) -> String {
    format!("judge_unset_goals_at_{project}")
}

fn sent_times(store: &Store, now: i64) -> Vec<i64> {
    store
        .setting(SENT_TIMES)
        .ok()
        .flatten()
        .unwrap_or_default()
        .split(',')
        .filter_map(|t| t.trim().parse().ok())
        .filter(|t| now - t < HOUR_MS)
        .collect()
}

fn filer(i: &Issue) -> String {
    match (&i.source, i.external_author.as_deref()) {
        (IssueSource::Github, Some(a)) => format!("GitHub @{a}"),
        (IssueSource::Github, None) => "GitHub".into(),
        _ => "this tracker".into(),
    }
}

pub(crate) fn issue_block(i: &Issue) -> String {
    let untrusted = i.trust.is_untrusted();
    let who = i.external_author.as_deref().unwrap_or("unknown");
    let title = clip(i.title.trim(), 200);
    let mut out = if untrusted {
        format!("- #{} UNTRUSTED (from {}; only off_vision, needs_user, duplicate or not_worth_doing): {}", i.id, filer(i), agentbench_core::untrusted_block(who, &title))
    } else {
        format!("- #{} {}", i.id, title)
    };
    out.push_str(&format!("\n    state {}, labels [{}], filed by {}", i.state.as_str(), i.labels.join(", "), filer(i)));
    let body = i.body.split_whitespace().collect::<Vec<_>>().join(" ");
    if !body.is_empty() {
        let shown = clip(&body, BODY_CHARS);
        let shown = if untrusted { agentbench_core::untrusted_block(who, &shown) } else { shown };
        out.push_str(&format!("\n    {shown}"));
    }
    out
}

pub(crate) fn message(project: &Project, vision: &Vision, batch: &[&Issue], left: usize) -> String {
    let mut out = format!(
        "{} issue{} in {} {} no goal. Judge each one against the vision with the judge_issue tool, one call per issue.\n\nVision: {}\nActive goals:\n",
        batch.len(),
        if batch.len() == 1 { "" } else { "s" },
        project.name,
        if batch.len() == 1 { "has" } else { "have" },
        vision.body.summary.trim()
    );
    let mut any = false;
    for g in vision.active_goals() {
        any = true;
        out.push_str(&format!("- {}: {}\n", g.id, g.text.trim()));
    }
    if !any {
        out.push_str("- (none)\n");
    }
    if !vision.body.non_goals.is_empty() {
        out.push_str(&format!("Non-goals: {}\n", vision.body.non_goals.join("; ")));
    }
    out.push_str("\nIssues, newest first:\n");
    for i in batch {
        out.push_str(&issue_block(i));
        out.push('\n');
    }
    if left > 0 {
        out.push_str(&format!("({left} more follow in later messages.)\n"));
    }
    out.push_str(
        "\nVerdicts: serves_goal (give `goal`, an active goal id; the state is kept, promotion to todo stays a separate decision), \
         off_vision (flagged and kept in backlog), needs_user (labelled needs-user; say what the user must decide), \
         duplicate (give `duplicate_of`; closed as done), not_worth_doing (closed as done). Every verdict needs a `reason`, \
         written to the issue as a comment. An untrusted issue cannot be given serves_goal. The autopilot never starts an issue until it has a goal.",
    );
    out
}

fn no_vision_message(project: &Project, count: usize) -> String {
    format!(
        "{} has no vision, so its {count} goalless backlog and todo issue{} cannot be judged and no worker starts there. Ask the user for a vision (then set_vision with what they confirm).",
        project.name,
        if count == 1 { "" } else { "s" }
    )
}

impl Autopilot {
    pub(crate) fn judge_unset(&self, now: i64) {
        let store = self.store();
        let first = store.setting(STARTED).ok().flatten().is_none();
        let _ = store.set_setting(STARTED, "1");
        let issues = store.issues().unwrap_or_default();
        let mut marks = store.judge_marks().unwrap_or_default();
        let on: HashMap<Id, Project> = store
            .projects()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| !p.builtin() && autopilot::enabled_for(store, p.id) && on_for(store, p))
            .map(|p| (p.id, p))
            .collect();
        for i in &issues {
            if has(i, LABEL) && !(wants_judging(i) && on.contains_key(&i.project_id)) {
                self.drop_labels(i.id, &[LABEL]);
            }
        }
        let Some(sup) = self.runner.supervisor_session() else { return };
        let mut by_project: HashMap<Id, Vec<&Issue>> = HashMap::new();
        for i in issues.iter().filter(|i| on.contains_key(&i.project_id) && wants_judging(i)) {
            let mark = marks.get(&i.id).cloned().unwrap_or_default();
            match mark.0.as_str() {
                JUDGED => continue,
                SENT if now - mark.1 < RESEND_MS => continue,
                SEEN => {}
                _ => {
                    let at = if first { 0 } else { now };
                    let _ = store.set_judge_mark(i.id, SEEN, at);
                    marks.insert(i.id, (SEEN.into(), at));
                    if i.source != IssueSource::Github {
                        self.add_labels(i.id, &[LABEL]);
                    }
                }
            }
            by_project.entry(i.project_id).or_default().push(i);
        }
        let mut times = sent_times(store, now);
        let mut projects: Vec<Id> = by_project.keys().copied().collect();
        projects.sort();
        for pid in projects {
            let p = &on[&pid];
            let mut waiting = by_project.remove(&pid).unwrap_or_default();
            let Some(vision) = store.vision(pid).ok().flatten() else {
                let last: i64 = autopilot::number(store, &no_vision_key(pid), 0);
                if last == 0 || now - last >= NO_VISION_MS {
                    let _ = store.set_setting(&no_vision_key(pid), &now.to_string());
                    let origin = Origin::named(format!("{SOURCE} {}", p.name), None);
                    if self.runner.tell(&sup, &no_vision_message(p, waiting.len()), origin).is_ok() {
                        self.log(Some(pid), None, &format!("asked the supervisor to get a vision for {} before judging its goalless issues", p.name));
                    }
                }
                continue;
            };
            let oldest = waiting.iter().map(|i| marks.get(&i.id).map_or(now, |m| if m.0 == SEEN { m.1 } else { 0 })).min().unwrap_or(now);
            let last: i64 = autopilot::number(store, &last_key(pid), 0);
            if now - oldest < WINDOW_MS || (last > 0 && now - last < WINDOW_MS) || times.len() >= BATCHES_PER_HOUR {
                continue;
            }
            waiting.sort_by_key(|i| std::cmp::Reverse((i.created_at, i.id)));
            let left = waiting.len().saturating_sub(BATCH);
            let batch: Vec<&Issue> = waiting.into_iter().take(BATCH).collect();
            let text = message(p, &vision, &batch, left);
            let origin = Origin::named(format!("{SOURCE} {}", p.name), None);
            match self.runner.tell(&sup, &text, origin) {
                Ok(()) => {
                    for i in &batch {
                        let _ = store.set_judge_mark(i.id, SENT, now);
                    }
                    let _ = store.set_setting(&last_key(pid), &now.to_string());
                    times.push(now);
                    let list: Vec<String> = times.iter().map(i64::to_string).collect();
                    let _ = store.set_setting(SENT_TIMES, &list.join(","));
                    let ids: Vec<String> = batch.iter().map(|i| format!("#{}", i.id)).collect();
                    self.log(Some(pid), None, &format!("asked the supervisor to judge {} against the vision of {}", ids.join(" "), p.name));
                }
                Err(e) => self.log(Some(pid), None, &format!("could not ask the supervisor to judge issues in {}: {}", p.name, clip(&e.0, 160))),
            }
        }
    }
}

pub struct Judgement<'a> {
    pub issue: Id,
    pub verdict: &'a str,
    pub goal: Option<&'a str>,
    pub duplicate_of: Option<Id>,
    pub reason: &'a str,
}

pub fn judge(store: &Store, j: &Judgement) -> Result<Issue, String> {
    let issue = store.issue(j.issue).map_err(|e| e.0)?;
    let reason = j.reason.trim();
    if reason.is_empty() {
        return Err("judge_issue needs a `reason`".into());
    }
    if !VERDICTS.contains(&j.verdict) {
        return Err(format!("{} is not a verdict: {}", j.verdict, VERDICTS.join(", ")));
    }
    let untrusted = issue.trust.is_untrusted();
    let mut labels: Vec<String> = issue.labels.iter().filter(|l| !l.eq_ignore_ascii_case(LABEL)).cloned().collect();
    let mut patch = IssuePatch::default();
    let comment = match j.verdict {
        "serves_goal" => {
            if untrusted {
                return Err(format!("#{} is from an untrusted author; it can only be closed or labelled (off_vision, needs_user, duplicate, not_worth_doing), not given a goal", issue.id));
            }
            let goal = j.goal.map(str::trim).filter(|g| !g.is_empty()).ok_or("serves_goal needs `goal`, an active goal id of the project")?;
            let vision = store.vision(issue.project_id).map_err(|e| e.0)?.ok_or("that project has no vision")?;
            if !vision.active_goals().any(|g| g.id == goal) {
                let active: Vec<&str> = vision.active_goals().map(|g| g.id.as_str()).collect();
                return Err(format!("{goal} is not an active goal of that project; active goals: {}", if active.is_empty() { "none".into() } else { active.join(", ") }));
            }
            patch.goal = Some(goal.to_string());
            format!("serves goal {goal}: {reason}")
        }
        "off_vision" => {
            patch.off_vision = Some(true);
            patch.state = Some(IssueState::Backlog);
            format!("off vision: {reason}")
        }
        "needs_user" => {
            if !labels.iter().any(|l| l.eq_ignore_ascii_case(autopilot::NEEDS_USER)) {
                labels.push(autopilot::NEEDS_USER.into());
            }
            format!("needs the user: {reason}")
        }
        "duplicate" => {
            let other = j.duplicate_of.ok_or("duplicate needs `duplicate_of`, the issue it duplicates")?;
            if other == issue.id {
                return Err("an issue cannot duplicate itself".into());
            }
            store.issue(other).map_err(|e| e.0)?;
            patch.state = Some(IssueState::Done);
            format!("duplicate of #{other}: {reason}")
        }
        _ => {
            patch.state = Some(IssueState::Done);
            format!("not worth doing: {reason}")
        }
    };
    if labels != issue.labels {
        patch.labels = Some(labels);
    }
    let after = store.update_issue(issue.id, &patch).map_err(|e| e.0)?;
    store.set_judge_mark(issue.id, JUDGED, crate::store::now()).map_err(|e| e.0)?;
    store
        .add_comment(issue.id, &NewComment { author: Some("supervisor".into()), body: format!("judged against the vision: {comment}") })
        .map_err(|e| e.0)?;
    let _ = store.log_autopilot(Some(issue.project_id), Some(issue.id), &format!("judged #{}: {}", issue.id, clip(&comment, 200)));
    Ok(after)
}

pub fn all_answered(store: &Store) -> bool {
    let marks = store.judge_marks().unwrap_or_default();
    !marks.iter().any(|(id, (s, _))| s == SENT && store.issue(*id).is_ok_and(|i| wants_judging(&i)))
}
