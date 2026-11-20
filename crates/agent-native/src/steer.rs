use regex::Regex;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    NewFile,
    Append,
    Targeted,
    Regex,
}

const SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "mjs", "cjs", "ts", "tsx", "jsx", "toml", "json", "yaml", "yml", "md", "html",
    "css", "scss", "sh", "go", "c", "h", "cc", "cpp", "hpp", "java", "kt", "rb", "sql", "conf",
    "cfg", "ini", "env", "xml", "svelte", "vue", "lua", "swift", "zig", "gcad", "gasm",
];

const KEPT_OUT: &[&str] = &["/tmp/", "target/", "node_modules/", ".git/", "/dev/", "dist/", "build/"];
const LOG_ENDINGS: &[&str] = &[".log", ".out", ".pid", ".tmp", ".err"];

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("steer pattern"))
}

fn heredoc_tag() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r#"<<-?\s*['"]?([A-Za-z_][A-Za-z0-9_]*)['"]?"#)
}

fn split_heredocs(command: &str) -> (String, bool) {
    let mut head = String::new();
    let mut tag: Option<String> = None;
    let mut any = false;
    for line in command.lines() {
        if let Some(t) = &tag {
            if line.trim() == t {
                tag = None;
            }
            continue;
        }
        head.push_str(line);
        head.push('\n');
        if let Some(c) = heredoc_tag().captures(line) {
            tag = Some(c[1].to_string());
            any = true;
        }
    }
    (head, any)
}

fn kept_out(text: &str) -> bool {
    let t = text.trim_matches(|c| c == '"' || c == '\'');
    t.starts_with("/tmp")
        || t.starts_with("/var/tmp")
        || t.starts_with("$TMPDIR")
        || t.starts_with("${TMPDIR")
        || t.starts_with("~/.cache")
        || KEPT_OUT.iter().any(|k| t.contains(k))
        || LOG_ENDINGS.iter().any(|e| t.ends_with(e))
}

fn known_extension(target: &str) -> bool {
    let t = target.trim_matches(|c| c == '"' || c == '\'');
    t.rsplit_once('.').is_some_and(|(_, ext)| SOURCE_EXTENSIONS.contains(&ext))
}

fn feeds_a_program(head: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"\b(ssh|scp|kubectl|docker|podman|psql|mysql|sqlite3|adb|rsync|sudo|helm|nc)\b")
        .is_match(head)
}

fn producer_before(before: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    let segment = before.rsplit(['|', ';', '&']).next().unwrap_or(before);
    re(&R, r"\b(cat|printf|echo)\b").is_match(segment)
}

fn written_target(head: &str, heredoc: bool) -> Option<(Shape, String)> {
    static REDIRECT: OnceLock<Regex> = OnceLock::new();
    static TEE: OnceLock<Regex> = OnceLock::new();
    let redirect = re(&REDIRECT, r#"(?:^|[^<>&0-9\w=\-])(>>|>)\s*("[^"]+"|'[^']+'|[^\s&|;<>()"']+)"#);
    for c in redirect.captures_iter(head) {
        let whole = c.get(0).expect("whole match");
        let target = c[2].to_string();
        if kept_out(&target) {
            continue;
        }
        let before = &head[..whole.start()];
        if known_extension(&target) || heredoc || producer_before(before) {
            let shape = if &c[1] == ">>" { Shape::Append } else { Shape::NewFile };
            return Some((shape, target));
        }
    }
    let tee = re(&TEE, r#"\btee\s+((?:-[A-Za-z]+\s+)*)("[^"]+"|'[^']+'|[^\s&|;<>()"']+)"#);
    for c in tee.captures_iter(head) {
        let target = c[2].to_string();
        if kept_out(&target) {
            continue;
        }
        let before = &head[..c.get(0).expect("whole match").start()];
        if known_extension(&target) || heredoc || producer_before(before) {
            let shape = if c[1].contains('a') { Shape::Append } else { Shape::NewFile };
            return Some((shape, target));
        }
    }
    None
}

fn in_place(head: &str) -> Option<Shape> {
    static SED: OnceLock<Regex> = OnceLock::new();
    static PERL: OnceLock<Regex> = OnceLock::new();
    let sed = re(&SED, r"\bsed\s+(?:[^|;&\n]*\s)?(?:-[A-Za-z]*i\S*|--in-place\S*)(?:\s|$)");
    let perl = re(&PERL, r"\bperl\s+(?:[^|;&\n]*\s)?-[0-9plna]*i\S*(?:\s|$)");
    let hit = sed.find(head).or_else(|| perl.find(head))?;
    let begin = head[..hit.start()].rfind('\n').map_or(0, |i| i + 1);
    let end = head[hit.end()..].find('\n').map_or(head.len(), |i| hit.end() + i);
    let line = &head[begin..end];
    if KEPT_OUT.iter().any(|k| line.contains(k)) || LOG_ENDINGS.iter().any(|e| line.contains(e)) {
        return None;
    }
    let several = line.contains("find ") || line.contains("xargs") || line.contains("*");
    Some(if several { Shape::Regex } else { Shape::Targeted })
}

fn inline_code(head: &str, full: &str) -> Option<Shape> {
    static RUNNER: OnceLock<Regex> = OnceLock::new();
    static WRITES: OnceLock<Regex> = OnceLock::new();
    if !re(&RUNNER, r"\b(python3?|node|nodejs|ruby|deno|bun|perl)\b").is_match(head) {
        return None;
    }
    let writes = re(
        &WRITES,
        r#"open\s*\([^)]*,\s*['"][wax]\+?[bt]?['"]|mode\s*=\s*['"][wax]|\.write_(?:text|bytes)\s*\(|\b(?:writeFile|appendFile)(?:Sync)?\s*\(|File\.(?:write|open\([^)]*['"][wa]['"])"#,
    );
    if !writes.is_match(full) {
        return None;
    }
    if KEPT_OUT.iter().any(|k| full.contains(k)) || LOG_ENDINGS.iter().any(|e| full.contains(e)) {
        return None;
    }
    Some(if full.contains("re.sub") || full.contains(".replace(") {
        Shape::Regex
    } else {
        Shape::Targeted
    })
}

pub fn edit_shaped(command: &str) -> Option<Shape> {
    let (head, heredoc) = split_heredocs(command);
    if feeds_a_program(&head) {
        return None;
    }
    if let Some(shape) = in_place(&head) {
        return Some(shape);
    }
    if let Some(shape) = inline_code(&head, command) {
        return Some(shape);
    }
    written_target(&head, heredoc).map(|(shape, _)| shape)
}

const SLEEP_LIMIT_SECS: f64 = 30.0;

fn top_level_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == '\\' && q == '"' {
                    if let Some(n) = chars.next() {
                        current.push(n);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                '\\' => {
                    current.push(c);
                    if let Some(n) = chars.next() {
                        current.push(n);
                    }
                }
                ';' | '&' | '\n' | '|' | '(' | ')' | '{' | '}' => {
                    segments.push(std::mem::take(&mut current));
                }
                _ => current.push(c),
            },
        }
    }
    segments.push(current);
    segments
}

fn duration_secs(word: &str) -> Option<f64> {
    let (number, unit) = match word.char_indices().find(|(_, c)| c.is_alphabetic()) {
        Some((i, _)) => word.split_at(i),
        None => (word, "s"),
    };
    let n: f64 = number.parse().ok()?;
    let scale = match unit {
        "s" | "" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        _ => return None,
    };
    Some(n * scale)
}

fn sleep_secs(segment: &str) -> Option<f64> {
    let mut words = segment.split_whitespace().peekable();
    while matches!(words.peek(), Some(&("do" | "then" | "else" | "nohup" | "exec" | "time"))) {
        words.next();
    }
    if words.peek() == Some(&"timeout") {
        words.next();
        while words.peek().is_some_and(|w| w.starts_with('-')) {
            words.next();
        }
        words.next();
    }
    if words.next()? != "sleep" {
        return None;
    }
    let mut total = 0.0;
    let mut any = false;
    for w in words {
        total += duration_secs(w)?;
        any = true;
    }
    any.then_some(total)
}

pub fn sleep_wait(command: &str) -> Option<u64> {
    let (head, _) = split_heredocs(command);
    let longest = top_level_segments(&head)
        .iter()
        .filter_map(|s| sleep_secs(s))
        .fold(0.0_f64, f64::max);
    (longest >= SLEEP_LIMIT_SECS).then_some(longest as u64)
}

pub fn sleep_refusal(seconds: u64) -> String {
    format!(
        "Refused: `sleep {seconds}` to wait for work. Don't sleep to wait for work. Run it with `background: true` and wait with `bash_output` `until: \"exit\"` (or a regex such as \"test result:|error|FAILED\"), which wakes as soon as it finishes. Nothing was run."
    )
}

pub fn note(shape: Shape) -> &'static str {
    match shape {
        Shape::NewFile => {
            "[note: that command wrote a file from bash. Use write_file to create a file: the content is passed as is, nothing to quote or escape. It ran as usual. Next time use write_file, and edit_file's `then` to run a build or test straight after.]"
        }
        Shape::Append => {
            "[note: that command appended to a file from bash. Use edit_file to add to a file: it is reviewable, validated and shows a diff. It ran as usual. Next time use edit_file, and its `then` to run a build or test straight after.]"
        }
        Shape::Targeted => {
            "[note: that command edited a file from bash. Use edit_file for a targeted change: it checks the text matches, shows a diff and takes several files in one call. It ran as usual. Next time use edit_file, and its `then` to run a build or test straight after.]"
        }
        Shape::Regex => {
            "[note: that command rewrote files with a pattern from bash. Use replace_in_files for a regex change across files (dry_run previews it) or edit_file for exact text: both show what changed. It ran as usual.]"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_sleeps_used_as_waits_are_found() {
        for (command, secs) in [
            ("sleep 300; cat x", 300),
            ("nohup cargo test > log 2>&1 & sleep 300; cat log", 300),
            ("cargo build && sleep 5m && cat log", 300),
            ("timeout 400 sleep 100", 100),
            ("sleep 580; cat /tmp/pdl417/small.log", 580),
            ("sleep 240; ssh spark 'cat /tmp/retest-dry.log'", 240),
            ("sleep 30", 30),
            ("sleep 1m 30", 90),
        ] {
            assert_eq!(sleep_wait(command), Some(secs), "{command}");
        }
    }

    #[test]
    fn short_sleeps_scripts_and_remote_programs_are_left_alone() {
        for command in [
            "sleep 2",
            "sleep 0.5; echo hi",
            "while ! curl -s localhost:80; do sleep 1; done",
            "bash run.sh",
            "./wait-for-it.sh --timeout 300",
            "ssh spark 'sleep 600'",
            "echo \"sleep 300\"",
            "grep -rn 'sleep 100' src",
            "cat > /tmp/x.sh <<EOF\nsleep 300\nEOF",
        ] {
            assert_eq!(sleep_wait(command), None, "{command}");
        }
    }

    #[test]
    fn edit_shaped_commands_are_named() {
        let cases: &[(&str, Shape)] = &[
            ("sed -i 's/a/b/' src/main.rs", Shape::Targeted),
            ("sed -i.bak 's/a/b/' src/main.rs", Shape::Targeted),
            ("sed -ni '1,3p' notes.md", Shape::Targeted),
            ("cd x && sed --in-place 's/a/b/' lib.rs", Shape::Targeted),
            ("find . -name '*.rs' | xargs sed -i 's/a/b/'", Shape::Regex),
            ("perl -pi -e 's/a/b/' src/lib.rs", Shape::Targeted),
            ("perl -i -pe 's/a/b/' src/lib.rs", Shape::Targeted),
            ("cat > src/new.rs <<'EOF'\nfn main() {}\nEOF", Shape::NewFile),
            ("cat > notes <<EOF\nhello\nEOF\ncargo build", Shape::NewFile),
            ("cat >> Cargo.toml <<EOF\n[dep]\nEOF", Shape::Append),
            ("cat <<EOF > a.py\nprint(1)\nEOF", Shape::NewFile),
            ("echo 'x' > config.toml", Shape::NewFile),
            ("echo 'x' >> config.toml", Shape::Append),
            ("printf 'a' | tee src/a.rs", Shape::NewFile),
            ("echo hi | tee -a docs/x.md", Shape::Append),
            ("python3 -c \"open('a.rs','w').write('x')\"", Shape::Targeted),
            ("python3 - <<'PY'\nfrom pathlib import Path\nPath('a.rs').write_text('x')\nPY", Shape::Targeted),
            ("python3 - <<'PY'\nimport re\ns=open('a.rs').read()\nopen('a.rs','w').write(re.sub('a','b',s))\nPY", Shape::Regex),
            ("node -e \"require('fs').writeFileSync('a.js','x')\"", Shape::Targeted),
            ("ruby -e \"File.write('a.rb','x')\"", Shape::Targeted),
        ];
        for (command, shape) in cases {
            assert_eq!(edit_shaped(command), Some(*shape), "{command}");
        }
    }

    #[test]
    fn programs_builds_and_scratch_writes_are_left_alone() {
        for command in [
            "cargo build 2>&1 | tee build.log",
            "cargo test > /tmp/out.txt 2>&1",
            "cargo build > target/out.txt",
            "git commit -m \"$(cat <<'EOF'\nmessage\nEOF\n)\"",
            "psql mydb <<EOF\nselect 1;\nEOF",
            "kubectl apply -f - <<EOF\nkind: Pod\nEOF",
            "ssh host 'cat > /etc/x.conf' <<EOF\nx\nEOF",
            "docker exec -i c sh -c 'cat > /f.conf' < a.conf",
            "cat > /tmp/note.md <<EOF\nx\nEOF",
            "sed -i 's/a/b/' /tmp/x.rs",
            "sed -n '1,5p' src/main.rs",
            "perl -Mwarnings -e 'print 1'",
            "python3 -c \"open('/tmp/x','w').write('1')\"",
            "python3 -c \"print(open('a.rs').read())\"",
            "echo done >> run.log",
            "npm run build > node_modules/.cache/out.json",
            "ls -la > files.list",
            "cargo test 2>&1 >/dev/null",
            "echo a 2>&1",
            "git diff | head",
            "cat src/main.rs",
            "echo \"a -> b\"",
        ] {
            assert_eq!(edit_shaped(command), None, "{command}");
        }
    }
}
