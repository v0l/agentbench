use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq)]
pub struct Tree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub bare: bool,
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(if err.is_empty() { format!("git {} failed", args.join(" ")) } else { err })
    }
}

pub fn parse_worktrees(porcelain: &str) -> Vec<Tree> {
    let mut trees = Vec::new();
    let mut current: Option<Tree> = None;
    for line in porcelain.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            trees.extend(current.take());
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "worktree" => {
                trees.extend(current.take());
                current = Some(Tree {
                    path: PathBuf::from(value),
                    head: None,
                    branch: None,
                    bare: false,
                });
            }
            "HEAD" => {
                if let Some(t) = &mut current {
                    t.head = Some(value.to_string());
                }
            }
            "branch" => {
                if let Some(t) = &mut current {
                    t.branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).to_string());
                }
            }
            "bare" => {
                if let Some(t) = &mut current {
                    t.bare = true;
                }
            }
            _ => {}
        }
    }
    trees
}

pub fn worktrees(dir: &Path) -> Result<Vec<Tree>, String> {
    Ok(parse_worktrees(&git(dir, &["worktree", "list", "--porcelain"])?))
}

pub fn main_worktree(dir: &Path) -> Result<Tree, String> {
    let trees = worktrees(dir)?;
    trees
        .into_iter()
        .find(|t| !t.bare)
        .ok_or_else(|| format!("{} has no checked out worktree", dir.display()))
}

/// Make `dir` a repository with one empty commit, so worktrees can branch off it.
pub fn init(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    git(dir, &["init", "-q", "-b", "main"])?;
    let commit = ["commit", "-q", "--allow-empty", "-m", "Initial commit"];
    if git(dir, &commit).is_err() {
        // No git identity configured on this machine.
        let mut args = vec![
            "-c",
            "user.name=agentbench",
            "-c",
            "user.email=agentbench@localhost",
            "-c",
            "commit.gpgsign=false",
        ];
        args.extend(commit);
        git(dir, &args)?;
    }
    Ok(())
}

pub fn remote_url(dir: &Path) -> Option<String> {
    git(dir, &["remote", "get-url", "origin"]).ok().filter(|u| !u.is_empty())
}

pub fn repo_name(main: &Path, remote: Option<&str>) -> String {
    let from_remote = remote.and_then(|r| {
        let tail = r.trim_end_matches('/').trim_end_matches(".git");
        tail.rsplit(['/', ':']).next().filter(|s| !s.is_empty()).map(str::to_string)
    });
    from_remote.unwrap_or_else(|| {
        main.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into())
    })
}

pub fn sibling_path(main: &Path, branch: &str) -> PathBuf {
    let base = main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or("repo".into());
    let leaf = format!("{base}-{}", branch.replace('/', "-"));
    main.parent().unwrap_or(main).join(".worktrees").join(base).join(leaf)
}

pub fn branch_exists(dir: &Path, branch: &str) -> bool {
    git(dir, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok()
}

pub fn add_worktree(main: &Path, branch: &str, base: Option<&str>) -> Result<PathBuf, String> {
    let path = sibling_path(main, branch);
    if path.exists() {
        reuse_or_clear(main, branch, &path)?;
        if path.exists() {
            return Ok(path);
        }
    }
    create_worktree(main, branch, base, &path)?;
    Ok(path)
}

fn reuse_or_clear(main: &Path, branch: &str, path: &Path) -> Result<(), String> {
    let registered = worktrees(main)?.iter().any(|t| t.path == path && t.branch.as_deref() == Some(branch));
    if !registered {
        return Err(format!("{} already exists", path.display()));
    }
    if is_dirty(path)? {
        return Err(format!("{} has uncommitted changes", path.display()));
    }
    if is_merged(main, branch) {
        remove_worktree(main, path, false)?;
        delete_branch(main, branch)?;
    }
    Ok(())
}

fn create_worktree(main: &Path, branch: &str, base: Option<&str>, path: &Path) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let target = path.to_string_lossy().into_owned();
    if branch_exists(main, branch) {
        git(main, &["worktree", "add", &target, branch])?;
    } else {
        let mut args = vec!["worktree", "add", "-b", branch, &target];
        if let Some(base) = base {
            args.push(base);
        }
        git(main, &args)?;
    }
    Ok(())
}

pub fn remove_worktree(main: &Path, path: &Path, force: bool) -> Result<(), String> {
    let target = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&target);
    git(main, &args)?;
    Ok(())
}

pub fn delete_branch(main: &Path, branch: &str) -> Result<(), String> {
    git(main, &["branch", "-d", branch]).map(|_| ())
}

pub fn current_branch(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).ok().filter(|b| b != "HEAD")
}

pub fn merge_branch(main: &Path, branch: &str) -> Result<(), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["merge", "--no-edit", branch])
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let _ = git(main, &["merge", "--abort"]);
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    let text = text.trim();
    Err(if text.is_empty() { format!("git merge --no-edit {branch} failed") } else { text.to_string() })
}

pub fn default_branch(main: &Path, configured: Option<&str>) -> Option<String> {
    if let Some(b) = configured.filter(|b| !b.is_empty()) {
        return Some(b.to_string());
    }
    if let Ok(head) = git(main, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        && let Some(b) = head.strip_prefix("origin/")
        && branch_exists(main, b)
    {
        return Some(b.to_string());
    }
    for b in ["main", "master"] {
        if branch_exists(main, b) {
            return Some(b.to_string());
        }
    }
    current_branch(main)
}

pub struct TempTree {
    main: PathBuf,
    pub path: PathBuf,
}

impl TempTree {
    pub fn add(main: &Path, branch: &str) -> Result<TempTree, String> {
        TempTree::make(main, &[branch])
    }

    pub fn detached(main: &Path, rev: &str) -> Result<TempTree, String> {
        TempTree::make(main, &["--detach", rev])
    }

    fn make(main: &Path, what: &[&str]) -> Result<TempTree, String> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("agentbench-merge-{}-{nanos}-{seq}", std::process::id()));
        let path_text = path.to_string_lossy().into_owned();
        let mut args = vec!["worktree", "add", path_text.as_str()];
        args.extend_from_slice(what);
        git(main, &args)?;
        Ok(TempTree { main: main.to_path_buf(), path })
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        if remove_worktree(&self.main, &self.path, true).is_err() {
            let _ = std::fs::remove_dir_all(&self.path);
            let _ = git(&self.main, &["worktree", "prune"]);
        }
    }
}

pub fn reset_hard(dir: &Path, rev: &str) -> Result<(), String> {
    git(dir, &["reset", "--hard", "-q", rev]).map(|_| ())
}

pub fn is_protected_rejection(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("repository rule violations")
        || t.contains("protected branch")
        || t.contains("gh006")
        || t.contains("gh013")
}

pub fn open_pull_request(dir: &Path, branch: &str, base: &str, title: &str, body: &str) -> Result<String, String> {
    let out = Command::new(std::env::var("AGENTBENCH_GH").unwrap_or_else(|_| "gh".into()))
        .current_dir(dir)
        .args(["pr", "create", "--head", branch, "--base", base, "--title", title, "--body", body])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run gh: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let url = |text: &str| {
        text.split_whitespace().find(|w| w.starts_with("http://") || w.starts_with("https://")).map(str::to_string)
    };
    if out.status.success() {
        return url(&stdout).ok_or_else(|| "gh pr create printed no URL".to_string());
    }
    if stderr.contains("already exists")
        && let Some(u) = url(&stderr)
    {
        return Ok(u);
    }
    Err(stderr.trim().to_string())
}

pub fn branch_tip(main: &Path, branch: &str) -> Option<String> {
    git(main, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).ok()
}

pub fn remote_branch_tip(main: &Path, remote: &str, branch: &str) -> Option<String> {
    let out = git(main, &["ls-remote", remote, &format!("refs/heads/{branch}")]).ok()?;
    out.split_whitespace().next().map(str::to_string)
}

pub fn dirty_files(path: &Path, limit: usize) -> Vec<String> {
    git(path, &["status", "--short"])
        .map(|o| o.lines().take(limit).map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

pub fn conflicts_with(main: &Path, branch: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(main)
        .args(["merge-tree", "--write-tree", "--name-only", "--no-messages", "HEAD", branch])
        .output()
        .ok()?;
    match out.status.code() {
        Some(1) => {
            let text = String::from_utf8_lossy(&out.stdout);
            let files: Vec<&str> = text.lines().skip(1).filter(|l| !l.trim().is_empty()).collect();
            Some(files.join(", "))
        }
        _ => None,
    }
}

pub fn is_merged(main: &Path, branch: &str) -> bool {
    git(main, &["merge-base", "--is-ancestor", branch, "HEAD"]).is_ok()
}

pub fn head(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "HEAD"]).ok()
}

pub fn clone(remote: &str, dest: &Path) -> Result<(), String> {
    let parent = dest.parent().ok_or_else(|| format!("{} has no parent folder", dest.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let out = Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg("--")
        .arg(remote)
        .arg(dest)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub fn free_bytes(dir: &Path) -> Option<u64> {
    let out = Command::new("df").args(["-Pk"]).arg(dir).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let kb: u64 = text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb * 1024)
}

pub fn diff_stat(dir: &Path, base: &str) -> Result<String, String> {
    git(dir, &["diff", "--stat", &format!("{base}...HEAD")])
}

pub fn push_remote(main: &Path) -> Option<String> {
    let remotes = git(main, &["remote"]).ok()?;
    let names: Vec<&str> = remotes.lines().map(str::trim).filter(|r| !r.is_empty()).collect();
    names.iter().find(|r| **r == "origin").or(names.first()).map(|r| r.to_string())
}

pub fn push(main: &Path, remote: &str, branch: &str) -> Result<(), String> {
    git(main, &["push", remote, branch]).map(|_| ())
}

pub fn in_remote(main: &Path) -> bool {
    let Some(remote) = push_remote(main) else { return false };
    let Some(branch) = current_branch(main) else { return false };
    let _ = git(main, &["fetch", "-q", &remote]);
    git(main, &["merge-base", "--is-ancestor", "HEAD", &format!("{remote}/{branch}")]).is_ok()
}

pub fn is_dirty(path: &Path) -> Result<bool, String> {
    Ok(!git(path, &["status", "--porcelain"])?.is_empty())
}

pub fn status_entries(path: &Path) -> Vec<(String, String)> {
    git(path, &["status", "--porcelain", "-uall"])
        .map(|o| {
            o.lines()
                .filter(|l| l.len() > 3)
                .map(|l| {
                    let rest = &l[3..];
                    let file = rest.rsplit(" -> ").next().unwrap_or(rest).trim_matches('"');
                    (l[..2].to_string(), file.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn stash_count(main: &Path) -> usize {
    git(main, &["stash", "list"]).map(|o| o.lines().count()).unwrap_or(0)
}

pub fn operation_in_progress(main: &Path) -> Option<&'static str> {
    let has = |name: &str| {
        git(main, &["rev-parse", "--git-path", name])
            .map(|p| {
                let p = PathBuf::from(p);
                if p.is_absolute() { p } else { main.join(p) }
            })
            .is_ok_and(|p| p.exists())
    };
    if has("MERGE_HEAD") {
        Some("a merge")
    } else if has("rebase-merge") || has("rebase-apply") {
        Some("a rebase")
    } else {
        None
    }
}

pub fn changed_files(main: &Path, branch: &str) -> Vec<String> {
    git(main, &["diff", "--name-only", &format!("HEAD...{branch}")])
        .map(|o| o.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn stash_all(main: &Path, message: &str, include_untracked: bool) -> Result<String, String> {
    let mut args = vec!["stash", "push"];
    if include_untracked {
        args.push("-u");
    }
    args.extend(["-m", message]);
    git(main, &args)
}

#[derive(Debug, Default, PartialEq)]
pub struct Blocking {
    pub tracked: Vec<String>,
    pub colliding: Vec<String>,
}

impl Blocking {
    pub fn is_empty(&self) -> bool {
        self.tracked.is_empty() && self.colliding.is_empty()
    }

    pub fn kind(&self) -> &'static str {
        if self.tracked.is_empty() { "untracked files that would be overwritten" } else { "tracked changes" }
    }

    pub fn paths(&self) -> Vec<String> {
        if self.tracked.is_empty() { self.colliding.clone() } else { self.tracked.clone() }
    }
}

pub fn blocking_dirt(main: &Path, branch: Option<&str>) -> Blocking {
    let tracked = git(main, &["status", "--porcelain", "-uno"])
        .map(|o| o.lines().filter(|l| l.len() > 3).map(|l| l[3..].trim_matches('"').to_string()).collect())
        .unwrap_or_default();
    let untracked: Vec<String> = git(main, &["ls-files", "--others", "--exclude-standard"])
        .map(|o| o.lines().map(str::to_string).collect())
        .unwrap_or_default();
    let added: Vec<String> = match branch {
        Some(b) => git(main, &["diff", "--name-only", "--diff-filter=A", &format!("HEAD...{b}")])
            .map(|o| o.lines().map(str::to_string).collect())
            .unwrap_or_default(),
        None => Vec::new(),
    };
    let colliding = untracked.into_iter().filter(|f| branch.is_none() || added.contains(f)).collect();
    Blocking { tracked, colliding }
}

const BIG_UNTRACKED_BYTES: u64 = 50 * 1024 * 1024;

fn size_of(path: &Path) -> u64 {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::read_dir(path)
            .map(|d| d.flatten().map(|e| size_of(&e.path())).sum())
            .unwrap_or(0),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

pub fn big_untracked(main: &Path) -> Vec<String> {
    git(main, &["status", "--porcelain"])
        .map(|o| {
            o.lines()
                .filter(|l| l.starts_with("?? "))
                .map(|l| l[3..].trim_matches('"').to_string())
                .filter(|f| size_of(&main.join(f.trim_end_matches('/'))) > BIG_UNTRACKED_BYTES)
                .collect()
        })
        .unwrap_or_default()
}

pub fn commit_paths(main: &Path, message: &str, paths: &[String]) -> Result<String, String> {
    let mut add = vec!["add", "-A"];
    if !paths.is_empty() {
        add.push("--");
        add.extend(paths.iter().map(String::as_str));
    }
    git(main, &add)?;
    git(main, &["commit", "-q", "-m", message])?;
    head(main).ok_or_else(|| "no head after the commit".to_string())
}

pub fn clean_dry_run(main: &Path, files: &[String]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut args = vec!["clean", "-n", "--"];
    args.extend(files.iter().map(String::as_str));
    git(main, &args).unwrap_or_default()
}

pub fn restore_tracked(main: &Path, files: &[String]) -> Result<(), String> {
    let mut args = vec!["checkout", "HEAD", "--"];
    args.extend(files.iter().map(String::as_str));
    git(main, &args).map(|_| ())
}

pub fn remove_untracked(main: &Path, files: &[String]) -> Result<(), String> {
    let mut args = vec!["clean", "-f", "--"];
    args.extend(files.iter().map(String::as_str));
    git(main, &args).map(|_| ())
}

pub fn remove_added(main: &Path, files: &[String]) -> Result<(), String> {
    let mut args = vec!["rm", "-f", "-q", "--"];
    args.extend(files.iter().map(String::as_str));
    git(main, &args).map(|_| ())
}

pub fn dirty_count(path: &Path) -> Result<usize, String> {
    Ok(git(path, &["status", "--porcelain"])?.lines().count())
}

pub fn newest_dirty_edit(path: &Path) -> i64 {
    let Ok(out) = git(path, &["status", "--porcelain", "-uall"]) else { return 0 };
    out.lines()
        .take(5000)
        .filter_map(|l| l.get(3..))
        .map(|p| p.rsplit(" -> ").next().unwrap_or(p).trim_matches('"'))
        .filter_map(|p| std::fs::metadata(path.join(p)).and_then(|m| m.modified()).ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .max()
        .unwrap_or(0)
}

pub fn commit_times(path: &Path, limit: usize) -> Vec<i64> {
    let n = format!("-{limit}");
    git(path, &["log", &n, "--format=%ct"])
        .map(|out| out.lines().filter_map(|l| l.trim().parse::<i64>().ok()).map(|t| t * 1000).collect())
        .unwrap_or_default()
}

pub fn valid_branch(main: &Path, branch: &str) -> bool {
    !branch.trim().is_empty() && git(main, &["check-ref-format", "--branch", branch]).is_ok()
}

#[cfg(test)]
pub mod testing {
    use std::path::Path;
    use std::process::Command;

    pub fn repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "commit.gpgsign", "false"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            let run =
                || Command::new("git").arg("-C").arg(dir).args(&args).output().expect("git runs");
            let mut out = run();
            for _ in 0..3 {
                if out.status.success() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                out = run();
            }
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parses_branches_detached_and_bare() {
        let text = "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\nworktree /r-det\nHEAD bbb\ndetached\n\nworktree /r-feat\nHEAD ccc\nbranch refs/heads/feature/x\n";
        let trees = parse_worktrees(text);
        assert_eq!(trees.len(), 3);
        assert_eq!(trees[0].branch.as_deref(), Some("main"));
        assert_eq!(trees[1].branch, None);
        assert_eq!(trees[1].head.as_deref(), Some("bbb"));
        assert_eq!(trees[2].branch.as_deref(), Some("feature/x"));
        let bare = parse_worktrees("worktree /b.git\nbare\n");
        assert!(bare[0].bare);
    }

    #[test]
    fn names_come_from_the_remote_then_the_folder() {
        assert_eq!(
            repo_name(Path::new("/x/local"), Some("git@github.com:v0l/printfarm.git")),
            "printfarm"
        );
        assert_eq!(
            repo_name(Path::new("/x/local"), Some("https://github.com/v0l/snort/")),
            "snort"
        );
        assert_eq!(repo_name(Path::new("/x/local"), None), "local");
    }

    #[test]
    fn worktrees_sit_beside_the_repo_not_inside_it() {
        assert_eq!(
            sibling_path(Path::new("/home/k/git/snort"), "fix/login"),
            PathBuf::from("/home/k/git/.worktrees/snort/snort-fix-login")
        );
    }

    #[test]
    fn a_leftover_worktree_is_recreated_when_clean_and_merged_reused_with_commits_and_refused_when_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        testing::repo(&main);
        let wt = add_worktree(&main, "issue-1", None).unwrap();
        let again = add_worktree(&main, "issue-1", None).unwrap();
        assert_eq!(again, wt, "clean and empty: recreated");
        assert!(again.is_dir());

        std::fs::write(wt.join("work.txt"), "x").unwrap();
        for args in [vec!["add", "."], vec!["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "work"]] {
            assert!(Command::new("git").arg("-C").arg(&wt).args(&args).status().unwrap().success());
        }
        let reused = add_worktree(&main, "issue-1", None).unwrap();
        assert_eq!(reused, wt);
        assert!(wt.join("work.txt").exists(), "real commits are kept");

        std::fs::write(wt.join("scratch"), "x").unwrap();
        let e = add_worktree(&main, "issue-1", None).unwrap_err();
        assert!(e.contains("uncommitted"), "{e}");
        assert!(wt.join("scratch").exists());
    }

    #[test]
    fn worktree_lifecycle_against_a_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        testing::repo(&main);
        assert_eq!(main_worktree(&main).unwrap().branch.as_deref(), Some("main"));
        assert!(valid_branch(&main, "fix-login"));
        assert!(!valid_branch(&main, "bad..name"));

        let wt = add_worktree(&main, "fix-login", None).unwrap();
        assert!(wt.is_dir());
        std::fs::write(wt.join("wip"), "x").unwrap();
        assert!(add_worktree(&main, "fix-login", None).is_err(), "the folder is taken");
        let trees = worktrees(&main).unwrap();
        assert_eq!(trees.len(), 2);
        assert_eq!(trees[1].branch.as_deref(), Some("fix-login"));
        assert_eq!(main_worktree(&wt).unwrap().path, trees[0].path, "found from inside a worktree");

        std::fs::write(wt.join("scratch.txt"), "x").unwrap();
        assert!(is_dirty(&wt).unwrap());
        assert!(remove_worktree(&main, &wt, false).is_err(), "dirty trees need force");
        remove_worktree(&main, &wt, true).unwrap();
        assert!(!wt.exists());
        assert!(branch_exists(&main, "fix-login"), "the branch outlives its worktree");
        let again = add_worktree(&main, "fix-login", None).unwrap();
        assert!(again.is_dir(), "an existing branch can be checked out again");
    }
}
