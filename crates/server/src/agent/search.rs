//! `search_files`: grep for the agent, with a regex, include and exclude globs,
//! context lines, and the .gitignore rules a developer expects.

use super::files::{self, number};
use egui_bench::agent::grants::Access;
use egui_bench::agent::workspace::Workspace;
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use regex::RegexBuilder;
use serde_json::Value;
use std::fs;

const MAX_MATCHES: usize = 100;
const MAX_FILES: usize = 300;
const MAX_FILE_BYTES: u64 = 2_000_000;
const MAX_LINE_CHARS: usize = 300;
const MAX_CONTEXT: usize = 10;
const SKIPPED: [&str; 4] = [".git", "target", "node_modules", ".DS_Store"];

const PATTERN: [&str; 3] = ["pattern", "query", "regex"];
const INCLUDE: [&str; 5] = ["include", "glob", "globs", "file_pattern", "files"];
const EXCLUDE: [&str; 2] = ["exclude", "exclude_glob"];

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Content,
    Files,
    Count,
}

fn globs(args: &Value, keys: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        match args.get(*key) {
            Some(Value::String(s)) => out.extend(
                // a model may send "*.rs, *.toml"; braces such as {a,b} stay whole
                split_globs(s),
            ),
            Some(Value::Array(list)) => {
                out.extend(list.iter().filter_map(Value::as_str).flat_map(split_globs))
            }
            _ => {}
        }
    }
    out
}

fn split_globs(text: &str) -> Vec<String> {
    let (mut out, mut depth, mut cur) = (Vec::new(), 0, String::new());
    for c in text.chars() {
        match c {
            '{' => depth += 1,
            '}' if depth > 0 => depth -= 1,
            ',' | ' ' if depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn flag(args: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| args.get(*k)?.as_bool())
}

fn clip(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_string();
    }
    let head: String = line.chars().take(MAX_LINE_CHARS).collect();
    format!("{head}...")
}

pub fn search(ws: &Workspace, args: &Value) -> Result<String, String> {
    let pattern = files::text(args, &PATTERN).ok_or("missing `pattern`")?;
    if pattern.is_empty() {
        return Err("`pattern` is empty".into());
    }
    let source = if flag(args, &["literal", "fixed"]).unwrap_or(false) {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let case = flag(args, &["case_sensitive"]).unwrap_or(false);
    let re = RegexBuilder::new(&source)
        .case_insensitive(!case)
        .multi_line(true)
        .build()
        .map_err(|e| {
            format!(
                "`pattern` is not a valid regex ({e}); escape special characters with \\, or pass literal=true to match the text as written"
            )
        })?;
    let mode = match files::text(args, &["output", "mode", "output_mode"]).unwrap_or("content") {
        "files" | "files_with_matches" | "list" => Mode::Files,
        "count" => Mode::Count,
        _ => Mode::Content,
    };
    let context = number(args, &["context", "C", "-C"]).unwrap_or(0).min(MAX_CONTEXT);
    let before = number(args, &["before", "B", "-B"]).unwrap_or(context).min(MAX_CONTEXT);
    let after = number(args, &["after", "A", "-A"]).unwrap_or(context).min(MAX_CONTEXT);
    let cap = number(args, &["limit", "max_results", "head_limit"]).filter(|n| *n > 0);
    let cap = cap.unwrap_or(if mode == Mode::Content { MAX_MATCHES } else { MAX_FILES });

    let path = files::text(args, &files::PATH);
    let start = ws.resolve_for(path.unwrap_or(""), Access::Read)?;
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
            // a leading ! in an include is an exclusion, as it is in a .gitignore
            o.add(g).map_err(|e| format!("bad include glob {g:?}: {e}"))?;
        }
        for g in &exclude {
            o.add(&format!("!{}", g.trim_start_matches('!')))
                .map_err(|e| format!("bad exclude glob {g:?}: {e}"))?;
        }
        walk.overrides(o.build().map_err(|e| format!("bad glob: {e}"))?);
    }

    let context_on = before > 0 || after > 0;
    let mut out: Vec<String> = Vec::new();
    let (mut matches, mut matched_files, mut seen) = (0usize, 0usize, 0usize);
    let mut stopped = false;
    'files: for entry in walk.build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let file = entry.path();
        let Ok(meta) = fs::metadata(file) else { continue };
        if meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(file) else { continue };
        if bytes[..bytes.len().min(8000)].contains(&0) {
            continue;
        }
        seen += 1;
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let hit: Vec<usize> = (0..lines.len()).filter(|i| re.is_match(lines[*i])).collect();
        if hit.is_empty() {
            continue;
        }
        matched_files += 1;
        let shown = ws.shown(file);
        match mode {
            Mode::Files => out.push(shown),
            Mode::Count => out.push(format!("{shown}: {}", hit.len())),
            Mode::Content => {
                let room = cap - matches;
                if hit.len() > room {
                    stopped = true;
                }
                let hit = &hit[..hit.len().min(room)];
                matches += hit.len();
                // merge each hit's context window into runs of lines to print
                let mut runs: Vec<(usize, usize)> = Vec::new();
                for &i in hit {
                    let (from, to) = (i.saturating_sub(before), (i + after).min(lines.len() - 1));
                    match runs.last_mut() {
                        Some(last) if from <= last.1 + 1 => last.1 = last.1.max(to),
                        _ => runs.push((from, to)),
                    }
                }
                for (from, to) in runs {
                    if context_on && !out.is_empty() {
                        out.push("--".into());
                    }
                    for (j, line) in lines.iter().enumerate().take(to + 1).skip(from) {
                        let sep = if hit.binary_search(&j).is_ok() { ':' } else { '-' };
                        out.push(format!("{shown}{sep}{}{sep} {}", j + 1, clip(line)));
                    }
                }
                if stopped {
                    break 'files;
                }
            }
        }
        if mode != Mode::Content && matched_files >= cap {
            stopped = true;
            break;
        }
    }
    if matched_files == 0 {
        let scope =
            if include.is_empty() { String::new() } else { format!(" in {}", include.join(", ")) };
        return Ok(format!(
            "no match for {pattern:?}{scope} ({seen} file{} searched)",
            if seen == 1 { "" } else { "s" }
        ));
    }
    if stopped {
        out.push(format!(
            "[stopped at {cap} {}; narrow the search with path, include or a longer pattern]",
            match mode {
                Mode::Content => "matches",
                _ => "files",
            }
        ));
    }
    Ok(out.join("\n"))
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
        w("src/main.rs", "fn main() {\n    run();\n    // TODO later\n}\n");
        w("src/ui/view.tsx", "const x = 1;\n// todo: style\n");
        w("Cargo.toml", "name = \"todo-app\"\n");
        w("notes.md", "TODO write docs\n");
        w("target/out.rs", "TODO build output\n");
        w("skip/me.rs", "TODO ignored\n");
        w(".gitignore", "skip/\n");
        w("bin.dat", "TODO\0binary");
        let ws = Workspace::open(dir.path()).unwrap();
        (dir, ws)
    }

    #[test]
    fn include_globs_pick_files_by_name_or_path() {
        let (_d, ws) = tree();
        let out = search(&ws, &json!({"pattern": "todo", "include": ["*.rs"]})).unwrap();
        assert_eq!(out, "src/main.rs:3:     // TODO later");
        let out = search(&ws, &json!({"pattern": "todo", "include": "*.{tsx,md}"})).unwrap();
        assert_eq!(out, "notes.md:1: TODO write docs\nsrc/ui/view.tsx:2: // todo: style");
        let out = search(&ws, &json!({"pattern": "todo", "include": "src/ui/**"})).unwrap();
        assert_eq!(out, "src/ui/view.tsx:2: // todo: style");
        let out = search(&ws, &json!({"pattern": "todo", "include": "*.rs, *.md"})).unwrap();
        assert!(
            out.contains("main.rs") && out.contains("notes.md") && !out.contains("view"),
            "{out}"
        );
    }

    #[test]
    fn excludes_gitignore_target_and_binaries_are_skipped() {
        let (_d, ws) = tree();
        let out = search(&ws, &json!({"pattern": "todo", "output": "files"})).unwrap();
        assert_eq!(out, "Cargo.toml\nnotes.md\nsrc/main.rs\nsrc/ui/view.tsx");
        let out = search(
            &ws,
            &json!({"pattern": "todo", "output": "files", "exclude": ["*.md", "src/ui/**"]}),
        )
        .unwrap();
        assert_eq!(out, "Cargo.toml\nsrc/main.rs");
    }

    #[test]
    fn regex_case_literal_and_count() {
        let (_d, ws) = tree();
        let out =
            search(&ws, &json!({"pattern": "TODO|fn main", "output": "count", "path": "src"}))
                .unwrap();
        assert_eq!(out, "src/main.rs: 2\nsrc/ui/view.tsx: 1");
        let out = search(
            &ws,
            &json!({"pattern": "TODO", "case_sensitive": true, "output": "files", "path": "src"}),
        )
        .unwrap();
        assert_eq!(out, "src/main.rs");
        let out = search(&ws, &json!({"pattern": "run\\(\\);", "path": "src/main.rs"})).unwrap();
        assert_eq!(out, "src/main.rs:2:     run();");
        let out = search(&ws, &json!({"pattern": "main(", "literal": true})).unwrap();
        assert_eq!(out, "src/main.rs:1: fn main() {");
        assert!(search(&ws, &json!({"pattern": "main("})).unwrap_err().contains("literal=true"));
    }

    #[test]
    fn context_lines_merge_and_use_dashes() {
        let (_d, ws) = tree();
        let out =
            search(&ws, &json!({"pattern": "run|later", "include": "*.rs", "context": 1})).unwrap();
        assert_eq!(
            out,
            "src/main.rs-1- fn main() {\nsrc/main.rs:2:     run();\nsrc/main.rs:3:     // TODO later\nsrc/main.rs-4- }"
        );
        let out =
            search(&ws, &json!({"pattern": "fn main|^}", "path": "src/main.rs", "context": 0}))
                .unwrap();
        assert_eq!(out, "src/main.rs:1: fn main() {\nsrc/main.rs:4: }");
        let out = search(
            &ws,
            &json!({"pattern": "fn main|^}", "path": "src/main.rs", "before": 0, "after": 1}),
        )
        .unwrap();
        assert_eq!(
            out,
            "src/main.rs:1: fn main() {\nsrc/main.rs-2-     run();\n--\nsrc/main.rs:4: }"
        );
    }

    #[test]
    fn misses_and_limits_are_explained() {
        let (_d, ws) = tree();
        let out = search(&ws, &json!({"pattern": "zzz", "include": "*.rs"})).unwrap();
        assert!(out.starts_with("no match for \"zzz\" in *.rs (1 file searched)"), "{out}");
        let out = search(&ws, &json!({"pattern": "todo", "limit": 2})).unwrap();
        assert!(out.lines().count() == 3 && out.contains("[stopped at 2 matches"), "{out}");
        assert!(search(&ws, &json!({"pattern": "x", "path": "../.."})).is_err());
        assert!(search(&ws, &json!({"pattern": ""})).is_err());
    }
}
