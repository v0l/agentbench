use super::grants::{Access, Grants};
use std::fs;
use std::path::{Component, Path, PathBuf};

const MAX_READ_BYTES: usize = 50_000;
const MAX_READ_LINES: usize = 2000;
const MAX_LIST: usize = 400;
const MAX_HITS: usize = 100;
const MAX_SEARCHED_BYTES: u64 = 1_000_000;
const SKIPPED: [&str; 4] = [".git", "target", "node_modules", ".DS_Store"];

#[derive(Clone, Debug)]
pub struct Workspace {
    root: PathBuf,
    grants: Grants,
}

impl Workspace {
    pub fn open(root: &Path) -> Result<Self, String> {
        fs::create_dir_all(root).map_err(|e| format!("{}: {e}", root.display()))?;
        let root = root.canonicalize().map_err(|e| format!("{}: {e}", root.display()))?;
        Ok(Self { root, grants: Grants::default() })
    }

    pub fn with_grants(mut self, grants: Grants) -> Self {
        self.grants = grants;
        self
    }

    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        self.place(path, Access::Read, false)
    }

    pub fn resolve_for(&self, path: &str, access: Access) -> Result<PathBuf, String> {
        self.place(path, access, true)
    }

    fn place(&self, path: &str, access: Access, granted: bool) -> Result<PathBuf, String> {
        let path = path.trim();
        if path.is_empty() {
            return Ok(self.root.clone());
        }
        let given = Path::new(path);
        let joined = if given.is_absolute() { given.to_path_buf() } else { self.root.join(given) };
        let mut out = PathBuf::new();
        for part in joined.components() {
            match part {
                Component::ParentDir => {
                    out.pop();
                }
                Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        let mut probe = out.clone();
        while !probe.exists() && probe.pop() {}
        let real = probe.canonicalize().map_err(|e| format!("{path}: {e}"))?;
        let write = access == Access::Write;
        let inside = out.starts_with(&self.root) && real.starts_with(&self.root);
        if inside || (granted && self.grants.covers(&real, write)) {
            return Ok(out);
        }
        let hint = match granted {
            true if write && self.grants.covers(&real, false) => {
                format!("{path} is shared read only; ask again with write=true to change it")
            }
            true => format!(
                "{path} is outside the workspace {}; request_access asks the user to share it",
                self.root.display()
            ),
            false => format!("{path} is outside the workspace {}", self.root.display()),
        };
        Err(hint)
    }

    pub fn shown(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let text = rel.to_string_lossy().replace('\\', "/");
        if text.is_empty() { ".".to_string() } else { text }
    }

    pub fn read(
        &self,
        path: &str,
        offset: Option<usize>,
        limit: Option<usize>,
    ) -> Result<String, String> {
        let file = self.resolve_for(path, Access::Read)?;
        if file.is_dir() {
            return Err(format!("{} is a folder, use list_files", self.shown(&file)));
        }
        let bytes = fs::read(&file).map_err(|e| format!("{}: {e}", self.shown(&file)))?;
        let text = String::from_utf8(bytes)
            .map_err(|_| format!("{} is not a text file", self.shown(&file)))?;
        let lines: Vec<&str> = text.lines().collect();
        let first = offset.unwrap_or(1).max(1);
        if first > lines.len().max(1) {
            return Err(format!("{} has {} lines", self.shown(&file), lines.len()));
        }
        let want = limit.unwrap_or(MAX_READ_LINES).min(MAX_READ_LINES);
        let mut out = String::new();
        let mut last = first - 1;
        for line in lines.iter().skip(first - 1).take(want) {
            if out.len() + line.len() + 1 > MAX_READ_BYTES && last >= first {
                break;
            }
            out.push_str(line);
            out.push('\n');
            last += 1;
        }
        if last < lines.len() {
            out.push_str(&format!(
                "\n[lines {first}-{last} of {}; call again with offset={} for more]\n",
                lines.len(),
                last + 1
            ));
        }
        Ok(out)
    }

    pub fn write(&self, path: &str, content: &str) -> Result<String, String> {
        let file = self.resolve_for(path, Access::Write)?;
        if file == self.root || file.is_dir() {
            return Err(format!("{} is a folder", self.shown(&file)));
        }
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", self.shown(dir)))?;
        }
        let existed = file.exists();
        fs::write(&file, content).map_err(|e| format!("{}: {e}", self.shown(&file)))?;
        let verb = if existed { "overwrote" } else { "created" };
        Ok(format!("{verb} {} ({} bytes)", self.shown(&file), content.len()))
    }

    pub fn edit(&self, path: &str, old: &str, new: &str, all: bool) -> Result<String, String> {
        if old.is_empty() {
            return Err("`old` is empty; use write_file to create or replace a file".into());
        }
        let file = self.resolve_for(path, Access::Write)?;
        let text = fs::read_to_string(&file).map_err(|e| format!("{}: {e}", self.shown(&file)))?;
        let found = text.matches(old).count();
        if found == 0 {
            return Err(format!(
                "`old` is not in {}; read_file it and copy it exactly",
                self.shown(&file)
            ));
        }
        if found > 1 && !all {
            return Err(format!(
                "`old` is in {} {found} times; add surrounding lines to make it unique, or pass all=true",
                self.shown(&file)
            ));
        }
        let changed = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
        fs::write(&file, changed).map_err(|e| format!("{}: {e}", self.shown(&file)))?;
        Ok(format!("edited {} ({found} replacement{})", self.shown(&file), plural(found)))
    }

    pub fn list(&self, path: Option<&str>, depth: Option<usize>) -> Result<String, String> {
        let start = self.resolve_for(path.unwrap_or(""), Access::Read)?;
        if !start.is_dir() {
            return Err(format!("{} is not a folder", self.shown(&start)));
        }
        let depth = depth.unwrap_or(2).clamp(1, 6);
        let mut lines = Vec::new();
        let mut more = false;
        self.walk(&start, depth, &mut |p, dir| {
            if lines.len() >= MAX_LIST {
                more = true;
                return false;
            }
            lines.push(format!("{}{}", self.shown(p), if dir { "/" } else { "" }));
            true
        });
        if lines.is_empty() {
            return Ok(format!("{} is empty", self.shown(&start)));
        }
        if more {
            lines.push(format!("[stopped at {MAX_LIST} entries; list a subfolder]"));
        }
        Ok(lines.join("\n"))
    }

    pub fn search(&self, pattern: &str, path: Option<&str>) -> Result<String, String> {
        let needle = pattern.to_lowercase();
        if needle.is_empty() {
            return Err("`pattern` is empty".into());
        }
        let start = self.resolve_for(path.unwrap_or(""), Access::Read)?;
        let mut hits = Vec::new();
        let mut more = false;
        let mut look = |file: &Path| {
            let Ok(meta) = fs::metadata(file) else { return };
            if meta.len() > MAX_SEARCHED_BYTES {
                return;
            }
            let Ok(text) = fs::read_to_string(file) else { return };
            for (n, line) in text.lines().enumerate() {
                if line.to_lowercase().contains(&needle) {
                    if hits.len() >= MAX_HITS {
                        more = true;
                        return;
                    }
                    let shown: String = line.trim().chars().take(200).collect();
                    hits.push(format!("{}:{}: {shown}", self.shown(file), n + 1));
                }
            }
        };
        if start.is_file() {
            look(&start);
        } else {
            self.walk(&start, 8, &mut |p, dir| {
                if !dir {
                    look(p);
                }
                true
            });
        }
        if hits.is_empty() {
            return Ok(format!("no match for {pattern:?}"));
        }
        if more {
            hits.push(format!("[stopped at {MAX_HITS} matches; narrow the search]"));
        }
        Ok(hits.join("\n"))
    }

    fn walk(&self, dir: &Path, depth: usize, visit: &mut dyn FnMut(&Path, bool) -> bool) -> bool {
        let Ok(read) = fs::read_dir(dir) else { return true };
        let mut entries: Vec<_> = read.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            if SKIPPED.iter().any(|s| name == *s) {
                continue;
            }
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            let is_dir = kind.is_dir();
            if !visit(&path, is_dir) {
                return false;
            }
            if is_dir && depth > 1 && !self.walk(&path, depth - 1, visit) {
                return false;
            }
        }
        true
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("bench-ws-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Workspace::open(&dir).unwrap()
    }

    #[test]
    fn paths_cannot_leave_the_workspace() {
        let ws = space("escape");
        assert!(ws.resolve("a/b.gcad").is_ok());
        assert!(ws.resolve("../x").is_err());
        assert!(ws.resolve("a/../../x").is_err());
        assert!(ws.resolve("/etc/passwd").is_err());
        assert!(ws.resolve("a/../b").is_ok());
        assert_eq!(ws.resolve("").unwrap(), ws.root());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_workspace_is_refused() {
        let ws = space("link");
        let outside = std::env::temp_dir().join(format!("bench-ws-out-{}", std::process::id()));
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, ws.root().join("door")).unwrap();
        assert!(ws.resolve("door/file").is_err());
        assert!(ws.write("door/file", "x").is_err());
        assert!(!outside.join("file").exists());
        let _ = fs::remove_dir_all(outside);
    }

    fn shared(name: &str) -> (Workspace, PathBuf) {
        let ws = space(name);
        let elsewhere =
            std::env::temp_dir().join(format!("bench-ws-{name}-else-{}", std::process::id()));
        let _ = fs::remove_dir_all(&elsewhere);
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("part.gcad"), "rect 1 1\n").unwrap();
        (ws, elsewhere.canonicalize().unwrap())
    }

    #[test]
    fn a_folder_outside_is_reachable_only_once_granted_and_only_as_far_as_granted() {
        let (ws, elsewhere) = shared("grant");
        let file = elsewhere.join("part.gcad");
        let file = file.to_str().unwrap();
        assert!(ws.read(file, None, None).unwrap_err().contains("request_access"));
        assert!(ws.list(Some(elsewhere.to_str().unwrap()), None).is_err());

        ws.grants().grant(elsewhere.clone(), false);
        assert_eq!(ws.read(file, None, None).unwrap(), "rect 1 1\n");
        assert!(ws.list(Some(elsewhere.to_str().unwrap()), None).unwrap().contains("part.gcad"));
        assert!(
            ws.search("rect", Some(elsewhere.to_str().unwrap())).unwrap().contains("part.gcad:1")
        );
        assert!(ws.write(file, "x").unwrap_err().contains("read only"));
        assert!(ws.edit(file, "rect", "circle", false).unwrap_err().contains("read only"));
        assert_eq!(fs::read_to_string(elsewhere.join("part.gcad")).unwrap(), "rect 1 1\n");

        ws.grants().grant(elsewhere.clone(), true);
        assert!(ws.edit(file, "rect", "circle", false).is_ok());
        assert!(ws.write(elsewhere.join("new.gcad").to_str().unwrap(), "x").is_ok());
        assert!(fs::read_to_string(elsewhere.join("part.gcad")).unwrap().contains("circle"));

        ws.grants().revoke(&elsewhere);
        assert!(ws.read(file, None, None).is_err());
        let _ = fs::remove_dir_all(elsewhere);
    }

    #[test]
    fn a_grant_does_not_reach_a_sibling_or_a_parent() {
        let (ws, elsewhere) = shared("sibling");
        let inner = elsewhere.join("inner");
        fs::create_dir_all(&inner).unwrap();
        fs::write(inner.join("a.txt"), "a").unwrap();
        ws.grants().grant(inner.clone(), true);
        assert!(ws.read(inner.join("a.txt").to_str().unwrap(), None, None).is_ok());
        assert!(ws.read(elsewhere.join("part.gcad").to_str().unwrap(), None, None).is_err());
        let up = format!("{}/../part.gcad", inner.display());
        assert!(ws.read(&up, None, None).is_err());
        let _ = fs::remove_dir_all(elsewhere);
    }

    #[cfg(unix)]
    #[test]
    fn a_link_inside_a_granted_folder_does_not_widen_the_grant() {
        let (ws, elsewhere) = shared("grantlink");
        let secret = std::env::temp_dir().join(format!("bench-ws-secret-{}", std::process::id()));
        fs::create_dir_all(&secret).unwrap();
        fs::write(secret.join("k.txt"), "KEY").unwrap();
        std::os::unix::fs::symlink(&secret, elsewhere.join("door")).unwrap();
        ws.grants().grant(elsewhere.clone(), true);
        let through = elsewhere.join("door").join("k.txt");
        assert!(ws.read(through.to_str().unwrap(), None, None).is_err());
        assert!(ws.write(elsewhere.join("door").join("n.txt").to_str().unwrap(), "x").is_err());
        let _ = (fs::remove_dir_all(elsewhere), fs::remove_dir_all(secret));
    }

    #[test]
    fn a_file_is_written_read_and_edited() {
        let ws = space("rw");
        assert!(
            ws.write("parts/plate.gcad", "rect 40 30\nextrude 3\n").unwrap().starts_with("created")
        );
        assert!(
            ws.write("parts/plate.gcad", "rect 40 30\nextrude 3\n")
                .unwrap()
                .starts_with("overwrote")
        );
        assert_eq!(ws.read("parts/plate.gcad", None, None).unwrap(), "rect 40 30\nextrude 3\n");
        assert!(ws.edit("parts/plate.gcad", "extrude 3", "extrude 5", false).is_ok());
        assert!(ws.read("parts/plate.gcad", None, None).unwrap().contains("extrude 5"));
        assert!(ws.edit("parts/plate.gcad", "nothing here", "x", false).is_err());
        assert!(ws.edit("parts/plate.gcad", "", "x", false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn listing_and_searching_do_not_follow_a_link_out() {
        let ws = space("walklink");
        let outside =
            std::env::temp_dir().join(format!("bench-ws-walk-out-{}", std::process::id()));
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "TOPSECRET").unwrap();
        std::os::unix::fs::symlink(&outside, ws.root().join("door")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), ws.root().join("peek.txt")).unwrap();
        ws.write("real.txt", "TOPSECRET too\n").unwrap();
        let listing = ws.list(None, None).unwrap();
        assert!(
            listing.contains("real.txt") && !listing.contains("door") && !listing.contains("peek")
        );
        let hits = ws.search("topsecret", None).unwrap();
        assert!(hits.contains("real.txt:1"), "{hits}");
        assert_eq!(hits.lines().count(), 1, "{hits}");
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn an_edit_that_could_hit_two_places_is_refused() {
        let ws = space("dup");
        ws.write("a.txt", "x\nx\n").unwrap();
        assert!(ws.edit("a.txt", "x", "y", false).unwrap_err().contains("2 times"));
        assert!(ws.edit("a.txt", "x", "y", true).unwrap().contains("2 replacements"));
        assert_eq!(ws.read("a.txt", None, None).unwrap(), "y\ny\n");
    }

    #[test]
    fn a_long_file_is_read_in_pages() {
        let ws = space("page");
        let body: String = (1..=3000).map(|n| format!("line {n}\n")).collect();
        ws.write("big.txt", &body).unwrap();
        let first = ws.read("big.txt", None, None).unwrap();
        assert!(first.contains("line 1\n"));
        assert!(first.contains("call again with offset="));
        let next = ws.read("big.txt", Some(2001), Some(5)).unwrap();
        assert!(next.starts_with("line 2001\n"));
        assert!(ws.read("big.txt", Some(9000), None).is_err());
    }

    #[test]
    fn listing_skips_build_folders_and_search_finds_lines() {
        let ws = space("list");
        ws.write("a/one.gcad", "let w=40\n").unwrap();
        ws.write("target/junk.txt", "let w=1\n").unwrap();
        ws.write("b.gasm", "mate W=9\n").unwrap();
        let listing = ws.list(None, None).unwrap();
        assert!(
            listing.contains("a/") && listing.contains("a/one.gcad") && listing.contains("b.gasm")
        );
        assert!(!listing.contains("target"));
        let hits = ws.search("LET W", None).unwrap();
        assert!(hits.contains("a/one.gcad:1: let w=40"));
        assert!(!hits.contains("target"));
        assert!(ws.search("zzz", None).unwrap().starts_with("no match"));
    }
}
