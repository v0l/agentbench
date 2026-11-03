use crate::store::{Store, StoreError};
use agentbench_core::{CliLogin, Onboarding, Provider, ProviderCheck, ProviderKind};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const PROVIDERS: &str = "providers";
const STATUS: &str = "onboarding";
const STEP: &str = "onboarding_step";
const PROBE: Duration = Duration::from_secs(30);
const CLI_STATUS: Duration = Duration::from_secs(8);
const ANTHROPIC_VERSION: &str = "2023-06-01";
const SUPERVISOR_DEFAULT: &str = "high";

pub const CLAUDE_INSTALL: &str = "curl -fsSL https://claude.ai/install.sh | bash";
pub const CODEX_INSTALL: &str = "npm install -g @openai/codex";

const CLAUDE_LEVELS: [(&str, &str); 3] = [("low", "haiku"), ("medium", "sonnet"), ("high", "opus")];

pub fn cli_agents() -> Vec<crate::agent::JsonCommand> {
    let claude = json!({
        "name": "claude",
        "label": "Claude Code",
        "program": "claude",
        "args": ["-p", "--output-format", "stream-json", "--verbose", "--dangerously-skip-permissions"],
        "resume_args": ["--resume", "{session}"],
        "model_args": ["--model", "{model}"],
        "session": "/session_id",
        "text": "/message/content",
        "input_tokens": "/message/usage/input_tokens",
        "output_tokens": "/message/usage/output_tokens",
    });
    let codex = json!({
        "name": "codex",
        "label": "Codex",
        "program": "codex",
        "args": ["exec", "--json", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox"],
        "model_args": ["--model", "{model}"],
        "session": "/thread_id",
        "text": "/item/text",
        "error": "/error/message",
    });
    [claude, codex].into_iter().filter_map(|v| serde_json::from_value(v).ok()).collect()
}

pub fn claude_models() -> Vec<String> {
    CLAUDE_LEVELS.iter().map(|(_, m)| m.to_string()).collect()
}

fn readable(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn claude_login_on_disk(dir: &Path, state: &Path) -> Option<&'static str> {
    let creds = readable(&dir.join(".credentials.json"));
    if creds.as_ref().is_some_and(|c| c.get("claudeAiOauth").is_some()) {
        return Some("signed in with a Claude subscription");
    }
    if readable(state).is_some_and(|c| c.get("oauthAccount").is_some()) {
        return Some("signed in with a Claude account");
    }
    None
}

fn codex_login_on_disk(dir: &Path) -> Option<&'static str> {
    let auth = readable(&dir.join("auth.json"))?;
    let set = |k: &str| auth.get(k).is_some_and(|v| !v.is_null() && v != "");
    if set("tokens") {
        return Some("signed in with a ChatGPT account");
    }
    if set("OPENAI_API_KEY") {
        return Some("signed in with an OpenAI API key");
    }
    None
}

fn cli_status(program: &Path, args: &[&str]) -> Option<(bool, String)> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(status) => {
                let out = child.wait_with_output().ok()?;
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                return Some((status.success(), text.trim().to_string()));
            }
            None if start.elapsed() > CLI_STATUS => {
                let _ = child.kill();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn detect(
    found: Option<&'static str>,
    program: Option<PathBuf>,
    status_args: &[&str],
    install: &str,
    login: &str,
) -> CliLogin {
    let mut out = CliLogin {
        installed: program.is_some(),
        path: program.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        install: install.into(),
        login: login.into(),
        ..Default::default()
    };
    if let Some(found) = found {
        out.logged_in = true;
        out.detail = found.into();
        return out;
    }
    match program.as_deref().and_then(|p| cli_status(p, status_args)) {
        Some((true, text)) => {
            out.logged_in = true;
            out.detail = text.lines().next().unwrap_or("signed in").to_string();
        }
        Some((false, text)) => out.detail = text.lines().next().unwrap_or_default().to_string(),
        None if program.is_some() => out.detail = "the CLI did not answer".into(),
        None => {}
    }
    out
}

pub fn claude_login(dir: &Path, state: &Path, program: Option<PathBuf>) -> CliLogin {
    let found = claude_login_on_disk(dir, state);
    detect(found, program, &["auth", "status"], CLAUDE_INSTALL, "claude auth login")
}

pub fn codex_login(dir: &Path, program: Option<PathBuf>) -> CliLogin {
    let found = codex_login_on_disk(dir);
    detect(found, program, &["login", "status"], CODEX_INSTALL, "codex login")
}

fn user_home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn detect_claude() -> CliLogin {
    let (dir, state) = match env_dir("CLAUDE_CONFIG_DIR") {
        Some(d) => (d.clone(), d.join(".claude.json")),
        None => (user_home().join(".claude"), user_home().join(".claude.json")),
    };
    claude_login(&dir, &state, crate::agent::resolve(Path::new("claude")))
}

pub fn detect_codex() -> CliLogin {
    let dir = env_dir("CODEX_HOME").unwrap_or_else(|| user_home().join(".codex"));
    codex_login(&dir, crate::agent::resolve(Path::new("codex")))
}

pub fn login_command(cli: &str) -> Option<(PathBuf, Vec<&'static str>)> {
    match cli {
        "claude" => Some((crate::agent::resolve(Path::new("claude"))?, vec!["auth", "login"])),
        "codex" => Some((crate::agent::resolve(Path::new("codex"))?, vec!["login"])),
        "codex_device" => {
            Some((crate::agent::resolve(Path::new("codex"))?, vec!["login", "--device-auth"]))
        }
        _ => None,
    }
}

pub fn saved(store: &Store) -> Vec<Provider> {
    store
        .setting(PROVIDERS)
        .ok()
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(store: &Store, list: &[Provider]) -> Result<(), StoreError> {
    let text = serde_json::to_string(list).map_err(|e| StoreError(e.to_string()))?;
    store.set_setting(PROVIDERS, &text)
}

pub fn masked(list: &[Provider]) -> Vec<Provider> {
    list.iter()
        .map(|p| Provider { key: String::new(), has_key: !p.key.trim().is_empty(), ..p.clone() })
        .collect()
}

pub fn merged(old: &[Provider], new: Vec<Provider>) -> Vec<Provider> {
    let mut out: Vec<Provider> = Vec::new();
    for mut p in new {
        if out.iter().any(|o| o.kind == p.kind) {
            continue;
        }
        p.url = p.url.trim().to_string();
        p.key = p.key.trim().to_string();
        if p.kind.subscription() {
            p.url.clear();
            p.key.clear();
        } else if p.key.is_empty() && p.has_key {
            p.key =
                old.iter().find(|o| o.kind == p.kind).map(|o| o.key.clone()).unwrap_or_default();
        }
        p.has_key = !p.key.is_empty();
        for m in [&mut p.low, &mut p.medium, &mut p.high, &mut p.supervisor] {
            *m = m.trim().to_string();
        }
        out.push(p);
    }
    let first = out.iter().position(|p| p.default).or(if out.is_empty() { None } else { Some(0) });
    for (i, p) in out.iter_mut().enumerate() {
        p.default = Some(i) == first;
    }
    out
}

pub fn status(store: &Store) -> String {
    store.setting(STATUS).ok().flatten().unwrap_or_default()
}

pub fn set_status(store: &Store, status: &str, step: Option<&str>) -> Result<(), StoreError> {
    match status.trim() {
        "" => store.delete_setting(STATUS)?,
        s => store.set_setting(STATUS, s)?,
    }
    if let Some(step) = step {
        match step.trim() {
            "" => store.delete_setting(STEP)?,
            s => store.set_setting(STEP, s)?,
        }
    }
    Ok(())
}

pub fn default_provider(list: &[Provider]) -> Option<&Provider> {
    list.iter().find(|p| p.default)
}

pub fn default_agent(list: &[Provider]) -> String {
    default_provider(list).map(|p| p.kind.agent().to_string()).unwrap_or_default()
}

pub fn onboarding(store: &Store, model_set: bool, claude: CliLogin, codex: CliLogin) -> Onboarding {
    let list = saved(store);
    let state = status(store);
    let needed = state == "open" || (state.is_empty() && list.is_empty() && !model_set);
    Onboarding {
        needed,
        status: if state.is_empty() { "new".into() } else { state },
        step: store.setting(STEP).ok().flatten().unwrap_or_default(),
        default_agent: default_agent(&list),
        providers: masked(&list),
        claude,
        codex,
    }
}

pub fn native_model(
    list: &[Provider],
    current: &crate::agent::native::Model,
) -> Option<crate::agent::native::Model> {
    let p = default_provider(list)
        .filter(|p| !p.kind.subscription())
        .or_else(|| list.iter().find(|p| !p.kind.subscription()))?;
    let url = if p.url.is_empty() { p.kind.default_url().to_string() } else { p.url.clone() };
    let first = [&p.medium, &p.high, &p.low].into_iter().find(|m| !m.is_empty()).cloned();
    let mut out = current.clone();
    out.url = url;
    out.key = p.key.clone();
    out.model = first.unwrap_or_else(|| current.model.clone());
    let level =
        |m: &str| crate::agent::native::Level { model: m.to_string(), ..Default::default() };
    out.low = level(&p.low);
    out.medium = level(&p.medium);
    out.high = level(&p.high);
    Some(out)
}

pub fn apply(
    store: &Store,
    shared: Option<&crate::agent::native::Shared>,
) -> Result<(), StoreError> {
    let Some(shared) = shared else {
        return Ok(());
    };
    if let Some(next) = native_model(&saved(store), &shared.get()) {
        let text = serde_json::to_string(&next).map_err(|e| StoreError(e.to_string()))?;
        store.set_setting("model", &text)?;
        shared.set(next);
    }
    Ok(())
}

fn subscription_kind(agent: &str) -> Option<ProviderKind> {
    match agent {
        "claude" => Some(ProviderKind::ClaudeSubscription),
        "codex" => Some(ProviderKind::CodexSubscription),
        _ => None,
    }
}

pub fn level_model(store: &Store, agent: &str, level: &str) -> Option<String> {
    let kind = subscription_kind(agent)?;
    let list = saved(store);
    let own = list.iter().find(|p| p.kind == kind).and_then(|p| match level {
        "low" => Some(&p.low),
        "medium" => Some(&p.medium),
        "high" => Some(&p.high),
        _ => None,
    });
    let chosen = own.map(|m| m.trim()).filter(|m| !m.is_empty()).map(str::to_string);
    chosen.or_else(|| {
        if kind != ProviderKind::ClaudeSubscription {
            return None;
        }
        CLAUDE_LEVELS.iter().find(|(l, _)| *l == level).map(|(_, m)| m.to_string())
    })
}

pub fn supervisor_model(store: &Store, agent: &str) -> Option<String> {
    let list = saved(store);
    let kind = subscription_kind(agent);
    let p = match kind {
        Some(kind) => list.iter().find(|p| p.kind == kind),
        None => default_provider(&list)
            .filter(|p| !p.kind.subscription())
            .or_else(|| list.iter().find(|p| !p.kind.subscription())),
    };
    let chosen = p.map(|p| p.supervisor.trim().to_string()).filter(|m| !m.is_empty());
    chosen.or_else(|| (kind != Some(ProviderKind::CodexSubscription)).then(|| SUPERVISOR_DEFAULT.into()))
}

fn base(url: &str) -> String {
    url.trim().trim_end_matches('/').trim_end_matches("/chat/completions").to_string()
}

fn effective_url(p: &Provider) -> String {
    base(if p.url.trim().is_empty() { p.kind.default_url() } else { &p.url })
}

fn explain(status: reqwest::StatusCode, body: &str) -> String {
    let text = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("error"))
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().chars().take(300).collect());
    if text.is_empty() { status.to_string() } else { format!("{status}: {text}") }
}

fn anthropic(p: &Provider) -> bool {
    p.kind == ProviderKind::AnthropicKey
}

fn authed(p: &Provider, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let key = p.key.trim();
    if key.is_empty() {
        return req;
    }
    let req = req.bearer_auth(key);
    if anthropic(p) {
        return req.header("x-api-key", key).header("anthropic-version", ANTHROPIC_VERSION);
    }
    req
}

pub async fn models(p: &Provider) -> Result<Vec<String>, String> {
    match p.kind {
        ProviderKind::ClaudeSubscription => return Ok(claude_models()),
        ProviderKind::CodexSubscription => return Ok(Vec::new()),
        _ => {}
    }
    let url = effective_url(p);
    if url.is_empty() {
        return Err("the server address is empty".into());
    }
    if p.kind.needs_key() && p.key.trim().is_empty() {
        return Err("paste an API key first".into());
    }
    let req = authed(p, crate::http::client().get(format!("{url}/models")).timeout(PROBE));
    let resp =
        req.send().await.map_err(|e| format!("{url}: {}", crate::agent::native::describe(&e)))?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| crate::agent::native::describe(&e))?;
    if !status.is_success() {
        return Err(explain(status, &text));
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| format!("not a model list: {e}"))?;
    let mut ids: Vec<String> = v
        .get("data")
        .or_else(|| v.get("models"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").or_else(|| m.get("name")).and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

async fn cli_check(kind: ProviderKind) -> ProviderCheck {
    let found = tokio::task::spawn_blocking(move || match kind {
        ProviderKind::ClaudeSubscription => (detect_claude(), "Claude Code"),
        _ => (detect_codex(), "Codex"),
    })
    .await;
    let Ok((cli, name)) = found else {
        return ProviderCheck { ok: false, message: "could not check the login".into() };
    };
    if !cli.installed && !cli.logged_in {
        return ProviderCheck {
            ok: false,
            message: format!("{name} is not installed: {}", cli.install),
        };
    }
    if cli.logged_in {
        return ProviderCheck { ok: true, message: cli.detail };
    }
    let why = if cli.detail.is_empty() { "not signed in".to_string() } else { cli.detail };
    ProviderCheck { ok: false, message: format!("{why}. Run: {}", cli.login) }
}

pub async fn test(p: &Provider) -> ProviderCheck {
    if p.kind.subscription() {
        return cli_check(p.kind).await;
    }
    let fail = |message: String| ProviderCheck { ok: false, message };
    let url = effective_url(p);
    if url.is_empty() {
        return fail("the server address is empty".into());
    }
    if p.kind.needs_key() && p.key.trim().is_empty() {
        return fail("paste an API key first".into());
    }
    let model = match [&p.medium, &p.high, &p.low].into_iter().find(|m| !m.is_empty()) {
        Some(m) => m.clone(),
        None => match models(p).await {
            Ok(list) => match list.into_iter().next() {
                Some(m) => m,
                None => return fail("the server lists no models".into()),
            },
            Err(e) => return fail(e),
        },
    };
    let ask = "Reply with the single word ok.";
    let body = json!({ "model": model, "max_tokens": 16, "stream": false, "messages": [{ "role": "user", "content": ask }] });
    let req = crate::http::client().post(format!("{url}/chat/completions")).json(&body);
    let resp = match authed(p, req.timeout(PROBE)).send().await {
        Ok(r) => r,
        Err(e) => return fail(format!("{url}: {}", crate::agent::native::describe(&e))),
    };
    let status = resp.status();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return fail(crate::agent::native::describe(&e)),
    };
    if !status.is_success() {
        return fail(explain(status, &text));
    }
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if v.get("choices").is_none() {
        let shown: String = text.chars().take(200).collect();
        return fail(format!("{url} answered, but not like a chat server: {shown}"));
    }
    ProviderCheck { ok: true, message: format!("{model} answered") }
}

pub async fn health(list: Vec<Provider>) -> Vec<(ProviderKind, ProviderCheck)> {
    let checks = list.iter().map(|p| async move { (p.kind, test(p).await) });
    futures_util::future::join_all(checks).await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    pub(crate) fn fake_server(routes: Vec<(&'static str, u16, &'static str)>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let line = head.lines().next().unwrap_or_default().to_string();
                let (code, body) = routes
                    .iter()
                    .find(|(path, _, _)| line.contains(path))
                    .map(|(_, c, b)| (*c, *b))
                    .unwrap_or((404, "{}"));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://{addr}/v1")
    }

    fn endpoint(url: &str) -> Provider {
        Provider { kind: ProviderKind::Endpoint, url: url.into(), ..Default::default() }
    }

    #[test]
    fn the_claude_and_codex_agents_are_there_without_any_config() {
        let names: Vec<_> = cli_agents().into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["claude", "codex"]);
    }

    #[test]
    fn an_existing_claude_and_codex_login_is_found_and_missing_ones_reported() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let claude_at = |p: Option<PathBuf>| {
            claude_login(&home.join(".claude"), &home.join(".claude.json"), p)
        };
        let codex_at = |p: Option<PathBuf>| codex_login(&home.join(".codex"), p);

        let none = claude_at(None);
        assert!(!none.installed && !none.logged_in, "{none:?}");
        assert!(none.install.contains("claude"));
        assert_eq!(none.login, "claude auth login");
        let none = codex_at(None);
        assert!(!none.installed && !none.logged_in, "{none:?}");
        assert_eq!(none.login, "codex login");

        write(&home.join(".claude/.credentials.json"), r#"{"claudeAiOauth":{"accessToken":"x"}}"#);
        write(&home.join(".codex/auth.json"), r#"{"tokens":{"id_token":"x"}}"#);
        let claude = claude_at(Some(PathBuf::from("/bin/claude")));
        assert!(claude.installed && claude.logged_in, "{claude:?}");
        assert!(!claude.detail.contains("accessToken"), "no token in {claude:?}");
        let codex = codex_at(None);
        assert!(codex.logged_in && !codex.installed, "{codex:?}");
    }

    #[test]
    fn a_claude_account_in_the_state_file_counts_and_an_empty_codex_file_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write(&home.join(".claude.json"), r#"{"oauthAccount":{"emailAddress":"a@b"}}"#);
        assert!(claude_login(&home.join(".claude"), &home.join(".claude.json"), None).logged_in);
        write(&home.join(".codex/auth.json"), r#"{"OPENAI_API_KEY":null,"tokens":null}"#);
        assert!(!codex_login(&home.join(".codex"), None).logged_in);
    }

    #[cfg(unix)]
    #[test]
    fn without_a_file_the_cli_is_asked_for_its_status() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let home = dir.path().join("home");
        let yes = claude_login(&home, &home.join("s.json"), Some(script("in", "echo 'Logged in as me'")));
        assert!(yes.logged_in && yes.detail == "Logged in as me", "{yes:?}");
        let no = codex_login(&home, Some(script("out", "echo 'Not logged in'; exit 1")));
        assert!(no.installed && !no.logged_in && no.detail == "Not logged in", "{no:?}");
    }

    #[tokio::test]
    async fn a_working_endpoint_passes_the_test_call() {
        let url = fake_server(vec![
            ("/models", 200, r#"{"data":[{"id":"small"},{"id":"big"}]}"#),
            ("/chat/completions", 200, r#"{"choices":[{"message":{"content":"ok"}}]}"#),
        ]);
        let got = test(&endpoint(&url)).await;
        assert!(got.ok, "{got:?}");
        assert!(got.message.contains("big"), "{got:?}");
        assert_eq!(models(&endpoint(&url)).await.unwrap(), ["big", "small"]);
    }

    #[tokio::test]
    async fn a_refused_key_shows_the_servers_own_error() {
        let url = fake_server(vec![
            ("/models", 401, r#"{"error":{"message":"Incorrect API key provided"}}"#),
            ("/chat/completions", 401, r#"{"error":{"message":"Incorrect API key provided"}}"#),
        ]);
        let p = Provider {
            kind: ProviderKind::OpenaiKey,
            url,
            key: "sk-bad".into(),
            ..Default::default()
        };
        let got = test(&p).await;
        assert!(!got.ok);
        assert!(
            got.message.contains("401") && got.message.contains("Incorrect API key provided"),
            "{got:?}"
        );
        let tried = Provider { medium: "gpt".into(), ..p };
        let got = test(&tried).await;
        assert!(!got.ok && got.message.contains("Incorrect API key provided"), "{got:?}");
    }

    #[tokio::test]
    async fn an_unreachable_server_and_a_missing_key_fail_with_a_reason() {
        let got = test(&endpoint("http://127.0.0.1:9/v1")).await;
        assert!(!got.ok && got.message.contains("127.0.0.1:9"), "{got:?}");
        let got = test(&Provider { kind: ProviderKind::AnthropicKey, ..Default::default() }).await;
        assert_eq!(got.message, "paste an API key first");
    }

    #[tokio::test]
    async fn the_anthropic_test_call_uses_the_chat_endpoint_the_agent_uses() {
        let url = fake_server(vec![
            ("/models", 200, r#"{"data":[{"id":"claude-x"}]}"#),
            ("/chat/completions", 200, r#"{"choices":[{"message":{"content":"ok"}}]}"#),
        ]);
        let p = Provider {
            kind: ProviderKind::AnthropicKey,
            url,
            key: "sk-ant".into(),
            ..Default::default()
        };
        let got = test(&p).await;
        assert!(got.ok && got.message.contains("claude-x"), "{got:?}");
    }

    #[test]
    fn saving_keeps_a_key_left_blank_and_names_exactly_one_default() {
        let old = vec![Provider {
            kind: ProviderKind::OpenaiKey,
            key: "sk-keep".into(),
            ..Default::default()
        }];
        let sent = vec![
            Provider { kind: ProviderKind::OpenaiKey, has_key: true, ..Default::default() },
            Provider {
                kind: ProviderKind::ClaudeSubscription,
                url: "x".into(),
                key: "y".into(),
                default: true,
                ..Default::default()
            },
            Provider { kind: ProviderKind::OpenaiKey, key: "dup".into(), ..Default::default() },
        ];
        let got = merged(&old, sent);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].key, "sk-keep");
        assert!(got[1].default && !got[0].default);
        assert!(got[1].url.is_empty() && got[1].key.is_empty());
        assert!(masked(&got)[0].key.is_empty() && masked(&got)[0].has_key);
        assert!(merged(&old, vec![]).is_empty());
        let cleared =
            merged(&old, vec![Provider { kind: ProviderKind::OpenaiKey, ..Default::default() }]);
        assert!(cleared[0].key.is_empty() && cleared[0].default);
    }

    #[test]
    fn the_default_native_provider_becomes_the_agents_model_and_levels() {
        let list = vec![Provider {
            kind: ProviderKind::OpenaiKey,
            key: "sk".into(),
            default: true,
            low: "mini".into(),
            medium: "mid".into(),
            high: "big".into(),
            ..Default::default()
        }];
        let m = native_model(&list, &Default::default()).unwrap();
        assert_eq!(
            (m.url.as_str(), m.key.as_str(), m.model.as_str()),
            ("https://api.openai.com/v1", "sk", "mid")
        );
        assert_eq!(m.level("high").unwrap().model, "big");
        assert_eq!(m.level("low").unwrap().model, "mini");
        let subscription = vec![Provider {
            kind: ProviderKind::ClaudeSubscription,
            default: true,
            ..Default::default()
        }];
        assert!(native_model(&subscription, &Default::default()).is_none());
    }

    #[test]
    fn levels_and_the_supervisor_follow_the_chosen_models_with_claude_defaults() {
        let store = Store::memory().unwrap();
        assert_eq!(level_model(&store, "claude", "high").as_deref(), Some("opus"));
        assert_eq!(level_model(&store, "codex", "high"), None);
        assert_eq!(level_model(&store, "native", "high"), None);
        assert_eq!(supervisor_model(&store, "claude").as_deref(), Some("high"));
        assert_eq!(supervisor_model(&store, "codex"), None);
        let list = merged(
            &[],
            vec![Provider {
                kind: ProviderKind::ClaudeSubscription,
                medium: "sonnet-x".into(),
                supervisor: "opus-x".into(),
                ..Default::default()
            }],
        );
        save(&store, &list).unwrap();
        assert_eq!(level_model(&store, "claude", "medium").as_deref(), Some("sonnet-x"));
        assert_eq!(level_model(&store, "claude", "low").as_deref(), Some("haiku"));
        assert_eq!(supervisor_model(&store, "claude").as_deref(), Some("opus-x"));
    }

    #[test]
    fn the_setup_is_needed_only_when_nothing_is_set_up_or_decided() {
        let store = Store::memory().unwrap();
        let o = |store: &Store, model_set| {
            onboarding(store, model_set, Default::default(), Default::default())
        };
        assert!(o(&store, false).needed);
        assert!(!o(&store, true).needed, "an existing model setting is respected");
        set_status(&store, "open", Some("models")).unwrap();
        let resumed = o(&store, true);
        assert!(resumed.needed && resumed.step == "models", "a setup left half done comes back");
        set_status(&store, "skipped", None).unwrap();
        assert!(!o(&store, false).needed);
        assert_eq!(o(&store, false).status, "skipped");
        set_status(&store, "", None).unwrap();
        save(&store, &[endpoint("http://x/v1")]).unwrap();
        assert!(!o(&store, false).needed);
    }
}
