use agentbench_core::{GithubStatus, Project, Tracker};
use std::process::{Command, Stdio};

pub mod caps;
pub mod map;
pub mod outbox;
pub mod ready;
pub mod sync;
pub mod trust;

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
    let every = poll_secs(store) * 1000;
    let mut done = Vec::new();
    for p in store.projects().unwrap_or_default() {
        if !matches!(p.tracker, Tracker::Github { .. }) {
            continue;
        }
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
            let result = tokio::task::spawn_blocking(move || {
                let gh = gh_program();
                let now = crate::store::now();
                outbox::drain(&store, &gh, now);
                sync_due(&store, &gh, now)
            })
            .await;
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

fn valid_part(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn owner_repo(path: &str) -> Option<String> {
    let path = path.trim().trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    (valid_part(owner) && valid_part(repo)).then(|| format!("{owner}/{repo}"))
}

pub fn repo_from_remote(remote: Option<&str>, override_repo: Option<&str>) -> Option<String> {
    if let Some(o) = override_repo.map(str::trim).filter(|o| !o.is_empty()) {
        return owner_repo(o);
    }
    let remote = remote?.trim();
    let rest = ["https://", "http://", "ssh://", "git://"]
        .iter()
        .find_map(|p| remote.strip_prefix(p))
        .map(|r| {
            let r = r.split_once('@').map_or(r, |(_, h)| h);
            r.split_once('/').map(|(host, path)| (host.split(':').next().unwrap_or(host), path))
        });
    match rest {
        Some(Some((host, path))) => {
            host.eq_ignore_ascii_case("github.com").then(|| owner_repo(path)).flatten()
        }
        Some(None) => None,
        None => {
            let (host, path) = remote.split_once(':')?;
            let host = host.split_once('@').map_or(host, |(_, h)| h);
            host.eq_ignore_ascii_case("github.com").then(|| owner_repo(path)).flatten()
        }
    }
}

pub fn project_repo(project: &Project) -> Option<String> {
    let own = match &project.tracker {
        Tracker::Github { repo } => Some(repo.as_str()),
        Tracker::Internal => None,
    };
    repo_from_remote(project.remote.as_deref(), own)
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
