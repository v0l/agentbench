use crate::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

pub const FILE: &str = "guards.toml";
const TRUST_SEEDED: &str = "guards_trust_rules_seeded";
const TRUST_RULES: [&str; 3] = ["worker-gh-writes", "untrusted-no-github-writes", "worker-git-push-main"];
const OWNER_SEEDED: &str = "guards_owner_rules_seeded";
const OWNER_RULES: [&str; 1] = ["owner-approves-and-merges"];
const OWNER_PATTERN_V1: &str = r"(?i)/api/outbox/[^/\s]+/approve|/api/projects/[^/\s]+/pulls/[^/\s]+/merge|\bagentbench(-server)?\s+github\s+(approve|merge)\b|\bgh\s+pr\s+merge\b";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub tools: Vec<String>,
    #[serde(default)]
    pub fields: Vec<String>,
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unless: Option<String>,
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
tools = ["start_session", "send_message", "file_issue"]
fields = ["prompt", "text", "body"]
pattern = '(?i)\bpush\b[^.\n]{0,40}\b(to|into|onto)\s+(origin[ /])?(main|master)\b|\bgit\s+push\s+(-\S+\s+)*\S+\s+(\S+:)?(main|master)\b|\bmerge\b[^.\n]{0,30}\band push\b'
unless = '(?i)\b(never|not|don.t|do not|no|without)\s+(git\s+)?(merge and\s+)?push'
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
require_field = "_brief"
require = '(?i)^\(user chat\)$|\bapproved to post\b'
action = "refuse"
message = "A worker posts GitHub comments or reviews only when the supervisor's brief says `approved to post`. Put the review in your reply instead; the supervisor decides whether it is posted."

[[rule]]
id = "worker-gh-writes"
tools = ["bash"]
fields = ["command"]
pattern = '''(?i)\bgh\s+(issue\s+(create|edit|close|reopen|comment|delete|transfer|lock|unlock|pin|unpin|develop)|pr\s+(create|merge|close|reopen|edit|ready|review|comment|lock|unlock|update-branch)|label\s+(create|edit|delete|clone))\b|\bgh\s+api\b[^\n]*(-X\s*|--method[ =]\s*)(POST|PATCH|PUT|DELETE)\b|\bgh\s+api\b[^\n]*\s(-f|-F|--field|--raw-field|--input)\s'''
require_field = "_brief"
require = '(?i)^\(user chat\)$|\bapproved to post\b'
action = "refuse"
message = "A worker changes GitHub (issues, pull requests, labels, comments) only when the brief says `approved to post`. Write to the agentbench outbox with comment_on_issue and update_issue instead."

[[rule]]
id = "untrusted-no-github-writes"
tools = ["bash"]
fields = ["command"]
pattern = '''(?i)\bgh\s+(issue|pr|label|release|repo|workflow|run)\s+(create|edit|close|reopen|comment|delete|transfer|lock|unlock|pin|unpin|develop|merge|ready|review|update-branch|clone|rename|archive|fork|cancel|rerun|enable|disable)\b|\bgh\s+api\b[^\n]*((-X\s*|--method[ =]\s*)(POST|PATCH|PUT|DELETE)\b|\s(-f|-F|--field|--raw-field|--input)\s)|\bgit\s+push\b'''
require_field = "_untrusted"
require = '^(|no)$'
action = "refuse"
message = "This issue was written by someone who is not a trusted maintainer. Workers on it get no write access to GitHub and no push: commit on your branch and report in your reply; the outbox is the only way out."

[[rule]]
id = "worker-git-push-main"
tools = ["bash"]
fields = ["command"]
pattern = '''(?i)\bgit\s+push\b[^\n;&|]*(\s(origin\s+)?(main|master)\b|:(main|master)\b|--force|\s-f\b|--mirror|--all\b|--delete|\s:\S)'''
require_field = "_brief"
require = '^\(user chat\)$'
action = "refuse"
message = "Workers never push to main or master, force push or delete remote branches: they commit on their branch and the supervisor or autopilot merges."

[[rule]]
id = "owner-approves-and-merges"
tools = ["bash"]
fields = ["command"]
pattern = '''(?i)/api/outbox/[^/\s]+/approve|/api/projects/[^/\s]+/pulls/[^/\s]+/merge|\bgithub\s+(approve|merge)\b|\bgh\s+pr\s+merge\b'''
require_field = "_brief"
require = '^\(user chat\)$'
action = "refuse"
message = "Only the owner approves outbox rows and merges pull requests. Say in your reply what should be approved or merged; the owner does it in the app or with agentbench github."
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
    let mut rules: Rules = toml::from_str(&text).map_err(|e| format!("{}: {e}", p.display()))?;
    for (key, ids) in [(TRUST_SEEDED, &TRUST_RULES[..]), (OWNER_SEEDED, &OWNER_RULES[..])] {
        if store.setting(key).ok().flatten().is_some() {
            continue;
        }
        let seeded: Rules = toml::from_str(DEFAULTS).map_err(|e| e.to_string())?;
        for r in seeded.rules.into_iter().filter(|r| ids.contains(&r.id.as_str())) {
            if !rules.rules.iter().any(|x| x.id == r.id) {
                rules.rules.push(r);
            }
        }
        let _ = save(store, &rules);
        let _ = store.set_setting(key, "1");
    }
    let current: Rules = toml::from_str(DEFAULTS).map_err(|e| e.to_string())?;
    let mut upgraded = false;
    if let Some(new) = current.rules.iter().find(|r| r.id == "owner-approves-and-merges") {
        for r in rules.rules.iter_mut().filter(|r| r.id == new.id && r.pattern == OWNER_PATTERN_V1) {
            r.pattern = new.pattern.clone();
            upgraded = true;
        }
    }
    if upgraded {
        let _ = save(store, &rules);
    }
    Ok(rules)
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
    if let Some(r) = &rule.unless {
        regex::Regex::new(r).map_err(|e| format!("{}: unless: {e}", rule.id))?;
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
        if let Some(u) = &r.unless
            && text.iter().any(|t| regex::Regex::new(u).is_ok_and(|u| u.is_match(t)))
        {
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
        for ok in ["Fix it and commit on your branch.", "git push -u origin issue-12-fix", "The main loop is slow.", "Never push to main; the supervisor merges."] {
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
    fn a_worker_posting_a_review_is_refused_without_an_approved_brief() {
        let post = |cmd: &str, brief: &str| matches(&rules(), "bash", &json!({"command": cmd, "_brief": brief}));
        let h = post("gh pr review 41 --comment -b 'nice'", "Review PR #41 and report").unwrap();
        assert!(!h.ask && h.id == "pr-reviews");
        assert!(post("gh pr comment 41 -b hi", "").is_some(), "no brief, no posting");
        assert!(post("gh api repos/a/b/pulls/4/comments -f body=x", "Review PR #4").is_some());
        assert!(post("gh pr review 41 --approve", "Review PR #41. You are approved to post the review.").is_none());
        assert!(post("gh pr review 41 --approve", "(user chat)").is_none(), "the user's own chat is theirs to decide");
        for ok in ["gh pr view 41", "gh pr diff 41", "gh api repos/a/b/pulls/4/comments", "cargo test"] {
            assert!(post(ok, "").is_none(), "{ok}");
        }
    }

    fn bash(cmd: &str, brief: &str, untrusted: &str) -> Option<String> {
        matches(&rules(), "bash", &json!({"command": cmd, "_brief": brief, "_untrusted": untrusted})).map(|h| h.id)
    }

    #[test]
    fn a_worker_is_refused_gh_issue_and_pr_writes_without_an_approved_brief() {
        let writes = [
            "gh issue close 4",
            "gh issue create --title x",
            "gh issue edit 4 --add-label ready",
            "gh issue comment 4 -b hi",
            "gh pr create --fill",
            "gh pr close 9",
            "gh pr ready 9",
            "gh label create ready",
            "gh api repos/o/r/issues/4 -X PATCH -f state=closed",
            "gh api repos/o/r/issues/4/labels -f labels[]=ready",
            "gh api --method DELETE repos/o/r/issues/4/labels/ready",
        ];
        for w in writes {
            assert!(bash(w, "Fix the bug", "no").is_some(), "{w}");
            assert!(bash(w, "Fix it. You are approved to post.", "no").is_none(), "{w}");
        }
        for ok in ["gh issue view 4", "gh issue list", "gh pr view 9", "gh pr diff 9", "gh pr checks 9", "gh api repos/o/r/issues/4", "cargo test", "git push -u origin issue-12-fix"] {
            assert!(bash(ok, "Fix the bug", "no").is_none(), "{ok}");
        }
    }

    #[test]
    fn a_worker_on_an_untrusted_issue_gets_no_github_write_and_no_push_even_when_approved() {
        let approved = "Fix it. You are approved to post.";
        for w in ["gh issue close 4", "gh pr create --fill", "gh api repos/o/r/issues/4 -X PATCH -f state=closed", "git push -u origin issue-12-fix", "git push origin HEAD"] {
            assert_eq!(bash(w, approved, "yes").as_deref(), Some("untrusted-no-github-writes"), "{w}");
        }
        for ok in ["gh issue view 4", "git commit -am x", "cargo test", "git status"] {
            assert!(bash(ok, approved, "yes").is_none(), "{ok}");
        }
    }

    #[test]
    fn only_the_owner_approves_outbox_rows_and_merges_pull_requests() {
        let agent = [
            "curl -X POST -H \"Authorization: Bearer $AGENTBENCH_TOKEN\" $AGENTBENCH_URL/api/outbox/12/approve",
            "curl -X POST http://127.0.0.1:7420/api/projects/14/pulls/9/merge",
            "agentbench github merge 9 --project demo",
            "agentbench github approve 12",
            "agentbench-server github approve 99",
            "ab github approve 1",
            "ab() { agentbench-server \"$@\"; }; ab github merge 4",
            "gh pr merge 9 --squash",
        ];
        for w in agent {
            assert_eq!(bash(w, "Fix it. You are approved to post.", "no").as_deref(), Some("owner-approves-and-merges"), "{w}");
        }
        assert!(bash(agent[0], "(user chat)", "no").is_none());
        for ok in ["curl $AGENTBENCH_URL/api/outbox", "curl $AGENTBENCH_URL/api/projects/14/pulls", "agentbench github outbox"] {
            assert!(bash(ok, "Fix the bug", "no").is_none(), "{ok}");
        }
    }

    #[test]
    fn the_old_owner_pattern_in_an_existing_file_is_upgraded() {
        let s = Store::memory().unwrap();
        let file = path(&s);
        let v1 = DEFAULTS.replace(r"\bgithub\s+(approve|merge)\b", r"\bagentbench(-server)?\s+github\s+(approve|merge)\b");
        assert_ne!(v1, DEFAULTS);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, v1).unwrap();
        s.set_setting(TRUST_SEEDED, "1").unwrap();
        s.set_setting(OWNER_SEEDED, "1").unwrap();
        let r = load(&s).unwrap();
        assert!(matches(&r, "bash", &json!({"command": "ab github approve 1", "_brief": "x"})).is_some());
    }

    #[test]
    fn the_owner_rule_is_seeded_into_a_file_that_already_had_the_trust_rules() {
        let s = Store::memory().unwrap();
        let file = path(&s);
        let old = DEFAULTS.split("[[rule]]\nid = \"owner-approves-and-merges\"").next().unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, old).unwrap();
        s.set_setting(TRUST_SEEDED, "1").unwrap();
        assert!(load(&s).unwrap().rules.iter().any(|r| r.id == "owner-approves-and-merges"));
    }

    #[test]
    fn workers_never_push_main_force_or_delete_a_remote_branch() {
        for w in ["git push origin main", "git push origin HEAD:master", "git push --force origin issue-1", "git push -f", "git push origin :old", "git push --delete origin x", "git push --all"] {
            assert_eq!(bash(w, "Fix the bug", "no").as_deref(), Some("worker-git-push-main"), "{w}");
        }
        assert!(bash("git push origin main", "(user chat)", "no").is_none());
        assert!(bash("git push -u origin issue-12-fix", "Fix the bug", "no").is_none());
    }

    #[test]
    fn the_trust_rules_are_seeded_into_an_existing_file_once_in_the_stores_own_temp_dir() {
        let s = Store::memory().unwrap();
        let file = path(&s);
        let real = std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".agents/memory")).unwrap_or_default();
        assert!(!file.starts_with(&real), "{} is the real guards file", file.display());
        assert!(file.starts_with(std::env::temp_dir()), "{} is not in a temp dir", file.display());
        let old = DEFAULTS.split("[[rule]]\nid = \"worker-gh-writes\"").next().unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, old).unwrap();
        let first = load(&s).unwrap();
        for id in TRUST_RULES {
            assert!(first.rules.iter().any(|r| r.id == id), "{id} was not seeded into {}", file.display());
        }
        assert!(first.rules.iter().any(|r| r.id == "worker-no-push") && first.rules.iter().any(|r| r.id == "pr-reviews"));
        assert!(std::fs::read_to_string(&file).unwrap().contains("untrusted-no-github-writes"));
        let mut kept = first;
        kept.rules.retain(|r| r.id != "worker-git-push-main");
        save(&s, &kept).unwrap();
        assert!(!load(&s).unwrap().rules.iter().any(|r| r.id == "worker-git-push-main"), "a rule the supervisor removed stays removed");
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
