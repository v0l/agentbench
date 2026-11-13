use std::fs;
use std::path::{Path, PathBuf};

const MAX_SKILL_BYTES: usize = 120_000;
const MAX_SIBLINGS: usize = 40;
const DESCRIPTION_CHARS: usize = 240;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

pub fn roots(workspace: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".agents").join("skills"));
    }
    roots.push(workspace.join(".agents").join("skills"));
    roots
}

pub fn discover(roots: &[PathBuf], first: &[String]) -> Vec<Skill> {
    let mut found: Vec<Skill> = Vec::new();
    for root in roots {
        let Ok(read) = fs::read_dir(root) else { continue };
        let mut dirs: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for dir in dirs {
            let Ok(text) = fs::read_to_string(dir.join("SKILL.md")) else { continue };
            let (name, description) = frontmatter(&text);
            let name = name
                .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default();
            if name.is_empty() || found.iter().any(|s| s.name == name) {
                continue;
            }
            found.push(Skill { name, description: description.unwrap_or_default(), dir });
        }
    }
    found.sort_by_key(|s| (!first.contains(&s.name), s.name.clone()));
    found
}

pub fn listing(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return "No skills are installed.".to_string();
    }
    skills
        .iter()
        .map(|s| match s.description.is_empty() {
            true => format!("- {}", s.name),
            false => format!("- {}: {}", s.name, short(&s.description)),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn load(skills: &[Skill], name: Option<&str>, file: Option<&str>) -> Result<String, String> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(format!("Skills you can load with load_skill:\n{}", listing(skills)));
    };
    let want = name.to_lowercase();
    let skill = skills.iter().find(|s| s.name.to_lowercase() == want).ok_or_else(|| {
        format!("no skill called {name:?}. Skills you can load:\n{}", listing(skills))
    })?;
    let target = match file.map(str::trim).filter(|f| !f.is_empty()) {
        None => skill.dir.join("SKILL.md"),
        Some(f) => inside(&skill.dir, f)?,
    };
    let bytes = fs::read(&target).map_err(|e| format!("{}: {e}", target.display()))?;
    let mut text =
        String::from_utf8(bytes).map_err(|_| format!("{} is not a text file", target.display()))?;
    if text.len() > MAX_SKILL_BYTES {
        let mut cut = MAX_SKILL_BYTES;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[cut here: the file is longer]\n");
    }
    let mut out = format!("Skill {} is in {}.\n", skill.name, skill.dir.display());
    let others = siblings(&skill.dir);
    if file.is_none() && !others.is_empty() {
        out.push_str(&format!(
            "Other files in it, which load_skill reads with file=: {}\n",
            others.join(", ")
        ));
    }
    out.push('\n');
    out.push_str(&text);
    Ok(out)
}

fn inside(dir: &Path, file: &str) -> Result<PathBuf, String> {
    let given = Path::new(file);
    if given.is_absolute() || file.contains("..") {
        return Err(format!("{file} is not a file inside the skill"));
    }
    let target = dir.join(given);
    let real = target.canonicalize().map_err(|e| format!("{file}: {e}"))?;
    let base = dir.canonicalize().map_err(|e| e.to_string())?;
    if !real.starts_with(&base) {
        return Err(format!("{file} is not a file inside the skill"));
    }
    Ok(real)
}

fn siblings(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((at, depth)) = stack.pop() {
        let Ok(read) = fs::read_dir(&at) else { continue };
        let mut entries: Vec<_> = read.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                if depth < 2 {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let rel = path.strip_prefix(dir).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            if rel != "SKILL.md" && !rel.starts_with('.') && out.len() < MAX_SIBLINGS {
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

fn short(text: &str) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one.char_indices().nth(DESCRIPTION_CHARS) {
        Some((i, _)) => format!("{}...", &one[..i]),
        None => one,
    }
}

fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let Some(rest) = text.strip_prefix("---") else { return (None, None) };
    let Some(end) = rest.find("\n---") else { return (None, None) };
    let lines: Vec<&str> = rest[..end].lines().collect();
    let (mut name, mut description) = (None, None);
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim();
        if key != "name" && key != "description" {
            continue;
        }
        let mut value = value.trim().to_string();
        if matches!(value.as_str(), ">" | "|" | ">-" | "|-") {
            value.clear();
            while i < lines.len() && (lines[i].starts_with(' ') || lines[i].starts_with('\t')) {
                value.push_str(lines[i].trim());
                value.push(' ');
                i += 1;
            }
        }
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim().to_string();
        match key {
            "name" => name = Some(value),
            _ => description = Some(value),
        }
    }
    (name, description)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bench-skills-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn add(root: &Path, name: &str, text: &str) {
        fs::create_dir_all(root.join(name)).unwrap();
        fs::write(root.join(name).join("SKILL.md"), text).unwrap();
    }

    #[test]
    fn a_skill_is_found_by_its_frontmatter() {
        let root = skill_dir("find");
        add(&root, "gcad", "---\nname: gcad\ndescription: Model parts.\n---\n\n# gcad\nbody\n");
        add(&root, "other", "---\ndescription: >\n  folded\n  text\n---\nbody");
        add(&root, "bare", "no frontmatter at all");
        let skills = discover(std::slice::from_ref(&root), &[]);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["bare", "gcad", "other"]);
        assert_eq!(skills[1].description, "Model parts.");
        assert_eq!(skills[2].description, "folded text");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_hosts_own_skills_come_first() {
        let root = skill_dir("first-list");
        add(&root, "aaa", "---\ndescription: a\n---\n");
        add(&root, "gcad", "---\ndescription: g\n---\n");
        add(&root, "zzz", "---\ndescription: z\n---\n");
        let skills =
            discover(std::slice::from_ref(&root), &["zzz".to_string(), "gcad".to_string()]);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["gcad", "zzz", "aaa"]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_first_root_wins_a_name() {
        let (a, b) = (skill_dir("first"), skill_dir("second"));
        add(&a, "gcad", "---\ndescription: mine\n---\n");
        add(&b, "gcad", "---\ndescription: theirs\n---\n");
        let skills = discover(&[a.clone(), b.clone()], &[]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "mine");
        let _ = (fs::remove_dir_all(a), fs::remove_dir_all(b));
    }

    #[test]
    fn loading_returns_the_skill_and_names_its_other_files() {
        let root = skill_dir("load");
        add(&root, "agentee", "---\nname: agentee\ndescription: x\n---\nthe manual");
        fs::write(root.join("agentee").join("practice.md"), "practice text").unwrap();
        let skills = discover(std::slice::from_ref(&root), &[]);
        let got = load(&skills, Some("AGENTEE"), None).unwrap();
        assert!(got.contains("the manual"));
        assert!(got.contains("practice.md"));
        let more = load(&skills, Some("agentee"), Some("practice.md")).unwrap();
        assert!(more.contains("practice text"));
        assert!(load(&skills, Some("agentee"), Some("../x")).is_err());
        assert!(load(&skills, Some("agentee"), Some("/etc/passwd")).is_err());
        assert!(load(&skills, Some("nope"), None).unwrap_err().contains("agentee"));
        assert!(load(&skills, None, None).unwrap().contains("- agentee: x"));
        let _ = fs::remove_dir_all(root);
    }
}
