use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agentbench_core::Id;

pub fn default_root() -> PathBuf {
    if let Some(dir) = crate::env_var("MEMORY") {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".agents").join("memory")
}

pub const GLOBAL: &str = "global";
const PROJECT_FILE: &str = ".project";
const HISTORY: &str = ".history";
pub const BACKOFF_GROWTH: usize = 1024;
pub const BACKOFF_MS: i64 = 6 * 3_600_000;
pub const CHANGED: &str = "changed since you read it";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project(Id),
}

pub struct Memory {
    root: PathBuf,
    owned: bool,
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    resolve: Mutex<()>,
    claims: Mutex<HashSet<String>>,
    attempted: Mutex<HashMap<String, (usize, i64)>>,
}

impl Drop for Memory {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

pub fn hash_note(body: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in body.trim().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.trim_matches('-').chars().take(60).collect();
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "note".into()
    } else {
        out
    }
}

fn parse(stem: &str, text: &str) -> (String, String) {
    if let Some(rest) = text.strip_prefix("# ") {
        let (head, body) = rest.split_once('\n').unwrap_or((rest, ""));
        let topic = head.trim();
        if !topic.is_empty() {
            return (topic.to_string(), body.trim().to_string());
        }
    }
    (stem.to_string(), text.trim().to_string())
}

fn render(topic: &str, body: &str) -> String {
    format!("# {topic}\n\n{}\n", body.trim())
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

struct NoteFile {
    path: PathBuf,
    topic: String,
    body: String,
    modified: std::time::SystemTime,
}

impl Memory {
    pub fn open(root: PathBuf) -> Memory {
        Memory::build(root, false)
    }

    pub fn temporary() -> Memory {
        let root = std::env::temp_dir().join(format!("agentbench-memory-{}", uuid::Uuid::new_v4()));
        Memory::build(root, true)
    }

    fn build(root: PathBuf, owned: bool) -> Memory {
        Memory {
            root,
            owned,
            locks: Mutex::new(HashMap::new()),
            resolve: Mutex::new(()),
            claims: Mutex::new(HashSet::new()),
            attempted: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn is_empty(&self) -> bool {
        match std::fs::read_dir(&self.root) {
            Ok(entries) => !entries.flatten().any(|e| {
                let path = e.path();
                path.is_dir() && (path.join(PROJECT_FILE).exists() || e.file_name() == GLOBAL)
            }),
            Err(_) => true,
        }
    }

    fn lock_for(&self, path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(|e| e.into_inner());
        locks.entry(path.to_path_buf()).or_default().clone()
    }

    fn folder(&self, scope: Scope, name: Option<&str>) -> Option<PathBuf> {
        let _guard = self.resolve.lock().unwrap_or_else(|e| e.into_inner());
        let id = match scope {
            Scope::Global => {
                let dir = self.root.join(GLOBAL);
                if name.is_some() {
                    std::fs::create_dir_all(&dir).ok()?;
                }
                return dir.is_dir().then_some(dir);
            }
            Scope::Project(id) => id.to_string(),
        };
        let mut taken = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                taken.push(entry.file_name().to_string_lossy().into_owned());
                let marker = std::fs::read_to_string(path.join(PROJECT_FILE)).unwrap_or_default();
                if marker.trim() == id {
                    return Some(path);
                }
            }
        }
        let name = name?;
        let mut folder = slug(name);
        if folder == GLOBAL || taken.contains(&folder) || folder.starts_with('.') {
            folder = format!("{folder}-{id}");
        }
        let dir = self.root.join(folder);
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::write(dir.join(PROJECT_FILE), format!("{id}\n")).ok()?;
        Some(dir)
    }

    fn files(&self, dir: &Path) -> Vec<NoteFile> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else { return out };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") || !path.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let (topic, body) = parse(&stem, &text);
            if body.is_empty() {
                continue;
            }
            let modified = entry.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            out.push(NoteFile { path, topic, body, modified });
        }
        out.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.topic.cmp(&b.topic)));
        out
    }

    pub fn list(&self, scope: Scope) -> Vec<(String, String)> {
        let Some(dir) = self.folder(scope, None) else { return Vec::new() };
        self.files(&dir).into_iter().map(|f| (f.topic, f.body)).collect()
    }

    pub fn hash(&self, scope: Scope, topic: &str) -> String {
        self.list(scope)
            .into_iter()
            .find(|(t, _)| t == topic)
            .map(|(_, b)| hash_note(&b))
            .unwrap_or_default()
    }

    fn path_for(&self, dir: &Path, topic: &str) -> PathBuf {
        let existing = self.files(dir).into_iter().find(|f| f.topic == topic);
        if let Some(f) = existing {
            return f.path;
        }
        let base = slug(topic);
        let mut candidate = dir.join(format!("{base}.md"));
        let mut n = 2;
        while candidate.exists() {
            candidate = dir.join(format!("{base}-{n}.md"));
            n += 1;
        }
        candidate
    }

    pub fn set(
        &self,
        scope: Scope,
        scope_name: &str,
        topic: &str,
        body: &str,
        based_on: Option<&str>,
    ) -> Result<bool, String> {
        let dir = self.folder(scope, Some(scope_name)).ok_or("memory folder unavailable")?;
        let path = self.path_for(&dir, topic);
        let lock = self.lock_for(&path);
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let current = self.files(&dir).into_iter().find(|f| f.path == path);
        let current_hash = current.as_ref().map(|f| hash_note(&f.body)).unwrap_or_default();
        if let Some(base) = based_on {
            if base.trim() != current_hash {
                return Err(format!(
                    "note `{topic}` changed since you read it (now {current_hash}); recall it again and retry with based_on set to the new hash"
                ));
            }
        } else if current.is_some() {
            return Err(format!(
                "note `{topic}` exists; pass based_on={current_hash} (from recall) to replace it"
            ));
        }
        let body = body.trim();
        if body.is_empty() {
            if current.is_none() {
                return Ok(false);
            }
            std::fs::remove_file(&path).map_err(err)?;
        } else {
            let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
            std::fs::write(&tmp, render(topic, body)).map_err(err)?;
            std::fs::rename(&tmp, &path).map_err(err)?;
        }
        Ok(true)
    }

    pub fn sizes(&self, scope: Scope) -> (usize, usize) {
        let notes = self.list(scope);
        (notes.iter().map(|(_, b)| b.len()).sum(), notes.iter().map(|(_, b)| b.len()).max().unwrap_or(0))
    }

    pub fn fingerprint(&self, scope: Scope) -> String {
        let mut notes = self.list(scope);
        notes.sort();
        hash_note(&notes.iter().map(|(t, b)| format!("{t}\n{b}\n")).collect::<String>())
    }

    pub fn claim(&self, key: &str) -> bool {
        self.claims.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string())
    }

    pub fn release(&self, key: &str) {
        self.claims.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    pub fn backed_off(&self, key: &str, size: usize, now: i64) -> bool {
        let tried = self.attempted.lock().unwrap_or_else(|e| e.into_inner()).get(key).copied();
        tried.is_some_and(|(was, at)| size < was + BACKOFF_GROWTH && now - at < BACKOFF_MS)
    }

    pub fn note_attempt(&self, key: &str, size: usize, now: i64) {
        self.attempted.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), (size, now));
    }

    fn history(dir: &Path) -> PathBuf {
        dir.join(HISTORY)
    }

    fn keep_copy(&self, dir: &Path, current: &[NoteFile]) -> Result<String, String> {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let version = format!("{millis}-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
        let target = Memory::history(dir).join(&version);
        std::fs::create_dir_all(&target).map_err(err)?;
        for f in current {
            let name = f.path.file_name().ok_or("note without a file name")?;
            std::fs::copy(&f.path, target.join(name)).map_err(err)?;
        }
        Ok(version)
    }

    pub fn versions(&self, scope: Scope) -> Vec<String> {
        let Some(dir) = self.folder(scope, None) else { return Vec::new() };
        let mut out: Vec<String> = std::fs::read_dir(Memory::history(&dir))
            .map(|r| r.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        out.sort();
        out.reverse();
        out
    }

    fn lock_all(&self, current: &[NoteFile]) -> Vec<Arc<Mutex<()>>> {
        let mut paths: Vec<&PathBuf> = current.iter().map(|f| &f.path).collect();
        paths.sort();
        paths.into_iter().map(|p| self.lock_for(p)).collect()
    }

    pub fn replace_all(
        &self,
        scope: Scope,
        expected: &[(String, String)],
        notes: &[(String, String)],
    ) -> Result<String, String> {
        let dir = self.folder(scope, None).ok_or("memory folder unavailable")?;
        let locks = self.lock_all(&self.files(&dir));
        let _held: Vec<_> = locks.iter().map(|l| l.lock().unwrap_or_else(|e| e.into_inner())).collect();
        let current = self.files(&dir);
        let moved = current.len() != expected.len()
            || expected.iter().any(|(topic, hash)| {
                !current.iter().any(|f| &f.topic == topic && &hash_note(&f.body) == hash)
            });
        if moved {
            return Err(format!("{CHANGED}: notes were edited while the compaction ran"));
        }
        let version = self.keep_copy(&dir, &current)?;
        for (topic, body) in notes {
            let existing = current.iter().find(|f| &f.topic == topic);
            if existing.is_some_and(|f| f.body.trim() == body.trim()) {
                continue;
            }
            let path = match existing {
                Some(f) => f.path.clone(),
                None => self.path_for(&dir, topic),
            };
            let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
            std::fs::write(&tmp, render(topic, body)).map_err(err)?;
            std::fs::rename(&tmp, &path).map_err(err)?;
        }
        for f in &current {
            if !notes.iter().any(|(t, _)| t == &f.topic) {
                std::fs::remove_file(&f.path).map_err(err)?;
            }
        }
        Ok(version)
    }

    pub fn restore(&self, scope: Scope, version: &str) -> Result<String, String> {
        let dir = self.folder(scope, None).ok_or("memory folder unavailable")?;
        if version.contains(['/', '\\']) || version.starts_with('.') {
            return Err(format!("no version {version}"));
        }
        let source = Memory::history(&dir).join(version);
        if !source.is_dir() {
            return Err(format!("no version {version}"));
        }
        let locks = self.lock_all(&self.files(&dir));
        let _held: Vec<_> = locks.iter().map(|l| l.lock().unwrap_or_else(|e| e.into_inner())).collect();
        let current = self.files(&dir);
        let kept = self.keep_copy(&dir, &current)?;
        for f in &current {
            std::fs::remove_file(&f.path).map_err(err)?;
        }
        for entry in std::fs::read_dir(&source).map_err(err)?.flatten() {
            std::fs::copy(entry.path(), dir.join(entry.file_name())).map_err(err)?;
        }
        Ok(kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing::project;
    use crate::store::Store;
    use crate::tools::StoreApi;
    use agent_native::Api;
    use serde_json::json;

    fn folder(root: &Path, id: Id) -> PathBuf {
        std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| std::fs::read_to_string(p.join(PROJECT_FILE)).is_ok_and(|s| s.trim() == id.to_string()))
            .unwrap()
    }

    #[test]
    fn remember_writes_a_markdown_file() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/a");
        let p = main.project_id;
        s.set_note(p, "Test Cmds", "cargo test", "agent", None, None).unwrap();
        let dir = folder(s.memory_root(), p);
        assert_eq!(std::fs::read_to_string(dir.join("test-cmds.md")).unwrap(), "# Test Cmds\n\ncargo test\n");
        assert_eq!(s.notes(p).unwrap(), [("Test Cmds".to_string(), "cargo test".to_string())]);
    }

    #[test]
    fn recall_reads_an_externally_edited_file() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/a");
        let p = main.project_id;
        s.set_note(p, "t", "one", "agent", None, None).unwrap();
        let dir = folder(s.memory_root(), p);
        std::fs::write(dir.join("t.md"), "# t\n\nedited by hand\n").unwrap();
        std::fs::write(dir.join("fresh.md"), "# fresh\n\nnew file\n").unwrap();
        let seen = s.notes(p).unwrap();
        assert!(seen.contains(&("t".to_string(), "edited by hand".to_string())), "{seen:?}");
        assert!(seen.contains(&("fresh".to_string(), "new file".to_string())), "{seen:?}");
        assert_eq!(s.note_hash(p, "t").unwrap(), hash_note("edited by hand"));
    }

    #[test]
    fn a_stale_based_on_is_refused_after_an_outside_edit() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/a");
        let p = main.project_id;
        s.set_note(p, "t", "one", "agent", None, None).unwrap();
        let read = s.note_hash(p, "t").unwrap();
        let dir = folder(s.memory_root(), p);
        std::fs::write(dir.join("t.md"), "# t\n\nsomeone else\n").unwrap();
        let refused = s.set_note(p, "t", "mine", "agent", None, Some(&read)).unwrap_err();
        assert!(refused.0.contains("changed since"), "{}", refused.0);
        assert!(s.set_note(p, "t", "mine", "agent", None, None).is_err());
        assert_eq!(s.notes(p).unwrap()[0].1, "someone else");
    }

    #[test]
    fn forgetting_deletes_the_file() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/a");
        let p = main.project_id;
        s.set_note(p, "t", "one", "agent", None, None).unwrap();
        let h = s.note_hash(p, "t").unwrap();
        assert!(s.set_note(p, "t", "", "agent", None, Some(&h)).unwrap());
        assert!(!folder(s.memory_root(), p).join("t.md").exists());
        assert!(s.notes(p).unwrap().is_empty());
    }

    #[test]
    fn a_renamed_project_keeps_its_notes() {
        let s = Store::memory().unwrap();
        let (proj, main) = project(&s, "/a");
        let p = main.project_id;
        s.set_note(p, "t", "one", "agent", None, None).unwrap();
        let patch = agentbench_core::ProjectPatch { name: Some("Renamed".into()), ..Default::default() };
        s.update_project(proj.id, &patch).unwrap();
        assert_eq!(s.notes(p).unwrap().len(), 1);
        let h = s.note_hash(p, "t").unwrap();
        s.set_note(p, "t", "two", "agent", None, Some(&h)).unwrap();
        let folders = std::fs::read_dir(s.memory_root()).unwrap().flatten().filter(|e| e.path().is_dir()).count();
        assert_eq!(folders, 1);
    }

    #[test]
    fn global_notes_live_in_the_global_folder() {
        let s = Store::memory().unwrap();
        s.set_global_note("style", "no em dashes").unwrap();
        assert!(s.memory_root().join("global/style.md").exists());
        assert_eq!(s.global_notes().unwrap(), [("style".to_string(), "no em dashes".to_string())]);
        s.set_global_note("style", "").unwrap();
        assert!(s.global_notes().unwrap().is_empty());
    }

    #[test]
    fn the_tools_see_notes_edited_on_disk() {
        let s = Store::memory().unwrap();
        let (_, main) = project(&s, "/a");
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let api = StoreApi::new(s.clone());
        api.call("remember", &json!({"topic": "t", "note": "one"}), me.id).unwrap();
        let dir = folder(s.memory_root(), main.project_id);
        std::fs::write(dir.join("t.md"), "# t\n\ntwo\n").unwrap();
        let seen = api.call("recall", &json!({}), me.id).unwrap();
        assert_eq!(seen[0]["note"], "two");
    }
}
