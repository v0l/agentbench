use crate::store::Store;
use agentbench_core::{AutoMerge, Id, PostMode, PrCheck, PullRequest};
use serde_json::Value;

pub const OWNER: &str = "user";

pub struct Fresh {
    pub head: String,
    pub state: String,
    pub draft: bool,
    pub checks: Vec<PrCheck>,
}

pub fn fresh(gh: &str, repo: &str, number: i64) -> Result<Fresh, String> {
    let r = super::sync::call(gh, &["api", &format!("repos/{repo}/pulls/{number}")]);
    if !r.ok {
        return Err(format!("could not read pull request #{number}: {}", r.stderr.trim()));
    }
    let v: Value = serde_json::from_str(&r.stdout).map_err(|e| format!("bad JSON from gh: {e}"))?;
    let head = v.pointer("/head/sha").and_then(Value::as_str).unwrap_or_default().to_string();
    let merged = v.get("merged").and_then(Value::as_bool).unwrap_or(false);
    let state = if merged { "merged".to_string() } else { v.get("state").and_then(Value::as_str).unwrap_or("open").to_string() };
    let draft = v.get("draft").and_then(Value::as_bool).unwrap_or(false);
    let c = super::sync::call(gh, &["api", &format!("repos/{repo}/commits/{head}/check-runs?per_page=100")]);
    if !c.ok {
        return Err(format!("could not read the checks of #{number}: {}", c.stderr.trim()));
    }
    let cv: Value = serde_json::from_str(&c.stdout).map_err(|e| format!("bad JSON from gh: {e}"))?;
    Ok(Fresh { head, state, draft, checks: super::sync::check_runs(&cv) })
}

fn checks_line(checks: &[PrCheck]) -> Option<String> {
    let red: Vec<&str> = checks.iter().filter(|c| c.failed()).map(|c| c.name.as_str()).collect();
    if !red.is_empty() {
        return Some(format!("checks failed: {}", red.join(", ")));
    }
    let running: Vec<&str> = checks.iter().filter(|c| !c.done()).map(|c| c.name.as_str()).collect();
    if !running.is_empty() {
        return Some(format!("checks still running: {}", running.join(", ")));
    }
    None
}

fn owner_approved(store: &Store, pr: &PullRequest, head: &str) -> Result<(), String> {
    if pr.reviewed_sha.as_deref() != Some(head) {
        return Err("the head commit has no review yet; review it first".into());
    }
    let row = pr.review_row.and_then(|id| store.outbox_item(id).ok()).ok_or("the review row is missing")?;
    let payload: Value = serde_json::from_str(&row.payload).unwrap_or(Value::Null);
    if payload.get("sha").and_then(Value::as_str) != Some(head) {
        return Err("the review row is for another commit".into());
    }
    if row.approved_by != OWNER || !matches!(row.status.as_str(), "approved" | "posting" | "done") {
        return Err("the owner has not approved the review row in the outbox".into());
    }
    if super::outbox::review_event(&payload) == "request_changes" {
        return Err("the approved review asks for changes".into());
    }
    Ok(())
}

pub fn gh_merge(store: &Store, gh: &str, repo: &str, project: Id, pr: &PullRequest, head: &str) -> Result<String, String> {
    let method = store.project(project).map(|p| p.merge_method).unwrap_or_default();
    let n = pr.number.to_string();
    let flag = format!("--{}", method.as_str());
    let r = super::sync::call(gh, &["pr", "merge", &n, "-R", repo, &flag, "--match-head-commit", head]);
    if !r.ok {
        return Err(format!("gh pr merge refused: {}", r.stderr.trim()));
    }
    store.set_pr_state(project, pr.number, "merged").map_err(|e| e.0)?;
    super::flow::pr_finished(store, project, pr.number, pr.issue_id, true);
    let _ = store.log_autopilot(Some(project), pr.issue_id, &format!("merged pull request #{} with {}", pr.number, method.as_str()));
    Ok(format!("merged #{} ({})", pr.number, method.as_str()))
}

fn mergeable(store: &Store, gh: &str, project: Id, number: i64) -> Result<(String, PullRequest, Fresh), String> {
    let p = store.project(project).map_err(|e| e.0)?;
    if !p.tracker.is_github() {
        return Err("this project does not use GitHub".into());
    }
    let repo = super::project_repo(&p).ok_or("no GitHub repo for this project")?;
    let pr = store.pull_request(project, number).map_err(|e| e.0)?.ok_or(format!("no mirrored pull request #{number}"))?;
    let f = fresh(gh, &repo, number)?;
    if f.state != "open" {
        return Err(format!("#{number} is {}", f.state));
    }
    if f.draft {
        return Err(format!("#{number} is a draft"));
    }
    let _ = store.set_pr_checks(project, number, &f.head, &f.checks);
    if let Some(why) = checks_line(&f.checks) {
        return Err(format!("not merged: {why}"));
    }
    Ok((repo, pr, f))
}

pub fn merge(store: &Store, gh: &str, project: Id, number: i64, by: &str) -> Result<String, String> {
    if by != OWNER {
        return Err("only the owner merges a pull request".into());
    }
    let (repo, pr, f) = mergeable(store, gh, project, number)?;
    owner_approved(store, &pr, &f.head).map_err(|e| format!("not merged: {e}"))?;
    gh_merge(store, gh, &repo, project, &pr, &f.head)
}

pub fn auto_candidate(store: &Store, pr: &PullRequest) -> bool {
    pr.state == "open"
        && !pr.draft
        && pr.by_agentbench
        && !pr.trust.is_untrusted()
        && pr.reviewed_sha.as_deref() == Some(pr.head_sha.as_str())
        && pr.review_verdict.as_deref() == Some("approve")
        && pr.checks_green()
        && !pr.checks.is_empty()
        && pr.issue_id.and_then(|i| store.issue(i).ok()).is_some_and(|i| i.ready_by.is_some() && !i.trust.is_untrusted())
}

pub fn automerge(store: &Store, gh: &str) -> Vec<String> {
    let mut done = Vec::new();
    if super::outbox::is_paused(store) {
        return done;
    }
    for p in store.projects().unwrap_or_default() {
        if !p.tracker.is_github() || p.automerge != AutoMerge::Trusted || p.post_mode == PostMode::Off {
            continue;
        }
        for pr in store.pull_requests(p.id).unwrap_or_default().into_iter().filter(|pr| auto_candidate(store, pr)) {
            let result = mergeable(store, gh, p.id, pr.number).and_then(|(repo, _, f)| {
                if f.head != pr.head_sha || f.checks.is_empty() {
                    return Err("the head moved or has no checks".into());
                }
                gh_merge(store, gh, &repo, p.id, &pr, &f.head)
            });
            match result {
                Ok(line) => done.push(line),
                Err(e) => {
                    let key = format!("gh_automerge_{}_{}_{}", p.id, pr.number, pr.head_sha);
                    if store.setting(&key).ok().flatten().is_none() {
                        let _ = store.set_setting(&key, "1");
                        let _ = store.log_autopilot(Some(p.id), pr.issue_id, &format!("automerge of #{} held: {e}", pr.number));
                    }
                }
            }
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::testing::Rig;
    use agentbench_core::Trust;
    use serde_json::json;

    fn review_row(r: &Rig, number: i64, event: &str, sha: &str) -> Id {
        let ask = super::super::outbox::Ask {
            project: r.project,
            kind: "review".into(),
            number,
            text: "Looks right.".into(),
            params: json!({"event": event, "comments": [], "sha": sha}),
            asked_by: "reviewer".into(),
            trusted: true,
        };
        let (id, _) = super::super::outbox::propose(&r.store, ask).unwrap();
        r.store.set_pr_review(r.project, number, sha, event, id, 0).unwrap();
        id
    }

    fn merged(r: &Rig) -> usize {
        r.calls().iter().filter(|c| c.starts_with("pr merge")).count()
    }

    #[test]
    fn merge_refuses_on_red_or_running_checks_and_without_owner_approval() {
        let r = Rig::new(PostMode::Draft, "");
        r.pr(9, "kieran", Trust::Trusted, &r.head);
        let row = review_row(&r, 9, "approve", &r.head);
        r.checks(9, &r.head, &["success", "failure"]);
        crate::github::outbox::approve(&r.store, row, None, OWNER).unwrap();
        let e = merge(&r.store, &r.gh, r.project, 9, OWNER).unwrap_err();
        assert!(e.contains("checks failed: job1"), "{e}");
        r.checks(9, &r.head, &["success", "running"]);
        let e = merge(&r.store, &r.gh, r.project, 9, OWNER).unwrap_err();
        assert!(e.contains("still running"), "{e}");
        assert_eq!(merged(&r), 0);

        let r = Rig::new(PostMode::Draft, "");
        r.pr(9, "kieran", Trust::Trusted, &r.head);
        r.checks(9, &r.head, &["success"]);
        let e = merge(&r.store, &r.gh, r.project, 9, OWNER).unwrap_err();
        assert!(e.contains("no review"), "{e}");
        let row = review_row(&r, 9, "approve", &r.head);
        let e = merge(&r.store, &r.gh, r.project, 9, OWNER).unwrap_err();
        assert!(e.contains("owner has not approved"), "{e}");
        assert!(merge(&r.store, &r.gh, r.project, 9, "supervisor").unwrap_err().contains("only the owner"));
        crate::github::outbox::approve(&r.store, row, None, OWNER).unwrap();
        r.put("pull.json", r#"{"number":9,"state":"open","merged":false,"head":{"sha":"moved"}}"#);
        let e = merge(&r.store, &r.gh, r.project, 9, OWNER).unwrap_err();
        assert!(e.contains("no review"), "a new head needs its own review: {e}");
        assert_eq!(merged(&r), 0);
        r.checks(9, &r.head, &["success"]);
        r.store.update_project(r.project, &agentbench_core::ProjectPatch { merge_method: Some(agentbench_core::MergeMethod::Squash), ..Default::default() }).unwrap();
        merge(&r.store, &r.gh, r.project, 9, OWNER).unwrap();
        let call = r.calls().into_iter().find(|c| c.starts_with("pr merge")).unwrap();
        assert_eq!(call, format!("pr merge 9 -R o/r --squash --match-head-commit {}", r.head));
        assert!(!call.contains("--admin"), "branch protection is never bypassed");
        assert_eq!(r.store.pull_request(r.project, 9).unwrap().unwrap().state, "merged");
    }

    #[test]
    fn branch_protection_refusing_the_merge_is_reported_not_bypassed() {
        let r = Rig::new(PostMode::Draft, "");
        r.pr(9, "kieran", Trust::Trusted, &r.head);
        r.checks(9, &r.head, &["success"]);
        let row = review_row(&r, 9, "approve", &r.head);
        crate::github::outbox::approve(&r.store, row, None, OWNER).unwrap();
        r.put("merge_fails", "GraphQL: Pull request is not mergeable: the base branch policy prohibits the merge");
        let e = merge(&r.store, &r.gh, r.project, 9, OWNER).unwrap_err();
        assert!(e.contains("base branch policy"), "{e}");
        assert_eq!(merged(&r), 1);
        assert_eq!(r.store.pull_request(r.project, 9).unwrap().unwrap().state, "open");
    }

    #[test]
    fn automerge_takes_only_a_green_approved_agentbench_pr_for_a_ready_issue() {
        let r = Rig::new(PostMode::Auto, "");
        let issue = r.issue(4, Trust::Trusted, true);
        r.pr(9, "agentbench", Trust::Trusted, &r.head);
        r.store.link_pr(r.project, 9, Some(4)).unwrap();
        r.checks(9, &r.head, &["success"]);
        review_row(&r, 9, "approve", &r.head);
        assert!(automerge(&r.store, &r.gh).is_empty(), "automerge is off by default");
        r.store.update_project(r.project, &agentbench_core::ProjectPatch { automerge: Some(AutoMerge::Trusted), ..Default::default() }).unwrap();
        assert!(automerge(&r.store, &r.gh).is_empty(), "a pull request agentbench did not open is left alone");
        r.store.set_pr_by_agentbench(r.project, 9).unwrap();
        r.checks(9, &r.head, &["failure"]);
        assert!(automerge(&r.store, &r.gh).is_empty(), "red checks hold it");
        r.checks(9, &r.head, &["success"]);
        r.store.set_pr_review(r.project, 9, &r.head, "comment", 0, 0).unwrap();
        assert!(automerge(&r.store, &r.gh).is_empty(), "only an approve verdict");
        review_row(&r, 9, "approve", &r.head);
        r.store.set_ready(issue, None, None, None, 0).unwrap();
        assert!(automerge(&r.store, &r.gh).is_empty(), "only for a ready issue");
        r.store.set_ready(issue, Some("approved"), Some("label"), None, 0).unwrap();
        assert_eq!(merged(&r), 0);
        assert_eq!(automerge(&r.store, &r.gh).len(), 1);
        assert_eq!(merged(&r), 1);
        assert_eq!(r.store.issue(issue).unwrap().state, agentbench_core::IssueState::Done);
        assert!(automerge(&r.store, &r.gh).is_empty());
    }
}
