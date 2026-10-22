use crate::git;
use crate::store::{SeenTree, Store, StoreError};
use foreman_core::{Id, Project, Worktree};
use std::path::{Path, PathBuf};

fn err(e: String) -> StoreError {
    StoreError(e)
}

fn canonical(path: &Path) -> Result<PathBuf, StoreError> {
    let expanded = match path.strip_prefix("~") {
        Ok(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        Err(_) => path.to_path_buf(),
    };
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

pub fn add(store: &Store, path: &Path, name: Option<&str>) -> Result<Project, StoreError> {
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

pub fn sync(store: &Store, project_id: Id, created: Option<&Path>) -> Result<bool, StoreError> {
    let project = store.project(project_id)?;
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
        let p = add(&s, &side, None).unwrap();
        assert_eq!(p.name, "snort", "added from inside a worktree, still names the repo");
        assert_eq!(Path::new(&p.path), main.canonicalize().unwrap());
        assert_eq!(p.branch.as_deref(), Some("main"));
        let trees = s.worktrees(Some(p.id)).unwrap();
        assert_eq!(trees.len(), 2);
        assert!(trees[0].main);
        assert!(!trees[1].created_by_foreman, "an agent-made tree is adopted, not owned");
        assert!(add(&s, &main, None).is_err(), "the same repo twice");
    }

    #[test]
    fn a_plain_folder_is_not_a_project() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::memory().unwrap();
        let e = add(&s, dir.path(), None).unwrap_err();
        assert!(e.0.contains("not a git repo"), "{e}");
    }

    #[test]
    fn worktrees_made_and_removed_through_foreman() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        repo(&main);
        let s = Store::memory().unwrap();
        let p = add(&s, &main, Some("mine")).unwrap();
        assert_eq!(p.name, "mine");

        let w = add_worktree(&s, p.id, "fix-login", None).unwrap();
        assert!(w.created_by_foreman);
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
        let p = add(&s, &main, None).unwrap();
        git::add_worktree(&main, "agent-made", None).unwrap();
        assert!(sync_all(&s).unwrap());
        let names: Vec<_> = s.worktrees(Some(p.id)).unwrap().iter().map(|w| w.label()).collect();
        assert_eq!(names, ["main", "agent-made"]);
    }
}
