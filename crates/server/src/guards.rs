use crate::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub const FILE: &str = "guards.toml";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub tools: Vec<String>,
    #[serde(default)]
    pub fields: Vec<String>,
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require: Option<String>,
    #[serde(default = "refuse")]
    pub action: String,
    pub message: String,
}

fn refuse() -> String {
    "refuse".into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Rules {
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

pub const DEFAULTS: &str = r#"[[rule]]
id = "worker-no-push"
tools = ["start_session", "send_message", "file_issue", "update_issue"]
fields = ["prompt", "text", "body"]
pattern = '(?i)\bpush\b[^.\n]{0,40}\b(to|into|onto)\s+(origin[ /])?(main|master)\b|\bgit\s+push\s+(-\S+\s+)*\S+\s+(\S+:)?(main|master)\b|\bmerge\b[^.\n]{0,30}\band push\b'
action = "refuse"
message = "Workers never push to main or master: they commit on their branch and the supervisor or autopilot merges. Rewrite the brief to end at 'commit on your branch'."

[[rule]]
id = "pr-review-model"
tools = ["start_session"]
fields = ["prompt", "title"]
pattern = '(?i)\b(review|reviewing)\b[^.\n]{0,40}\b(pr|prs|pull requests?)\b|\bgh pr (review|diff|view)\b|\bpr review\b'
require_field = "model"
require = '(?i)^\s*(high|[\w.-]*x-high|[\w.-]*opus[\w.-]*)\s*$'
action = "refuse"
message = "PR reviews run on the top model: start it again with model=high."

[[rule]]
id = "pr-reviews"
tools = ["bash"]
fields = ["command"]
pattern = '(?i)\bgh\s+(pr\s+(review|comment)|issue\s+comment)\b|\bgh\s+api\b[^\n]*(/comments|/reviews)\b[^\n]*(-X\s*POST|--method\s+POST|-f\s|--field\s|-F\s)'
action = "ask"
message = "Posting GitHub comments or reviews needs the user's approval each time."
"#;

pub fn path(store: &Store) -> PathBuf {
    store.memory_root().join(FILE)
}

pub fn load(store: &Store) -> Result<Rules, String> {
    let p = path(store);
    if !p.exists() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, DEFAULTS);
    }
    let text = std::fs::read_to_string(&p).unwrap_or_else(|_| DEFAULTS.to_string());
    toml::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))
}

pub fn save(store: &Store, rules: &Rules) -> Result<(), String> {
    for r in &rules.rules {
        check(r)?;
    }
    let text = toml::to_string_pretty(rules).map_err(|e| e.to_string())?;
    std::fs::write(path(store), text).map_err(|e| e.to_string())
}

pub fn check(rule: &Rule) -> Result<(), String> {
    if rule.id.trim().is_empty() || rule.tools.is_empty() || rule.message.trim().is_empty() {
        return Err("a rule needs an id, tools, a pattern and a message".into());
    }
    regex::Regex::new(&rule.pattern).map_err(|e| format!("{}: pattern: {e}", rule.id))?;
    if let Some(r) = &rule.require {
        regex::Regex::new(r).map_err(|e| format!("{}: require: {e}", rule.id))?;
    }
    if !matches!(rule.action.as_str(), "refuse" | "ask") {
        return Err(format!("{}: action is refuse or ask", rule.id));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub id: String,
    pub ask: bool,
    pub message: String,
}

impl Hit {
    pub fn refusal(&self) -> String {
        format!("refused by the guard rule `{}`: {}", self.id, self.message)
    }
}

fn field(args: &Value, name: &str) -> String {
    match args.get(name) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

pub fn matches(rules: &Rules, tool: &str, args: &Value) -> Option<Hit> {
    rules.rules.iter().find_map(|r| {
        if !r.tools.iter().any(|t| t == tool) {
            return None;
        }
        let pattern = regex::Regex::new(&r.pattern).ok()?;
        let text: Vec<String> = if r.fields.is_empty() {
            vec![args.to_string()]
        } else {
            r.fields.iter().map(|f| field(args, f)).collect()
        };
        if !text.iter().any(|t| pattern.is_match(t)) {
            return None;
        }
        if let (Some(f), Some(req)) = (&r.require_field, &r.require) {
            let ok = regex::Regex::new(req).ok()?.is_match(&field(args, f));
            if ok {
                return None;
            }
        }
        Some(Hit { id: r.id.clone(), ask: r.action == "ask", message: r.message.clone() })
    })
}

pub fn hit(store: &Store, tool: &str, args: &Value) -> Option<Hit> {
    match load(store) {
        Ok(rules) => matches(&rules, tool, args),
        Err(e) => {
            tracing::warn!("guard rules unreadable: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rules() -> Rules {
        toml::from_str(DEFAULTS).unwrap()
    }

    fn id(tool: &str, args: Value) -> Option<String> {
        matches(&rules(), tool, &args).map(|h| h.id)
    }

    #[test]
    fn the_default_rules_parse_and_check() {
        for r in rules().rules {
            check(&r).unwrap();
        }
    }

    #[test]
    fn a_brief_that_pushes_to_main_is_refused_and_a_normal_one_is_not() {
        let push = ["Fix it, then git push origin main.", "When done, merge and push.", "Push the fix to master when tests pass."];
        for p in push {
            assert_eq!(id("start_session", json!({"prompt": p})).as_deref(), Some("worker-no-push"), "{p}");
        }
        assert_eq!(id("file_issue", json!({"title": "t", "body": "then push it to main"})).as_deref(), Some("worker-no-push"));
        assert_eq!(id("send_message", json!({"text": "push to main now"})).as_deref(), Some("worker-no-push"));
        for ok in ["Fix it and commit on your branch.", "git push -u origin issue-12-fix", "The main loop is slow."] {
            assert_eq!(id("start_session", json!({"prompt": ok})), None, "{ok}");
        }
    }

    #[test]
    fn a_pr_review_off_the_top_model_is_refused() {
        let brief = json!({"prompt": "Review PR #41 in snort and report findings"});
        assert_eq!(id("start_session", brief.clone()).as_deref(), Some("pr-review-model"));
        let mut low = brief.clone();
        low["model"] = json!("low");
        assert_eq!(id("start_session", low).as_deref(), Some("pr-review-model"));
        let mut high = brief;
        high["model"] = json!("high");
        assert_eq!(id("start_session", high), None);
        assert_eq!(id("start_session", json!({"prompt": "Fix the login bug"})), None);
    }

    #[test]
    fn posting_a_github_review_asks_and_reading_a_pr_does_not() {
        let h = matches(&rules(), "bash", &json!({"command": "gh pr review 41 --comment -b 'nice'"})).unwrap();
        assert!(h.ask && h.id == "pr-reviews");
        assert!(matches(&rules(), "bash", &json!({"command": "gh pr comment 41 -b hi"})).is_some());
        assert!(matches(&rules(), "bash", &json!({"command": "gh api repos/a/b/pulls/4/comments -f body=x"})).is_some());
        for ok in ["gh pr view 41", "gh pr diff 41", "gh api repos/a/b/pulls/4/comments", "cargo test"] {
            assert!(matches(&rules(), "bash", &json!({"command": ok})).is_none(), "{ok}");
        }
    }

    #[test]
    fn rules_live_in_one_file_and_a_new_rule_needs_no_code() {
        let s = Store::memory().unwrap();
        assert!(hit(&s, "start_session", &json!({"prompt": "push to main"})).is_some());
        assert!(path(&s).exists(), "seeded with the defaults");
        let mut r = load(&s).unwrap();
        r.rules.push(Rule {
            id: "no-rm-rf".into(),
            tools: vec!["bash".into()],
            fields: vec!["command".into()],
            pattern: r"rm -rf /\s*$".into(),
            action: "refuse".into(),
            message: "no".into(),
            ..Default::default()
        });
        save(&s, &r).unwrap();
        assert_eq!(hit(&s, "bash", &json!({"command": "rm -rf /"})).unwrap().id, "no-rm-rf");
        let bad = Rule { id: "x".into(), tools: vec!["bash".into()], pattern: "(".into(), message: "m".into(), action: "refuse".into(), ..Default::default() };
        assert!(save(&s, &Rules { rules: vec![bad] }).is_err());
    }
}
