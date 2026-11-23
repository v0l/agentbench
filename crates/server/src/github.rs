use agentbench_core::{GithubStatus, Project, Tracker};
use std::process::{Command, Stdio};

pub mod caps;
pub mod ci;
pub mod flow;
pub mod map;
pub mod merge;
pub mod outbox;
pub mod ready;
pub mod review;
pub mod sync;
#[cfg(test)]
pub mod testing;
pub mod trust;
pub mod webhook;

pub use sync::{Outcome, SyncState, load_state, sync_project};

pub const POLL_SETTING: &str = "github_poll_secs";

pub fn poll_secs(store: &crate::store::Store) -> i64 {
    store
        .setting(POLL_SETTING)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|s| *s >= 5)
        .unwrap_or(sync::POLL_SECS)
}

pub fn sync_due(store: &crate::store::Store, gh: &str, now: i64) -> Vec<(agentbench_core::Id, Outcome)> {
    let base = poll_secs(store);
    let mut done = Vec::new();
    for p in store.projects().unwrap_or_default() {
        if !matches!(p.tracker, Tracker::Github { .. }) {
            continue;
        }
        let every = if webhook::healthy(store, p.id, now) { base.max(webhook::SLOW_POLL_SECS) } else { base } * 1000;
        let state = load_state(store, p.id);
        if state.last_attempt.is_some_and(|t| now - t < every) {
            continue;
        }
        let out = sync_project(store, gh, p.id, now, false);
        if out != Outcome::Skipped {
            done.push((p.id, out));
        }
    }
    done
}

pub fn spawn(runner: crate::runner::Runner) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let store = runner.store().clone();
            let worker = runner.clone();
            let result = tokio::task::spawn_blocking(move || {
                let gh = gh_program();
                let now = crate::store::now();
                outbox::drain(&store, &gh, now);
                let synced = sync_due(&store, &gh, now);
                let tell = |s: &agentbench_core::Session, text: &str| {
                    worker.tell(s, text, crate::runner::Origin::new("autopilot", Some(s.id))).map_err(|e| e.0)
                };
                ci::rework(&store, &gh, &tell);
                merge::automerge(&store, &gh);
                synced
            })
            .await;
            if let Some(target) = runner.titles().and_then(|t| t.target()) {
                flow::triage_issues(runner.store(), &target, crate::store::now()).await;
            }
            match result {
                Ok(done) if done.iter().any(|(_, o)| *o == Outcome::Synced) => {
                    let _ = runner.store().set_setting("github_poll_changed", &crate::store::now().to_string());
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("github sync task failed: {e}"),
            }
        }
    });
}

pub fn gh_program() -> String {
    std::env::var("AGENTBENCH_GH").unwrap_or_else(|_| "gh".into())
}

pub use agentbench_core::repo_from_remote;

pub fn project_repo(project: &Project) -> Option<String> {
    project.github_repo()
}

fn run(gh: &str, args: &[&str]) -> std::io::Result<(bool, String)> {
    let out = Command::new(gh).args(args).stdin(Stdio::null()).output()?;
    Ok((out.status.success(), String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

pub fn status(store: &crate::store::Store, gh: &str, project: &Project, last_sync: Option<i64>) -> GithubStatus {
    let mut s = GithubStatus {
        tracker: project.tracker.clone(),
        post_mode: project.post_mode,
        repo: project_repo(project),
        last_sync,
        sync_error: load_state(store, project.id).error,
        poll_secs: Some(poll_secs(store)),
        webhook_url: webhook::url(store, project.id),
        webhook_secret_set: webhook::secret(store, project.id).is_some(),
        webhook_last: webhook::last_delivery(store, project.id),
        paused: outbox::is_paused(store),
        ..Default::default()
    };
    match run(gh, &["auth", "status"]) {
        Err(_) => {
            s.problem = Some("gh missing".into());
            return s;
        }
        Ok((false, _)) => {
            s.installed = true;
            s.problem = Some("not authenticated".into());
            return s;
        }
        Ok((true, _)) => {
            s.installed = true;
            s.authenticated = true;
        }
    }
    let Some(repo) = s.repo.clone() else {
        s.problem = Some("no GitHub repo for this project".into());
        return s;
    };
    if let Ok((true, out)) = run(gh, &["api", &format!("repos/{repo}"), "--jq", ".permissions.push"]) {
        s.write_access = Some(out == "true");
    }
    if let Ok((true, out)) = run(gh, &["api", "rate_limit", "--jq", ".rate.remaining"]) {
        s.rate_limit_remaining = out.parse().ok();
    }
    s
}

#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_from_remote_reads_github_remotes() {
        let want = Some("o/r".to_string());
        for remote in [
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "https://github.com/o/r/",
            "https://user@github.com/o/r.git",
            "ssh://git@github.com/o/r.git",
            "git@github.com:o/r.git",
            "git@github.com:o/r",
        ] {
            assert_eq!(repo_from_remote(Some(remote), None), want, "{remote}");
        }
    }

    #[test]
    fn repo_from_remote_rejects_other_hosts_and_junk() {
        for remote in ["https://gitlab.com/o/r.git", "git@gitlab.com:o/r.git", "/srv/git/r.git", "https://github.com/o", ""] {
            assert_eq!(repo_from_remote(Some(remote), None), None, "{remote}");
        }
        assert_eq!(repo_from_remote(None, None), None);
    }

    #[test]
    fn an_override_wins_over_the_remote() {
        assert_eq!(
            repo_from_remote(Some("git@github.com:o/r.git"), Some("fork/r2")),
            Some("fork/r2".to_string())
        );
        assert_eq!(repo_from_remote(Some("https://gitlab.com/o/r"), Some("a/b")), Some("a/b".to_string()));
        assert_eq!(repo_from_remote(Some("git@github.com:o/r.git"), Some("  ")), Some("o/r".to_string()));
    }
}
