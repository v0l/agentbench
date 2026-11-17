use agentbench_core::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::PathBuf;

const RULE: &str = "API changes must be additive (see AGENTS.md): never remove or rename a field or enum variant, never change a field's type, never add a required request field. A deliberate break bumps API_VERSION and MIN_CLIENT_API.";

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn kept(path: &str, old: &Value, now: &Value, out: &mut Vec<String>) {
    if old.is_null() {
        return;
    }
    if kind(old) != kind(now) {
        out.push(format!("{path}: was {}, now {}", kind(old), kind(now)));
        return;
    }
    match (old, now) {
        (Value::Object(o), Value::Object(n)) => {
            for (key, value) in o {
                match n.get(key) {
                    Some(v) => kept(&format!("{path}.{key}"), value, v, out),
                    None => out.push(format!("{path}.{key}: removed or renamed")),
                }
            }
        }
        (Value::Array(o), Value::Array(n)) => {
            for (i, value) in o.iter().enumerate() {
                if let Some(v) = n.get(i) {
                    kept(&format!("{path}[{i}]"), value, v, out);
                }
            }
        }
        _ => {}
    }
}

pub fn decodes<T: DeserializeOwned>(name: &str, json: &Value) -> Result<T, String> {
    serde_json::from_value(json.clone()).map_err(|e| format!("{name} no longer decodes: {e}. {RULE}"))
}

pub fn response_still_holds<T: DeserializeOwned + Serialize>(name: &str, json: &Value) -> Result<(), String> {
    let decoded: T = decodes(name, json)?;
    let now = serde_json::to_value(decoded).unwrap();
    let mut lost = Vec::new();
    kept(name, json, &now, &mut lost);
    if lost.is_empty() { Ok(()) } else { Err(format!("{}. {RULE}", lost.join("; "))) }
}

fn fixtures() -> Vec<(u32, String, Value)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api");
    let mut out = Vec::new();
    for dir in std::fs::read_dir(&root).expect("tests/fixtures/api exists") {
        let dir = dir.unwrap().path();
        let version: u32 = dir.file_name().unwrap().to_str().unwrap().trim_start_matches('v').parse().unwrap();
        for file in std::fs::read_dir(&dir).unwrap() {
            let file = file.unwrap().path();
            let name = file.file_stem().unwrap().to_str().unwrap().to_string();
            out.push((version, name, serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap()));
        }
    }
    out
}

fn check(name: &str, json: &Value) -> Result<(), String> {
    match name {
        "overview" => response_still_holds::<Overview>(name, json),
        "sessions" => response_still_holds::<Vec<Session>>(name, json),
        "issues" => response_still_holds::<Vec<Issue>>(name, json),
        "messages" => response_still_holds::<Vec<Message>>(name, json),
        "changes" => response_still_holds::<ChangesFeed>(name, json),
        "request_new_project" => decodes::<NewProject>(name, json).map(|_| ()),
        "request_new_issue" => decodes::<NewIssue>(name, json).map(|_| ()),
        "request_issue_patch" => decodes::<IssuePatch>(name, json).map(|_| ()),
        "request_new_session" => decodes::<NewSession>(name, json).map(|_| ()),
        "request_send_message" => decodes::<SendMessage>(name, json).map(|_| ()),
        other => Err(format!("fixture {other} has no type in tests/compat.rs")),
    }
}

#[test]
fn every_recorded_fixture_of_every_version_still_holds() {
    let all = fixtures();
    assert!(all.len() >= 10, "fixtures are missing under tests/fixtures/api");
    let current = API_VERSION;
    assert!(all.iter().any(|(v, ..)| *v == current), "no fixtures recorded for api v{current}");
    let mut failures = Vec::new();
    for (version, name, json) in &all {
        if let Err(e) = check(name, json) {
            failures.push(format!("v{version}/{name}: {e}"));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn an_unknown_session_or_issue_state_decodes_as_unknown() {
    let s: SessionStatus = serde_json::from_str("\"hibernating\"").unwrap();
    assert_eq!(s, SessionStatus::Unknown);
    let i: IssueState = serde_json::from_str("\"someday\"").unwrap();
    assert_eq!(i, IssueState::Unknown);
}

#[test]
fn a_removed_field_fails_the_guard() {
    #[derive(Serialize, serde::Deserialize)]
    struct Now {
        id: i64,
    }
    let old = serde_json::json!({"id": 1, "title": "x"});
    let e = response_still_holds::<Now>("issue", &old).unwrap_err();
    assert!(e.contains("issue.title: removed or renamed") && e.contains("additive"), "{e}");
}

#[test]
fn a_renamed_field_fails_the_guard() {
    #[derive(Serialize, serde::Deserialize)]
    struct Now {
        id: i64,
        #[serde(default)]
        name: String,
    }
    let old = serde_json::json!({"id": 1, "title": "x"});
    let e = response_still_holds::<Now>("issue", &old).unwrap_err();
    assert!(e.contains("issue.title"), "{e}");
}

#[test]
fn a_changed_type_fails_the_guard() {
    #[derive(Serialize, serde::Deserialize)]
    struct Now {
        id: i64,
    }
    let old = serde_json::json!({"id": "1"});
    let e = response_still_holds::<Now>("issue", &old).unwrap_err();
    assert!(e.contains("additive"), "{e}");
}

#[test]
fn a_new_required_request_field_fails_the_guard() {
    #[derive(serde::Deserialize, Debug)]
    #[allow(dead_code)]
    struct Now {
        title: String,
        priority: i64,
    }
    let old = serde_json::json!({"title": "x"});
    let e = decodes::<Now>("request_new_issue", &old).unwrap_err();
    assert!(e.contains("priority") && e.contains("additive"), "{e}");
}

#[test]
fn an_added_optional_field_passes_the_guard() {
    #[derive(Serialize, serde::Deserialize)]
    struct Now {
        id: i64,
        #[serde(default)]
        extra: Option<String>,
    }
    let old = serde_json::json!({"id": 1});
    response_still_holds::<Now>("issue", &old).unwrap();
}

#[test]
fn compat_names_the_side_that_is_out_of_date() {
    assert_eq!(compat(3, 1, 3, 1), Compat::Ok);
    assert_eq!(compat(1, 1, 2, 2), Compat::AppOlder { app: 1, server: 2 });
    assert_eq!(compat(5, 4, 3, 1), Compat::ServerOlder { app: 5, server: 3 });
    assert_eq!(compat(5, 3, 3, 1), Compat::Ok);
}
