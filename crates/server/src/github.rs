use agentbench_core::{GithubStatus, Project, Tracker};
use std::process::{Command, Stdio};

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

pub fn status(gh: &str, project: &Project, last_sync: Option<i64>) -> GithubStatus {
    let mut s = GithubStatus {
        tracker: project.tracker.clone(),
        post_mode: project.post_mode,
        repo: project_repo(project),
        last_sync,
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
