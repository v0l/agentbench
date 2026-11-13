use super::files::{self, number};
use super::search::{EXCLUDE, INCLUDE, SKIPPED, flag, globs};
use agent_core::grants::Access;
use agent_core::workspace::Workspace;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::RegexBuilder;
use serde_json::Value;
use std::fs;

const MAX_FILES: usize = 500;
const MAX_FILE_BYTES: u64 = 5_000_000;
const SHOWN_CHANGES: usize = 40;
const PATTERN: [&str; 3] = ["pattern", "regex", "find"];
const REPLACEMENT: [&str; 4] = ["replacement", "replace", "with", "to"];

struct Change {
    line: usize,
    removed: String,
    added: String,
}

struct Rewrite {
    text: String,
    changes: Vec<Change>,
}

fn line_of(text: &str, byte: usize) -> usize {
    text[..byte].bytes().filter(|b| *b == b'\n').count() + 1
}

fn rewrite(re: &regex::Regex, text: &str, replacement: &str, limit: usize) -> Rewrite {
    let mut out = String::with_capacity(text.len());
    let mut changes = Vec::new();
    let mut last = 0;
    for caps in re.captures_iter(text) {
        let m = caps.get(0).unwrap();
        let mut added = String::new();
        caps.expand(replacement, &mut added);
        if added == m.as_str() {
            continue;
        }
        if changes.len() >= limit {
            break;
        }
        out.push_str(&text[last..m.start()]);
        out.push_str(&added);
        last = m.end();
        changes.push(Change {
            line: line_of(text, m.start()),
            removed: m.as_str().to_string(),
            added,
        });
    }
    out.push_str(&text[last..]);
    Rewrite { text: out, changes }
}

fn plural(n: usize, word: &str) -> String {
    format!(
        "{n} {word}{}",
        if n == 1 {
            ""
        } else if word.ends_with('h') {
            "es"
        } else {
            "s"
        }
    )
}

pub fn replace(ws: &Workspace, args: &Value) -> Result<String, String> {
    let pattern = files::text(args, &PATTERN).ok_or("missing `pattern`")?;
    if pattern.is_empty() {
        return Err("`pattern` is empty".into());
    }
    let replacement = args
        .as_object()
        .and_then(|o| REPLACEMENT.iter().find_map(|k| o.get(*k)))
        .and_then(Value::as_str)
        .ok_or("missing `replacement`; use an empty string to delete the matches")?;
    let literal = flag(args, &["literal", "fixed"]).unwrap_or(false);
    let dry_run = flag(args, &["dry_run", "preview"]).unwrap_or(false);
    let source = if literal { regex::escape(pattern) } else { pattern.to_string() };
    let re = RegexBuilder::new(&source)
        .case_insensitive(!flag(args, &["case_sensitive"]).unwrap_or(false))
        .multi_line(flag(args, &["multiline"]).unwrap_or(true))
        .dot_matches_new_line(flag(args, &["dot_all", "dotall"]).unwrap_or(false))
        .build()
        .map_err(|e| {
            format!(
                "`pattern` is not a valid regex ({e}); escape special characters with \\, or pass literal=true"
            )
        })?;
    let replacement =
        if literal { replacement.replace('$', "$$") } else { replacement.to_string() };
    let per_file =
        number(args, &["limit", "max_replacements"]).filter(|n| *n > 0).unwrap_or(usize::MAX);

    let path = files::text(args, &files::PATH);
    let start = ws.resolve_for(path.unwrap_or(""), Access::Write)?;
    let include = globs(args, &INCLUDE);
    let exclude = globs(args, &EXCLUDE);
    let mut walk = WalkBuilder::new(&start);
    walk.hidden(false)
        .require_git(false)
        .follow_links(false)
        .max_depth(Some(12))
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|e| !SKIPPED.iter().any(|s| e.file_name() == *s));
    if start.is_dir() && (!include.is_empty() || !exclude.is_empty()) {
        let mut o = OverrideBuilder::new(&start);
        for g in &include {
            o.add(g).map_err(|e| format!("bad include glob {g:?}: {e}"))?;
        }
        for g in &exclude {
            o.add(&format!("!{}", g.trim_start_matches('!')))
                .map_err(|e| format!("bad exclude glob {g:?}: {e}"))?;
        }
        walk.overrides(o.build().map_err(|e| format!("bad glob: {e}"))?);
    }

    let (mut report, mut files_changed, mut total, mut shown) = (Vec::new(), 0, 0, 0);
    let mut searched = 0;
    for entry in walk.build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let file = entry.path();
        if fs::metadata(file).map(|m| m.len() > MAX_FILE_BYTES).unwrap_or(true) {
            continue;
        }
        let Ok(bytes) = fs::read(file) else { continue };
        if bytes[..bytes.len().min(8000)].contains(&0) {
            continue;
        }
        let Ok(raw) = String::from_utf8(bytes) else { continue };
        searched += 1;
        let crlf = raw.contains("\r\n");
        let original = if crlf { raw.replace("\r\n", "\n") } else { raw };
        let done = rewrite(&re, &original, &replacement, per_file);
        if done.changes.is_empty() {
            continue;
        }
        if files_changed >= MAX_FILES {
            report.push(format!(
                "[stopped at {MAX_FILES} files; narrow the search with path or include]"
            ));
            break;
        }
        let name = ws.shown(file);
        if !dry_run {
            let content = if crlf { done.text.replace('\n', "\r\n") } else { done.text };
            fs::write(file, content).map_err(|e| format!("{name}: {e}"))?;
        }
        files_changed += 1;
        total += done.changes.len();
        report.push(format!("{name}: {}", plural(done.changes.len(), "replacement")));
        for c in &done.changes {
            if shown >= SHOWN_CHANGES {
                break;
            }
            shown += 1;
            report.push(format!("@@ line {}", c.line));
            for l in c.removed.lines() {
                report.push(format!("-{l}"));
            }
            for l in c.added.lines() {
                report.push(format!("+{l}"));
            }
        }
    }
    if total == 0 {
        return Ok(format!(
            "no match for {pattern:?} ({} searched), nothing changed",
            plural(searched, "file")
        ));
    }
    if total > shown {
        report.push(format!("... {} more not shown", total - shown));
    }
    let verb = if dry_run { "would replace" } else { "replaced" };
    let head = format!("{verb} {} in {}", plural(total, "match"), plural(files_changed, "file"));
    Ok(format!("{head}\n{}", report.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        let w = |p: &str, c: &str| {
            let f = dir.path().join(p);
            fs::create_dir_all(f.parent().unwrap()).unwrap();
            fs::write(f, c).unwrap();
        };
        w("a.rs", "let old_name = 1;\nold_name + 2\n");
        w("src/b.rs", "fn old_name() {}\n");
        w("src/c.txt", "old_name\n");
        w("crlf.rs", "old_name\r\nold_name\r\n");
        let ws = Workspace::open(dir.path()).unwrap();
        (dir, ws)
    }

    fn read(dir: &tempfile::TempDir, p: &str) -> String {
        fs::read_to_string(dir.path().join(p)).unwrap()
    }

    #[test]
    fn capture_groups_rewrite_one_file() {
        let (dir, w) = tree();
        let out = replace(
            &w,
            &json!({"path": "a.rs", "pattern": r"let (\w+) = (\d+)", "replacement": "const ${1}: u32 = $2", "case_sensitive": true}),
        )
        .unwrap();
        assert_eq!(read(&dir, "a.rs"), "const old_name: u32 = 1;\nold_name + 2\n");
        assert!(out.starts_with("replaced 1 match in 1 file"), "{out}");
        assert!(out.contains("@@ line 1\n-let old_name = 1\n+const old_name: u32 = 1"), "{out}");
    }

    #[test]
    fn a_folder_is_rewritten_through_include_globs() {
        let (dir, w) = tree();
        let out = replace(
            &w,
            &json!({"pattern": "old_name", "replacement": "new_name", "include": "*.rs", "path": "src"}),
        )
        .unwrap();
        assert_eq!(read(&dir, "src/b.rs"), "fn new_name() {}\n");
        assert_eq!(read(&dir, "src/c.txt"), "old_name\n");
        assert_eq!(read(&dir, "a.rs"), "let old_name = 1;\nold_name + 2\n");
        assert!(out.contains("1 match in 1 file"), "{out}");
    }

    #[test]
    fn dry_run_changes_nothing() {
        let (dir, w) = tree();
        let out = replace(&w, &json!({"pattern": "old_name", "replacement": "x", "dry_run": true}))
            .unwrap();
        assert!(out.starts_with("would replace 6 matches in 4 files"), "{out}");
        assert_eq!(read(&dir, "src/c.txt"), "old_name\n");
    }

    #[test]
    fn multi_line_patterns_and_crlf_files_keep_their_endings() {
        let (dir, w) = tree();
        replace(
            &w,
            &json!({"path": "crlf.rs", "pattern": "old_name\nold_name", "replacement": "one"}),
        )
        .unwrap();
        assert_eq!(read(&dir, "crlf.rs"), "one\r\n");
    }

    #[test]
    fn literal_text_does_not_expand_dollars_or_metacharacters() {
        let (dir, w) = tree();
        replace(&w, &json!({"path": "a.rs", "pattern": "old_name + 2", "replacement": "$1 (x)", "literal": true})).unwrap();
        assert_eq!(read(&dir, "a.rs"), "let old_name = 1;\n$1 (x)\n");
    }

    #[test]
    fn limit_caps_replacements_per_file_and_misses_are_plain() {
        let (dir, w) = tree();
        replace(
            &w,
            &json!({"path": "a.rs", "pattern": "old_name", "replacement": "n", "limit": 1}),
        )
        .unwrap();
        assert_eq!(read(&dir, "a.rs"), "let n = 1;\nold_name + 2\n");
        let out =
            replace(&w, &json!({"path": "a.rs", "pattern": "zzz", "replacement": "n"})).unwrap();
        assert!(out.contains("nothing changed"), "{out}");
        assert!(replace(&w, &json!({"pattern": "(", "replacement": "x"})).is_err());
    }
}
