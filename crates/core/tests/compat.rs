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

fn shape(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), shape(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(shape).fold(None, |acc, s| Some(unite(acc, s))).into_iter().collect()),
        other => Value::String(kind(other).to_string()),
    }
}

fn unite(acc: Option<Value>, next: Value) -> Value {
    match (acc, next) {
        (None, n) => n,
        (Some(Value::String(a)), n) if a == "null" => n,
        (Some(a), Value::String(n)) if n == "null" => a,
        (Some(Value::Object(mut a)), Value::Object(n)) => {
            for (k, v) in n {
                let merged = unite(a.remove(&k), v);
                a.insert(k, merged);
            }
            Value::Object(a)
        }
        (Some(Value::Array(a)), Value::Array(n)) => {
            let merged = a.into_iter().chain(n).fold(None, |acc, s| Some(unite(acc, s)));
            Value::Array(merged.into_iter().collect())
        }
        (Some(a), _) => a,
    }
}

fn conforms(path: &str, schema: &Value, now: &Value, out: &mut Vec<String>) {
    match (schema, now) {
        (Value::String(k), _) if k == "null" => {}
        (Value::String(k), v) if k == kind(v) => {}
        (Value::String(k), v) => out.push(format!("{path}: was {k}, now {}", kind(v))),
        (Value::Object(o), Value::Object(n)) => {
            for (key, sub) in o {
                match n.get(key) {
                    Some(v) => conforms(&format!("{path}.{key}"), sub, v, out),
                    None => out.push(format!("{path}.{key}: removed or renamed")),
                }
            }
        }
        (Value::Array(o), Value::Array(n)) => {
            if let Some(elem) = o.first() {
                for (i, v) in n.iter().enumerate() {
                    conforms(&format!("{path}[{i}]"), elem, v, out);
                }
            }
        }
        (s, v) => out.push(format!("{path}: was {}, now {}", kind(s), kind(v))),
    }
}

fn reserialised(name: &str, json: &Value) -> Result<Value, String> {
    fn via<T: DeserializeOwned + Serialize>(name: &str, json: &Value) -> Result<Value, String> {
        decodes::<T>(name, json).map(|t| serde_json::to_value(t).unwrap())
    }
    match name {
        "overview" => via::<Overview>(name, json),
        "sessions" => via::<Vec<Session>>(name, json),
        "issues" => via::<Vec<Issue>>(name, json),
        "messages" => via::<Vec<Message>>(name, json),
        "changes" => via::<ChangesFeed>(name, json),
        other => Err(format!("{other} is not a response type")),
    }
}

const RESPONSES: [&str; 5] = ["overview", "sessions", "issues", "messages", "changes"];

fn fixture_dir(version: u32) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/api/v{version}"))
}

fn latest_version() -> u32 {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api");
    std::fs::read_dir(root)
        .unwrap()
        .filter_map(|d| d.unwrap().file_name().to_str()?.trim_start_matches('v').parse().ok())
        .max()
        .unwrap()
}

fn current_fixture(name: &str) -> Value {
    let path = fixture_dir(latest_version()).join(format!("{name}.json"));
    serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|_| panic!("record {path:?} first"))).unwrap()
}

#[test]
fn record_the_response_schema() {
    if std::env::var("RECORD_API_FIXTURES").is_err() {
        return;
    }
    let mut schema = serde_json::Map::new();
    for name in RESPONSES {
        schema.insert(name.into(), shape(&reserialised(name, &current_fixture(name)).unwrap()));
    }
    std::fs::write(
        fixture_dir(latest_version()).join("schema.json"),
        serde_json::to_string_pretty(&Value::Object(schema)).unwrap() + "\n",
    )
    .unwrap();
}

#[test]
fn todays_serialised_responses_keep_every_recorded_schema() {
    let mut failures = Vec::new();
    let mut seen = 0;
    for dir in std::fs::read_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api")).unwrap() {
        let schema_file = dir.unwrap().path().join("schema.json");
        let Ok(bytes) = std::fs::read(&schema_file) else { continue };
        seen += 1;
        let schema: Value = serde_json::from_slice(&bytes).unwrap();
        for name in RESPONSES {
            let now = match reserialised(name, &current_fixture(name)) {
                Ok(v) => v,
                Err(e) => {
                    failures.push(e);
                    continue;
                }
            };
            let mut lost = Vec::new();
            conforms(name, &schema[name], &now, &mut lost);
            if !lost.is_empty() {
                failures.push(format!("{schema_file:?}: {}. {RULE}", lost.join("; ")));
            }
        }
    }
    assert!(seen > 0, "no schema.json recorded");
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn the_schema_guard_catches_each_kind_of_break() {
    let old = shape(&serde_json::json!({"id": 1, "title": "x", "tags": ["a"], "inner": {"n": 1}}));
    let check = |now: Value| {
        let mut out = Vec::new();
        conforms("t", &old, &now, &mut out);
        out
    };
    assert!(check(serde_json::json!({"id": 2, "title": "y", "tags": [], "inner": {"n": 5}, "new": true})).is_empty());
    assert!(check(serde_json::json!({"id": 1, "tags": ["a"], "inner": {"n": 1}}))[0].contains("t.title: removed"));
    assert!(check(serde_json::json!({"id": 1, "name": "x", "tags": ["a"], "inner": {"n": 1}}))[0].contains("t.title"));
    assert!(check(serde_json::json!({"id": "1", "title": "x", "tags": ["a"], "inner": {"n": 1}}))[0].contains("was number, now string"));
    assert!(check(serde_json::json!({"id": 1, "title": "x", "tags": ["a"], "inner": {"m": 1}}))[0].contains("t.inner.n"));
}

fn fixtures() -> Vec<(u32, String, Value)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api");
    let mut out = Vec::new();
    for dir in std::fs::read_dir(&root).expect("tests/fixtures/api exists") {
        let dir = dir.unwrap().path();
        let version: u32 = dir.file_name().unwrap().to_str().unwrap().trim_start_matches('v').parse().unwrap();
        for file in std::fs::read_dir(&dir).unwrap() {
            let file = file.unwrap().path();
            if file.file_stem().unwrap() == "schema" {
                continue;
            }
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
    assert!(latest_version() <= API_VERSION, "fixtures are recorded for a newer api than API_VERSION");
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
