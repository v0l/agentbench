use crate::git;
use crate::store::Store;
use agentbench_core::{
    AddMode, AddProject, Change, Id, Project, Proposal, ProposalKind, ProposalState, SessionId,
};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

#[derive(Debug)]
pub enum Filed {
    Existing(Project),
    Pending { proposal: Id, request: AddProject },
}

#[derive(Debug)]
pub enum Outcome {
    Added(Project),
    Declined(AddProject),
}

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn resolve(home: &Path, raw: &str) -> Result<PathBuf, String> {
    let home = home.canonicalize().map_err(|e| format!("{}: {e}", home.display()))?;
    let expanded = match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if raw == "~" => home.clone(),
        None => PathBuf::from(raw),
    };
    if !expanded.is_absolute() {
        return Err(format!("{raw} is not an absolute path"));
    }
    if expanded.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("{raw} may not contain .."));
    }
    let mut existing = expanded.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Err(format!("{raw} does not exist"));
        };
        rest.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| format!("{raw} does not exist"))?;
    }
    let mut full = existing.canonicalize().map_err(|e| format!("{raw}: {e}"))?;
    for part in rest.into_iter().rev() {
        full.push(part);
    }
    if !full.starts_with(&home) {
        return Err(format!("{} is outside {}; only folders under the home folder can be projects", full.display(), home.display()));
    }
    Ok(full)
}

fn empty_or_missing(dir: &Path) -> Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !dir.is_dir() || entries.next().is_some() {
        return Err(format!("{} already exists and is not an empty folder", dir.display()));
    }
    Ok(())
}

pub fn clone_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('-') || raw.contains("::") || raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{raw:?} is not a git remote"));
    }
    if let Some((scheme, rest)) = raw.split_once("://") {
        if !matches!(scheme, "https" | "ssh") || rest.split('/').next().is_none_or(str::is_empty) || !rest.contains('/') {
            return Err(format!("{raw} is not an https or ssh git remote"));
        }
        return Ok(raw.to_string());
    }
    if let Some((user_host, path)) = raw.split_once(':') {
        let scp = user_host.contains('@') && !user_host.starts_with('@') && !user_host.contains('/') && !path.is_empty() && !path.starts_with('/');
        if scp {
            return Ok(raw.to_string());
        }
        return Err(format!("{raw} is not an https or ssh git remote"));
    }
    let host = raw.split('/').next().unwrap_or_default();
    if host.contains('.') && raw.matches('/').count() >= 2 && !raw.starts_with('.') {
        return Ok(format!("https://{raw}"));
    }
    Err(format!("{raw} is not an https or ssh git remote; use github.com/owner/repo, an https URL or an ssh URL"))
}

fn plain_name(name: &str) -> Result<String, String> {
    if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) {
        return Err(format!("{name:?} is not a usable project name"));
    }
    Ok(name.to_string())
}

fn existing(store: &Store, path: &Path) -> Result<Option<Project>, String> {
    store.project_by_path(&path.to_string_lossy()).map_err(|e| e.0)
}

fn build(store: &Store, home: &Path, args: &Value, requester: SessionId) -> Result<Result<AddProject, Project>, String> {
    let path = text(args, "path");
    let given_name = text(args, "name");
    let clone = text(args, "clone");
    let create = args.get("create").and_then(Value::as_bool).unwrap_or(false);
    let mut request = AddProject {
        mode: AddMode::Import,
        path: String::new(),
        name: String::new(),
        remote: None,
        branch: None,
        head: None,
        free_bytes: None,
        requester: Some(requester),
    };
    if let Some(raw) = clone {
        let remote = clone_url(&raw)?;
        let name = match given_name.clone() {
            Some(n) => plain_name(&n)?,
            None => plain_name(&git::repo_name(Path::new(""), Some(&remote)))?,
        };
        let dest = match path {
            Some(p) => resolve(home, &p)?,
            None => resolve(home, &format!("~/git/{}", plain_name(&git::repo_name(Path::new(""), Some(&remote)))?))?,
        };
        if let Some(p) = existing(store, &dest)? {
            return Ok(Err(p));
        }
        empty_or_missing(&dest)?;
        let mut probe = dest.as_path();
        while !probe.exists() {
            probe = probe.parent().unwrap_or(Path::new("/"));
        }
        request.mode = AddMode::Clone;
        request.free_bytes = git::free_bytes(probe);
        request.remote = Some(remote);
        request.path = dest.to_string_lossy().into_owned();
        request.name = name;
        return Ok(Ok(request));
    }
    let raw = path.ok_or("give a `path`, or a `clone` remote")?;
    let dir = resolve(home, &raw)?;
    if create {
        if let Some(p) = existing(store, &dir)? {
            return Ok(Err(p));
        }
        empty_or_missing(&dir)?;
        request.mode = AddMode::Create;
        request.name = match given_name {
            Some(n) => plain_name(&n)?,
            None => git::repo_name(&dir, None),
        };
        request.path = dir.to_string_lossy().into_owned();
        return Ok(Ok(request));
    }
    if !dir.is_dir() {
        return Err(format!("{} is not a folder", dir.display()));
    }
    let main = git::main_worktree(&dir).map_err(|e| format!("{} is not a git checkout: {e}", dir.display()))?;
    let main_path = main.path.canonicalize().unwrap_or(main.path.clone());
    if !main_path.starts_with(home.canonicalize().unwrap_or(home.to_path_buf())) {
        return Err(format!("{} is outside the home folder", main_path.display()));
    }
    if let Some(p) = existing(store, &main_path)? {
        return Ok(Err(p));
    }
    let remote = git::remote_url(&main.path);
    request.name = match given_name {
        Some(n) => plain_name(&n)?,
        None => git::repo_name(&main.path, remote.as_deref()),
    };
    request.path = main_path.to_string_lossy().into_owned();
    request.remote = remote;
    request.branch = main.branch;
    request.head = main.head;
    Ok(Ok(request))
}

pub fn summary(r: &AddProject) -> String {
    let from = r.remote.as_deref().map(|u| format!(" from {u}")).unwrap_or_default();
    match r.mode {
        AddMode::Import => format!("Add the existing checkout {} as project {}", r.path, r.name),
        AddMode::Create => format!("Create a new repo for project {} at {}", r.name, r.path),
        AddMode::Clone => format!("Clone{from} to {} as project {}", r.path, r.name),
    }
}

fn pending_for(store: &Store, path: &str) -> Option<Proposal> {
    store.proposals(ProposalState::Pending).ok()?.into_iter().find(|p| {
        p.changes.iter().any(|c| matches!(c, Change::AddProject { request } if request.path == path))
    })
}

pub fn file(store: &Store, home: &Path, args: &Value, requester: SessionId) -> Result<Filed, String> {
    let request = match build(store, home, args, requester)? {
        Ok(r) => r,
        Err(project) => return Ok(Filed::Existing(project)),
    };
    if let Some(p) = pending_for(store, &request.path) {
        let request = p
            .changes
            .iter()
            .find_map(|c| match c {
                Change::AddProject { request } => Some(request.clone()),
                _ => None,
            })
            .unwrap_or(request);
        return Ok(Filed::Pending { proposal: p.id, request });
    }
    let proposal = Proposal {
        id: 0,
        kind: ProposalKind::AddProject,
        summary: summary(&request),
        issues: Vec::new(),
        sessions: vec![requester.to_string()],
        projects: Vec::new(),
        changes: vec![Change::AddProject { request: request.clone() }],
        state: ProposalState::Pending,
        created_at: 0,
    };
    let fingerprint = format!("add_project:{}:{}", request.path, requester);
    let id = store.add_proposal(&fingerprint, &proposal).map_err(|e| e.0)?;
    Ok(Filed::Pending { proposal: id, request })
}

pub fn request_of(proposal: &Proposal) -> Option<&AddProject> {
    proposal.changes.iter().find_map(|c| match c {
        Change::AddProject { request } => Some(request),
        _ => None,
    })
}

pub fn run(store: &Store, home: &Path, request: &AddProject) -> Result<Project, String> {
    let dest = resolve(home, &request.path)?;
    let name = Some(request.name.as_str());
    match request.mode {
        AddMode::Import => crate::projects::add(store, &dest, name, false),
        AddMode::Create => crate::projects::add(store, &dest, name, true),
        AddMode::Clone => {
            if let Some(p) = existing(store, &dest)? {
                return Err(format!("{} is already the project {}", dest.display(), p.name));
            }
            empty_or_missing(&dest)?;
            let remote = request.remote.as_deref().ok_or("no remote to clone")?;
            git::clone(remote, &dest)?;
            crate::projects::add(store, &dest, name, false)
        }
    }
    .map_err(|e| e.0)
}

pub fn decide(store: &Store, home: &Path, id: Id, accept: bool) -> Result<Outcome, String> {
    let proposal = store.proposal(id).map_err(|e| e.0)?;
    let request = request_of(&proposal).ok_or("that is not an add project request")?.clone();
    if proposal.state != ProposalState::Pending {
        return Err("that proposal was already decided".into());
    }
    if !accept {
        store.decide_proposal(id, ProposalState::Rejected).map_err(|e| e.0)?;
        return Ok(Outcome::Declined(request));
    }
    let project = run(store, home, &request)?;
    store.decide_proposal(id, ProposalState::Accepted).map_err(|e| e.0)?;
    Ok(Outcome::Added(project))
}

pub fn requester_message(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Added(p) => format!(
            "The user confirmed: {} is now project {} (id {}) at {}.",
            p.name, p.name, p.id, p.path
        ),
        Outcome::Declined(r) => format!(
            "The user declined adding {} at {}. It was not added; do not ask again unless they say so.",
            r.name, r.path
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::testing::repo;
    use serde_json::json;

    struct Rig {
        store: Store,
        home: tempfile::TempDir,
        me: SessionId,
    }

    fn rig() -> Rig {
        let store = Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&store, "/elsewhere");
        let me = store.add_session(main.id, "native", "w", None, None).unwrap().id;
        Rig { store, home: tempfile::tempdir().unwrap(), me }
    }

    impl Rig {
        fn projects(&self) -> usize {
            self.store.projects().unwrap().len()
        }

        fn file(&self, args: Value) -> Result<Filed, String> {
            file(&self.store, self.home.path(), &args, self.me)
        }

        fn pending(&self, args: Value) -> Id {
            match self.file(args).unwrap() {
                Filed::Pending { proposal, .. } => proposal,
                Filed::Existing(_) => panic!("expected a pending request"),
            }
        }
    }

    #[test]
    fn filing_a_request_adds_nothing() {
        let r = rig();
        let dir = r.home.path().join("ngrind");
        repo(&dir);
        let before = r.projects();
        let id = r.pending(json!({"path": dir}));
        assert_eq!(r.projects(), before);
        let p = r.store.proposal(id).unwrap();
        assert_eq!(p.kind, ProposalKind::AddProject);
        let req = request_of(&p).unwrap();
        assert_eq!((req.mode, req.name.as_str()), (AddMode::Import, "ngrind"));
        assert_eq!(req.branch.as_deref(), Some("main"));
        assert!(req.head.is_some());
        assert_eq!(r.pending(json!({"path": dir})), id, "asking twice shows one card");
    }

    #[test]
    fn accepting_adds_the_project_once_and_a_second_accept_changes_nothing() {
        let r = rig();
        let dir = r.home.path().join("ngrind");
        repo(&dir);
        let id = r.pending(json!({"path": dir}));
        let before = r.projects();
        let Outcome::Added(p) = decide(&r.store, r.home.path(), id, true).unwrap() else { panic!() };
        assert_eq!(p.name, "ngrind");
        assert_eq!(r.projects(), before + 1);
        assert!(decide(&r.store, r.home.path(), id, true).is_err());
        assert_eq!(r.projects(), before + 1);
        assert!(requester_message(&Outcome::Added(p.clone())).contains(&format!("id {}", p.id)));
    }

    #[test]
    fn rejecting_adds_nothing() {
        let r = rig();
        let dir = r.home.path().join("ngrind");
        repo(&dir);
        let id = r.pending(json!({"path": dir}));
        let before = r.projects();
        assert!(matches!(decide(&r.store, r.home.path(), id, false).unwrap(), Outcome::Declined(_)));
        assert_eq!(r.projects(), before);
        assert!(decide(&r.store, r.home.path(), id, true).is_err(), "decided once");
        assert_eq!(r.projects(), before);
    }

    #[test]
    fn a_non_git_folder_and_a_path_outside_home_are_refused() {
        let r = rig();
        let plain = r.home.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert!(r.file(json!({"path": plain})).unwrap_err().contains("not a git"));
        let outside = tempfile::tempdir().unwrap();
        repo(&outside.path().join("x"));
        let err = r.file(json!({"path": outside.path().join("x")})).err().unwrap();
        assert!(err.contains("outside"), "{err}");
        assert!(r.file(json!({"path": r.home.path().join("..").join("x")})).is_err());
        assert!(r.file(json!({"path": "relative/dir"})).is_err());
        assert!(r.file(json!({"path": outside.path().join("new"), "create": true})).is_err());
    }

    #[test]
    fn a_symlink_out_of_home_is_refused() {
        let r = rig();
        let outside = tempfile::tempdir().unwrap();
        repo(&outside.path().join("x"));
        let link = r.home.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let err = r.file(json!({"path": link.join("x")})).err().unwrap();
        assert!(err.contains("outside"), "{err}");
        assert!(r.file(json!({"path": link.join("fresh"), "create": true})).is_err());
    }

    #[test]
    fn a_path_that_is_already_a_project_returns_it() {
        let r = rig();
        let dir = r.home.path().join("ngrind");
        repo(&dir);
        let project = crate::projects::add(&r.store, &dir, None, false).unwrap();
        let before = r.store.proposals(ProposalState::Pending).unwrap().len();
        let Filed::Existing(p) = r.file(json!({"path": dir})).unwrap() else { panic!() };
        assert_eq!(p.id, project.id);
        assert_eq!(r.store.proposals(ProposalState::Pending).unwrap().len(), before);
    }

    #[test]
    fn create_makes_the_repo_only_after_accept() {
        let r = rig();
        let dir = r.home.path().join("git").join("fresh");
        let id = r.pending(json!({"path": dir, "create": true, "name": "Fresh"}));
        assert!(!dir.exists());
        let Outcome::Added(p) = decide(&r.store, r.home.path(), id, true).unwrap() else { panic!() };
        assert_eq!(p.name, "Fresh");
        assert!(dir.join(".git").exists());
        let taken = r.home.path().join("taken");
        std::fs::create_dir_all(&taken).unwrap();
        std::fs::write(taken.join("f"), "x").unwrap();
        assert!(r.file(json!({"path": taken, "create": true})).is_err());
    }

    #[test]
    fn clone_happens_only_after_accept_and_defaults_under_git() {
        let r = rig();
        let src = r.home.path().join("src");
        repo(&src);
        let bare = r.home.path().join("remote.git");
        let status = std::process::Command::new("git")
            .args(["clone", "-q", "--bare"])
            .arg(&src)
            .arg(&bare)
            .status()
            .unwrap();
        assert!(status.success());
        let dest = r.home.path().join("git").join("remote");
        let request = AddProject {
            mode: AddMode::Clone,
            path: dest.to_string_lossy().into_owned(),
            name: "remote".into(),
            remote: Some(bare.to_string_lossy().into_owned()),
            branch: None,
            head: None,
            free_bytes: None,
            requester: Some(r.me),
        };
        let proposal = Proposal {
            id: 0,
            kind: ProposalKind::AddProject,
            summary: summary(&request),
            issues: vec![],
            sessions: vec![],
            projects: vec![],
            changes: vec![Change::AddProject { request }],
            state: ProposalState::Pending,
            created_at: 0,
        };
        let id = r.store.add_proposal("clone:t", &proposal).unwrap();
        assert!(!dest.exists(), "nothing is cloned before the user accepts");
        let Outcome::Added(p) = decide(&r.store, r.home.path(), id, true).unwrap() else { panic!() };
        assert!(dest.join(".git").exists());
        assert_eq!(Path::new(&p.path), dest.canonicalize().unwrap());
    }

    #[test]
    fn a_clone_request_clones_nothing_and_defaults_to_git_name() {
        let r = rig();
        let id = r.pending(json!({"clone": "github.com/v0l/nostr-ssr"}));
        let req = request_of(&r.store.proposal(id).unwrap()).unwrap().clone();
        assert_eq!(req.mode, AddMode::Clone);
        assert_eq!(req.name, "nostr-ssr");
        assert_eq!(req.remote.as_deref(), Some("https://github.com/v0l/nostr-ssr"));
        assert_eq!(
            Path::new(&req.path),
            r.home.path().canonicalize().unwrap().join("git").join("nostr-ssr")
        );
        assert!(req.free_bytes.is_some());
        assert!(!r.home.path().join("git").exists());
    }

    #[test]
    fn only_https_and_ssh_remotes_are_cloned() {
        for ok in [
            "https://github.com/v0l/snort.git",
            "ssh://git@github.com/v0l/snort.git",
            "git@github.com:v0l/snort.git",
            "github.com/v0l/snort",
        ] {
            assert!(clone_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "file:///etc",
            "http://github.com/a/b",
            "/home/x/repo",
            "../repo",
            "ext::sh -c id",
            "--upload-pack=x",
            "git://github.com/a/b",
            "",
            "repo",
        ] {
            assert!(clone_url(bad).is_err(), "{bad}");
        }
    }
}
