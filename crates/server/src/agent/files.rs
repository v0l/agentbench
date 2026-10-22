use egui_bench::agent::grants::Access;
use egui_bench::agent::workspace::Workspace;
use serde_json::Value;
use std::fs;

const DIFF_LINES: usize = 60;

pub fn text<'a>(args: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| args.get(*k).and_then(Value::as_str))
}

pub fn number(args: &Value, keys: &[&str]) -> Option<usize> {
    keys.iter()
        .find_map(|k| {
            let v = args.get(*k)?;
            v.as_u64()
                .or_else(|| v.as_f64().map(|f| f as u64))
                .or_else(|| v.as_str()?.trim().parse().ok())
        })
        .map(|n| n as usize)
}

pub const PATH: [&str; 4] = ["path", "file_path", "filePath", "file"];
const OLD: [&str; 5] = ["old", "oldText", "old_text", "old_string", "old_str"];
const NEW: [&str; 5] = ["new", "newText", "new_text", "new_string", "new_str"];

pub fn args(raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    let parsed: Value = serde_json::from_str(raw).map_err(|e| {
        format!("the arguments were not valid JSON ({e}); send them again as one JSON object")
    })?;
    match parsed {
        Value::String(inner) => serde_json::from_str(&inner)
            .map_err(|e| format!("the arguments were a string, not a JSON object ({e})")),
        v => Ok(v),
    }
}

pub fn canonical(name: &str) -> &str {
    match name {
        "read" | "Read" | "view" => "read_file",
        "write" | "Write" | "create" => "write_file",
        "edit" | "Edit" | "str_replace" | "replace" => "edit_file",
        "grep" | "Grep" | "search" => "search_files",
        "ls" | "list" | "LS" => "list_files",
        "Bash" | "shell" | "sh" => "bash",
        other => other,
    }
}

struct Change {
    old: String,
    new: String,
    all: bool,
}

fn changes(args: &Value) -> Result<Vec<Change>, String> {
    let all = args
        .get("all")
        .or_else(|| args.get("replace_all"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut out = Vec::new();
    if let Some(list) = args.get("edits").and_then(Value::as_array) {
        for (i, e) in list.iter().enumerate() {
            let old = text(e, &OLD).ok_or_else(|| format!("edits[{i}] has no oldText"))?;
            let new = text(e, &NEW).unwrap_or_default();
            out.push(Change { old: old.to_string(), new: new.to_string(), all });
        }
    }
    if let Some(old) = text(args, &OLD) {
        out.push(Change {
            old: old.to_string(),
            new: text(args, &NEW).unwrap_or_default().to_string(),
            all,
        });
    }
    if out.is_empty() {
        return Err(
            "give `edits`: [{\"oldText\": ..., \"newText\": ...}], or `old` and `new`".into()
        );
    }
    if out.iter().any(|c| c.old.is_empty()) {
        return Err("an oldText is empty; to create or replace a whole file use write_file".into());
    }
    Ok(out)
}

fn fold(c: char) -> char {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        c => c,
    }
}

fn loose(text: &str) -> String {
    text.split('\n')
        .map(|l| l.trim_end().chars().map(fold).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn line_of(text: &str, byte: usize) -> usize {
    text[..byte].matches('\n').count()
}

struct Applied {
    text: String,
    line: usize,
    removed: Vec<String>,
    added: Vec<String>,
    count: usize,
    loose: bool,
}

fn apply(content: &str, change: &Change) -> Result<Applied, Option<String>> {
    let old = lf(&change.old);
    let new = lf(&change.new);
    let found = content.matches(&old).count();
    if found > 1 && !change.all {
        return Err(Some(format!(
            "oldText is in the file {found} times; add surrounding lines to make it unique, or pass all=true"
        )));
    }
    if found > 0 {
        let at = content.find(&old).expect("found above");
        let text =
            if change.all { content.replace(&old, &new) } else { content.replacen(&old, &new, 1) };
        return Ok(Applied {
            text,
            line: line_of(content, at),
            removed: old.lines().map(str::to_string).collect(),
            added: new.lines().map(str::to_string).collect(),
            count: found,
            loose: false,
        });
    }
    let base = loose(content);
    let target = loose(&old);
    if target.trim().is_empty() {
        return Err(None);
    }
    let hits = base.matches(&target).count();
    if hits == 0 {
        return Err(None);
    }
    if hits > 1 {
        return Err(Some(format!(
            "oldText matches {hits} places once trailing spaces and quotes are ignored; add surrounding lines"
        )));
    }
    let at = base.find(&target).expect("found above");
    let first = line_of(&base, at);
    let last = line_of(&base, at + target.len());
    let base_lines: Vec<&str> = base.split('\n').collect();
    let orig_lines: Vec<&str> = content.split('\n').collect();
    let start_byte: usize = base_lines[..first].iter().map(|l| l.len() + 1).sum();
    let block = base_lines[first..=last].join("\n");
    let rel = at - start_byte;
    let replaced = format!("{}{}{}", &block[..rel], new, &block[rel + target.len()..]);
    let mut out: Vec<String> = orig_lines[..first].iter().map(|s| s.to_string()).collect();
    out.push(replaced);
    out.extend(orig_lines[last + 1..].iter().map(|s| s.to_string()));
    Ok(Applied {
        text: out.join("\n"),
        line: first,
        removed: orig_lines[first..=last].iter().map(|s| s.to_string()).collect(),
        added: new.lines().map(str::to_string).collect(),
        count: 1,
        loose: true,
    })
}

fn nearest(content: &str, old: &str) -> Option<String> {
    let first = loose(old).lines().map(str::trim).find(|l| l.len() > 3)?.to_string();
    let hit = loose(content).lines().position(|l| l.trim() == first || l.contains(&first))?;
    Some(format!(
        " Its first line is at line {} but the lines after it differ; read_file around there and copy them exactly.",
        hit + 1
    ))
}

pub fn edit(ws: &Workspace, args: &Value) -> Result<String, String> {
    let path = text(args, &PATH).ok_or("missing `path`")?;
    let file = ws.resolve_for(path, Access::Write)?;
    let shown = ws.shown(&file);
    let raw = fs::read_to_string(&file).map_err(|e| format!("{shown}: {e}"))?;
    let crlf = raw.contains("\r\n");
    let original = lf(&raw);
    let changes = changes(args)?;
    for (i, c) in changes.iter().enumerate() {
        if let Err(why) = apply(&original, c) {
            let which = if changes.len() > 1 { format!("edits[{i}]: ") } else { String::new() };
            return Err(match why {
                Some(w) => format!("{which}{w} in {shown}"),
                None => format!(
                    "{which}oldText is not in {shown}.{}",
                    nearest(&original, &c.old)
                        .unwrap_or_else(|| " read_file it and copy the text exactly.".into())
                ),
            });
        }
    }
    let mut content = original.clone();
    let mut report = Vec::new();
    for (i, c) in changes.iter().enumerate() {
        match apply(&content, c) {
            Ok(a) => {
                content = a.text.clone();
                report.push(a);
            }
            Err(_) => {
                return Err(format!(
                    "edits[{i}] overlaps an earlier edit in {shown}; merge them into one edit. Nothing was changed."
                ));
            }
        }
    }
    let content = if crlf { content.replace('\n', "\r\n") } else { content };
    fs::write(&file, &content).map_err(|e| format!("{shown}: {e}"))?;
    let total: usize = report.iter().map(|a| a.count).sum();
    let mut out =
        format!("edited {shown}: {total} replacement{}", if total == 1 { "" } else { "s" });
    if report.iter().any(|a| a.loose) {
        out.push_str(" (matched ignoring trailing spaces and quote styles)");
    }
    let mut lines = 0;
    for a in &report {
        out.push_str(&format!("\n@@ line {}\n", a.line + 1));
        for l in &a.removed {
            if lines < DIFF_LINES {
                out.push_str(&format!("-{l}\n"));
            }
            lines += 1;
        }
        for l in &a.added {
            if lines < DIFF_LINES {
                out.push_str(&format!("+{l}\n"));
            }
            lines += 1;
        }
    }
    if lines > DIFF_LINES {
        out.push_str(&format!("... {} more diff lines\n", lines - DIFF_LINES));
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ws(content: &str) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), content).unwrap();
        let w = Workspace::open(dir.path()).unwrap();
        (dir, w)
    }

    fn read(dir: &tempfile::TempDir) -> String {
        fs::read_to_string(dir.path().join("a.rs")).unwrap()
    }

    #[test]
    fn every_argument_shape_a_model_sends_works() {
        for args in [
            json!({"path": "a.rs", "old": "one", "new": "ONE"}),
            json!({"path": "a.rs", "oldText": "one", "newText": "ONE"}),
            json!({"file_path": "a.rs", "old_string": "one", "new_string": "ONE"}),
            json!({"path": "a.rs", "edits": [{"oldText": "one", "newText": "ONE"}]}),
            json!({"path": "a.rs", "edits": [{"old": "one", "new": "ONE"}]}),
        ] {
            let (dir, w) = ws("one\ntwo\n");
            edit(&w, &args).unwrap_or_else(|e| panic!("{args}: {e}"));
            assert_eq!(read(&dir), "ONE\ntwo\n", "{args}");
        }
    }

    #[test]
    fn several_edits_are_matched_against_the_original_and_shown_as_a_diff() {
        let (dir, w) = ws("fn a() {}\nfn b() {}\nfn c() {}\n");
        let out = edit(
            &w,
            &json!({"path": "a.rs", "edits": [
                {"oldText": "fn a() {}", "newText": "fn a() { 1 }"},
                {"oldText": "fn c() {}", "newText": "fn c() { 3 }"}
            ]}),
        )
        .unwrap();
        assert_eq!(read(&dir), "fn a() { 1 }\nfn b() {}\nfn c() { 3 }\n");
        assert!(out.contains("2 replacements"), "{out}");
        assert!(out.contains("@@ line 3\n-fn c() {}\n+fn c() { 3 }"), "{out}");
    }

    #[test]
    fn trailing_spaces_and_smart_quotes_do_not_stop_a_match() {
        let (dir, w) = ws("let s = \"hi\";   \nlet t = 1;\n");
        let out = edit(&w, &json!({"path": "a.rs", "old": "let s = \u{201C}hi\u{201D};\nlet t = 1;", "new": "let s = \"yo\";\nlet t = 2;"})).unwrap();
        assert_eq!(read(&dir), "let s = \"yo\";\nlet t = 2;\n");
        assert!(out.contains("ignoring trailing spaces"), "{out}");
    }

    #[test]
    fn windows_line_endings_are_kept() {
        let (dir, w) = ws("a\r\nb\r\nc\r\n");
        edit(&w, &json!({"path": "a.rs", "old": "b\nc", "new": "B\nC"})).unwrap();
        assert_eq!(read(&dir), "a\r\nB\r\nC\r\n");
    }

    #[test]
    fn a_partial_line_match_keeps_the_rest_of_the_line() {
        let (dir, w) = ws("    let total = price * qty;  \n");
        edit(&w, &json!({"path": "a.rs", "old": "price * qty;", "new": "price * qty + tax;"}))
            .unwrap();
        assert_eq!(read(&dir), "    let total = price * qty + tax;  \n");
    }

    #[test]
    fn misses_say_where_to_look_and_change_nothing() {
        let (dir, w) = ws("fn main() {\n    run();\n}\n");
        let e =
            edit(&w, &json!({"path": "a.rs", "old": "fn main() {\n    start();\n}", "new": "x"}))
                .unwrap_err();
        assert!(e.contains("line 1"), "{e}");
        let e = edit(
            &w,
            &json!({"path": "a.rs", "edits": [{"oldText": "run();", "newText": "go();"}, {"oldText": "nope", "newText": "x"}]}),
        )
        .unwrap_err();
        assert!(e.starts_with("edits[1]"), "{e}");
        assert_eq!(read(&dir), "fn main() {\n    run();\n}\n", "a failed batch writes nothing");
    }

    #[test]
    fn duplicates_need_more_context_or_all() {
        let (dir, w) = ws("x\nx\n");
        assert!(
            edit(&w, &json!({"path": "a.rs", "old": "x", "new": "y"}))
                .unwrap_err()
                .contains("2 times")
        );
        edit(&w, &json!({"path": "a.rs", "old": "x", "new": "y", "all": true})).unwrap();
        assert_eq!(read(&dir), "y\ny\n");
    }

    #[test]
    fn overlapping_edits_are_refused() {
        let (dir, w) = ws("abc\n");
        let e = edit(&w, &json!({"path": "a.rs", "edits": [{"oldText": "ab", "newText": "X"}, {"oldText": "bc", "newText": "Y"}]})).unwrap_err();
        assert!(e.contains("overlaps"), "{e}");
        assert_eq!(read(&dir), "abc\n");
    }

    #[test]
    fn arguments_are_repaired_or_explained() {
        assert_eq!(args("").unwrap(), json!({}));
        assert_eq!(args(r#""{\"path\": \"a\"}""#).unwrap(), json!({"path": "a"}));
        assert!(args("{broken").unwrap_err().contains("not valid JSON"));
    }

    #[test]
    fn numbers_come_as_numbers_floats_or_strings() {
        assert_eq!(number(&json!({"offset": 5}), &["offset"]), Some(5));
        assert_eq!(number(&json!({"offset": 5.0}), &["offset"]), Some(5));
        assert_eq!(number(&json!({"offset": "5"}), &["offset"]), Some(5));
    }
}
