use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const PATIENCE: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Read,
    Write,
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub path: PathBuf,
    pub write: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub id: u64,
    pub path: PathBuf,
    pub write: bool,
    pub reason: String,
}

#[derive(Debug)]
struct Waiting {
    request: Request,
    reply: oneshot::Sender<Decision>,
}

#[derive(Debug, Default)]
struct State {
    granted: Vec<Grant>,
    waiting: Vec<Waiting>,
    next: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Grants {
    state: Arc<Mutex<State>>,
}

impl Grants {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn covers(&self, real: &Path, write: bool) -> bool {
        self.lock().granted.iter().any(|g| real.starts_with(&g.path) && (!write || g.write))
    }

    pub fn list(&self) -> Vec<Grant> {
        self.lock().granted.clone()
    }

    pub fn pending(&self) -> Vec<Request> {
        self.lock().waiting.iter().map(|w| w.request.clone()).collect()
    }

    pub fn grant(&self, path: PathBuf, write: bool) {
        let mut state = self.lock();
        match state.granted.iter_mut().find(|g| g.path == path) {
            Some(existing) => existing.write |= write,
            None => state.granted.push(Grant { path, write }),
        }
    }

    pub fn revoke(&self, path: &Path) {
        self.lock().granted.retain(|g| g.path != path);
    }

    pub fn decide(&self, id: u64, decision: Decision) {
        let mut state = self.lock();
        let Some(at) = state.waiting.iter().position(|w| w.request.id == id) else { return };
        let Waiting { request, reply } = state.waiting.remove(at);
        let decision = match (decision, request.write) {
            (Decision::Write, false) => Decision::Read,
            (d, _) => d,
        };
        drop(state);
        match decision {
            Decision::Read => self.grant(request.path, false),
            Decision::Write => self.grant(request.path, true),
            Decision::Deny => {}
        }
        let _ = reply.send(decision);
    }

    fn withdraw(&self, id: u64) {
        self.lock().waiting.retain(|w| w.request.id != id);
    }

    pub async fn ask(
        &self,
        path: PathBuf,
        write: bool,
        reason: &str,
        stop: &CancellationToken,
        ring: &(dyn Fn() + Sync),
    ) -> Result<Decision, String> {
        let (reply, answer) = oneshot::channel();
        let id = {
            let mut state = self.lock();
            state.next += 1;
            let id = state.next;
            let request = Request { id, path, write, reason: reason.trim().to_string() };
            state.waiting.push(Waiting { request, reply });
            id
        };
        ring();
        tokio::select! {
            decision = answer => decision.map_err(|_| "the request was dropped".to_string()),
            _ = stop.cancelled() => {
                self.withdraw(id);
                Err("stopped".into())
            }
            _ = tokio::time::sleep(PATIENCE) => {
                self.withdraw(id);
                Err("the user did not answer in ten minutes".into())
            }
        }
    }
}

const SECRETS: [&str; 15] = [
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".password-store",
    ".netrc",
    ".config/gcloud",
    ".local/share/keyrings",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    "Library/Keychains",
    "Library/Application Support/Google/Chrome",
];

#[cfg(unix)]
const SYSTEM: [&str; 11] = [
    "/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/boot", "/proc", "/sys",
    "/System",
];

#[cfg(windows)]
const SYSTEM: [&str; 2] = ["C:\\Windows", "C:\\Program Files"];

#[cfg(not(any(unix, windows)))]
const SYSTEM: [&str; 0] = [];

fn tidy(path: &Path) -> String {
    path.display().to_string()
}

pub fn vet(
    given: &str,
    write: bool,
    app_config: Option<&Path>,
    workspace: &Path,
) -> Result<PathBuf, String> {
    let home = dirs::home_dir().and_then(|h| h.canonicalize().ok());
    vet_with(given, write, workspace, home.as_deref(), app_config)
}

pub fn vet_with(
    given: &str,
    write: bool,
    workspace: &Path,
    home: Option<&Path>,
    app_config: Option<&Path>,
) -> Result<PathBuf, String> {
    let text = given.trim();
    if text.is_empty() {
        return Err("give the path of a folder or file".into());
    }
    let expanded = match (text.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(text),
    };
    let absolute = if expanded.is_absolute() { expanded } else { workspace.join(expanded) };
    let real = absolute.canonicalize().map_err(|e| format!("{text}: {e}"))?;
    if real.parent().is_none() {
        return Err(format!("{} is the top of a drive and too broad to share", tidy(&real)));
    }
    let mut secrets: Vec<PathBuf> = Vec::new();
    if let Some(home) = home {
        secrets.extend(SECRETS.iter().map(|s| home.join(s)));
    }
    if let Some(config) = app_config {
        secrets.push(config.canonicalize().unwrap_or_else(|_| config.to_path_buf()));
    }
    for secret in &secrets {
        if secret.starts_with(&real) || real.starts_with(secret) {
            let why = match real == *home.unwrap_or(Path::new("")) {
                true => "the whole home folder holds credentials",
                false => "it holds or contains credentials",
            };
            return Err(format!("{} cannot be shared: {why}", tidy(&real)));
        }
    }
    if write && SYSTEM.iter().any(|s| real.starts_with(s)) {
        return Err(format!("{} is a system folder and cannot be written", tidy(&real)));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn asking(
        grants: &Grants,
        path: &str,
        write: bool,
        reason: &str,
        stop: &CancellationToken,
    ) -> std::thread::JoinHandle<Result<Decision, String>> {
        let (asker, stop, path, reason) =
            (grants.clone(), stop.clone(), PathBuf::from(path), reason.to_string());
        std::thread::spawn(move || {
            crate::runtime().block_on(asker.ask(path, write, &reason, &stop, &|| {}))
        })
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("grants-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn a_grant_covers_what_is_under_it_and_only_as_far_as_it_goes() {
        let grants = Grants::default();
        grants.grant(PathBuf::from("/data/parts"), false);
        assert!(grants.covers(Path::new("/data/parts"), false));
        assert!(grants.covers(Path::new("/data/parts/a/b.gcad"), false));
        assert!(!grants.covers(Path::new("/data/parts/a"), true));
        assert!(!grants.covers(Path::new("/data/partsy"), false));
        assert!(!grants.covers(Path::new("/data"), false));
        grants.grant(PathBuf::from("/data/parts"), true);
        assert!(grants.covers(Path::new("/data/parts/a"), true));
        assert_eq!(grants.list().len(), 1);
    }

    #[test]
    fn a_revoked_grant_stops_covering() {
        let grants = Grants::default();
        grants.grant(PathBuf::from("/data/parts"), true);
        grants.revoke(Path::new("/data/parts"));
        assert!(!grants.covers(Path::new("/data/parts/a"), false));
        assert!(grants.list().is_empty());
    }

    #[test]
    fn the_user_answering_wakes_the_request_and_records_the_grant() {
        let grants = Grants::default();
        let stop = CancellationToken::new();
        let waiting = asking(&grants, "/data/parts", true, " for the plate ", &stop);
        let request = loop {
            if let Some(r) = grants.pending().pop() {
                break r;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(request.path, PathBuf::from("/data/parts"));
        assert!(request.write);
        assert_eq!(request.reason, "for the plate");
        assert!(!grants.covers(Path::new("/data/parts/a"), false), "asking is not granting");
        grants.decide(request.id, Decision::Read);
        assert_eq!(waiting.join().unwrap(), Ok(Decision::Read));
        assert!(grants.covers(Path::new("/data/parts/a"), false));
        assert!(!grants.covers(Path::new("/data/parts/a"), true), "read only was all it got");
        assert!(grants.pending().is_empty());
    }

    #[test]
    fn a_denial_grants_nothing() {
        let grants = Grants::default();
        let stop = CancellationToken::new();
        let waiting = asking(&grants, "/data/parts", false, "", &stop);
        let id = loop {
            if let Some(r) = grants.pending().pop() {
                break r.id;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        grants.decide(id, Decision::Deny);
        assert_eq!(waiting.join().unwrap(), Ok(Decision::Deny));
        assert!(grants.list().is_empty());
    }

    #[test]
    fn write_cannot_be_granted_for_a_read_request() {
        let grants = Grants::default();
        let stop = CancellationToken::new();
        let waiting = asking(&grants, "/data/parts", false, "", &stop);
        let id = loop {
            if let Some(r) = grants.pending().pop() {
                break r.id;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        grants.decide(id, Decision::Write);
        assert_eq!(waiting.join().unwrap(), Ok(Decision::Read));
        assert!(!grants.covers(Path::new("/data/parts"), true));
    }

    #[test]
    fn stopping_withdraws_a_waiting_request() {
        let grants = Grants::default();
        let stop = CancellationToken::new();
        let waiting = asking(&grants, "/data/parts", false, "", &stop);
        while grants.pending().is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.cancel();
        assert_eq!(waiting.join().unwrap(), Err("stopped".to_string()));
        assert!(grants.pending().is_empty());
        assert!(grants.list().is_empty());
    }

    #[test]
    fn an_answer_for_a_request_that_is_gone_does_nothing() {
        let grants = Grants::default();
        grants.decide(99, Decision::Write);
        assert!(grants.list().is_empty());
    }

    #[test]
    fn credentials_the_top_of_a_drive_and_the_home_folder_are_refused() {
        let root = scratch("vet");
        let home = root.join("home");
        let work = root.join("work");
        let config = home.join(".config").join("my-app");
        for dir in [".ssh", ".config/my-app", ".aws", "projects"] {
            fs::create_dir_all(home.join(dir)).unwrap();
        }
        fs::create_dir_all(&work).unwrap();
        let vet = |p: &str, write: bool| vet_with(p, write, &work, Some(&home), Some(&config));

        assert!(vet("/", false).unwrap_err().contains("too broad"));
        assert!(vet(home.to_str().unwrap(), false).unwrap_err().contains("whole home folder"));
        assert!(vet(root.to_str().unwrap(), false).unwrap_err().contains("credentials"));
        assert!(vet(home.join(".ssh").to_str().unwrap(), false).is_err());
        assert!(vet(home.join(".aws").to_str().unwrap(), false).is_err());
        assert!(vet(home.join(".config").to_str().unwrap(), false).is_err());
        assert!(vet(config.to_str().unwrap(), true).is_err());
        assert!(vet("~", false).is_err());
        assert!(vet("~/.ssh", false).is_err());

        let projects = home.join("projects");
        assert_eq!(vet(projects.to_str().unwrap(), true).unwrap(), projects);
        assert_eq!(vet("~/projects", false).unwrap(), projects);
        assert_eq!(vet("../home/projects", false).unwrap(), projects);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_link_into_a_secret_folder_is_refused_by_where_it_really_is() {
        let root = scratch("vet-link");
        let home = root.join("home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(root.join("work")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(home.join(".ssh"), root.join("work").join("keys")).unwrap();
            let got = vet_with("keys", false, &root.join("work"), Some(&home), None);
            assert!(got.is_err(), "{got:?}");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_missing_folder_is_reported_not_granted() {
        let root = scratch("vet-missing");
        let got = vet_with("nowhere", false, &root, None, None);
        assert!(got.unwrap_err().contains("nowhere"));
        assert!(vet_with("  ", false, &root, None, None).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn system_folders_cannot_be_written_but_a_read_is_not_this_functions_business() {
        let root = scratch("vet-system");
        assert!(vet_with("/usr", true, &root, None, None).unwrap_err().contains("system folder"));
        assert!(vet_with("/usr/bin", true, &root, None, None).is_err());
        assert!(vet_with("/usr", false, &root, None, None).is_ok());
        let _ = fs::remove_dir_all(root);
    }
}
