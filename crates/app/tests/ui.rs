use eframe::Storage as _;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use foreman::App;
use foreman_core::{Entry, NewIssue};
use foreman_server::agent::JsonCommand;
use foreman_server::store::Store;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const TOKEN: &str = "test-token";

struct Rig {
    url: String,
    store: Store,
    _dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
}

fn repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ],
    ] {
        assert!(Command::new("git").arg("-C").arg(dir).args(&args).status().unwrap().success());
    }
}

fn worktree(main: &Path, branch: &str) {
    let path = main.parent().unwrap().join(format!("demo-{branch}"));
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(main)
            .args(["worktree", "add", "-q", "-b", branch])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
}

fn rig() -> Rig {
    rig_with(Vec::new())
}

fn rig_with(extra: Vec<Box<dyn foreman_server::agent::Agent>>) -> Rig {
    rig_on("http://127.0.0.1:9/v1", extra)
}

/// A model server that lists `n` models, for as many requests as come.
fn model_server(n: usize) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let data: Vec<String> = (0..n)
            .map(|i| format!(r#"{{"id":"vendor-{}/model-{i:05}-instruct"}}"#, i % 40))
            .collect();
        let body = format!(r#"{{"data":[{}]}}"#, data.join(","));
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{addr}/v1")
}

fn rig_on(model_url: &str, extra: Vec<Box<dyn foreman_server::agent::Agent>>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("demo");
    repo(&main);
    let store = Store::open(&dir.path().join("f.db")).unwrap();
    store.set_setting("token", TOKEN).unwrap();
    worktree(&main, "request-log");
    worktree(&main, "fix-login");
    let project = foreman_server::projects::add(&store, &main, None, false).unwrap();
    store
        .add_issue(&NewIssue {
            project_id: project.id,
            title: "Write a README".into(),
            ..Default::default()
        })
        .unwrap();
    let main_tree = store.main_worktree(project.id).unwrap();
    let session = store.add_session(main_tree.id, "echo", "say hello", None, None).unwrap();
    store.add_message(session.id, &Entry::user("say hello")).unwrap();
    store
        .add_message(session.id, &Entry::Assistant { text: "Hello from **the agent**.".into() })
        .unwrap();
    let side = store
        .worktrees(Some(project.id))
        .unwrap()
        .into_iter()
        .find(|w| w.branch.as_deref() == Some("fix-login"))
        .unwrap();
    store.add_session(side.id, "echo", "#3 Fix the login loop", None, None).unwrap();
    let second = store.add_session(side.id, "echo", "review the fix", None, None).unwrap();
    store.set_status(second.id, foreman_core::SessionStatus::Running).unwrap();
    store
        .add_message(
            session.id,
            &Entry::Tool {
                call_id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls -la"}),
                output: Some("README.md".into()),
                error: false,
            },
        )
        .unwrap();

    let echo = JsonCommand {
        name: "echo".into(),
        label: Some("Echo".into()),
        program: PathBuf::from("sh"),
        args: vec!["-c".into(), r#"read p; echo "{\"text\":\"echo: $p\"}""#.into()],
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
    let model_url = model_url.to_string();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let s = store.clone();
    rt.spawn(async move {
        let config = foreman_server::Config {
            model: foreman_server::agent::native::Model {
                url: model_url.clone(),
                model: "test-model".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let model = foreman_server::agent::native::Shared::new(config.model.clone());
        let mut agents: Vec<Box<dyn foreman_server::agent::Agent>> = vec![Box::new(echo)];
        agents.push(Box::new(foreman_server::agent::Native::shared(model.clone())));
        agents.extend(extra);
        foreman_server::serve_agents(s, agents, Some(model), addr, std::future::pending())
            .await
            .unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    Rig { url: addr.to_string(), store, _dir: dir, _rt: rt }
}

fn harness(url: String) -> Harness<'static, App> {
    harness_at(url, [1280.0, 800.0])
}

fn harness_at(url: String, size: [f32; 2]) -> Harness<'static, App> {
    Harness::builder().with_size(size).wgpu().build_eframe(move |cc| {
        let mut storage = Mem::default();
        storage.0.insert("url".into(), url);
        storage.0.insert("token".into(), TOKEN.into());
        App::new(&cc.egui_ctx, Some(&storage), None)
    })
}

#[derive(Default)]
struct Mem(std::collections::HashMap<String, String>);

impl eframe::Storage for Mem {
    fn get_string(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
    fn set_string(&mut self, key: &str, value: String) {
        self.0.insert(key.into(), value);
    }
    fn remove_string(&mut self, key: &str) {
        self.0.remove(key);
    }
    fn flush(&mut self) {}
}

fn until(h: &mut Harness<'_, App>, what: &str, ok: impl Fn(&Harness<'_, App>) -> bool) {
    let start = Instant::now();
    while !ok(h) {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    h.run_steps(3);
}

fn shot(h: &mut Harness<'_, App>, name: &str) {
    if let Ok(dir) = std::env::var("FOREMAN_SHOTS") {
        let img = h.render().expect("rendered");
        img.save(Path::new(&dir).join(format!("{name}.png"))).unwrap();
    }
}

#[test]
fn the_tree_issues_and_a_transcript() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    shot(&mut h, "issues");
    let o = h.state().overview().unwrap();
    assert_eq!(o.issues[0].title, "Write a README");
    assert_eq!(o.worktrees.len(), 3);

    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    shot(&mut h, "session");

    click(&mut h, 1210.0, 23.0);
    h.run_steps(4);
    shot(&mut h, "start");
    click(&mut h, 760.0, 2000.0);
}

#[test]
fn the_status_line_shows_the_server_model_and_context() {
    let rig = rig();
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    rig.store
        .add_usage(
            session.id,
            &foreman_core::Usage {
                input_tokens: 2_000,
                cache_read_tokens: 6_000,
                context: 84_000,
                window: 128_000,
                model: "qwen3-coder".into(),
                host: "gpu.lan:11434".into(),
                turns: 1,
                ..Default::default()
            },
        )
        .unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    shot(&mut h, "status-line");
}

fn click(h: &mut Harness<'_, App>, x: f32, y: f32) {
    let pos = egui::pos2(x, y);
    h.event(egui::Event::PointerMoved(pos));
    h.step();
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
    }
}

#[test]
fn sending_from_the_window_runs_the_agent() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);

    click(&mut h, 800.0, 735.0);
    h.event(egui::Event::Text("ping".into()));
    h.run_steps(2);
    shot(&mut h, "typed");
    click(&mut h, 1220.0, 784.0);
    until(&mut h, "the reply", |h| h.state().transcript_len(session.id) >= 5);
    shot(&mut h, "reply");
    let msgs = rig.store.messages(session.id, 0).unwrap();
    assert_eq!(msgs[3].entry, Entry::user("ping"));
    assert_eq!(msgs[4].entry, Entry::Assistant { text: "echo: ping".into() });
}

#[test]
fn the_client_reaches_the_server() {
    let rig = rig();
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut c = foreman::client::Client::new(
        foreman::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        move || {
            let _ = tx.send(());
        },
    );
    c.send_message(session.id, "ping");
    rx.recv_timeout(Duration::from_secs(5)).expect("a reply");
    let replies = c.poll();
    match &replies[0] {
        foreman::client::Reply::Done(_) => {}
        foreman::client::Reply::Failed(e) | foreman::client::Reply::Offline(e) => panic!("{e}"),
        _ => panic!("odd reply"),
    }
}

#[test]
fn a_server_that_never_answers_is_offline_not_a_hang() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming().flatten() {
            held.push(s);
        }
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let mut c = foreman::client::Client::new(
        foreman::client::Server { url, token: TOKEN.into() },
        move || {
            let _ = tx.send(());
        },
    )
    .with_patience(Duration::from_millis(500));
    let started = Instant::now();
    c.overview();
    rx.recv_timeout(Duration::from_secs(5)).expect("a reply, not a hang");
    assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    match c.poll().pop() {
        Some(foreman::client::Reply::Offline(_)) => {}
        _ => panic!("expected the server to be called offline"),
    }
}

#[test]
fn without_a_local_server_the_window_asks_where_to_connect() {
    let rig = rig();
    let mut h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_eframe(|cc| App::new(&cc.egui_ctx, None, None));
    assert!(!h.state().is_local());
    h.run_steps(2);
    assert_eq!(h.state().settings_loaded(), Some(false), "the settings open by themselves");
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert!(h.state().settings_loaded().is_some(), "and cannot be closed with nowhere to go");
    h.state_mut().settings_set_server(&rig.url, TOKEN);
    h.state_mut().settings_save();
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    assert!(h.state().settings_loaded().is_none());
}

#[test]
fn the_window_starts_its_own_server_by_default() {
    let rig = rig();
    let url = format!("http://{}", rig.url);
    let mut h = Harness::builder().with_size([1280.0, 800.0]).wgpu().build_eframe(move |cc| {
        let url = url.clone();
        let local = foreman::LocalServer {
            start: Box::new(move || {
                Ok(foreman::client::Server { url: url.clone(), token: TOKEN.into() })
            }),
        };
        App::with_local(&cc.egui_ctx, None, None, Some(local))
    });
    assert!(h.state().is_local());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
}

#[test]
fn a_saved_remote_choice_is_kept() {
    let mut storage = Mem::default();
    storage.0.insert("remote".into(), "true".into());
    let h = Harness::builder().with_size([900.0, 600.0]).build_eframe(move |cc| {
        let local = foreman::LocalServer { start: Box::new(|| Err("must not start".into())) };
        App::with_local(&cc.egui_ctx, Some(&storage), None, Some(local))
    });
    assert!(!h.state().is_local());
}

struct Lister;

#[foreman_server::agent::async_trait]
impl foreman_server::agent::Agent for Lister {
    fn name(&self) -> &str {
        "lister"
    }
    fn label(&self) -> &str {
        "Lister"
    }
    fn available(&self) -> bool {
        true
    }
    fn about(&self) -> String {
        "test".into()
    }
    fn resumable(&self) -> bool {
        false
    }
    async fn models(&self) -> Result<Vec<String>, String> {
        Ok(vec!["alpha".into(), "beta".into()])
    }
    async fn run(
        &self,
        _turn: &foreman_server::agent::Turn<'_>,
        _io: &dyn foreman_server::agent::Io,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn a_new_chat_offers_a_dropdown_for_any_agent_that_lists_models() {
    let rig = rig_with(vec![Box::new(Lister)]);
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    click(&mut h, 1220.0, 23.0);
    h.run_steps(3);
    h.state_mut().pick_fresh_agent("lister");
    until(&mut h, "its models", |h| h.state().models_for("lister") == Some(2));
    h.run_steps(3);
    h.get_by_label("choose model").click();
    h.run_steps(4);
    h.get_by_label("SELECT MODEL");
    h.get_by_label("2 MODELS");
    shot(&mut h, "agent-model-picker");
}

struct Waiter;

#[foreman_server::agent::async_trait]
impl foreman_server::agent::Agent for Waiter {
    fn name(&self) -> &str {
        "waiter"
    }
    fn label(&self) -> &str {
        "Waiter"
    }
    fn available(&self) -> bool {
        true
    }
    fn about(&self) -> String {
        "test".into()
    }
    fn resumable(&self) -> bool {
        false
    }
    async fn run(
        &self,
        _turn: &foreman_server::agent::Turn<'_>,
        io: &dyn foreman_server::agent::Io,
    ) -> Result<(), String> {
        let q = foreman_core::Question {
            text: "which licence?".into(),
            options: vec!["MIT".into(), "Apache-2.0".into()],
            ..Default::default()
        };
        let a = io.ask(q).await?;
        io.emit(foreman_server::agent::Event::Text { delta: format!("going with {a}") });
        Ok(())
    }
}

struct Planner;

#[foreman_server::agent::async_trait]
impl foreman_server::agent::Agent for Planner {
    fn name(&self) -> &str {
        "planner"
    }
    fn label(&self) -> &str {
        "Planner"
    }
    fn available(&self) -> bool {
        true
    }
    fn about(&self) -> String {
        "test".into()
    }
    fn resumable(&self) -> bool {
        false
    }
    async fn run(
        &self,
        _turn: &foreman_server::agent::Turn<'_>,
        io: &dyn foreman_server::agent::Io,
    ) -> Result<(), String> {
        let q = |text: &str, options: &[&str], multiple| foreman_core::Question {
            text: text.into(),
            options: options.iter().map(|o| o.to_string()).collect(),
            multiple,
        };
        let a = io
            .ask_many(vec![
                q("which licence?", &["MIT", "Apache-2.0"], false),
                q("which targets?", &["linux", "mac", "windows"], true),
                q("anything else?", &[], false),
            ])
            .await?;
        io.emit(foreman_server::agent::Event::Text { delta: format!("plan: {}", a.join(" | ")) });
        Ok(())
    }
}

#[test]
fn several_questions_are_answered_together_with_multiple_choice() {
    let rig = rig_with(vec![Box::new(Planner)]);
    let project = rig.store.projects().unwrap()[0].id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let mut c = foreman::client::Client::new(
        foreman::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    c.add_session(&foreman_core::NewSession {
        project_id: project,
        agent: "planner".into(),
        prompt: Some("plan".into()),
        ..Default::default()
    });
    until(&mut h, "the waiting session", |h| {
        h.state().overview().is_some_and(|o| {
            o.sessions
                .iter()
                .any(|s| s.agent == "planner" && s.status == foreman_core::SessionStatus::Waiting)
        })
    });
    let s = h
        .state()
        .overview()
        .unwrap()
        .sessions
        .iter()
        .find(|s| s.agent == "planner")
        .unwrap()
        .clone();
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the questions", |h| h.state().transcript_len(s.id) == 2);
    shot(&mut h, "questions");
    // Nothing is sent until every question has an answer.
    h.get_by_label("Apache-2.0").click();
    h.get_by_label("linux").click();
    h.get_by_label("windows").click();
    h.run_steps(3);
    shot(&mut h, "questions-picked");
    assert!(
        !rig.store
            .messages(s.id, 0)
            .unwrap()
            .iter()
            .any(|m| matches!(&m.entry, Entry::Questions { answers: Some(_), .. }))
    );
    let fields = h.query_all_by_role(egui::accesskit::Role::TextInput).count();
    assert!(fields >= 3, "{fields} text fields");
    h.query_all_by_role(egui::accesskit::Role::TextInput).nth(2).unwrap().click();
    h.run_steps(2);
    h.event(egui::Event::Text("no".into()));
    h.run_steps(2);
    h.get_by_label("send answers").click();
    h.run_steps(5);
    until(&mut h, "the reply", |h| h.state().transcript_len(s.id) >= 3);
    let msgs = rig.store.messages(s.id, 0).unwrap();
    assert!(
        msgs.iter().any(|m| matches!(
            &m.entry,
            Entry::Questions { answers: Some(a), .. }
                if a == &["Apache-2.0", "linux, windows", "no"]
        )),
        "{msgs:?}"
    );
    assert_eq!(
        msgs.last().unwrap().entry,
        Entry::Assistant { text: "plan: Apache-2.0 | linux, windows | no".into() }
    );
    shot(&mut h, "questions-answered");
}

#[test]
fn a_question_is_answered_by_clicking_an_option() {
    let rig = rig_with(vec![Box::new(Waiter)]);
    let project = rig.store.projects().unwrap()[0].id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let mut c = foreman::client::Client::new(
        foreman::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    c.add_session(&foreman_core::NewSession {
        project_id: project,
        agent: "waiter".into(),
        prompt: Some("pick".into()),
        ..Default::default()
    });
    let mut sid = None;
    until(&mut h, "the waiting session", |h| {
        h.state().overview().is_some_and(|o| {
            o.sessions
                .iter()
                .any(|s| s.agent == "waiter" && s.status == foreman_core::SessionStatus::Waiting)
        })
    });
    let s = h
        .state()
        .overview()
        .unwrap()
        .sessions
        .iter()
        .find(|s| s.agent == "waiter")
        .unwrap()
        .clone();
    sid.get_or_insert(s.id);
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the question", |h| h.state().transcript_len(s.id) == 2);
    shot(&mut h, "question");
    h.get_by_label("Apache-2.0").click();
    until(&mut h, "the reply", |h| h.state().transcript_len(s.id) >= 3);
    let msgs = rig.store.messages(s.id, 0).unwrap();
    assert!(
        msgs.iter().any(
            |m| matches!(&m.entry, Entry::Question { answer: Some(a), .. } if a == "Apache-2.0")
        ),
        "{msgs:?}"
    );
    assert_eq!(
        msgs.last().unwrap().entry,
        Entry::Assistant { text: "going with Apache-2.0".into() }
    );
    shot(&mut h, "answered");
}

#[test]
fn the_header_counts_agents_by_status() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    shot(&mut h, "badge");
    let counts = foreman::status_counts(&h.state().overview().unwrap().sessions);
    assert_eq!(
        counts,
        vec![(foreman_core::SessionStatus::Idle, 2), (foreman_core::SessionStatus::Stopped, 1)]
    );
}

#[test]
fn clicking_a_session_in_the_tree_opens_it() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    click(&mut h, 100.0, 142.0);
    h.run_steps(3);
    let hello = rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    assert_eq!(h.state().open(), Some(hello.id), "the first session opens");
    click(&mut h, 120.0, 208.0);
    h.run_steps(3);
    let fix = rig
        .store
        .sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.title == "#3 Fix the login loop")
        .unwrap();
    assert_eq!(h.state().open(), Some(fix.id), "switching to another session");
}

#[test]
fn the_buttons_on_an_issue_card_work() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    click(&mut h, 236.0, 20.0);
    h.run_steps(3);
    shot(&mut h, "issues-tab");
    click(&mut h, 1107.0, 170.0);
    h.run_steps(3);
    shot(&mut h, "more");
    assert!(h.state().issue_open().is_some(), "more opens the issue");
}

#[test]
fn a_busy_transcript_reads_cleanly() {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "foreman", "The current credit system isnt very good, there isnt a way for users to see what they paid", None, None).unwrap();
    let add = |e: Entry| {
        rig.store.add_message(s.id, &e).unwrap();
    };
    add(Entry::user(
        "The current credit system isnt very good. Look at how credits are applied and tell me what to change.",
    ));
    add(Entry::Thinking {
        text: "I should find where credits are deducted first. Probably in the billing worker."
            .into(),
    });
    add(Entry::Tool {
        call_id: "1".into(),
        name: "search_files".into(),
        input: serde_json::json!({"pattern": "apply_credit"}),
        output: Some("src/billing.rs:88: fn apply_credit(\nsrc/api.rs:210: apply_credit(".into()),
        error: false,
    });
    add(Entry::Tool {
        call_id: "2".into(),
        name: "read_file".into(),
        input: serde_json::json!({"path": "src/billing.rs", "offset": 80, "limit": 40}),
        output: Some("fn apply_credit(user: &User, amount: u64) {\n    ...\n}".into()),
        error: false,
    });
    add(Entry::Thinking {
        text: "Credits are applied without a ledger row, so the user can never see them.".into(),
    });
    add(Entry::Tool {
        call_id: "3".into(),
        name: "edit_file".into(),
        input: serde_json::json!({"path": "src/billing.rs", "old": "    user.credit += amount;", "new": "    user.credit += amount;\n    ledger.record(user, amount, Reason::Credit);"}),
        output: Some("edited src/billing.rs".into()),
        error: false,
    });
    add(Entry::Tool {
        call_id: "4".into(),
        name: "bash".into(),
        input: serde_json::json!({"command": "cargo test -p billing"}),
        output: Some(
            "error[E0425]: cannot find value `ledger`\n --> src/billing.rs:91\n[exit 101 in 4.2s]"
                .into(),
        ),
        error: true,
    });
    add(Entry::Assistant { text: "Credits are added to the balance with no **ledger row**, which is why users cannot see them.\n\n1. Record every credit in `ledger`.\n2. Show the ledger on the billing page.\n\n```rust\nledger.record(user, amount, Reason::Credit);\n```".into() });
    add(Entry::Tool {
        call_id: "5".into(),
        name: "list_files".into(),
        input: serde_json::json!({}),
        output: None,
        error: false,
    });
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 9);
    h.run_steps(3);
    shot(&mut h, "transcript");
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, 2000.0),
        modifiers: egui::Modifiers::NONE,
        phase: egui::TouchPhase::Move,
    });
    h.event(egui::Event::PointerMoved(egui::pos2(800.0, 300.0)));
    h.run_steps(4);
    shot(&mut h, "transcript-top");
}

#[test]
fn on_a_phone_the_project_list_is_a_drawer_behind_a_hamburger() {
    let rig = rig();
    let mut h = harness_at(rig.url.clone(), [450.0, 1000.0]);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    assert!(h.query_by_label("add project").is_none(), "no side panel on a phone");
    shot(&mut h, "phone");
    h.get_by_label("projects").click();
    h.run_steps(3);
    h.get_by_label("add project");
    shot(&mut h, "phone-drawer");
    assert!(h.state().drawer_open());
    // A tap on the backdrop, right of the drawer, puts it away.
    click(&mut h, 440.0, 600.0);
    h.run_steps(3);
    assert!(!h.state().drawer_open(), "tapping outside closes it");
    assert!(h.query_by_label("add project").is_none());
    // Picking a chat closes it too, and opens the chat.
    h.get_by_label("projects").click();
    h.run_steps(3);
    click(&mut h, 100.0, 129.0);
    h.run_steps(3);
    assert!(!h.state().drawer_open(), "picking a chat closes the drawer");
    assert!(h.state().open().is_some(), "and opens it");
}

#[test]
fn a_bash_command_is_always_shown_whole_and_wrapped() {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "foreman", "risky", None, None).unwrap();
    let long = "cd /srv/app && rm -rf build/ dist/ node_modules/ && git clean -fdx -e keep.txt && \\\n  curl -fsSL https://example.com/install.sh | sudo bash -s -- --yes --prefix /usr/local --with-everything-enabled-and-then-some";
    rig.store.add_message(s.id, &Entry::user("clean it")).unwrap();
    rig.store
        .add_message(
            s.id,
            &Entry::Tool {
                call_id: "b1".into(),
                name: "bash".into(),
                input: serde_json::json!({ "command": long }),
                output: Some("done".into()),
                error: false,
            },
        )
        .unwrap();
    let mut h = harness_at(rig.url.clone(), [700.0, 800.0]);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 2);
    h.run_steps(4);
    shot(&mut h, "bash-command");
    // The end of the command is on screen without opening the row.
    h.get_by_label_contains("with-everything-enabled-and-then-some");
}

#[test]
fn on_a_phone_an_open_chat_fits_the_screen() {
    let rig = rig();
    let mut h = harness_at(rig.url.clone(), [450.0, 1000.0]);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(3);
    shot(&mut h, "phone-chat");
    h.get_by_label("send");
}

#[test]
fn settings_pair_a_phone_with_a_code_for_the_external_address() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(3);
    // This window reaches the server on loopback, which a phone cannot, so there is no code yet.
    assert!(h.state().settings_pairing().is_none());
    h.state_mut().settings_set_external("https://foreman.harkin.network");
    h.run_steps(3);
    let p = h.state().settings_pairing().expect("a code once the address is given");
    assert_eq!(p.url, "https://foreman.harkin.network");
    assert_eq!(p.token, TOKEN);
    shot(&mut h, "settings-pair");
}

#[test]
fn the_settings_key_opens_one_dialog_for_server_look_and_agent() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(2);
    until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
    h.run_steps(3);
    shot(&mut h, "settings");
    until(&mut h, "the installed fonts", |h| h.query_all_by_label("choose font").count() == 2);
    // The tab labels are painted, not widgets, so click the speech one by position.
    click(&mut h, 480.0, 118.0);
    h.run_steps(3);
    assert!(h.query_all_by_label("choose font").count() == 0, "speech is its own page");
    shot(&mut h, "settings-speech");
    h.state_mut().settings_set_speech_model("whisper-large-v3");
    h.state_mut().settings_set_text_size(18.0);
    h.run_steps(4);
    shot(&mut h, "settings-speech-long");
    h.state_mut().settings_set_text_size(13.0);
    h.run_steps(3);
    h.state_mut().settings_page("general");
    h.run_steps(3);
    assert!(h.query_by_label("name chats with a model").is_none(), "titles are on their own page");
    h.state_mut().settings_page("titles");
    h.run_steps(3);
    shot(&mut h, "settings-titles");
    h.get_by_label("name chats with a model");
    assert!(h.query_by_label("agent").is_none(), "one settings key, not three");
}

#[test]
fn a_new_text_size_applies_on_save_and_is_kept() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(2);
    until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
    until(&mut h, "the installed fonts", |h| h.query_all_by_label("choose font").count() == 2);
    let line = |h: &Harness<'_, App>| {
        h.ctx.fonts_mut(|f| f.row_height(&egui::FontId::proportional(foreman::fonts::BASE)))
    };
    let before = line(&h);
    h.state_mut().settings_set_text_size(16.0);
    h.state_mut().settings_save();
    until(&mut h, "the save", |h| h.state().settings_loaded().is_none());
    assert_eq!(h.state().text_size(), 16.0);
    until(&mut h, "the bigger fonts", |h| line(h) > before * 1.15);
    let mut kept = Mem::default();
    h.state().store(&mut kept);
    assert_eq!(kept.0.get("font_size").map(String::as_str), Some("16"));
}

#[test]
fn the_title_model_is_set_in_the_settings_and_saved_on_the_server() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(2);
    until(&mut h, "the title settings", |h| h.state().settings_title_loaded());
    h.run_steps(3);
    h.state_mut().settings_page("titles");
    h.run_steps(3);
    shot(&mut h, "title-settings");
    h.get_by_label("name chats with a model");
    h.state_mut().settings_set_title("http://127.0.0.1:1/v1", "tiny");
    h.run_steps(2);
    h.state_mut().settings_save();
    until(&mut h, "the save", |h| h.state().settings_loaded().is_none());
    let saved: serde_json::Value =
        serde_json::from_str(&rig.store.setting("title_model").unwrap().unwrap()).unwrap();
    assert_eq!(saved["model"], "tiny");
    assert_eq!(saved["url"], "http://127.0.0.1:1/v1");
    assert_eq!(saved["enabled"], true);
}

#[test]
fn the_model_levels_are_set_in_the_settings_and_saved_on_the_server() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(2);
    until(&mut h, "the title settings", |h| h.state().settings_title_loaded());
    h.run_steps(3);
    h.state_mut().settings_page("agent");
    h.run_steps(3);
    shot(&mut h, "model-levels");
    h.state_mut().settings_set_level("low", "", "tiny");
    h.state_mut().settings_set_level("high", "http://127.0.0.1:1/v1", "huge");
    h.run_steps(2);
    shot(&mut h, "model-levels-set");
    h.state_mut().settings_save();
    until(&mut h, "the save", |h| h.state().settings_loaded().is_none());
    let saved: serde_json::Value =
        serde_json::from_str(&rig.store.setting("model").unwrap().unwrap()).unwrap();
    assert_eq!(saved["low"]["model"], "tiny");
    assert_eq!(saved["high"]["model"], "huge");
    assert_eq!(saved["high"]["url"], "http://127.0.0.1:1/v1");
    assert_eq!(saved["medium"]["model"], "");
}

#[test]
fn a_long_model_list_is_a_filterable_dropdown() {
    let rig = rig_on(&model_server(5000), Vec::new());
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(2);
    until(&mut h, "the model list", |h| h.state().settings_models() == Some(5000));
    // The dialog grows as the title settings arrive and re-centres, so let it settle first.
    until(&mut h, "the title settings", |h| h.state().settings_title_loaded());
    h.state_mut().settings_page("agent");
    h.run_steps(8);
    shot(&mut h, "model-settings");
    h.get_by_label("choose model").click();
    h.run_steps(4);
    shot(&mut h, "model-picker");
    h.get_by_label("SELECT MODEL");
    h.get_by_label("5000 MODELS");
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(h.query_by_label("SELECT MODEL").is_none());

    h.get_by_label("choose model").click();
    h.run_steps(3);
    h.event(egui::Event::Text("model-04321".into()));
    h.run_steps(3);
    h.get_by_label("1 OF 5000 MODELS");
    shot(&mut h, "model-filtered");
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert!(h.query_by_label("SELECT MODEL").is_none());
    assert_eq!(h.state().settings_model().as_deref(), Some("vendor-1/model-04321-instruct"));
}

#[test]
fn close_stops_and_deletes_the_chat() {
    let rig = rig();
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(2);
    shot(&mut h, "close-header");
    click(&mut h, 1236.0, 69.0);
    h.run_steps(3);
    shot(&mut h, "close-confirm");
    click(&mut h, 760.0, 437.0);
    until(&mut h, "the chat to go", |h| h.state().open().is_none());
    let start = Instant::now();
    while rig.store.session(session.id).is_ok() {
        assert!(start.elapsed() < Duration::from_secs(5), "the session was not deleted");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(rig.store.messages(session.id, 0).unwrap().is_empty(), "its messages went with it");
}

#[test]
fn slash_shows_the_commands_and_new_opens_a_fresh_chat() {
    let rig = rig();
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    click(&mut h, 800.0, 730.0);
    h.event(egui::Event::Text("/".into()));
    h.run_steps(3);
    shot(&mut h, "slash");
    h.event(egui::Event::Text("new".into()));
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    h.run_steps(3);
    assert_eq!(h.state().open(), None, "/new leaves the chat for a fresh one");
    assert!(rig.store.session(session.id).is_ok(), "the old chat is kept");
}

#[test]
fn new_chat_stays_blank_while_other_agents_work() {
    let rig = rig();
    let project = rig.store.projects().unwrap()[0].id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    click(&mut h, 1220.0, 23.0);
    h.run_steps(3);
    assert_eq!(h.state().open(), None, "new chat starts blank");
    let mut other = foreman::client::Client::new(
        foreman::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    other.add_session(&foreman_core::NewSession {
        project_id: project,
        agent: "echo".into(),
        prompt: Some("someone else".into()),
        ..Default::default()
    });
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(h.state().open(), None, "another chat starting does not take over the blank one");

    click(&mut h, 800.0, 330.0);
    h.event(egui::Event::Text("mine".into()));
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "my chat", |h| h.state().open().is_some());
    let mine = rig.store.session(h.state().open().unwrap()).unwrap();
    let first = rig.store.messages(mine.id, 0).unwrap();
    assert!(matches!(&first[0].entry, Entry::User { text, .. } if text == "mine"), "{first:?}");
}

#[test]
fn each_new_chat_from_the_tree_is_a_new_session() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    let mut seen = Vec::new();
    for word in ["first", "second"] {
        // The main worktree row in the tree starts a blank chat.
        click(&mut h, 80.0, 110.0);
        h.run_steps(3);
        assert_eq!(h.state().open(), None, "{word}: the row opens a blank chat");
        // Let the overview refresh; a blank chat must not attach itself to an old session.
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(800) {
            h.step();
            std::thread::sleep(Duration::from_millis(30));
        }
        assert_eq!(h.state().open(), None, "{word}: still blank");
        click(&mut h, 800.0, 330.0);
        h.event(egui::Event::Text(word.into()));
        h.run_steps(2);
        h.key_press(egui::Key::Enter);
        until(&mut h, "a chat", |h| h.state().open().is_some());
        let id = h.state().open().unwrap();
        assert!(!seen.contains(&id), "{word}: reopened chat {id}");
        let first = rig.store.messages(id, 0).unwrap();
        assert!(
            matches!(&first[0].entry, Entry::User { text, .. } if text == word),
            "{word}: {first:?}"
        );
        seen.push(id);
    }
}

#[test]
fn a_new_chat_after_a_deleted_one_shows_none_of_its_messages() {
    let rig = rig();
    let main = rig.store.projects().unwrap()[0].id;
    let tree = rig.store.main_worktree(main).unwrap();
    let old = rig.store.add_session(tree.id, "echo", "old talk", None, None).unwrap();
    for text in ["one", "two", "three"] {
        rig.store.add_message(old.id, &Entry::user(text)).unwrap();
    }
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(old.id, old.project_id);
    until(&mut h, "its transcript", |h| h.state().transcript_len(old.id) == 3);

    let mut other = foreman::client::Client::new(
        foreman::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    other.delete_session(old.id);
    until(&mut h, "the chat to go", |h| h.state().open().is_none());

    click(&mut h, 80.0, 110.0);
    h.run_steps(3);
    click(&mut h, 800.0, 330.0);
    h.event(egui::Event::Text("fresh start".into()));
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "a chat", |h| h.state().open().is_some());
    let id = h.state().open().unwrap();
    assert_ne!(id, old.id, "a new chat never takes a deleted chat's id");
    until(&mut h, "its reply", |h| h.state().transcript_len(id) >= 2);
    h.run_steps(5);
    assert_eq!(h.state().transcript_len(id), 2, "only this chat's own messages");
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([40, 160, 220, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn drop_file(h: &mut Harness<'_, App>, name: &str, bytes: Vec<u8>) {
    h.input_mut().dropped_files.push(egui::DroppedFile {
        name: name.into(),
        bytes: Some(bytes.into()),
        ..Default::default()
    });
    h.step();
}

#[test]
fn a_dropped_picture_is_sent_with_the_message_and_shown_in_the_chat() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    click(&mut h, 80.0, 110.0);
    h.run_steps(3);
    drop_file(&mut h, "shot.png", png(64, 40));
    drop_file(&mut h, "notes.bin", vec![1, 2, 3]);
    until(&mut h, "both files in the composer", |h| h.state().pending_files() == 2);
    h.get_by_label("remove notes.bin").click();
    h.run_steps(2);
    assert_eq!(h.state().pending_files(), 1);
    shot(&mut h, "composer-attachment");
    click(&mut h, 800.0, 330.0);
    h.event(egui::Event::Text("what is this".into()));
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "a chat", |h| h.state().open().is_some());
    assert_eq!(h.state().pending_files(), 0, "sent files leave the composer");
    let id = h.state().open().unwrap();
    let first = rig.store.messages(id, 0).unwrap();
    let Entry::User { text, attachments, .. } = &first[0].entry else { panic!("{first:?}") };
    assert_eq!(text, "what is this");
    assert_eq!(attachments.len(), 1);
    assert_eq!(
        (attachments[0].name.as_str(), attachments[0].mime.as_str()),
        ("shot.png", "image/png")
    );
    until(&mut h, "the thumbnail", |h| h.state().thumbnails_ready() == 1);
    shot(&mut h, "chat-attachment");

    // A picture attached to an open chat must not push the composer off the window.
    drop_file(&mut h, "again.png", png(64, 40));
    until(&mut h, "the picture in the composer", |h| h.state().pending_files() == 1);
    h.run_steps(3);
    shot(&mut h, "composer-attachment-chat");
    let bottom = h.get_by_label("attach").rect().bottom();
    assert!(bottom <= 800.0, "the composer buttons end at {bottom}, below the 800 point window");
}

#[test]
fn the_composer_keeps_focus_when_a_file_is_attached() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(2);
    click(&mut h, 800.0, 735.0);
    h.event(egui::Event::Text("one".into()));
    h.run_steps(2);
    drop_file(&mut h, "pasted.png", png(64, 40));
    until(&mut h, "the picture in the composer", |h| h.state().pending_files() == 1);
    h.event(egui::Event::Text(" two".into()));
    h.run_steps(2);
    assert_eq!(h.state().draft_of(session.id), "one two", "typing carries on after the paste");
}

#[test]
fn a_long_draft_scrolls_in_the_composer_and_leaves_the_keys_on_screen() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(2);
    click(&mut h, 800.0, 735.0);
    let long = (0..60).map(|i| format!("line {i} of a very long dictation")).collect::<Vec<_>>();
    h.event(egui::Event::Text(long.join("\n")));
    h.run_steps(14);
    shot(&mut h, "composer-long");
    let send = h.get_by_label("send").rect();
    assert!(send.bottom() <= 800.0, "the send key ends at {}, below the window", send.bottom());
}

#[test]
fn escape_stops_the_agent_working_in_the_open_chat() {
    let slow = JsonCommand {
        name: "slow".into(),
        label: None,
        program: PathBuf::from("sh"),
        args: vec!["-c".into(), "read p; sleep 60".into()],
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
    let rig = rig_with(vec![Box::new(slow)]);
    let project = rig.store.projects().unwrap()[0].id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let mut other = foreman::client::Client::new(
        foreman::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    other.add_session(&foreman_core::NewSession {
        project_id: project,
        agent: "slow".into(),
        prompt: Some("take your time".into()),
        ..Default::default()
    });
    let running = |rig: &Rig| {
        rig.store
            .sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.agent == "slow")
            .filter(|s| s.status == foreman_core::SessionStatus::Running)
    };
    let start = Instant::now();
    let session = loop {
        if let Some(s) = running(&rig) {
            break s;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "the agent never started");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    };
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the chat to show it working", |h| {
        // Escape only stops a chat the window knows is busy, so wait for the window to know.
        h.state().open() == Some(session.id)
            && h.state()
                .overview()
                .is_some_and(|o| o.sessions.iter().any(|x| x.id == session.id && x.status.busy()))
    });
    h.key_press(egui::Key::Escape);
    let start = Instant::now();
    while running(&rig).is_some() {
        assert!(start.elapsed() < Duration::from_secs(10), "escape did not stop the agent");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn the_speak_key_sits_in_the_composer_and_says_what_is_missing() {
    let rig = rig();
    // A speech server with no address, so pressing the key never opens a real microphone.
    let url = rig.url.clone();
    let mut h = Harness::builder().with_size([1280.0, 800.0]).wgpu().build_eframe(move |cc| {
        let mut storage = Mem::default();
        storage.0.insert("url".into(), url);
        storage.0.insert("token".into(), TOKEN.into());
        storage.0.insert("voice_mode".into(), "server".into());
        App::new(&cc.egui_ctx, Some(&storage), None)
    });
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    click(&mut h, 80.0, 110.0);
    h.run_steps(3);
    shot(&mut h, "composer-speak");
    let speak = h.get_by_label("speak");
    let attach = h.get_by_label("attach");
    assert!(attach.rect().right() <= speak.rect().left(), "speak sits beside attach");
    assert!(speak.rect().bottom() <= 800.0);
    speak.click();
    h.run_steps(3);
    let e = h.state().last_error().expect("a notice");
    assert!(e.contains("speech server"), "{e}");
    assert!(!h.state().listening(), "nothing was opened");
}

fn extra_chats(rig: &Rig, n: usize) -> Vec<foreman_core::Session> {
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    (0..n)
        .map(|i| rig.store.add_session(main.id, "echo", &format!("extra {i}"), None, None).unwrap())
        .collect()
}

#[test]
fn opening_a_chat_replaces_the_focused_tile() {
    use foreman::app::Grid;
    let rig = rig();
    extra_chats(&rig, 3);
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::Quad);
    h.run_steps(2);
    let all = rig.store.sessions().unwrap();
    assert_eq!(all.len(), 6);
    let id: Vec<_> = all.iter().map(|s| s.id).collect();
    let project = all[0].project_id;
    for (at, sid) in id.iter().take(4).enumerate() {
        h.state_mut().focus_tile(at);
        h.state_mut().open_session(*sid, project);
    }
    h.run_steps(3);
    assert_eq!(h.state().shown(), id[..4].to_vec(), "one chat per tile");
    shot(&mut h, "grid");

    // A click on another chat replaces the chat in the focused tile and leaves the rest.
    h.state_mut().focus_tile(1);
    h.state_mut().open_session(id[4], project);
    h.run_steps(2);
    assert_eq!(h.state().shown(), vec![id[0], id[4], id[2], id[3]]);
    assert_eq!(h.state().open(), Some(id[4]));
    assert_eq!(h.state().focused_tile(), 1);

    // A chat already on screen is only focused; nothing is replaced.
    h.state_mut().open_session(id[3], project);
    h.run_steps(2);
    assert_eq!(h.state().shown(), vec![id[0], id[4], id[2], id[3]]);
    assert_eq!(h.state().open(), Some(id[3]));
    assert_eq!(h.state().focused_tile(), 3);
}

#[test]
fn the_six_tile_grid_shows_six_chats_and_is_remembered() {
    use foreman::app::Grid;
    let rig = rig();
    extra_chats(&rig, 4);
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::Six);
    h.run_steps(2);
    let all = rig.store.sessions().unwrap();
    assert_eq!(all.len(), 7);
    let pid = all[0].project_id;
    for (i, s) in all.iter().take(6).enumerate() {
        h.state_mut().focus_tile(i);
        h.state_mut().open_session(s.id, pid);
        h.run_steps(2);
    }
    for (i, s) in all.iter().take(6).enumerate() {
        assert_eq!(h.state().tile_chat(i), Some(s.id), "tile {i}");
    }
    h.run_steps(3);
    shot(&mut h, "six");
}

#[test]
fn clicking_a_chat_in_the_list_replaces_the_focused_tile() {
    use foreman::app::Grid;
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::Columns);
    h.run_steps(3);
    let all = rig.store.sessions().unwrap();
    let hello = all.iter().find(|s| s.title == "say hello").unwrap().id;
    let fix = all.iter().find(|s| s.title == "#3 Fix the login loop").unwrap().id;
    click(&mut h, 100.0, 142.0);
    h.run_steps(3);
    assert_eq!(h.state().tile_chat(0), Some(hello));
    // Focus the empty second tile, then click another chat: it fills that tile.
    h.state_mut().focus_tile(1);
    h.run_steps(2);
    click(&mut h, 120.0, 208.0);
    h.run_steps(3);
    assert_eq!(h.state().tile_chat(0), Some(hello));
    assert_eq!(h.state().tile_chat(1), Some(fix));
    // With the first tile focused, the same click replaces it.
    h.state_mut().focus_tile(0);
    click(&mut h, 120.0, 208.0);
    h.run_steps(3);
    assert_eq!(h.state().focused_tile(), 1, "it was already on screen, so it only took focus");
    shot(&mut h, "columns");
}

#[test]
fn closing_a_tiled_chat_leaves_its_tile_empty() {
    use foreman::app::Grid;
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::Rows);
    let all = rig.store.sessions().unwrap();
    h.state_mut().open_session(all[0].id, all[0].project_id);
    h.state_mut().focus_tile(1);
    h.state_mut().open_session(all[1].id, all[1].project_id);
    h.run_steps(3);
    assert_eq!(h.state().shown(), vec![all[0].id, all[1].id]);
    h.state_mut().forget_chat(all[1].id);
    h.run_steps(2);
    assert_eq!(h.state().shown(), vec![all[0].id]);
    assert_eq!(h.state().tile_chat(1), None);
    assert_eq!(h.state().open(), None, "the focused tile is now the empty one");
}

#[test]
fn the_grid_and_its_chats_are_remembered() {
    use foreman::app::Grid;
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::Quad);
    let all = rig.store.sessions().unwrap();
    h.state_mut().open_session(all[0].id, all[0].project_id);
    h.state_mut().focus_tile(1);
    h.state_mut().open_session(all[1].id, all[1].project_id);
    let mut saved = Mem::default();
    h.state().store(&mut saved);
    assert_eq!(saved.get_string("grid").as_deref(), Some("2x2"));
    let again = Harness::builder().with_size([1280.0, 800.0]).wgpu().build_eframe(|cc| {
        saved.0.insert("url".into(), rig.url.clone());
        saved.0.insert("token".into(), TOKEN.into());
        App::new(&cc.egui_ctx, Some(&saved), None)
    });
    assert_eq!(again.state().grid(), Grid::Quad);
    assert_eq!(again.state().tile_chat(0), Some(all[0].id));
    assert_eq!(again.state().tile_chat(1), Some(all[1].id));
    assert_eq!(again.state().focused_tile(), 1);
}

#[test]
fn long_markdown_wraps_inside_a_tile() {
    use foreman::app::Grid;
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "foreman", "a long answer", None, None).unwrap();
    let long = "On the 4mm gap: if 4mm is fine for your saw, I can switch the plan back. At 380 deep it doesn't change the number of boards either way. Do you want the same cut list with the other gap?";
    let add = |e: Entry| {
        rig.store.add_message(s.id, &e).unwrap();
    };
    add(Entry::user(format!("ok its fine, as is {long}")));
    add(Entry::Assistant {
        text: format!(
            "{long}\n\n- **Instrument shelf:** {long}\n- `cutplan.txt`: {long}\n\n{long} with `inline code` in it"
        ),
    });
    // Things that cannot wrap, in their own entries: they must not widen the rest.
    add(Entry::Assistant {
        text: format!(
            "| part | size | note |\n|---|---|---|\n| side | 2440x600 | {long} |\n\nhttps://example.com/{}",
            "a".repeat(160)
        ),
    });
    add(Entry::Assistant { text: format!("{long}\n\n- {long}\n- {long}") });
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::Quad);
    h.state_mut().focus_tile(1);
    h.state_mut().open_session(s.id, main.project_id);
    h.run_steps(6);
    shot(&mut h, "tile-wrap");
}

#[test]
fn markdown_sits_on_one_baseline_and_links_open() {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "foreman", "markdown", None, None).unwrap();
    rig.store
        .add_message(
            s.id,
            &Entry::Assistant {
                text: "Plain text with `inline code` in the middle, and a **bold** word.\n\nSee https://example.com/docs and [the guide](https://example.com/guide).\n\n- first `item` here\n- second item\n\n1. numbered `one`\n2. numbered two\n\n```rust\nfn main() {}\n```\n\n> quoted `code` text\n"
                    .into(),
            },
        )
        .unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, main.project_id);
    h.run_steps(6);
    shot(&mut h, "markdown-baseline");
}

/// A long chat: markdown, code blocks, tables, tool calls with long output and thinking.
fn long_chat(rig: &Rig, turns: usize) -> foreman_core::Session {
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "echo", "a very long chat", None, None).unwrap();
    let add = |e: Entry| {
        rig.store.add_message(s.id, &e).unwrap();
    };
    let long_out: String =
        (0..400).map(|i| format!("src/module_{i}/file.rs:{i}: fn handler_{i}(request: &Request) -> Response {{")).collect::<Vec<_>>().join("\n");
    for n in 0..turns {
        add(Entry::user(&format!(
            "Turn {n}: look at **how credits are applied** and `tell me` what to change, see https://example.com/{n}.\n\n- first\n- second"
        )));
        add(Entry::Thinking { text: format!("Thinking about turn {n}. Credits are applied without a ledger row, so the user can never see them. I should look in the billing worker.") });
        add(Entry::Tool {
            call_id: format!("a{n}"),
            name: "search_files".into(),
            input: serde_json::json!({"pattern": "apply_credit"}),
            output: Some("src/billing.rs:88: fn apply_credit(\nsrc/api.rs:210: apply_credit(".into()),
            error: false,
        });
        add(Entry::Tool {
            call_id: format!("b{n}"),
            name: "bash".into(),
            input: serde_json::json!({"command": "cargo test -p billing"}),
            output: Some(long_out.clone()),
            error: n % 7 == 3,
        });
        add(Entry::Assistant { text: format!("## Turn {n}\n\nCredits are added to the balance with no **ledger row**, which is why users cannot see them. This paragraph is long enough to wrap over a couple of lines on a phone so that the layout has some work to do, with a [link](https://example.com) in it.\n\n1. Record every credit in `ledger`.\n2. Show the ledger on the billing page.\n3. Add a test.\n\n```rust\nfn apply_credit(user: &mut User, amount: u64) {{\n    user.credit += amount;\n    ledger.record(user, amount, Reason::Credit);\n}}\n```\n\n| column | meaning |\n|---|---|\n| a | first |\n| b | second |\n\n> a quoted remark about turn {n}") });
    }
    s
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Times frames on a long transcript. Run with `--nocapture` to see the numbers.
fn time_frames(size: [f32; 2], turns: usize) {
    let rig = rig();
    let s = long_chat(&rig, turns);
    let total = turns * 5;
    let mut h = harness_at(rig.url.clone(), size);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    // The frame that first has the whole transcript lays every entry out once.
    let mut slowest = 0.0f64;
    let start = Instant::now();
    while h.state().transcript_len(s.id) < total {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out loading");
        let t = Instant::now();
        h.step();
        slowest = slowest.max(ms(t.elapsed()));
        std::thread::sleep(Duration::from_millis(30));
    }
    let t = Instant::now();
    h.step();
    slowest = slowest.max(ms(t.elapsed()));
    println!("PERF {}x{} {total} msgs opening the chat, slowest frame: {slowest:.1} ms", size[0], size[1]);
    h.run_steps(3);
    let time = |h: &mut Harness<'_, App>, what: &str, n: usize, each: &mut dyn FnMut(&mut Harness<'_, App>)| {
        let mut v = Vec::new();
        for _ in 0..n {
            each(h);
            let t = Instant::now();
            h.step();
            v.push(ms(t.elapsed()));
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        println!(
            "PERF {}x{} {total} msgs {what}: mean {mean:.1} ms, median {:.1} ms, max {:.1} ms",
            size[0], size[1], v[v.len() / 2], v[v.len() - 1]
        );
    };
    // The first frames lay everything out and fill the markdown cache.
    time(&mut h, "first frames", 3, &mut |_| {});
    time(&mut h, "idle at the bottom", 20, &mut |_| {});
    let mut y = 0.0;
    time(&mut h, "scrolling", 20, &mut |h| {
        y += 90.0;
        h.event(egui::Event::PointerMoved(egui::pos2(size[0] * 0.6, size[1] * 0.4)));
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, if (y as u32 / 900) % 2 == 0 { 90.0 } else { -90.0 }),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
    });
    shot(&mut h, &format!("long-chat-{}", size[0]));
    // A new width measures every entry again, as the first frame of a chat does.
    let mut w = size[0];
    time(&mut h, "resizing", 6, &mut |h| {
        w -= 7.0;
        h.set_size(egui::vec2(w, size[1]));
    });
}

#[test]
fn frame_times_on_a_long_transcript() {
    time_frames([1280.0, 800.0], 80);
}

#[test]
fn frame_times_on_a_phone_with_a_long_transcript() {
    time_frames([450.0, 1000.0], 80);
}

#[test]
fn a_long_transcript_draws_what_is_scrolled_to() {
    let rig = rig();
    let s = long_chat(&rig, 40);
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 200);
    h.run_steps(3);
    assert!(h.query_by_label_contains("Turn 39:").is_some(), "the end is in view");
    assert!(h.query_by_label_contains("Turn 0:").is_none(), "the start is not laid out");
    let wheel = |h: &mut Harness<'_, App>, dy: f32| {
        h.event(egui::Event::PointerMoved(egui::pos2(800.0, 300.0)));
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, dy),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        h.run_steps(3);
    };
    for _ in 0..60 {
        wheel(&mut h, 4000.0);
    }
    shot(&mut h, "long-chat-top");
    assert!(h.query_by_label_contains("Turn 0:").is_some(), "the start is drawn at the top");
    assert!(h.query_by_label_contains("Turn 39:").is_none(), "the end is not");
    for _ in 0..60 {
        wheel(&mut h, -4000.0);
    }
    assert!(h.query_by_label_contains("Turn 39:").is_some(), "back at the end");
    // A new message at the end is followed.
    click(&mut h, 800.0, 735.0);
    h.event(egui::Event::Text("the very last word".into()));
    h.run_steps(2);
    click(&mut h, 1220.0, 784.0);
    until(&mut h, "the reply", |h| h.state().transcript_len(s.id) >= 202);
    assert!(h.query_by_label_contains("echo: the very last word").is_some(), "follows the bottom");
}
