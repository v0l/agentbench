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
        let mut args = vec!["-c", "user.name=foreman", "-c", "user.email=foreman@localhost"];
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
        return Err(format!("{} already exists", path.display()));
    }
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
    Ok(path)
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

pub fn is_dirty(path: &Path) -> Result<bool, String> {
    Ok(!git(path, &["status", "--porcelain"])?.is_empty())
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
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
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
    fn worktree_lifecycle_against_a_real_repo() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        testing::repo(&main);
        assert_eq!(main_worktree(&main).unwrap().branch.as_deref(), Some("main"));
        assert!(valid_branch(&main, "fix-login"));
        assert!(!valid_branch(&main, "bad..name"));

        let wt = add_worktree(&main, "fix-login", None).unwrap();
        assert!(wt.is_dir());
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
