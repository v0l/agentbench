use crate::git;
use crate::store::Store;
use agentbench_core::{Id, KeptWorktree, Session, SessionId, SessionStatus, Worktree};
use std::path::Path;

pub const KEPT_SETTING: &str = "worktree_sweep_kept";
pub const SWEEP_EVERY_MS: i64 = 10 * 60_000;
pub const GRACE_MS: i64 = 30 * 60_000;

const BUILD_OUTPUT: [&str; 14] = [
    "target", "node_modules", "dist", "build", "out", ".venv", "venv", "__pycache__", ".pytest_cache",
    ".gradle", ".next", ".cache", ".turbo", "coverage",
];

#[derive(Debug, Default)]
pub struct Swept {
    pub removed: Vec<(Id, String)>,
    pub ended: Vec<SessionId>,
    pub pruned: Vec<(Id, String)>,
    pub kept: Vec<KeptWorktree>,
}

pub struct Sweep<'a> {
    pub now: i64,
    pub grace_ms: i64,
    pub running: &'a dyn Fn(SessionId) -> bool,
    pub worked: &'a dyn Fn(SessionId) -> bool,
}

fn is_build_output(file: &str) -> bool {
    let first = file.trim_end_matches('/').split('/').next().unwrap_or("");
    BUILD_OUTPUT.contains(&first)
}

fn kept(project: &agentbench_core::Project, tree: &Worktree, why: String) -> KeptWorktree {
    KeptWorktree {
        project_id: project.id,
        project: project.name.clone(),
        path: tree.path.clone(),
        branch: tree.branch.clone(),
        why,
    }
}

fn in_use(sessions: &[&Session], s: &Sweep, tree: &Worktree) -> Option<String> {
    if s.now - tree.created_at < s.grace_ms {
        return Some("it was made less than 30 minutes ago".into());
    }
    for o in sessions {
        if (s.running)(o.id) || o.status == SessionStatus::Running {
            return Some(format!("session {} is running in it", o.id));
        }
        if o.status == SessionStatus::Waiting {
            return Some(format!("session {} is waiting on an answer", o.id));
        }
        if o.parent.is_none() {
            return Some(format!("the user's chat {} uses it", o.id));
        }
        if (s.worked)(o.id) {
            return Some(format!("session {} is an autopilot worker", o.id));
        }
        if s.now - o.updated_at < s.grace_ms {
            return Some(format!("session {} was active in the last 30 minutes", o.id));
        }
    }
    None
}

pub fn sweep(store: &Store, s: &Sweep) -> Swept {
    let mut out = Swept::default();
    let sessions = store.sessions().unwrap_or_default();
    for project in store.projects().unwrap_or_default().iter().filter(|p| !p.builtin()) {
        let main = Path::new(&project.path);
        if !main.is_dir() {
            continue;
        }
        let gone: Vec<Worktree> = store
            .worktrees(Some(project.id))
            .unwrap_or_default()
            .into_iter()
            .filter(|w| !w.main && !Path::new(&w.path).exists())
            .collect();
        let listed_gone: Vec<String> = git::worktrees(main)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| !t.bare && !t.locked && !t.path.exists())
            .map(|t| t.path.to_string_lossy().into_owned())
            .collect();
        if !listed_gone.is_empty() && git::prune_worktrees(main).is_ok() {
            out.pruned.extend(listed_gone.into_iter().map(|p| (project.id, p)));
        }
        for w in &gone {
            let users: Vec<&Session> = sessions.iter().filter(|o| o.worktree_id == w.id).collect();
            if users.iter().all(|o| !(s.running)(o.id) && o.parent.is_some() && !(s.worked)(o.id)) {
                out.ended.extend(users.iter().map(|o| o.id));
                let _ = store.forget_worktree(w.id);
                if !out.pruned.iter().any(|(_, p)| *p == w.path) {
                    out.pruned.push((project.id, w.path.clone()));
                }
            }
        }
        let _ = crate::projects::sync(store, project.id, None);
        let Some(default) = git::default_branch(main, project.branch.as_deref()) else { continue };
        let locked: Vec<std::path::PathBuf> =
            git::worktrees(main).unwrap_or_default().into_iter().filter(|t| t.locked).map(|t| t.path).collect();
        for tree in store.worktrees(Some(project.id)).unwrap_or_default() {
            if tree.main || tree.missing || locked.iter().any(|l| Path::new(&tree.path) == l) {
                continue;
            }
            if let Some(k) = check(store, project, &tree, &default, &sessions, s, &mut out) {
                out.kept.push(k);
            }
        }
    }
    out
}

fn check(
    store: &Store,
    project: &agentbench_core::Project,
    tree: &Worktree,
    default: &str,
    sessions: &[Session],
    s: &Sweep,
    out: &mut Swept,
) -> Option<KeptWorktree> {
    let main = Path::new(&project.path);
    let path = Path::new(&tree.path);
    if tree.branch.as_deref() == Some(default) {
        return None;
    }
    let head = git::head(path)?;
    if !git::merged_into_default(main, &head, default) {
        let ahead = git::commits_ahead(main, &head, default);
        return Some(kept(project, tree, format!("not merged: {ahead} commits are not in {default}")));
    }
    let (tracked, untracked) = match git::tracked_and_untracked(path) {
        Ok(t) => t,
        Err(e) => return Some(kept(project, tree, format!("git status failed: {e}"))),
    };
    if !tracked.is_empty() {
        return Some(kept(project, tree, format!("uncommitted changes: {}", tracked.iter().take(5).cloned().collect::<Vec<_>>().join(", "))));
    }
    let stray: Vec<&String> = untracked.iter().filter(|f| !is_build_output(f)).collect();
    if !stray.is_empty() {
        return Some(kept(project, tree, format!("untracked files: {}", stray.iter().take(5).map(|f| f.as_str()).collect::<Vec<_>>().join(", "))));
    }
    let users: Vec<&Session> = sessions.iter().filter(|o| o.worktree_id == tree.id).collect();
    if let Some(why) = in_use(&users, s, tree) {
        return Some(kept(project, tree, format!("merged and clean, but {why}")));
    }
    if let Err(e) = crate::projects::remove_worktree(store, tree.id, !untracked.is_empty()) {
        return Some(kept(project, tree, format!("merged and clean, but removal failed: {}", e.0)));
    }
    out.ended.extend(users.iter().map(|o| o.id));
    if let Some(b) = tree.branch.as_deref() {
        let _ = git::delete_merged_branch(main, b, Some(default));
    }
    out.removed.push((project.id, tree.path.clone()));
    None
}

pub fn kept_text(kept: &[KeptWorktree], limit: usize) -> String {
    let mut out = format!("Worktrees the sweep kept ({}), never removed automatically:\n", kept.len());
    for k in kept.iter().take(limit) {
        out.push_str(&format!("- {}: {} ({})\n", k.project, k.path, k.why));
    }
    if kept.len() > limit {
        out.push_str(&format!("- and {} more\n", kept.len() - limit));
    }
    out
}

pub fn save_kept(store: &Store, kept: &[KeptWorktree]) {
    if let Ok(text) = serde_json::to_string(kept) {
        let _ = store.set_setting(KEPT_SETTING, &text);
    }
}

pub fn load_kept(store: &Store) -> Vec<KeptWorktree> {
    store
        .setting(KEPT_SETTING)
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    fn commit(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), "x").unwrap();
        git_in(dir, &["add", "."]);
        git_in(dir, &["commit", "-qm", file]);
    }

    fn tree(main: &Path, branch: &str) -> std::path::PathBuf {
        let wt = git::add_worktree(main, branch, None).unwrap();
        commit(&wt, &format!("{branch}.txt"));
        wt
    }

    #[test]
    fn the_sweep_removes_merged_clean_worktrees_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        git::testing::repo(&main);
        let store = Store::memory().unwrap();
        let project = crate::projects::add(&store, &main, None, false).unwrap();

        let merged = tree(&main, "merged");
        std::fs::create_dir_all(merged.join("target/debug")).unwrap();
        std::fs::write(merged.join("target/debug/app"), "bin").unwrap();
        let dirty = tree(&main, "dirty");
        let stray = tree(&main, "stray");
        let busy = tree(&main, "busy");
        let unmerged = tree(&main, "unmerged");
        let gone = tree(&main, "gone");
        for b in ["merged", "dirty", "stray", "busy", "unmerged", "gone"] {
            git_in(&main, &["merge", "-q", "--no-edit", b]);
        }
        commit(&unmerged, "more.txt");
        std::fs::write(dirty.join("dirty.txt"), "changed").unwrap();
        std::fs::write(stray.join("notes.md"), "mine").unwrap();
        std::fs::remove_dir_all(&gone).unwrap();

        crate::projects::sync(&store, project.id, None).unwrap();
        let by_path = |p: &Path| {
            store.worktrees(Some(project.id)).unwrap().into_iter().find(|w| Path::new(&w.path) == p.canonicalize().unwrap_or(p.to_path_buf())).unwrap()
        };
        let busy_tree = by_path(&busy);
        let worker = store.add_session(busy_tree.id, "echo", "busy", None, None).unwrap();
        let merged_tree = by_path(&merged);
        let finished = store.add_session(merged_tree.id, "echo", "finished", None, None).unwrap();
        let boss = store.add_session(store.main_worktree(project.id).unwrap().id, "echo", "boss", None, None).unwrap();
        store.set_parent(finished.id, Some(boss.id)).unwrap();
        store.set_parent(worker.id, Some(boss.id)).unwrap();

        let later = crate::store::now() + GRACE_MS * 2;
        let running = |id: SessionId| id == worker.id;
        let worked = |_: SessionId| false;
        let done = sweep(&store, &Sweep { now: later, grace_ms: GRACE_MS, running: &running, worked: &worked });

        assert!(!merged.exists(), "merged and clean, with only build output: removed");
        assert!(!git::branch_exists(&main, "merged"), "its branch is deleted");
        assert_eq!(done.ended, vec![finished.id], "the finished session in it is ended");
        assert!(store.session(finished.id).is_err());
        for kept in [&dirty, &stray, &busy, &unmerged] {
            assert!(kept.join(".git").exists(), "{} kept", kept.display());
        }
        assert!(store.session(worker.id).is_ok());
        let why = |p: &Path| {
            let p = p.canonicalize().unwrap().to_string_lossy().into_owned();
            done.kept.iter().find(|k| k.path == p).map(|k| k.why.clone()).unwrap_or_default()
        };
        assert!(why(&dirty).contains("uncommitted changes: dirty.txt"), "{:?}", done.kept);
        assert!(why(&stray).contains("untracked files: notes.md"), "{:?}", done.kept);
        assert!(why(&busy).contains("is running in it"), "{:?}", done.kept);
        assert!(why(&unmerged).contains("not merged: 1 commits"), "{:?}", done.kept);
        assert!(done.pruned.iter().any(|(_, p)| p.ends_with("r-gone")), "{:?}", done.pruned);
        let listed = git::worktrees(&main).unwrap();
        assert_eq!(listed.len(), 5, "main plus the four kept: {listed:?}");
        assert!(store.worktrees(Some(project.id)).unwrap().iter().all(|w| !w.path.ends_with("r-gone") && !w.path.ends_with("r-merged")));

        save_kept(&store, &done.kept);
        assert_eq!(load_kept(&store).len(), 4);
        let text = kept_text(&load_kept(&store), 20);
        assert!(text.contains("never removed automatically") && text.contains("r-unmerged"), "{text}");
    }

    #[test]
    fn the_sweep_leaves_a_fresh_worktree_and_a_user_chat_alone() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("r");
        git::testing::repo(&main);
        let store = Store::memory().unwrap();
        let project = crate::projects::add(&store, &main, None, false).unwrap();
        let fresh = git::add_worktree(&main, "fresh", None).unwrap();
        let chat = git::add_worktree(&main, "chat", None).unwrap();
        crate::projects::sync(&store, project.id, None).unwrap();
        let chat_tree = store
            .worktrees(Some(project.id))
            .unwrap()
            .into_iter()
            .find(|w| w.path.ends_with("r-chat"))
            .unwrap();
        store.add_session(chat_tree.id, "echo", "mine", None, None).unwrap();
        let no = |_: SessionId| false;
        let done = sweep(&store, &Sweep { now: crate::store::now(), grace_ms: GRACE_MS, running: &no, worked: &no });
        assert!(fresh.exists() && chat.exists());
        assert!(done.removed.is_empty());
        let done = sweep(&store, &Sweep { now: crate::store::now() + GRACE_MS * 2, grace_ms: GRACE_MS, running: &no, worked: &no });
        assert!(!fresh.exists(), "an old empty worktree is merged by definition");
        assert!(chat.exists(), "a user chat keeps its worktree");
        assert!(done.kept.iter().any(|k| k.why.contains("user's chat")), "{:?}", done.kept);
    }
}
