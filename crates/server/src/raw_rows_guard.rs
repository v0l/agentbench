use std::path::{Path, PathBuf};

const WINDOW: usize = 8;

const RAW_ROW_READERS: &[(&str, &str, &str)] = &[];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            sources(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn reads_a_variant(line: &str) -> bool {
    (line.contains("Entry::User {") || line.contains("Entry::Assistant {"))
        && (line.contains("matches!") || line.contains("=>") || line.contains("if let") || line.contains("let Entry"))
}

fn production_part(text: &str) -> &str {
    text.split("\n#[cfg(test)]\n").next().unwrap_or(text)
}

fn offenders(file: &str, text: &str) -> Vec<(usize, String)> {
    let text = production_part(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !reads_a_variant(line) {
            continue;
        }
        let window = &lines[i.saturating_sub(WINDOW)..=i];
        let reads_store = window.iter().any(|l| l.contains(".messages(") || l.contains("messages_page("));
        let goes_through_shown = window.iter().any(|l| l.contains("shown("));
        if reads_store && !goes_through_shown {
            let allowed = RAW_ROW_READERS.iter().any(|(f, needle, _)| file.ends_with(f) && line.contains(needle));
            if !allowed {
                found.push((i + 1, line.trim().to_string()));
            }
        }
    }
    found
}

#[test]
fn server_code_reads_user_and_assistant_rows_through_shown() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    let mut bad = Vec::new();
    for f in files {
        let name = f.strip_prefix(&root).unwrap().to_string_lossy().to_string();
        if name == "raw_rows_guard.rs" || name == "test_support.rs" || name.ends_with("_tests.rs") {
            continue;
        }
        for (line, text) in offenders(&name, &std::fs::read_to_string(&f).unwrap()) {
            bad.push(format!("{name}:{line}: {text}"));
        }
    }
    assert!(
        bad.is_empty(),
        "these read Entry::User or Entry::Assistant from raw store rows and miss Entry::Wire rows; wrap the rows in agentbench_core::shown(..) or add an allowlist entry with a reason:\n{}",
        bad.join("\n")
    );
}

#[test]
fn the_guard_flags_a_raw_read_and_accepts_a_shown_one() {
    let raw = "let rows = store.messages(id, 0).unwrap();\nrows.iter().any(|m| matches!(m.entry, Entry::User { .. }))\n";
    assert_eq!(offenders("x.rs", raw).len(), 1);
    let ok = "let rows = shown(&store.messages(id, 0).unwrap());\nrows.iter().any(|m| matches!(m.entry, Entry::User { .. }))\n";
    assert!(offenders("x.rs", ok).is_empty());
}

#[test]
fn every_allowlist_entry_names_a_reason() {
    assert!(RAW_ROW_READERS.iter().all(|(_, _, reason)| !reason.trim().is_empty()));
}
