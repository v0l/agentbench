use crate::git;
use crate::store::{SeenTree, Store, StoreError};
use agentbench_core::{Id, Project, Worktree};
use std::path::{Path, PathBuf};

fn err(e: String) -> StoreError {
    StoreError(e)
}

fn expand(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        Err(_) => path.to_path_buf(),
    }
}

fn canonical(path: &Path) -> Result<PathBuf, StoreError> {
    let expanded = expand(path);
    expanded.canonicalize().map_err(|e| StoreError(format!("{}: {e}", path.display())))
}

fn seen(trees: &[git::Tree], main: &Path) -> Vec<SeenTree> {
    trees
        .iter()
        .filter(|t| !t.bare)
        .map(|t| SeenTree {
            path: t.path.to_string_lossy().into_owned(),
            branch: t.branch.clone(),
            head: t.head.clone(),
            main: t.path == main,
        })
        .collect()
}

pub fn add(
    store: &Store,
    path: &Path,
    name: Option<&str>,
    create: bool,
) -> Result<Project, StoreError> {
    if create {
        let dir = expand(path);
        if dir.exists() && !dir.is_dir() {
            return Err(StoreError(format!("{} is not a folder", dir.display())));
        }
        if git::main_worktree(&dir).is_err() {
            git::init(&dir).map_err(err)?;
        }
    }
    let dir = canonical(path)?;
    let main = git::main_worktree(&dir)
        .map_err(|e| StoreError(format!("{} is not a git repo: {e}", dir.display())))?;
    let main_path = main.path.to_string_lossy().into_owned();
    if let Some(existing) = store.project_by_path(&main_path)? {
        return Err(StoreError(format!("{} is already the project {}", main_path, existing.name)));
    }
    let remote = git::remote_url(&main.path);
    let name = name
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| git::repo_name(&main.path, remote.as_deref()));
    let project =
        store.add_project(&name, &main_path, main.branch.as_deref(), remote.as_deref())?;
    sync(store, project.id, None)?;
    store.project(project.id)
}

/// Make sure the home project exists, pointing at the user's home folder. A folder that is
/// already a project in its own right is left as it is and gets no home project.
pub fn ensure_home(store: &Store, dir: &Path) -> Result<Project, StoreError> {
    if let Some(p) = store.home_project()? {
        return Ok(p);
    }
    store.add_home("chats", &dir.to_string_lossy())
}

pub fn sync(store: &Store, project_id: Id, created: Option<&Path>) -> Result<bool, StoreError> {
    let project = store.project(project_id)?;
    if project.home {
        return Ok(false);
    }
    let main = PathBuf::from(&project.path);
    let trees = git::worktrees(&main).map_err(err)?;
    let created = created.map(|p| p.to_string_lossy().into_owned());
    store.sync_worktrees(project_id, &seen(&trees, &main), created.as_deref())
}

pub fn sync_all(store: &Store) -> Result<bool, StoreError> {
    let mut changed = false;
    for p in store.projects()? {
        match sync(store, p.id, None) {
            Ok(c) => changed |= c,
            Err(e) => tracing::warn!("could not read the worktrees of {}: {e}", p.name),
        }
    }
    Ok(changed)
}

pub fn add_worktree(
    store: &Store,
    project_id: Id,
    branch: &str,
    base: Option<&str>,
) -> Result<Worktree, StoreError> {
    let project = store.project(project_id)?;
    if project.home {
        return Err(StoreError("the home folder is not a git repo, so it has no worktrees".into()));
    }
    let main = PathBuf::from(&project.path);
    let branch = branch.trim();
    if !git::valid_branch(&main, branch) {
        return Err(StoreError(format!("{branch:?} is not a usable branch name")));
    }
    let path = git::add_worktree(&main, branch, base.map(str::trim).filter(|b| !b.is_empty()))
        .map_err(err)?;
    let path = canonical(&path)?;
    sync(store, project_id, Some(&path))?;
    store
        .worktrees(Some(project_id))?
        .into_iter()
        .find(|w| Path::new(&w.path) == path)
        .ok_or_else(|| StoreError(format!("git made {} but does not list it", path.display())))
}

pub fn remove_worktree(store: &Store, worktree_id: Id, force: bool) -> Result<(), StoreError> {
    let w = store.worktree(worktree_id)?;
    if w.main {
        return Err(StoreError("the main worktree is the project itself".into()));
    }
    let project = store.project(w.project_id)?;
    let path = Path::new(&w.path);
    if path.exists() {
        if !force && git::is_dirty(path).unwrap_or(false) {
            return Err(StoreError(format!("{} has uncommitted changes", w.label())));
        }
        git::remove_worktree(Path::new(&project.path), path, force).map_err(err)?;
    }
    store.forget_worktree(worktree_id)?;
    sync(store, w.project_id, None)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testing::repo;

    #[test]
    fn a_repo_becomes_a_project_with_its_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("snort");
        repo(&main);
        let side = git::add_worktree(&main, "existing", None).unwrap();

        let s = Store::memory().unwrap();
        let p = add(&s, &side, None, false).unwrap();
        assert_eq!(p.name, "snort", "added from inside a worktree, still names the repo");
        assert_eq!(Path::new(&p.path), main.canonicalize().unwrap());
        assert_eq!(p.branch.as_deref(), Some("main"));
        let trees = s.worktrees(Some(p.id)).unwrap();
        assert_eq!(trees.len(), 2);
        assert!(trees[0].main);
        assert!(!trees[1].created_by_agentbench, "an agent-made tree is adopted, not owned");
        assert!(add(&s, &main, None, false).is_err(), "the same repo twice");
    }

    #[test]
    fn the_home_project_is_made_once_and_is_not_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::memory().unwrap();
        let home = ensure_home(&s, dir.path()).unwrap();
        assert!(home.home);
        assert_eq!(home.name, "chats");
        assert_eq!(ensure_home(&s, dir.path()).unwrap().id, home.id, "made once");
        let tree = s.main_worktree(home.id).unwrap();
        assert_eq!(Path::new(&tree.path), dir.path());

        assert!(!sync(&s, home.id, None).unwrap(), "never scanned for worktrees");
        assert_eq!(s.worktrees(Some(home.id)).unwrap().len(), 1);
        assert!(add_worktree(&s, home.id, "x", None).is_err());
        assert!(s.delete_project(home.id).is_err(), "built in");

        let repo_dir = dir.path().join("r");
        repo(&repo_dir);
        add(&s, &repo_dir, None, false).unwrap();
        let names: Vec<_> = s.projects().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["chats", "r"], "the chats card is listed first");
    }

    #[test]
    fn a_home_folder_that_is_a_project_already_gets_no_home_project() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let s = Store::memory().unwrap();
        add(&s, dir.path(), None, false).unwrap();
        assert!(ensure_home(&s, dir.path()).is_err());
    }

    #[test]
    fn a_plain_folder_is_not_a_project() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::memory().unwrap();
        let e = add(&s, dir.path(), None, false).unwrap_err();
        assert!(e.0.contains("not a git repo"), "{e}");
    }

    #[test]
    fn a_new_folder_can_be_made_into_a_project() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("deep").join("idea");
        let s = Store::memory().unwrap();
        assert!(add(&s, &fresh, None, false).is_err(), "missing folders are not made unasked");
        assert!(!fresh.exists());

        let p = add(&s, &fresh, None, true).unwrap();
        assert_eq!(p.name, "idea");
        assert!(fresh.join(".git").exists());
        let w = add_worktree(&s, p.id, "first", None).unwrap();
        assert!(Path::new(&w.path).is_dir(), "the empty commit lets worktrees branch off");

        let again = dir.path().join("again");
        repo(&again);
        assert!(add(&s, &again, None, true).is_ok(), "an existing repo is left as it is");
    }

    #[test]
    fn worktrees_made_and_removed_through_agentbench() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        repo(&main);
        let s = Store::memory().unwrap();
        let p = add(&s, &main, Some("mine"), false).unwrap();
        assert_eq!(p.name, "mine");

        let w = add_worktree(&s, p.id, "fix-login", None).unwrap();
        assert!(w.created_by_agentbench);
        assert_eq!(w.branch.as_deref(), Some("fix-login"));
        assert!(Path::new(&w.path).is_dir());
        assert!(add_worktree(&s, p.id, "bad..name", None).is_err());

        std::fs::write(Path::new(&w.path).join("wip.txt"), "x").unwrap();
        assert!(remove_worktree(&s, w.id, false).unwrap_err().0.contains("uncommitted"));
        remove_worktree(&s, w.id, true).unwrap();
        assert!(!Path::new(&w.path).exists());
        assert_eq!(s.worktrees(Some(p.id)).unwrap().len(), 1);
        let main_tree = s.main_worktree(p.id).unwrap();
        assert!(remove_worktree(&s, main_tree.id, true).is_err());
    }

    #[test]
    fn a_tree_an_agent_makes_later_is_picked_up_by_sync() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        repo(&main);
        let s = Store::memory().unwrap();
        let p = add(&s, &main, None, false).unwrap();
        git::add_worktree(&main, "agent-made", None).unwrap();
        assert!(sync_all(&s).unwrap());
        let names: Vec<_> = s.worktrees(Some(p.id)).unwrap().iter().map(|w| w.label()).collect();
        assert_eq!(names, ["main", "agent-made"]);
    }
}
