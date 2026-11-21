use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub argument_hint: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub source: String,
}

pub fn split_args(input: &str) -> Vec<String> {
    let chars: Vec<char> = input.chars().collect();
    let mut args = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let mut arg = String::new();
        let quote = chars[i];
        if (quote == '"' || quote == '\'')
            && let Some((text, end)) = quoted(&chars, i + 1, quote)
        {
            arg.push_str(&text);
            i = end;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            arg.push(chars[i]);
            i += 1;
        }
        args.push(arg);
    }
    args
}

fn quoted(chars: &[char], from: usize, quote: char) -> Option<(String, usize)> {
    let mut text = String::new();
    let mut i = from;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1) == Some(&quote) {
            text.push(quote);
            i += 2;
        } else if chars[i] == quote {
            return Some((text, i + 1));
        } else {
            text.push(chars[i]);
            i += 1;
        }
    }
    None
}

pub fn expand(body: &str, args: &[String]) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        match substitution(&chars, i, args) {
            Some((text, end)) => {
                out.push_str(&text);
                i = end;
            }
            None => {
                out.push('$');
                i += 1;
            }
        }
    }
    out
}

fn substitution(chars: &[char], at: usize, args: &[String]) -> Option<(String, usize)> {
    let rest: String = chars[at + 1..].iter().collect();
    let width = |s: &str| s.chars().count();
    if rest.starts_with("ARGUMENTS") {
        return Some((args.join(" "), at + 1 + width("ARGUMENTS")));
    }
    if rest.starts_with('@') {
        return Some((args.join(" "), at + 2));
    }
    if rest.starts_with(|c: char| c.is_ascii_digit()) {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        let n: usize = digits.parse().ok()?;
        let value = n.checked_sub(1).and_then(|i| args.get(i)).cloned().unwrap_or_default();
        return Some((value, at + 1 + digits.len()));
    }
    let inner_end = rest.strip_prefix('{').and_then(|r| r.find('}'))?;
    let inner = &rest[1..1 + inner_end];
    let end = at + 1 + width(&rest[..inner_end + 2]);
    let value = braced(inner, args)?;
    Some((value, end))
}

fn braced(inner: &str, args: &[String]) -> Option<String> {
    if let Some(default) = inner.strip_prefix("@:-") {
        let all = args.join(" ");
        return Some(if all.is_empty() { default.to_string() } else { all });
    }
    if let Some(range) = inner.strip_prefix("@:") {
        let mut parts = range.splitn(2, ':');
        let start: usize = parts.next()?.parse().ok()?;
        let len: Option<usize> = match parts.next() {
            Some(l) => Some(l.parse().ok()?),
            None => None,
        };
        let from = start.saturating_sub(1).min(args.len());
        let to = len.map_or(args.len(), |l| (from + l).min(args.len()));
        return Some(args[from..to].join(" "));
    }
    let (n, default) = inner.split_once(":-")?;
    let n: usize = n.parse().ok()?;
    let value = n.checked_sub(1).and_then(|i| args.get(i)).filter(|v| !v.is_empty());
    Some(value.cloned().unwrap_or_else(|| default.to_string()))
}

pub fn parse_template(name: &str, source: &str, content: &str) -> Template {
    let content = content.replace("\r\n", "\n");
    let mut description = String::new();
    let mut argument_hint = String::new();
    let mut body = content.as_str();
    if let Some(rest) = content.strip_prefix("---\n")
        && let Some(end) = rest.find("\n---")
    {
        let after = &rest[end + 4..];
        if after.is_empty() || after.starts_with('\n') {
            for line in rest[..end].lines() {
                let Some((key, value)) = line.split_once(':') else { continue };
                let value = unquote(value.trim());
                match key.trim() {
                    "description" => description = value,
                    "argument-hint" => argument_hint = value,
                    _ => {}
                }
            }
            body = after.strip_prefix('\n').unwrap_or(after);
        }
    }
    if description.is_empty() {
        let first = body.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
        description = first.chars().take(80).collect();
    }
    Template {
        name: name.to_string(),
        description,
        argument_hint,
        body: body.to_string(),
        source: source.to_string(),
    }
}

fn unquote(v: &str) -> String {
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].to_string();
        }
    }
    v.to_string()
}

fn read_dir(dir: &Path, source: &str, into: &mut Vec<Template>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut found: Vec<Template> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            if path.extension()? != "md" || !path.is_file() {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            if name.is_empty() || name.contains(char::is_whitespace) {
                return None;
            }
            let content = std::fs::read_to_string(&path).ok()?;
            Some(parse_template(&name, source, &content))
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    for t in found {
        match into.iter_mut().find(|x| x.name == t.name) {
            Some(existing) => *existing = t,
            None => into.push(t),
        }
    }
}

pub fn dirs(home: &Path, project: Option<&Path>) -> Vec<(PathBuf, &'static str)> {
    let mut dirs = vec![
        (home.join(".pi/agent/prompts"), "pi"),
        (home.join(".agents/prompts"), "personal"),
    ];
    if let Some(p) = project {
        dirs.push((p.join(".agents/prompts"), "project"));
    }
    dirs
}

pub fn load(home: &Path, project: Option<&Path>) -> Vec<Template> {
    let mut all = Vec::new();
    for (dir, source) in dirs(home, project) {
        read_dir(&dir, source, &mut all);
    }
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

pub fn command_name(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty()).then_some((name, args.trim()))
}

pub struct Expanded {
    pub name: String,
    pub text: String,
}

pub fn expand_command(text: &str, templates: &[Template]) -> Option<Expanded> {
    let (name, args) = command_name(text)?;
    let t = templates.iter().find(|t| t.name == name)?;
    Some(Expanded { name: name.to_string(), text: expand(&t.body, &split_args(args)) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        split_args(s)
    }

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments_split_on_whitespace_and_quotes() {
        assert_eq!(a("one  two\tthree"), v(&["one", "two", "three"]));
        assert_eq!(a("\"API compatibility\" x"), v(&["API compatibility", "x"]));
        assert_eq!(a("'a b' c"), v(&["a b", "c"]));
        assert_eq!(a(""), Vec::<String>::new());
    }

    #[test]
    fn an_apostrophe_inside_a_word_is_text() {
        assert_eq!(a("it doesn't work"), v(&["it", "doesn't", "work"]));
    }

    #[test]
    fn an_unclosed_quote_is_kept_as_text() {
        assert_eq!(a("\"open quote here"), v(&["\"open", "quote", "here"]));
    }

    #[test]
    fn an_escaped_quote_inside_a_quoted_argument_stays() {
        assert_eq!(a(r#""the \"fast\" path" x"#), v(&["the \"fast\" path", "x"]));
        assert_eq!(a(r#""keep \n and \\ as typed""#), v(&[r"keep \n and \\ as typed"]));
    }

    #[test]
    fn positional_and_all_arguments() {
        let args = v(&["x", "y z"]);
        assert_eq!(expand("$1|$2|$3|", &args), "x|y z||");
        assert_eq!(expand("$@ / $ARGUMENTS", &args), "x y z / x y z");
        assert_eq!(expand("$ARGUMENTSx", &args), "x y zx");
        assert_eq!(expand("$10", &args), "");
    }

    #[test]
    fn defaults_apply_when_the_argument_is_missing() {
        assert_eq!(expand("${1:-fallback, with commas}", &[]), "fallback, with commas");
        assert_eq!(expand("${1:-d}", &v(&["given"])), "given");
        assert_eq!(expand("${@:-everything}", &[]), "everything");
        assert_eq!(expand("${@:-everything}", &v(&["a", "b"])), "a b");
        assert_eq!(expand("${2:-two}", &v(&["a"])), "two");
    }

    #[test]
    fn slices_of_the_arguments() {
        let args = v(&["a", "b", "c", "d"]);
        assert_eq!(expand("${@:2}", &args), "b c d");
        assert_eq!(expand("${@:2:2}", &args), "b c");
        assert_eq!(expand("${@:9}", &args), "");
        assert_eq!(expand("${@:3:10}", &args), "c d");
    }

    #[test]
    fn unknown_dollars_and_substituted_text_are_left_alone() {
        assert_eq!(expand("cost $ and $x and ${oops}", &[]), "cost $ and $x and ${oops}");
        assert_eq!(expand("$1", &v(&["$2", "no"])), "$2");
        assert_eq!(expand("é $1 é", &v(&["ü"])), "é ü é");
    }

    #[test]
    fn frontmatter_gives_the_description_and_hint() {
        let t = parse_template(
            "review",
            "personal",
            "---\ndescription: Review staged changes\nargument-hint: \"[focus]\"\n---\nReview $1\n",
        );
        assert_eq!(t.description, "Review staged changes");
        assert_eq!(t.argument_hint, "[focus]");
        assert_eq!(t.body, "Review $1\n");
    }

    #[test]
    fn the_first_line_is_the_description_without_frontmatter() {
        let t = parse_template("x", "pi", "\n\nDo the thing\nmore\n");
        assert_eq!(t.description, "Do the thing");
        assert_eq!(t.body, "\n\nDo the thing\nmore\n");
    }

    #[test]
    fn project_overrides_personal_overrides_pi() {
        let root = std::env::temp_dir().join(format!("prompts-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let home = root.join("home");
        let project = root.join("repo");
        let put = |dir: PathBuf, file: &str, text: &str| {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(file), text).unwrap();
        };
        put(home.join(".pi/agent/prompts"), "a.md", "pi a");
        put(home.join(".pi/agent/prompts"), "b.md", "pi b");
        put(home.join(".pi/agent/prompts"), "c.md", "pi c");
        put(home.join(".agents/prompts"), "b.md", "personal b");
        put(home.join(".agents/prompts"), "c.md", "personal c");
        put(project.join(".agents/prompts"), "c.md", "project c");
        put(project.join(".agents/prompts"), "ignored.txt", "no");
        let all = load(&home, Some(&project));
        let body = |n: &str| all.iter().find(|t| t.name == n).unwrap().body.clone();
        assert_eq!(all.len(), 3);
        assert_eq!(body("a"), "pi a");
        assert_eq!(body("b"), "personal b");
        assert_eq!(body("c"), "project c");
        assert_eq!(load(&home, None).iter().find(|t| t.name == "c").unwrap().body, "personal c");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_command_expands_only_when_a_template_exists() {
        let ts = vec![parse_template("hi", "personal", "Hello ${1:-world}")];
        let e = expand_command("/hi there now", &ts).unwrap();
        assert_eq!(e.name, "hi");
        assert_eq!(e.text, "Hello there");
        assert!(expand_command("/nope x", &ts).is_none());
        assert!(expand_command("plain /hi", &ts).is_none());
    }
}
