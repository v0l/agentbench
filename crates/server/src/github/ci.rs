use super::outbox::sanitize;
use crate::store::Store;
use agentbench_core::{Id, IssueState, PullRequest, Session};

pub const MAX_CI_REWORKS: u32 = 2;
const LOG_CLIP: usize = 6_000;

pub fn needs_rework(pr: &PullRequest) -> bool {
    pr.state == "open" && pr.by_agentbench && pr.checks_red() && pr.ci_sent_sha.as_deref() != Some(pr.head_sha.as_str())
}

fn clip_tail(text: &str) -> String {
    let t = text.trim();
    if t.len() <= LOG_CLIP {
        return t.to_string();
    }
    format!("...{}", &t[t.ceil_char_boundary(t.len() - LOG_CLIP)..])
}

pub fn failed_log(gh: &str, repo: &str, pr: &PullRequest) -> String {
    let mut runs: Vec<i64> = pr.checks.iter().filter(|c| c.failed()).filter_map(|c| c.run_id).collect();
    runs.dedup();
    let mut out = String::new();
    for run in runs {
        let r = super::sync::call(gh, &["run", "view", &run.to_string(), "--log-failed", "-R", repo]);
        out.push_str(if r.ok { &r.stdout } else { &r.stderr });
        out.push('\n');
    }
    if out.trim().is_empty() {
        let names: Vec<&str> = pr.checks.iter().filter(|c| c.failed()).map(|c| c.name.as_str()).collect();
        out = format!("no log could be read for the failed checks: {}", names.join(", "));
    }
    sanitize(&clip_tail(&out))
}

pub fn followup(pr: &PullRequest, log: &str) -> String {
    let red: Vec<&str> = pr.checks.iter().filter(|c| c.failed()).map(|c| c.name.as_str()).collect();
    format!(
        "CI is red on your pull request #{} at {} (failed: {}). The failing log, clipped:\n\n```\n{log}\n```\n\nFix the cause on your branch, run the same commands locally, commit, and end with a conclusion and a final `DONE` line. Do not push; the autopilot updates the pull request.",
        pr.number,
        pr.head_sha,
        red.join(", ")
    )
}

fn worker_session(store: &Store, issue: Id) -> Option<Session> {
    store.sessions().ok()?.into_iter().filter(|s| s.issue_id == Some(issue)).max_by_key(|s| s.created_at)
}

pub fn rework(store: &Store, gh: &str, tell: &dyn Fn(&Session, &str) -> Result<(), String>) -> usize {
    let mut sent = 0;
    for p in store.projects().unwrap_or_default().into_iter().filter(|p| p.tracker.is_github()) {
        let Some(repo) = super::project_repo(&p) else { continue };
        for pr in store.pull_requests(p.id).unwrap_or_default().into_iter().filter(needs_rework) {
            let Some(issue) = pr.issue_id else { continue };
            if pr.ci_reworks >= MAX_CI_REWORKS {
                let key = format!("gh_ci_gave_up_{}_{}_{}", p.id, pr.number, pr.head_sha);
                if store.setting(&key).ok().flatten().is_none() {
                    let _ = store.set_setting(&key, "1");
                    let _ = store.add_comment_local(
                        issue,
                        &agentbench_core::NewComment {
                            author: Some(super::flow::BOT_AUTHOR.into()),
                            body: format!("CI is still red on pull request #{} after {MAX_CI_REWORKS} follow-ups; it needs a person.", pr.number),
                        },
                    );
                }
                continue;
            }
            let Some(session) = worker_session(store, issue) else { continue };
            let text = followup(&pr, &failed_log(gh, &repo, &pr));
            if tell(&session, &text).is_err() {
                continue;
            }
            let _ = store.set_pr_ci_sent(p.id, pr.number, &pr.head_sha);
            let _ = store.add_worker(session.id, issue, p.id);
            let _ = store.set_mirror_state(issue, IssueState::Doing);
            let _ = store.log_autopilot(
                Some(p.id),
                Some(issue),
                &format!("CI red on pull request #{}: sent the worker back with the failing log ({} of {MAX_CI_REWORKS})", pr.number, pr.ci_reworks + 1),
            );
            sent += 1;
        }
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::testing::Rig;
    use agentbench_core::{PostMode, Trust};
    use std::sync::Mutex;

    #[test]
    fn a_red_check_sends_the_worker_back_once_with_the_log_and_at_most_twice() {
        let r = Rig::new(PostMode::Draft, "");
        let issue = r.issue(4, Trust::Trusted, true);
        let tree = r.store.main_worktree(r.project).unwrap();
        let worker = r.store.add_session(tree.id, "echo", "w", None, Some(issue)).unwrap();
        r.pr(9, "agentbench", Trust::Trusted, "s1");
        r.store.link_pr(r.project, 9, Some(4)).unwrap();
        r.store.set_pr_by_agentbench(r.project, 9).unwrap();
        let told: Mutex<Vec<(agentbench_core::SessionId, String)>> = Mutex::new(Vec::new());
        let tell = |s: &Session, text: &str| {
            told.lock().unwrap().push((s.id, text.to_string()));
            Ok(())
        };
        r.checks(9, "s1", &["success", "running"]);
        assert_eq!(rework(&r.store, &r.gh, &tell), 0, "nothing while a check still runs");
        r.checks(9, "s1", &["success", "failure"]);
        assert_eq!(rework(&r.store, &r.gh, &tell), 1);
        assert_eq!(rework(&r.store, &r.gh, &tell), 0, "the same red head is sent back once");
        {
            let told = told.lock().unwrap();
            assert_eq!(told.len(), 1);
            assert_eq!(told[0].0, worker.id);
            assert!(told[0].1.contains("error[E0308]: mismatched types") && told[0].1.contains("job1"), "{}", told[0].1);
        }
        assert!(r.calls().iter().any(|c| c == "run view 551 --log-failed -R o/r"), "{:?}", r.calls());
        assert_eq!(r.store.workers().unwrap().iter().filter(|w| w.session == worker.id).count(), 1, "the autopilot judges it again");
        assert_eq!(r.store.issue(issue).unwrap().state, IssueState::Doing);

        r.pr(9, "agentbench", Trust::Trusted, "s2");
        r.checks(9, "s2", &["failure"]);
        assert_eq!(rework(&r.store, &r.gh, &tell), 1);
        r.pr(9, "agentbench", Trust::Trusted, "s3");
        r.checks(9, "s3", &["failure"]);
        assert_eq!(rework(&r.store, &r.gh, &tell), 0, "at most twice");
        assert_eq!(told.lock().unwrap().len(), 2);
        assert!(r.store.comments(issue).unwrap().iter().any(|c| c.body.contains("needs a person")));
    }

    #[test]
    fn a_red_check_on_someone_elses_pr_sends_nobody_back() {
        let r = Rig::new(PostMode::Draft, "");
        let issue = r.issue(4, Trust::Trusted, true);
        let tree = r.store.main_worktree(r.project).unwrap();
        r.store.add_session(tree.id, "echo", "w", None, Some(issue)).unwrap();
        r.pr(9, "kieran", Trust::Trusted, "s1");
        r.store.link_pr(r.project, 9, Some(4)).unwrap();
        r.checks(9, "s1", &["failure"]);
        let tell = |_: &Session, _: &str| -> Result<(), String> { panic!("nobody is told") };
        assert_eq!(rework(&r.store, &r.gh, &tell), 0);
    }
}
