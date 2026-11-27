use crate::runner::Runner;
use crate::store::Store;
use agentbench_core::{RESTART_DRAIN_DEFAULT, RestartSettings, Restarted, SessionId};
use std::sync::atomic::Ordering;
use std::time::Duration;

pub const DRAIN_SETTING: &str = "restart_drain_secs";
pub const SUPERVISOR_SETTING: &str = "supervisor_may_restart";
const REPORT_SETTING: &str = "restart_report";
pub const DRAIN_MAX: u64 = 3600;
const GRACE: Duration = Duration::from_secs(5);
const SIGNAL_DRAIN: Duration = Duration::from_secs(20);
pub const SYSTEMD_RESTART: i32 = 75;

pub fn settings(store: &Store) -> RestartSettings {
    RestartSettings {
        drain_secs: store
            .setting(DRAIN_SETTING)
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(RESTART_DRAIN_DEFAULT)
            .min(DRAIN_MAX),
        supervisor_may_restart: store.setting(SUPERVISOR_SETTING).ok().flatten().as_deref() == Some("1"),
    }
}

pub fn save(store: &Store, s: &RestartSettings) -> Result<RestartSettings, String> {
    store.set_setting(DRAIN_SETTING, &s.drain_secs.min(DRAIN_MAX).to_string()).map_err(|e| e.0)?;
    store
        .set_setting(SUPERVISOR_SETTING, if s.supervisor_may_restart { "1" } else { "0" })
        .map_err(|e| e.0)?;
    Ok(settings(store))
}

pub fn drain_limit(store: &Store, asked: Option<u64>) -> Duration {
    Duration::from_secs(asked.unwrap_or_else(|| settings(store).drain_secs).min(DRAIN_MAX))
}

pub async fn restart(runner: &Runner, asked: Option<u64>, by: Option<SessionId>) -> Restarted {
    let limit = drain_limit(runner.store(), asked);
    let mut done = runner.shut_down(limit, by).await;
    if let Some(by) = by {
        let report = serde_json::json!({ "session": by, "summary": done.summary() });
        let _ = runner.store().set_setting(REPORT_SETTING, &report.to_string());
    }
    runner.relaunch.store(true, Ordering::SeqCst);
    done.restarting = true;
    let signal = runner.restart.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        signal.notify_one();
    });
    done
}

pub fn report_for(store: &Store, session: SessionId) -> Option<String> {
    let raw = store.setting(REPORT_SETTING).ok().flatten()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v["session"].as_str() != Some(session.to_string().as_str()) {
        return None;
    }
    let _ = store.delete_setting(REPORT_SETTING);
    v["summary"].as_str().map(str::to_string)
}

pub fn relaunching(runner: &Runner) -> bool {
    runner.relaunch.load(Ordering::SeqCst)
}

pub async fn stopped(runner: Runner, signal: impl std::future::Future<Output = ()> + Send + 'static) {
    let restart = runner.restart.clone();
    tokio::select! {
        _ = signal => {}
        _ = restart.notified() => {}
    }
    let mut limit = drain_limit(runner.store(), None);
    if !relaunching(&runner) && under_systemd() {
        limit = limit.min(SIGNAL_DRAIN);
    }
    if !relaunching(&runner) {
        tracing::info!("stopping: letting running turns finish, up to {} s; signal again to stop now", limit.as_secs());
    }
    let done = tokio::select! {
        d = runner.shut_down(limit, None) => d,
        _ = terminated() => {
            tracing::info!("stopping now");
            runner.drain(Duration::ZERO, None).await
        }
    };
    tracing::info!("{}", done.summary());
}

pub async fn terminated() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

pub async fn serve_until<F>(serving: F, stopping: impl std::future::Future<Output = ()>) -> Result<(), String>
where
    F: std::future::Future<Output = Result<(), String>>,
{
    tokio::pin!(serving);
    tokio::select! {
        r = &mut serving => return r,
        _ = stopping => {}
    }
    tokio::select! {
        r = &mut serving => r,
        _ = tokio::time::sleep(GRACE) => Ok(()),
    }
}

pub fn program() -> std::path::PathBuf {
    let argv0 = std::env::args_os().next().map(std::path::PathBuf::from);
    if let Some(p) = argv0.filter(|p| p.is_absolute() && p.is_file()) {
        return p;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| "agentbench-server".into());
    let text = exe.to_string_lossy();
    match text.strip_suffix(" (deleted)") {
        Some(t) => t.into(),
        None => exe,
    }
}

#[cfg(unix)]
fn under_systemd() -> bool {
    let parent = std::os::unix::process::parent_id();
    std::env::var_os("INVOCATION_ID").is_some()
        && std::fs::read_to_string(format!("/proc/{parent}/comm")).is_ok_and(|c| c.trim() == "systemd")
}

#[cfg(not(unix))]
fn under_systemd() -> bool {
    false
}

#[cfg(unix)]
pub fn relaunch() -> String {
    use std::os::unix::process::CommandExt;
    if under_systemd() {
        tracing::info!("exiting with {SYSTEMD_RESTART} so systemd starts the server again");
        std::process::exit(SYSTEMD_RESTART);
    }
    let program = program();
    tracing::info!("restarting {}", program.display());
    let e = std::process::Command::new(&program).args(std::env::args_os().skip(1)).exec();
    format!("could not restart {}: {e}", program.display())
}

#[cfg(not(unix))]
pub fn relaunch() -> String {
    let program = program();
    match std::process::Command::new(&program).args(std::env::args_os().skip(1)).spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => format!("could not restart {}: {e}", program.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::JsonCommand;
    use agentbench_core::SessionStatus;

    struct Served {
        server: crate::tui::Server,
        store: Store,
        session: SessionId,
        serving: tokio::task::JoinHandle<Result<(), String>>,
        _dir: tempfile::TempDir,
    }

    async fn served(seconds: u32) -> Served {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::memory().unwrap();
        crate::projects::ensure_supervisor(&store, &dir.path().join("sup")).unwrap();
        let (_, main) = crate::store::testing::project(&store, dir.path().to_str().unwrap());
        let session = store.add_session(main.id, "slow", "slow", None, None).unwrap().id;
        let slow = JsonCommand {
            name: "slow".into(),
            label: None,
            program: "sh".into(),
            args: vec!["-c".into(), format!(r#"read p; sleep {seconds}; echo "{{\"text\":\"done: $p\"}}""#)],
            resume_args: vec![],
            model_args: vec![],
            prompt_on_stdin: true,
            session: None,
            text: Some("/text".into()),
            error: None,
            input_tokens: None,
            output_tokens: None,
            plain: false,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let token = crate::token(&store).unwrap();
        let serving = tokio::spawn(crate::serve_agents(
            store.clone(),
            vec![Box::new(slow)],
            None,
            listener,
            dir.path().join("models"),
            std::future::pending(),
        ));
        let server = crate::tui::Server { url, token };
        let r = crate::http::client()
            .post(format!("{}/api/sessions/{session}/messages", server.base()))
            .bearer_auth(&server.token)
            .json(&serde_json::json!({"text": "build"}))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.status());
        for _ in 0..200 {
            if store.session(session).unwrap().status == SessionStatus::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Served { server, store, session, serving, _dir: dir }
    }

    #[tokio::test]
    async fn the_restart_command_waits_for_a_turn_that_finishes_and_counts_it() {
        let s = served(1).await;
        let done = crate::cli::ask_restart(&s.server, Some(10)).await.unwrap();
        assert_eq!(done, Restarted { waited: 1, finished: 1, interrupted: 0, restarting: true, ..Default::default() });
        assert_eq!(
            done.summary(),
            "waited for 1 running turn: 1 finished, 0 parked at a tool boundary, 0 interrupted by the restart"
        );
        let ended = tokio::time::timeout(Duration::from_secs(10), s.serving).await.expect("the server stops to restart");
        assert!(ended.unwrap().is_ok());
        assert_eq!(s.store.session(s.session).unwrap().status, SessionStatus::Idle);
        assert!(s.store.setting("restart_interrupted").unwrap().is_none());
    }

    #[tokio::test]
    async fn the_restart_command_interrupts_a_turn_that_outlives_the_drain() {
        let s = served(30).await;
        let started = std::time::Instant::now();
        let done = crate::cli::ask_restart(&s.server, Some(1)).await.unwrap();
        assert_eq!(
            done,
            Restarted { waited: 1, finished: 0, interrupted: 1, restarting: true, parked: vec![], cut: vec![s.session] }
        );
        assert!(done.summary().ends_with(&format!("; interrupted: {}", s.session)), "{}", done.summary());
        assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
        tokio::time::timeout(Duration::from_secs(10), s.serving).await.expect("the server stops").unwrap().unwrap();
        let kept = s.store.setting("restart_interrupted").unwrap().unwrap();
        assert!(kept.contains(&s.session.to_string()), "{kept}");
    }

    #[test]
    fn the_drain_limit_defaults_to_ten_minutes_and_is_kept() {
        let store = Store::memory().unwrap();
        assert_eq!(settings(&store), RestartSettings { drain_secs: 600, supervisor_may_restart: false });
        save(&store, &RestartSettings { drain_secs: 30, supervisor_may_restart: true }).unwrap();
        assert_eq!(drain_limit(&store, None), Duration::from_secs(30));
        assert_eq!(drain_limit(&store, Some(5)), Duration::from_secs(5));
        save(&store, &RestartSettings { drain_secs: 99_999, supervisor_may_restart: false }).unwrap();
        assert_eq!(settings(&store).drain_secs, DRAIN_MAX);
    }

    struct Native {
        store: Store,
        url: String,
        seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        worker: SessionId,
        other: SessionId,
        dir: tempfile::TempDir,
    }

    fn call(command: &str) -> String {
        use crate::titles::fake::streamed;
        let args = serde_json::json!({ "command": command }).to_string();
        streamed(serde_json::json!({"tool_calls": [
            {"index": 0, "id": "c1", "function": {"name": "bash", "arguments": args}},
        ]}))
    }

    fn native(replies: Vec<(u16, String)>) -> Native {
        let dir = tempfile::tempdir().unwrap();
        let (url, seen) = crate::titles::fake::server(replies);
        let store = Store::memory().unwrap();
        crate::projects::ensure_supervisor(&store, &dir.path().join("sup")).unwrap();
        let (_, main) = crate::store::testing::project(&store, dir.path().to_str().unwrap());
        let worker = store.add_session(main.id, "native", "build", None, None).unwrap().id;
        let other = store.add_session(main.id, "echo", "later", None, None).unwrap().id;
        Native { store, url, seen, worker, other, dir }
    }

    async fn serve(n: &Native) -> (crate::tui::Server, tokio::task::JoinHandle<Result<(), String>>) {
        use crate::agent::native::{Model, Native as Agent};
        let model = Agent::new(Model { url: n.url.clone(), model: "m".into(), ..Model::default() });
        let echo = JsonCommand {
            name: "echo".into(),
            label: None,
            program: "sh".into(),
            args: vec!["-c".into(), r#"read p; echo "{\"text\":\"ran: $p\"}""#.into()],
            resume_args: vec![],
            model_args: vec![],
            prompt_on_stdin: true,
            session: None,
            text: Some("/text".into()),
            error: None,
            input_tokens: None,
            output_tokens: None,
            plain: false,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let token = crate::token(&n.store).unwrap();
        let serving = tokio::spawn(crate::serve_agents(
            n.store.clone(),
            vec![Box::new(model), Box::new(echo)],
            None,
            listener,
            n.dir.path().join("models"),
            std::future::pending(),
        ));
        (crate::tui::Server { url, token }, serving)
    }

    async fn say(server: &crate::tui::Server, session: SessionId, text: &str) {
        let r = crate::http::client()
            .post(format!("{}/api/sessions/{session}/messages", server.base()))
            .bearer_auth(&server.token)
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "{}", r.status());
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..600 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("never: {what}");
    }

    fn in_call(store: &Store, session: SessionId) -> bool {
        shown(store, session).iter().any(|e| matches!(e, agentbench_core::Entry::Tool { name, .. } if name == "bash"))
    }

    fn shown(store: &Store, session: SessionId) -> Vec<agentbench_core::Entry> {
        agentbench_core::shown(&store.messages(session, 0).unwrap()).into_iter().map(|m| m.entry).collect()
    }

    #[tokio::test]
    async fn a_turn_in_a_tool_call_is_parked_when_it_returns_and_carries_on_after_the_restart() {
        use crate::titles::fake::streamed;
        let n = native(vec![
            (200, call("sleep 5; echo built-ok")),
            (200, streamed(serde_json::json!({"content": "all built"}))),
        ]);
        let (server, serving) = serve(&n).await;
        say(&server, n.worker, "build it").await;
        until("the tool call starts", || in_call(&n.store, n.worker)).await;
        let begun = std::time::Instant::now();
        let restart = tokio::spawn({
            let server = server.clone();
            async move { crate::cli::ask_restart(&server, Some(60)).await.unwrap() }
        });
        until("the drain starts", || n.store.setting("restart_draining").unwrap().is_some()).await;
        say(&server, n.other, "later").await;
        let done = restart.await.unwrap();
        assert!(begun.elapsed() >= Duration::from_secs(3), "the call ran to its end: {:?}", begun.elapsed());
        assert_eq!(
            done,
            Restarted { waited: 1, finished: 0, interrupted: 0, restarting: true, parked: vec![n.worker], cut: vec![] }
        );
        assert!(done.summary().contains(&format!("parked: {}", n.worker)), "{}", done.summary());
        tokio::time::timeout(Duration::from_secs(10), serving).await.expect("the server stops").unwrap().unwrap();
        assert_eq!(n.seen.lock().unwrap().len(), 1, "no model call after the tool boundary");
        assert!(shown(&n.store, n.other).iter().all(|e| !matches!(e, agentbench_core::Entry::Assistant { .. })));

        let (_back, serving) = serve(&n).await;
        until("the parked turn carries on", || n.seen.lock().unwrap().len() == 2).await;
        until("the parked turn ends", || n.store.session(n.worker).unwrap().status == SessionStatus::Idle).await;
        let asked = n.seen.lock().unwrap()[1]["messages"].as_array().unwrap().clone();
        let last = asked.last().unwrap();
        assert_eq!(last["role"], "tool", "{asked:?}");
        assert!(last["content"].as_str().unwrap().contains("built-ok"), "{last}");
        assert!(
            !asked.iter().any(|m| m["content"].as_str().is_some_and(|c| c.contains("server restarted"))),
            "no restart prompt: {asked:?}"
        );
        let worker = shown(&n.store, n.worker);
        assert!(
            worker.iter().any(|e| matches!(e, agentbench_core::Entry::Tool { name, output: Some(o), .. } if name == "bash" && o.contains("built-ok"))),
            "{worker:?}"
        );
        assert!(matches!(worker.last(), Some(agentbench_core::Entry::Assistant { text }) if text == "all built"), "{worker:?}");
        until("the held turn runs", || {
            shown(&n.store, n.other)
                .iter()
                .any(|e| matches!(e, agentbench_core::Entry::Assistant { text } if text == "ran: later"))
        })
        .await;
        serving.abort();
    }

    #[tokio::test]
    async fn a_tool_call_that_never_returns_is_cut_at_the_drain_limit_and_reported() {
        let n = native(vec![(200, call("sleep 25; echo never"))]);
        let (server, serving) = serve(&n).await;
        say(&server, n.worker, "build it").await;
        until("the tool call starts", || in_call(&n.store, n.worker)).await;
        let begun = std::time::Instant::now();
        let done = crate::cli::ask_restart(&server, Some(1)).await.unwrap();
        assert!(begun.elapsed() < Duration::from_secs(15), "{:?}", begun.elapsed());
        assert_eq!(
            done,
            Restarted { waited: 1, finished: 0, interrupted: 1, restarting: true, parked: vec![], cut: vec![n.worker] }
        );
        assert!(done.summary().contains(&format!("interrupted: {}", n.worker)), "{}", done.summary());
        tokio::time::timeout(Duration::from_secs(15), serving).await.expect("the server stops").unwrap().unwrap();
        let kept = n.store.setting("restart_interrupted").unwrap().unwrap();
        assert!(kept.contains(&n.worker.to_string()), "{kept}");
    }
}
