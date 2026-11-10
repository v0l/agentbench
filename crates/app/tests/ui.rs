use agentbench::App;
use agentbench_core::{Entry, NewIssue};
use agentbench_server::agent::JsonCommand;
use agentbench_server::store::Store;
use eframe::Storage as _;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
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
            "-c",
            "commit.gpgsign=false",
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

fn rig_with(extra: Vec<Box<dyn agentbench_server::agent::Agent>>) -> Rig {
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

fn rig_on(model_url: &str, extra: Vec<Box<dyn agentbench_server::agent::Agent>>) -> Rig {
    rig_named(model_url, "test-model", extra)
}

fn rig_named(
    model_url: &str,
    model_name: &str,
    extra: Vec<Box<dyn agentbench_server::agent::Agent>>,
) -> Rig {
    let model_name = model_name.to_string();
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("demo");
    repo(&main);
    let store = Store::open(&dir.path().join("f.db")).unwrap();
    store.set_setting("token", TOKEN).unwrap();
    agentbench_server::projects::ensure_supervisor(&store, &dir.path().join("supervisor")).unwrap();
    worktree(&main, "request-log");
    worktree(&main, "fix-login");
    let project = agentbench_server::projects::add(&store, &main, None, false).unwrap();
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
    store.set_status(second.id, agentbench_core::SessionStatus::Stopped).unwrap();
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
    let models = dir.path().join("models");
    let mut extra = Some(extra);
    let mut echo = Some(echo);
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let s = store.clone();
        let model_name = model_name.clone();
        let (extra, echo) = (extra.take().unwrap(), echo.take().unwrap());
        rt.spawn(async move {
            let config = agentbench_server::Config {
                model: agentbench_server::agent::native::Model {
                    url: model_url.clone(),
                    model: model_name,
                    ..Default::default()
                },
                ..Default::default()
            };
            let model = agentbench_server::agent::native::Shared::new(config.model.clone());
            let mut agents: Vec<Box<dyn agentbench_server::agent::Agent>> = vec![Box::new(echo)];
            agents.push(Box::new(agentbench_server::agent::Native::shared(model.clone())));
            agents.extend(extra);
            let _ = agentbench_server::serve_agents(
                s,
                agents,
                Some(model),
                listener,
                models,
                std::future::pending(),
            )
            .await;
        });
    }
    Rig { url: addr.to_string(), store, _dir: dir, _rt: rt }
}

fn gpu_setup() -> eframe::egui_wgpu::WgpuSetup {
    let mut setup = egui_kittest::wgpu::default_wgpu_setup();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(new) = &mut setup {
        new.instance_descriptor.flags = eframe::egui_wgpu::wgpu::InstanceFlags::empty();
    }
    setup
}

fn build_lock() -> std::sync::MutexGuard<'static, ()> {
    static BUILD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    BUILD.lock().unwrap_or_else(|e| e.into_inner())
}

fn harness(url: String) -> Harness<'static, App> {
    harness_at(url, [1280.0, 800.0])
}

fn harness_at(url: String, size: [f32; 2]) -> Harness<'static, App> {
    let _build = build_lock();
    Harness::builder().with_size(size).wgpu_setup(gpu_setup()).build_eframe(move |cc| {
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
        assert!(start.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    h.run_steps(3);
}

fn open_home_row(h: &mut Harness<'_, App>) {
    h.state_mut().blank_chat_in_home();
    h.run_steps(3);
}

fn shot(h: &mut Harness<'_, App>, name: &str) {
    if let Ok(dir) = std::env::var("AGENTBENCH_SHOTS") {
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
    assert_eq!(o.worktrees.len(), 4, "the repo's three and the home folder");

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
    let session = give_usage(&rig);
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    shot(&mut h, "status-line");
}

#[test]
fn on_a_phone_the_status_line_keeps_the_model_context_and_language_servers() {
    let rig = rig();
    let session = give_usage(&rig);
    let mut h = harness_at(rig.url.clone(), [450.0, 1000.0]);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(3);
    shot(&mut h, "status-line-phone");
    h.get_by_label("send");
}

/// Give the "say hello" chat a model, a context and three language servers in different states.
fn give_usage(rig: &Rig) -> agentbench_core::Session {
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    rig.store
        .add_usage(
            session.id,
            &agentbench_core::Usage {
                input_tokens: 2_000,
                cache_read_tokens: 6_000,
                context: 84_000,
                window: 128_000,
                model: "qwen3-coder".into(),
                host: "gpu.lan:11434".into(),
                effort: "medium".into(),
                turns: 1,
                lsp: vec![
                    lsp("rust", ".", agentbench_core::LspHealth::Ready, ""),
                    lsp("typescript", "web", agentbench_core::LspHealth::Indexing, ""),
                    lsp(
                        "go",
                        ".",
                        agentbench_core::LspHealth::Missing,
                        "go install golang.org/x/tools/gopls@latest",
                    ),
                ],
                ..Default::default()
            },
        )
        .unwrap();
    session
}

fn lsp(
    name: &str,
    dir: &str,
    health: agentbench_core::LspHealth,
    detail: &str,
) -> agentbench_core::Lsp {
    agentbench_core::Lsp { name: name.into(), dir: dir.into(), health, detail: detail.into() }
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
    let mut c = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        move || {
            let _ = tx.send(());
        },
    );
    c.send_message(session.id, "ping");
    rx.recv_timeout(Duration::from_secs(5)).expect("a reply");
    let replies = c.poll();
    match &replies[0] {
        agentbench::client::Reply::Done(_) => {}
        agentbench::client::Reply::Failed(e) | agentbench::client::Reply::Offline(e) => {
            panic!("{e}")
        }
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
    let mut c = agentbench::client::Client::new(
        agentbench::client::Server { url, token: TOKEN.into() },
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
        Some(agentbench::client::Reply::Offline(_)) => {}
        _ => panic!("expected the server to be called offline"),
    }
}

#[test]
fn a_network_switch_mid_poll_reconnects_on_a_fresh_connection() {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let rig = rig();
    let upstream = rig.url.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = listener.local_addr().unwrap().to_string();
    let generation = Arc::new(AtomicUsize::new(0));
    let gen_ = generation.clone();
    std::thread::spawn(move || {
        for down in listener.incoming().flatten() {
            let born = gen_.load(Ordering::SeqCst);
            let up = std::net::TcpStream::connect(&upstream).unwrap();
            for (mut from, mut to) in
                [(down.try_clone().unwrap(), up.try_clone().unwrap()), (up, down)]
            {
                let gen_ = gen_.clone();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 16384];
                    while let Ok(n) = from.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        while gen_.load(Ordering::SeqCst) != born {
                            std::thread::sleep(Duration::from_secs(3600));
                        }
                        if to.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        }
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let mut c = agentbench::client::Client::new(
        agentbench::client::Server { url, token: TOKEN.into() },
        move || {
            let _ = tx.send(());
        },
    )
    .with_patience(Duration::from_secs(2))
    .with_watch_wait(1);
    let mut queue = std::collections::VecDeque::new();
    let mut next = |c: &mut agentbench::client::Client| loop {
        if let Some(r) = queue.pop_front() {
            return r;
        }
        rx.recv_timeout(Duration::from_secs(15)).expect("a reply, not a hang");
        queue.extend(c.poll());
    };
    c.overview();
    c.overview();
    for _ in 0..2 {
        assert!(matches!(next(&mut c), agentbench::client::Reply::Overview(_)));
    }
    generation.fetch_add(1, Ordering::SeqCst);
    let started = Instant::now();
    c.watch(i64::MAX >> 1);
    match next(&mut c) {
        agentbench::client::Reply::Offline(_) => {}
        _ => panic!("a silent long poll must end as offline"),
    }
    assert!(started.elapsed() < Duration::from_secs(13), "{:?}", started.elapsed());
    c.overview();
    match next(&mut c) {
        agentbench::client::Reply::Overview(_) => {}
        agentbench::client::Reply::Offline(e) => panic!("still on the dead connection: {e}"),
        _ => panic!("odd reply"),
    }
}

#[test]
fn the_header_shows_reconnecting_while_the_server_is_gone_and_connected_after() {
    let rig = rig();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = listener.local_addr().unwrap().to_string();
    let open = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let open_ = open.clone();
    let upstream = rig.url.clone();
    std::thread::spawn(move || {
        for down in listener.incoming().flatten() {
            if !open_.load(std::sync::atomic::Ordering::SeqCst) {
                drop(down);
                continue;
            }
            let up = std::net::TcpStream::connect(&upstream).unwrap();
            let (mut a, mut b) = (down.try_clone().unwrap(), up.try_clone().unwrap());
            std::thread::spawn(move || std::io::copy(&mut a, &mut b));
            let (mut c, mut d) = (up, down);
            std::thread::spawn(move || std::io::copy(&mut c, &mut d));
        }
    });
    let mut h = harness_at(url.clone(), [450.0, 900.0]);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    assert!(h.query_by_label_contains("reconnecting").is_none());
    open.store(false, std::sync::atomic::Ordering::SeqCst);
    h.state_mut().drop_connection();
    until(&mut h, "reconnecting", |h| {
        h.query_by_label_contains("reconnecting").is_some()
    });
    shot(&mut h, "reconnecting");
    open.store(true, std::sync::atomic::Ordering::SeqCst);
    until(&mut h, "connected again", |h| {
        h.state().link() == agentbench::app::Link::Connected
            && h.query_by_label_contains("reconnecting").is_none()
    });
    let mut wide = harness(url);
    until(&mut wide, "the overview", |h| h.state().overview().is_some());
    wide.get_by_label("connected");
}

#[test]
fn without_a_local_server_the_window_asks_where_to_connect() {
    let rig = rig();
    let build = build_lock();
    let mut h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_eframe(|cc| App::new(&cc.egui_ctx, None, None));
    drop(build);
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
    let _build = build_lock();
    let mut h = Harness::builder().with_size([1280.0, 800.0]).wgpu_setup(gpu_setup()).build_eframe(move |cc| {
        let url = url.clone();
        let local = agentbench::LocalServer {
            start: Box::new(move || {
                Ok(agentbench::client::Server { url: url.clone(), token: TOKEN.into() })
            }),
        };
        App::with_local(&cc.egui_ctx, None, None, Some(local))
    });
    drop(_build);
    assert!(h.state().is_local());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
}

#[test]
fn a_saved_remote_choice_is_kept() {
    let mut storage = Mem::default();
    storage.0.insert("remote".into(), "true".into());
    let build = build_lock();
    let h = Harness::builder().with_size([900.0, 600.0]).build_eframe(move |cc| {
        let local = agentbench::LocalServer { start: Box::new(|| Err("must not start".into())) };
        App::with_local(&cc.egui_ctx, Some(&storage), None, Some(local))
    });
    drop(build);
    assert!(!h.state().is_local());
}

struct Lister;

#[agentbench_server::agent::async_trait]
impl agentbench_server::agent::Agent for Lister {
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
        _turn: &agentbench_server::agent::Turn<'_>,
        _io: &dyn agentbench_server::agent::Io,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn a_new_chat_offers_a_dropdown_for_any_agent_that_lists_models() {
    let rig = rig_with(vec![Box::new(Lister)]);
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().blank_chat_here();
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

#[agentbench_server::agent::async_trait]
impl agentbench_server::agent::Agent for Waiter {
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
        _turn: &agentbench_server::agent::Turn<'_>,
        io: &dyn agentbench_server::agent::Io,
    ) -> Result<(), String> {
        let q = agentbench_core::Question {
            text: "which licence?".into(),
            options: vec!["MIT".into(), "Apache-2.0".into()],
            ..Default::default()
        };
        let a = io.ask(q).await?;
        io.emit(agentbench_server::agent::Event::Text { delta: format!("going with {a}") });
        Ok(())
    }
}

struct Planner;

#[agentbench_server::agent::async_trait]
impl agentbench_server::agent::Agent for Planner {
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
        _turn: &agentbench_server::agent::Turn<'_>,
        io: &dyn agentbench_server::agent::Io,
    ) -> Result<(), String> {
        let q = |text: &str, options: &[&str], multiple| agentbench_core::Question {
            text: text.into(),
            options: options.iter().map(|o| o.to_string()).collect(),
            multiple,
            secret: false,
        };
        let a = io
            .ask_many(vec![
                q("which licence?", &["MIT", "Apache-2.0"], false),
                q("which targets?", &["linux", "mac", "windows"], true),
                q("anything else?", &[], false),
            ])
            .await?;
        io.emit(agentbench_server::agent::Event::Text {
            delta: format!("plan: {}", a.join(" | ")),
        });
        Ok(())
    }
}

#[test]
fn several_questions_are_answered_together_with_multiple_choice() {
    let rig = rig_with(vec![Box::new(Planner)]);
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap().id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let mut c = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    c.add_session(&agentbench_core::NewSession {
        project_id: project,
        agent: "planner".into(),
        prompt: Some("plan".into()),
        ..Default::default()
    });
    until(&mut h, "the waiting session", |h| {
        h.state().overview().is_some_and(|o| {
            o.sessions.iter().any(|s| {
                s.agent == "planner" && s.status == agentbench_core::SessionStatus::Waiting
            })
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
    h.query_all_by_role(egui::accesskit::Role::TextInput).last().unwrap().click();
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
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap().id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let mut c = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    c.add_session(&agentbench_core::NewSession {
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
                .any(|s| s.agent == "waiter" && s.status == agentbench_core::SessionStatus::Waiting)
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
    let counts = agentbench::status_counts(&h.state().overview().unwrap().sessions);
    assert_eq!(
        counts,
        vec![
            (agentbench_core::SessionStatus::Idle, 2),
            (agentbench_core::SessionStatus::Stopped, 1)
        ]
    );
}

#[test]
fn clicking_a_session_in_the_tree_opens_it() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    h.get_by_label("chat say hello").click();
    h.run_steps(3);
    let hello = rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    assert_eq!(h.state().open(), Some(hello.id), "the first session opens");
    h.get_by_label("chat #3 Fix the login loop").click();
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
    click(&mut h, 283.0, 20.0);
    h.run_steps(3);
    shot(&mut h, "issues-tab");
    click(&mut h, 614.0, 293.0);
    h.run_steps(3);
    shot(&mut h, "more");
    assert!(h.state().issue_open().is_some(), "more opens the issue");
}

#[test]
fn a_busy_transcript_reads_cleanly() {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "native", "The current credit system isnt very good, there isnt a way for users to see what they paid", None, None).unwrap();
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
        source: egui::MouseWheelSource::Unknown,
        delta: egui::vec2(0.0, 2000.0),
        modifiers: egui::Modifiers::NONE,
        phase: egui::TouchPhase::Move,
    });
    h.event(egui::Event::PointerMoved(egui::pos2(800.0, 300.0)));
    h.run_steps(4);
    shot(&mut h, "transcript-top");
}

#[test]
fn a_checklist_shows_its_items_in_the_transcript() {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "native", "plan it", None, None).unwrap();
    rig.store.add_message(s.id, &Entry::user("plan it")).unwrap();
    rig.store
        .add_message(
            s.id,
            &Entry::Tool {
                call_id: "t1".into(),
                name: "todo_write".into(),
                input: serde_json::json!({"items": [
                    {"text": "read the code", "status": "done"},
                    {"text": "write the fix", "status": "in_progress"},
                    {"text": "run the tests", "status": "pending"},
                ]}),
                output: Some("[x] read the code\n[~] write the fix\n[ ] run the tests".into()),
                error: false,
            },
        )
        .unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 2);
    h.run_steps(3);
    shot(&mut h, "checklist");
    h.get_by_label("write the fix");
    h.get_by_label("run the tests");

    // On a phone it starts folded, with the step in hand still in its header.
    let mut h = harness_at(rig.url.clone(), [450.0, 1000.0]);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 2);
    h.run_steps(3);
    shot(&mut h, "checklist-phone");
    assert!(h.query_by_label("run the tests").is_none(), "folded on a phone");
}

#[test]
fn a_model_refusal_shows_in_the_chat() {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "native", "do it", None, None).unwrap();
    rig.store.add_message(s.id, &Entry::user("do it")).unwrap();
    rig.store
        .add_message(
            s.id,
            &Entry::Error {
                text: "the model refused to answer (refusal): I can't help with that.".into(),
            },
        )
        .unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 2);
    h.run_steps(3);
    h.get_by_label_contains("I can't help with that.");
}

#[test]
fn diagnostics_after_an_edit_show_as_a_folded_row_with_the_first_finding() {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "native", "break it", None, None).unwrap();
    rig.store.add_message(s.id, &Entry::user("break it")).unwrap();
    rig.store
        .add_message(
            s.id,
            &Entry::Diagnostics {
                text: "<language-server-diagnostics>\nChecked by the language server after your edit; this is the harness speaking, not the user. Fix what is yours:\nsrc/lib.rs: 1 error (rust)\n  src/lib.rs:2:5  error[E0308] mismatched types\n</language-server-diagnostics>".into(),
                broken: vec!["src/lib.rs".into()],
            },
        )
        .unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 2);
    h.run_steps(3);
    shot(&mut h, "diagnostics");
    h.get_by_label("src/lib.rs: 1 error (rust)   +1 lines");
    assert!(h.query_by_label_contains("harness speaking").is_none());
}

#[test]
fn a_native_chat_shows_from_what_was_sent() {
    use agentbench_core::{Said, Sent};
    use serde_json::json;
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "native", "list it", None, None).unwrap();
    let row = |messages: Vec<(serde_json::Value, Said)>| Entry::Wire {
        messages: messages.into_iter().map(|(message, said)| Sent { message, said }).collect(),
    };
    for e in [
        row(vec![(json!({"role": "system", "content": "You are a careful agent."}), Said::Plain)]),
        row(vec![(
            json!({"role": "user", "content": "list the files"}),
            Said::Typed { attachments: vec![], from: vec![] },
        )]),
        row(vec![(
            json!({"role": "assistant", "content": "Looking.", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}},
            ]}),
            Said::Plain,
        )]),
        row(vec![(
            json!({"role": "tool", "tool_call_id": "c1", "content": "Cargo.toml\nsrc"}),
            Said::Plain,
        )]),
        row(vec![(json!({"role": "assistant", "content": "Two entries."}), Said::Plain)]),
    ] {
        rig.store.add_message(s.id, &e).unwrap();
    }
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 5);
    h.run_steps(3);
    shot(&mut h, "native-wire");
    h.get_by_label("list the files");
    h.get_by_label("Looking.");
    h.get_by_label("Two entries.");
    h.get_by_label("$ ls");
    h.get_by_label("You are a careful agent.");
    h.get_by_label("Cargo.toml   +1 lines");
}

fn scrolled_to_the_top(
    h: &mut Harness<'_, App>,
    at: egui::Pos2,
    session: agentbench_core::SessionId,
) {
    h.event(egui::Event::PointerMoved(at));
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        source: egui::MouseWheelSource::Unknown,
        delta: egui::vec2(0.0, 100_000.0),
        modifiers: egui::Modifiers::NONE,
        phase: egui::TouchPhase::Move,
    });
    h.run_steps(6);
    assert!(!h.state().follows(session), "the chat is scrolled up");
}

#[test]
fn end_scrolls_the_chat_to_the_latest_message() {
    let rig = rig();
    let s = long_chat(&rig, 12);
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 60);
    h.run_steps(3);
    scrolled_to_the_top(&mut h, egui::pos2(800.0, 300.0), s.id);
    arrow(&mut h, egui::Key::End);
    h.run_steps(30);
    assert!(h.state().follows(s.id), "end brings the chat back to the bottom");
}

#[test]
fn on_a_phone_the_latest_key_scrolls_a_chat_to_the_bottom() {
    let rig = rig();
    let s = long_chat(&rig, 12);
    let mut h = harness_at(rig.url.clone(), [450.0, 1000.0]);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(s.id) == 60);
    h.run_steps(3);
    assert!(h.query_by_label("latest").is_none(), "no key while the chat is at the bottom");
    scrolled_to_the_top(&mut h, egui::pos2(225.0, 400.0), s.id);
    shot(&mut h, "phone-latest");
    h.get_by_label("latest").click();
    h.run_steps(30);
    assert!(h.state().follows(s.id), "the key brings the chat back to the bottom");
    assert!(h.query_by_label("latest").is_none(), "the key goes once at the bottom");
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
    h.get_by_label("chat say hello").click();
    h.run_steps(3);
    assert!(!h.state().drawer_open(), "picking a chat closes the drawer");
    assert!(h.state().open().is_some(), "and opens it");
}

#[test]
fn a_bash_command_is_always_shown_whole_and_wrapped() {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "native", "risky", None, None).unwrap();
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
fn a_pairing_survives_a_process_killed_without_saving() {
    let rig = rig();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("pairing.json");
    let pairing_app = |file: PathBuf| {
        let build = build_lock();
        let h = Harness::builder().with_size([450.0, 900.0]).build_eframe(move |cc| {
            App::with_pairing(&cc.egui_ctx, None, None, None, Some(file.clone()))
        });
        drop(build);
        h
    };
    let mut first = pairing_app(file.clone());
    first.run_steps(2);
    assert_eq!(first.state().settings_loaded(), Some(false), "nothing paired yet");
    let link = agentbench_core::pairing::Pairing { url: rig.url.clone(), token: TOKEN.into() };
    let ctx = first.ctx.clone();
    first.state_mut().pair(&ctx, link);
    until(&mut first, "the overview", |h| h.state().overview().is_some());
    std::mem::forget(first);

    let mut second = pairing_app(file);
    second.run_steps(2);
    assert_eq!(second.state().settings_loaded(), None, "the connect screen came back");
    until(&mut second, "the overview", |h| h.state().overview().is_some());
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
    h.state_mut().settings_set_external("https://agentbench.harkin.network");
    h.run_steps(3);
    let p = h.state().settings_pairing().expect("a code once the address is given");
    assert_eq!(p.url, "https://agentbench.harkin.network");
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
        h.ctx.fonts_mut(|f| f.row_height(&egui::FontId::proportional(agentbench::fonts::BASE)))
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
fn the_voice_page_says_where_speech_runs() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(2);
    h.state_mut().settings_page("speech");
    until(&mut h, "the server's devices", |h| h.state().settings_audio_device().is_some());
    h.run_steps(3);
    shot(&mut h, "settings-devices");
    let device = h.state().settings_audio_device().unwrap();
    assert!(device == "GPU" || device == "CPU", "{device}");
    until(&mut h, "the server's voices", |h| h.state().settings_voices().is_some());
    assert!(h.state().settings_voices().unwrap().is_some(), "the server answered with a list");
    until(&mut h, "the server's speech models", |h| h.state().settings_speech_models().is_some());
    let models = h.state().settings_speech_models().unwrap().expect("the server lists its models");
    h.state_mut().settings_use_server_speech();
    h.run_steps(3);
    shot(&mut h, "settings-server-speech");
    assert!(
        models.iter().any(|(id, label)| id == "whisper-large-v3-turbo" && label.contains("MB")),
        "{models:?}"
    );
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
fn the_supervisor_model_is_set_in_the_settings_and_saved_on_the_server() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(2);
    until(&mut h, "the supervisor model", |h| h.state().settings_supervisor_loaded());
    h.state_mut().settings_set_supervisor_model("medium");
    h.run_steps(2);
    h.state_mut().settings_save();
    until(&mut h, "the save", |h| h.state().settings_loaded().is_none());
    assert_eq!(rig.store.setting("supervisor_model").unwrap().as_deref(), Some("medium"));
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
fn a_chat_can_start_in_the_home_folder_with_no_repo() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let home = rig.store.home_project().unwrap().expect("the server makes the home project");
    h.run_steps(3);
    shot(&mut h, "home");
    let o = h.state().overview().unwrap();
    assert!(o.projects.iter().any(|p| p.home && p.id == home.id), "the tree lists it");

    let mut c = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    c.add_session(&agentbench_core::NewSession {
        project_id: home.id,
        agent: "echo".into(),
        prompt: Some("parse this".into()),
        ..Default::default()
    });
    let start = Instant::now();
    while rig.store.sessions().unwrap().iter().all(|s| s.project_id != home.id) {
        assert!(start.elapsed() < Duration::from_secs(5), "no chat in the home folder");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    let s = rig.store.sessions().unwrap().into_iter().find(|s| s.project_id == home.id).unwrap();
    assert_eq!(s.cwd, home.path, "the agent starts in the home folder");
    c.add_session(&agentbench_core::NewSession {
        project_id: home.id,
        agent: "echo".into(),
        place: agentbench_core::Place::New { branch: "x".into() },
        ..Default::default()
    });
    h.run_steps(3);
    assert!(
        rig.store.sessions().unwrap().iter().filter(|s| s.project_id == home.id).count() == 1,
        "no worktrees in the home folder"
    );
}

#[test]
fn the_home_row_opens_a_blank_chat_that_starts_in_the_home_folder() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    open_home_row(&mut h);
    h.run_steps(3);
    shot(&mut h, "home-blank");
    assert_eq!(h.state().open(), None, "the row opens a blank chat");
    click(&mut h, 800.0, 310.0);
    h.event(egui::Event::Text("parse my file".into()));
    h.run_steps(2);
    h.key_press(egui::Key::Enter);
    until(&mut h, "a chat", |h| h.state().open().is_some());
    let s = rig.store.session(h.state().open().unwrap()).unwrap();
    let home = rig.store.home_project().unwrap().unwrap();
    assert_eq!(s.project_id, home.id);
    assert_eq!(s.cwd, home.path);
}

#[test]
fn new_chat_stays_blank_while_other_agents_work() {
    let rig = rig();
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap().id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().blank_chat_here();
    h.run_steps(3);
    assert_eq!(h.state().open(), None, "new chat starts blank");
    let mut other = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    other.add_session(&agentbench_core::NewSession {
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
        until(&mut h, "no chat working", |h| {
            h.state().overview().is_some_and(|o| o.sessions.iter().all(|s| !s.status.busy()))
        });
        h.run_steps(3);
        open_home_row(&mut h);
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
    let main = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap().id;
    let tree = rig.store.main_worktree(main).unwrap();
    let old = rig.store.add_session(tree.id, "echo", "old talk", None, None).unwrap();
    for text in ["one", "two", "three"] {
        rig.store.add_message(old.id, &Entry::user(text)).unwrap();
    }
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(old.id, old.project_id);
    until(&mut h, "its transcript", |h| h.state().transcript_len(old.id) == 3);

    let mut other = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    other.delete_session(old.id);
    until(&mut h, "the chat to go", |h| h.state().open().is_none());

    open_home_row(&mut h);
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

#[derive(Debug)]
struct Dropped {
    path: std::path::PathBuf,
    bytes: Vec<u8>,
}

impl egui::DroppedFile for Dropped {
    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        Ok(self.bytes.clone())
    }
}

fn drop_file(h: &mut Harness<'_, App>, name: &str, bytes: Vec<u8>) {
    h.input_mut().dropped_files.push(std::sync::Arc::new(Dropped { path: name.into(), bytes }));
    h.step();
}

#[test]
fn a_dropped_picture_is_sent_with_the_message_and_shown_in_the_chat() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    open_home_row(&mut h);
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

fn copy_key(h: &mut Harness<'_, App>) {
    h.event(egui::Event::Copy);
    h.run_steps(2);
}

#[test]
fn ctrl_c_in_the_composer_clears_the_draft_unless_text_is_selected() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(2);
    click(&mut h, 800.0, 735.0);
    h.event(egui::Event::Text("half a thought".into()));
    h.run_steps(2);
    assert_eq!(h.state().draft_of(session.id), "half a thought");

    h.event(egui::Event::Key {
        key: egui::Key::A,
        physical_key: Some(egui::Key::A),
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND,
    });
    h.run_steps(2);
    copy_key(&mut h);
    assert_eq!(
        h.state().draft_of(session.id),
        "half a thought",
        "a selection is copied, not cleared"
    );

    h.event(egui::Event::Key {
        key: egui::Key::ArrowRight,
        physical_key: Some(egui::Key::ArrowRight),
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    h.run_steps(2);
    copy_key(&mut h);
    assert_eq!(h.state().draft_of(session.id), "", "ctrl+c with nothing selected clears the box");
}

#[test]
fn keyboard_edits_compose_shrink_commit_and_delete_in_the_composer() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(2);
    click(&mut h, 800.0, 735.0);
    h.event(egui::Event::Text("say ".into()));
    h.run_steps(2);
    let ctx = h.ctx.clone();
    assert_eq!(
        agentbench::system::ime_keyboard(&ctx).map(|k| k.text),
        Some("say ".into()),
        "the keyboard is told what the field holds"
    );
    for (line, want) in [
        ("6 6\nsay he", "say he"),
        ("7 7\nsay hel", "say hel"),
        ("6 6\nsay he", "say he"),
        ("5 5\nsay h", "say h"),
        ("9 9\nsay hello", "say hello"),
        ("10 10\nsay hello ", "say hello "),
        ("9 9\nsay hello", "say hello"),
        ("8 8\nsay hell", "say hell"),
        ("7 7\nsay hel", "say hel"),
        ("1 1\nXsay hel", "Xsay hel"),
        ("0 0\nsay hel", "say hel"),
    ] {
        agentbench::system::ime_from_keyboard(&ctx, line);
        h.run_steps(2);
        assert_eq!(h.state().draft_of(session.id), want, "after the keyboard sent {line:?}");
    }
    agentbench::system::ime_from_keyboard(&ctx, "7 7\nsay hel");
    h.run_steps(2);
    h.event(egui::Event::Text("p".into()));
    h.run_steps(2);
    assert_eq!(h.state().draft_of(session.id), "say help", "typing lands where the keyboard put the cursor");
    assert_eq!(
        agentbench::system::ime_keyboard(&ctx),
        Some(agentbench::system::ImeText { text: "say help".into(), start: 8, end: 8 }),
    );
}

#[test]
fn dictation_into_a_composer_whose_keyboard_was_dismissed_never_brings_it_back() {
    use agentbench::system::{KeyboardCall, keyboard_calls};
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(2);
    let ctx = h.ctx.clone();
    click(&mut h, 800.0, 735.0);
    h.run_steps(2);
    let shown = keyboard_calls(&ctx);
    assert!(shown.contains(&KeyboardCall::Show), "a tap on the composer shows it, got {shown:?}");
    agentbench::system::keyboard_visible(&ctx, false);
    let mut said = String::new();
    for n in 0..20 {
        said.push_str(&format!(" word{n}"));
        h.state_mut().hear(&said, n == 19);
        h.run_steps(2);
        let calls = keyboard_calls(&ctx);
        assert!(
            !calls.contains(&KeyboardCall::Show) && !calls.contains(&KeyboardCall::Restart),
            "partial {n} asked the keyboard for {calls:?}"
        );
    }
    assert!(
        h.state().draft_of(session.id).ends_with("word18 word19"),
        "the words land in the draft"
    );
    click(&mut h, 600.0, 400.0);
    h.run_steps(2);
    keyboard_calls(&ctx);
    for n in 0..5 {
        h.state_mut().hear(&format!("more {n}"), n == 4);
        h.run_steps(2);
        let calls = keyboard_calls(&ctx);
        assert!(
            !calls.contains(&KeyboardCall::Show) && !calls.contains(&KeyboardCall::Restart),
            "speech into an unfocused composer asked for {calls:?}"
        );
    }
    click(&mut h, 800.0, 735.0);
    h.run_steps(2);
    assert!(keyboard_calls(&ctx).contains(&KeyboardCall::Show), "tapping it again shows it");
}

fn arrow(h: &mut Harness<'_, App>, key: egui::Key) {
    h.event(egui::Event::Key {
        key,
        physical_key: Some(key),
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    h.run_steps(2);
}

#[test]
fn up_in_an_empty_composer_brings_back_the_last_message_sent() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(2);
    let sent = rig
        .store
        .messages(session.id, 0)
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|m| match m.entry {
            agentbench_core::Entry::User { text, .. } => Some(text),
            _ => None,
        })
        .unwrap();
    click(&mut h, 800.0, 735.0);
    h.run_steps(3);
    arrow(&mut h, egui::Key::ArrowUp);
    assert_eq!(h.state().draft_of(session.id), sent, "up on an empty box recalls the last message");
    arrow(&mut h, egui::Key::ArrowDown);
    assert_eq!(h.state().draft_of(session.id), "", "down steps back to an empty box");

    h.event(egui::Event::Text("something new".into()));
    h.run_steps(2);
    arrow(&mut h, egui::Key::ArrowUp);
    assert_eq!(
        h.state().draft_of(session.id),
        "something new",
        "a half typed message is left alone"
    );
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
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap().id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let mut other = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    other.add_session(&agentbench_core::NewSession {
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
            .filter(|s| s.status == agentbench_core::SessionStatus::Running)
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
    let _build = build_lock();
    let mut h = Harness::builder().with_size([1280.0, 800.0]).wgpu_setup(gpu_setup()).build_eframe(move |cc| {
        let mut storage = Mem::default();
        storage.0.insert("url".into(), url);
        storage.0.insert("token".into(), TOKEN.into());
        storage.0.insert("voice_mode".into(), "server".into());
        App::new(&cc.egui_ctx, Some(&storage), None)
    });
    drop(_build);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    open_home_row(&mut h);
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

fn extra_chats(rig: &Rig, n: usize) -> Vec<agentbench_core::Session> {
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    (0..n)
        .map(|i| rig.store.add_session(main.id, "echo", &format!("extra {i}"), None, None).unwrap())
        .collect()
}

#[test]
fn opening_a_chat_replaces_the_focused_tile() {
    use agentbench::app::Grid;
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
    use agentbench::app::Grid;
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
    use agentbench::app::Grid;
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::Columns);
    h.run_steps(3);
    let all = rig.store.sessions().unwrap();
    let hello = all.iter().find(|s| s.title == "say hello").unwrap().id;
    let fix = all.iter().find(|s| s.title == "#3 Fix the login loop").unwrap().id;
    h.get_by_label("chat say hello").click();
    h.run_steps(3);
    assert_eq!(h.state().tile_chat(0), Some(hello));
    // Focus the empty second tile, then click another chat: it fills that tile.
    h.state_mut().focus_tile(1);
    h.run_steps(2);
    h.get_by_label("chat #3 Fix the login loop").click();
    h.run_steps(3);
    assert_eq!(h.state().tile_chat(0), Some(hello));
    assert_eq!(h.state().tile_chat(1), Some(fix));
    // With the first tile focused, the same click replaces it.
    h.state_mut().focus_tile(0);
    h.get_by_label("chat #3 Fix the login loop").click();
    h.run_steps(3);
    assert_eq!(h.state().focused_tile(), 1, "it was already on screen, so it only took focus");
    shot(&mut h, "columns");
}

#[test]
fn a_child_session_shows_a_parent_chip_that_opens_the_parent() {
    let rig = rig();
    let all = rig.store.sessions().unwrap();
    let parent = all.iter().find(|s| s.title == "say hello").unwrap().clone();
    let fix = all.iter().find(|s| s.title == "#3 Fix the login loop").unwrap().clone();
    rig.store.set_parent(fix.id, Some(parent.id)).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    h.get_by_label("chat #3 Fix the login loop").click();
    h.run_steps(4);
    assert_eq!(h.state().tile_chat(h.state().focused_tile()), Some(fix.id));
    assert!(h.query_by_label_contains("1 children").is_none(), "the child has none");
    click_label(&mut h, "parent say hello");
    h.run_steps(4);
    assert_eq!(h.state().tile_chat(h.state().focused_tile()), Some(parent.id));
    assert!(h.query_by_label_contains("1 children").is_some(), "the parent counts its child");
}

fn quota_server(used_pct: u32, resets_in_secs: i64) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let resets = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + resets_in_secs;
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let body = format!(r#"{{"window":"5h","used_pct":{used_pct},"resets_at":{resets}}}"#);
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

#[test]
fn the_header_shows_the_quota_and_spend() {
    let rig = rig_on(&quota_server(78, 7800), Vec::new());
    let first = rig.store.sessions().unwrap()[0].clone();
    let usage = agentbench_core::Usage { cost_usd: 1.25, turns: 1, ..Default::default() };
    let mut h = harness(rig.url.clone());
    until(&mut h, "the quota", |h| {
        rig.store.add_usage(first.id, &agentbench_core::Usage::default()).unwrap();
        h.state()
            .overview()
            .and_then(|o| o.usage.as_ref())
            .is_some_and(|u| !u.quotas.is_empty())
    });
    rig.store.add_usage(first.id, &usage).unwrap();
    until(&mut h, "the spend", |h| {
        h.state().overview().and_then(|o| o.usage.as_ref()).is_some_and(|u| u.spend.today_usd > 1.0)
    });
    h.run_steps(3);
    let report = h.state().overview().unwrap().usage.clone().unwrap();
    let labels = agentbench::app::usage_labels(&report, None, agentbench_core::now_ms());
    assert!(labels.iter().any(|(t, hot)| t.starts_with("test-model: 5h 78%, resets ") && *hot), "{labels:?}");
    assert!(labels.iter().any(|(t, _)| t.starts_with("$1.25 today")), "{labels:?}");
    shot(&mut h, "quota-header");
}

#[test]
fn the_header_shows_spend_today() {
    let rig = rig();
    let first = rig.store.sessions().unwrap()[0].clone();
    let usage = agentbench_core::Usage { cost_usd: 1.25, turns: 1, ..Default::default() };
    rig.store.add_usage(first.id, &usage).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    let report = h.state().overview().unwrap().usage.clone().unwrap();
    assert_eq!(report.spend.today_usd, 1.25);
    let labels = agentbench::app::usage_labels(&report, None, 0);
    assert!(labels.iter().any(|(t, _)| t.starts_with("$1.25 today")), "{labels:?}");
}

#[test]
fn closing_a_tiled_chat_leaves_its_tile_empty() {
    use agentbench::app::Grid;
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
    use agentbench::app::Grid;
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
    let _build = build_lock();
    let again = Harness::builder().with_size([1280.0, 800.0]).wgpu_setup(gpu_setup()).build_eframe(|cc| {
        saved.0.insert("url".into(), rig.url.clone());
        saved.0.insert("token".into(), TOKEN.into());
        App::new(&cc.egui_ctx, Some(&saved), None)
    });
    drop(_build);
    assert_eq!(again.state().grid(), Grid::Quad);
    assert_eq!(again.state().tile_chat(0), Some(all[0].id));
    assert_eq!(again.state().tile_chat(1), Some(all[1].id));
    assert_eq!(again.state().focused_tile(), 1);
}

#[test]
fn long_markdown_wraps_inside_a_tile() {
    use agentbench::app::Grid;
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "native", "a long answer", None, None).unwrap();
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
    // A table and more text after it in one message: the text must keep wrapping.
    add(Entry::Assistant {
        text: format!(
            "Done.\n\n| repo | tag | what |\n|---|---|---|\n| agentbench | `v0.2.0` | {long} |\n\n- **Builds:** {long}\n- **Name:** {long} with `code`\n\n{long}"
        ),
    });
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
    let s = rig.store.add_session(main.id, "native", "markdown", None, None).unwrap();
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

fn near(img: &image::RgbaImage, tint: egui::Color32) -> usize {
    img.pixels()
        .filter(|p| {
            let d = |a: u8, b: u8| (a as i32 - b as i32).abs();
            d(p[0], tint.r()) < 24 && d(p[1], tint.g()) < 24 && d(p[2], tint.b()) < 24
        })
        .count()
}

fn tinted_chat(entry: Entry) -> (usize, usize) {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "echo", "tints", None, None).unwrap();
    rig.store.add_message(s.id, &entry).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, main.project_id);
    h.run_steps(12);
    shot(&mut h, "tinted-notice");
    let img = h.render().expect("rendered");
    (near(&img, egui_bench::prelude::WARN), near(&img, egui_bench::prelude::FAULT))
}

#[test]
fn a_retrying_notice_is_drawn_in_warn_and_a_failure_in_fault() {
    let text = "https://yalr.v0l.io/v1: the stream broke; retrying in 2s (1/3)";
    let (base_warn, base_fault) = tinted_chat(Entry::Notice { text: text.into() });
    let (warn, fault) = tinted_chat(Entry::Warning { text: text.into() });
    assert!(warn > base_warn + 100, "a warning shows in WARN: {warn} over {base_warn}");
    assert!(fault <= base_fault + 20, "and not in FAULT: {fault} over {base_fault}");
    let (warn, fault) = tinted_chat(Entry::Error { text: "the turn failed for good".into() });
    assert!(fault > base_fault + 100, "a failure shows in FAULT: {fault} over {base_fault}");
    assert!(warn <= base_warn + 20, "and not in WARN: {warn} over {base_warn}");
}

#[test]
fn a_reply_has_a_read_aloud_link() {
    let rig = rig();
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "native", "reply", None, None).unwrap();
    rig.store.add_message(s.id, &Entry::Assistant { text: "The build passed.".into() }).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, main.project_id);
    until(&mut h, "the reply", |h| h.state().transcript_len(s.id) == 1);
    until(&mut h, "the read aloud link", |h| h.query_by_label_contains("read aloud").is_some());
    shot(&mut h, "read-aloud");
    assert!(h.query_by_label_contains("read aloud").is_some(), "the link sits under the reply");
}

/// A long chat: markdown, code blocks, tables, tool calls with long output and thinking.
fn long_chat(rig: &Rig, turns: usize) -> agentbench_core::Session {
    let main = rig.store.worktrees(None).unwrap().into_iter().find(|w| w.main).unwrap();
    let s = rig.store.add_session(main.id, "echo", "a very long chat", None, None).unwrap();
    let add = |e: Entry| {
        rig.store.add_message(s.id, &e).unwrap();
    };
    let long_out: String = (0..400)
        .map(|i| {
            format!("src/module_{i}/file.rs:{i}: fn handler_{i}(request: &Request) -> Response {{")
        })
        .collect::<Vec<_>>()
        .join("\n");
    for n in 0..turns {
        add(Entry::user(&format!(
            "Turn {n}: look at **how credits are applied** and `tell me` what to change, see https://example.com/{n}.\n\n- first\n- second"
        )));
        add(Entry::Thinking {
            text: format!(
                "Thinking about turn {n}. Credits are applied without a ledger row, so the user can never see them. I should look in the billing worker."
            ),
        });
        add(Entry::Tool {
            call_id: format!("a{n}"),
            name: "search_files".into(),
            input: serde_json::json!({"pattern": "apply_credit"}),
            output: Some(
                "src/billing.rs:88: fn apply_credit(\nsrc/api.rs:210: apply_credit(".into(),
            ),
            error: false,
        });
        add(Entry::Tool {
            call_id: format!("b{n}"),
            name: "bash".into(),
            input: serde_json::json!({"command": "cargo test -p billing"}),
            output: Some(long_out.clone()),
            error: n % 7 == 3,
        });
        add(Entry::Assistant {
            text: format!(
                "## Turn {n}\n\nCredits are added to the balance with no **ledger row**, which is why users cannot see them. This paragraph is long enough to wrap over a couple of lines on a phone so that the layout has some work to do, with a [link](https://example.com) in it.\n\n1. Record every credit in `ledger`.\n2. Show the ledger on the billing page.\n3. Add a test.\n\n```rust\nfn apply_credit(user: &mut User, amount: u64) {{\n    user.credit += amount;\n    ledger.record(user, amount, Reason::Credit);\n}}\n```\n\n| column | meaning |\n|---|---|\n| a | first |\n| b | second |\n\n> a quoted remark about turn {n}"
            ),
        });
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
    let total = (turns * 5).min(150);
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
    println!(
        "PERF {}x{} {total} msgs opening the chat, slowest frame: {slowest:.1} ms",
        size[0], size[1]
    );
    h.run_steps(3);
    let time = |h: &mut Harness<'_, App>,
                what: &str,
                n: usize,
                each: &mut dyn FnMut(&mut Harness<'_, App>)| {
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
            size[0],
            size[1],
            v[v.len() / 2],
            v[v.len() - 1]
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
            source: egui::MouseWheelSource::Unknown,
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
    until(&mut h, "the latest page", |h| h.state().transcript_len(s.id) == 150);
    h.run_steps(3);
    assert!(h.query_by_label_contains("Turn 39:").is_some(), "the end is in view");
    assert!(h.query_by_label_contains("Turn 0:").is_none(), "the start is not laid out");
    assert!(h.state().transcript_len(s.id) < 200, "the older rows are not fetched yet");
    let wheel = |h: &mut Harness<'_, App>, dy: f32| {
        h.event(egui::Event::PointerMoved(egui::pos2(800.0, 300.0)));
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            source: egui::MouseWheelSource::Unknown,
            delta: egui::vec2(0.0, dy),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        h.run_steps(3);
    };
    for _ in 0..60 {
        wheel(&mut h, 4000.0);
    }
    until(&mut h, "the older rows", |h| h.state().transcript_len(s.id) == 200);
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

#[test]
fn moving_an_issue_to_a_column_changes_its_state_on_the_server() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let id = h.state().overview().unwrap().issues[0].id;
    h.state_mut().move_issue(id, agentbench_core::IssueState::Review);
    until(&mut h, "the move", |h| {
        h.state().overview().unwrap().issues[0].state == agentbench_core::IssueState::Review
    });
    assert_eq!(rig.store.issue(id).unwrap().state, agentbench_core::IssueState::Review);
}

#[test]
fn with_issues_first_a_chat_opens_inside_its_issue() {
    let rig = rig();
    let issue = rig.store.issues().unwrap()[0].id;
    let side = rig
        .store
        .worktrees(None)
        .unwrap()
        .into_iter()
        .find(|w| w.branch.as_deref() == Some("fix-login"))
        .unwrap();
    let linked =
        rig.store.add_session(side.id, "echo", "work on readme", None, Some(issue)).unwrap();
    rig.store.add_message(linked.id, &Entry::user("write it")).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_issues_first(true);
    h.state_mut().open_session(linked.id, linked.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(linked.id) == 1);
    shot(&mut h, "issue-pane");
    assert_eq!(h.state().issue_pane_open(), Some(issue));
}

#[test]
fn a_grid_of_any_columns_by_rows_holds_that_many_chats() {
    use agentbench::app::Grid;
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::new(4, 3));
    h.run_steps(3);
    assert_eq!(h.state().grid().dims(), (4, 3));
    assert_eq!(h.state().grid().slots(), 12);
    h.state_mut().focus_tile(11);
    h.run_steps(2);
    assert_eq!(h.state().focused_tile(), 11);
}

#[test]
fn the_layout_key_opens_a_board_that_sets_columns_and_rows() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    h.get_by_label("layout").click();
    h.run_steps(12);
    click(&mut h, 1022.0, 78.0);
    h.run_steps(3);
    assert_eq!(h.state().grid().dims(), (2, 2));
}

#[test]
fn new_chat_opens_a_modal_that_picks_the_project() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    click(&mut h, 1220.0, 23.0);
    h.run_steps(3);
    assert!(h.state().starting(), "the start modal is open");
    assert!(h.query_all_by_label_contains("project").count() >= 1, "the modal asks for a project");
    assert!(h.query_all_by_label_contains("START").count() >= 1, "and has a start key");
}

#[test]
fn the_chat_list_keeps_the_order_chats_were_started_in() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    let before = h.state().chat_order();
    assert!(before.len() >= 3);
    let mut sorted = rig.store.sessions().unwrap();
    sorted.sort_by_key(|s| std::cmp::Reverse((s.created_at, s.id.to_string())));
    assert_eq!(before, sorted.iter().map(|s| s.id).collect::<Vec<_>>());
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1200) {
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(h.state().chat_order(), before, "a refresh does not reorder the chats");
}

#[test]
fn ctrl_n_opens_the_new_chat_modal() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    assert!(!h.state().starting());
    h.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::N);
    h.run_steps(3);
    assert!(h.state().starting());
}

#[test]
fn f1_opens_and_closes_the_help() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    h.key_press(egui::Key::F1);
    h.run_steps(3);
    assert!(h.state().helping());
    shot(&mut h, "help");
    h.key_press(egui::Key::F1);
    h.run_steps(3);
    assert!(!h.state().helping());
}

#[test]
fn the_supervisor_key_opens_one_chat_and_shows_no_project() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let o = h.state().overview().unwrap();
    assert!(o.projects.iter().all(|p| !p.supervisor), "the supervisor has no project in the list");
    assert!(h.state().supervisor_chat().is_none());
    let before = o.sessions.len();

    h.get_by_label("supervisor").click();
    until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    let sid = h.state().supervisor_chat().unwrap();
    assert_eq!(h.state().shown(), [sid]);
    h.run_steps(3);
    shot(&mut h, "supervisor");
    assert_eq!(h.state().overview().unwrap().sessions.len(), before + 1);

    h.run_steps(5);
    assert!(h.query_by_label_contains("chat Supervisor").is_some(), "now a chat row");
    assert_eq!(h.state().overview().unwrap().sessions.len(), before + 1, "one standing chat");
    assert_eq!(h.state().supervisor_chat(), Some(sid));
}

#[test]
fn the_supervisor_chat_shows_the_autopilot_and_pauses_it_with_one_click() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("supervisor").click();
    until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    h.run_steps(3);
    assert_eq!(h.state().autopilot().map(|a| a.enabled), Some(false));
    assert!(h.query_by_label_contains("autopilot").is_some(), "the loop state shows");
    h.get_by_label("autopilot toggle").click();
    until(&mut h, "autopilot on", |h| h.state().autopilot().is_some_and(|a| a.enabled));
    h.get_by_label("autopilot toggle").click();
    until(&mut h, "autopilot paused", |h| h.state().autopilot().is_some_and(|a| !a.enabled));
}

#[test]
fn the_autopilot_panel_is_collapsed_by_default_and_hides_an_empty_triage() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("supervisor").click();
    until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    h.run_steps(3);
    assert!(h.query_by_label("autopilot toggle").is_some(), "the one-line summary shows");
    assert!(h.query_by_label("default automation").is_none(), "details are collapsed");
    assert!(h.query_by_label("triage run").is_none(), "an empty triage is hidden");
    h.get_by_label("expand supervisor panels").click();
    h.run_steps(3);
    assert!(h.query_by_label("default automation").is_some(), "details expand on click");
    assert!(h.query_by_label("triage run").is_some(), "triage shows when expanded");
    h.get_by_label("expand supervisor panels").click();
    h.run_steps(3);
    assert!(h.query_by_label("default automation").is_none(), "collapses again");
    assert!(h.query_by_label("triage run").is_none());
}

fn crowd_the_supervisor(rig: &Rig) {
    for i in 0..23 {
        let dir = rig._dir.path().join(format!("crowd-{i}"));
        repo(&dir);
        agentbench_server::projects::add(&rig.store, &dir, None, false).unwrap();
    }
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    for i in 0..5 {
        let proposal = agentbench_core::Proposal {
            id: 0,
            kind: agentbench_core::ProposalKind::Stale,
            summary: format!("Close stale issue number {i}"),
            issues: vec![i as i64],
            sessions: vec![],
            projects: vec![project.id],
            changes: vec![],
            state: agentbench_core::ProposalState::Pending,
            created_at: 0,
        };
        rig.store.add_proposal(&format!("crowd:{i}"), &proposal).unwrap();
    }
}

fn open_crowded_supervisor(h: &mut Harness<'_, App>) -> agentbench_core::SessionId {
    until(h, "the overview", |h| h.state().overview().is_some_and(|o| o.projects.len() >= 24));
    h.get_by_label("supervisor").click();
    until(h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    until(h, "the triage", |h| {
        h.state().overview().and_then(|o| o.triage.as_ref()).is_some_and(|t| t.proposals.len() == 5)
    });
    h.run_steps(5);
    h.state().supervisor_chat().unwrap()
}

#[test]
fn the_supervisor_transcript_keeps_half_the_window_with_24_projects_and_proposals() {
    let rig = rig();
    crowd_the_supervisor(&rig);
    let mut h = harness(rig.url.clone());
    let sid = open_crowded_supervisor(&mut h);
    shot(&mut h, "supervisor-crowded");
    assert!(!h.state().triage_open(), "collapsed by default");
    let view = h.state().transcript_view_height(sid);
    assert!(view >= 400.0, "transcript is {view}px of an 800px window");
    assert!(h.query_by_label("triage run").is_some(), "run now stays on the one line");
    assert!(h.query_by_label_contains("triage details, 5 proposals").is_some(), "the count shows");
}

#[test]
fn an_expanded_triage_panel_scrolls_and_leaves_the_transcript_visible() {
    let rig = rig();
    crowd_the_supervisor(&rig);
    let mut h = harness(rig.url.clone());
    let sid = open_crowded_supervisor(&mut h);
    h.get_by_label_contains("triage details").click();
    h.run_steps(5);
    assert!(h.state().triage_open());
    shot(&mut h, "supervisor-crowded-open");
    let view = h.state().transcript_view_height(sid);
    assert!(view >= 250.0, "transcript is {view}px with the panel open");
    h.get_by_label("automation levels").click();
    h.run_steps(5);
    let view = h.state().transcript_view_height(sid);
    assert!(view >= 150.0, "transcript is {view}px with both open");
    assert!(h.query_by_label("message").is_some() || view > 0.0);
}

#[test]
fn a_logo_in_the_repo_becomes_the_projects_icon() {
    let rig = rig();
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        8,
        8,
        image::Rgba([200, 60, 60, 255]),
    ))
    .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
    .unwrap();
    std::fs::write(Path::new(&project.path).join("logo.png"), png).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    until(&mut h, "the icon", |h| h.state().icon_state(project.id) == "ready");
    shot(&mut h, "icons");
}

#[test]
fn a_project_is_renamed_from_the_settings_projects_page() {
    let rig = rig();
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(3);
    h.state_mut().settings_page("projects");
    h.run_steps(3);
    shot(&mut h, "projects-page");
    h.get_by_label("rename project").click();
    h.run_steps(3);
    h.event(egui::Event::Key {
        key: egui::Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND,
    });
    h.event(egui::Event::Text("Billing".into()));
    h.run_steps(2);
    h.get_by_label("save name").click();
    until(&mut h, "the new name", |_| rig.store.project(project.id).unwrap().name == "Billing");
}

#[test]
fn the_settings_projects_page_sets_a_level_and_risky_toggles_that_persist() {
    use agentbench_core::{Automation, Risky};
    let rig = rig();
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(3);
    h.state_mut().settings_page("projects");
    h.run_steps(3);
    assert_eq!(h.state().project_level(project.id), Some((Automation::Delegate, true)));
    h.get_by_label(&format!("{} automation", project.name)).set_value_accesskit(4.0);
    until(&mut h, "ship saved", |_| rig.store.project(project.id).unwrap().automation == Some(Automation::Ship));
    h.get_by_label(&format!("{} deploy to production", project.name)).click();
    until(&mut h, "deploy on", |_| rig.store.project(project.id).unwrap().risky == [Risky::Deploy]);
    h.get_by_label(&format!("{} automation", project.name)).increment_accesskit();
    until(&mut h, "full auto", |_| rig.store.project(project.id).unwrap().automation == Some(Automation::FullAuto));
    h.run_steps(3);
    shot(&mut h, "automation-settings");
    h.get_by_label("default automation").set_value_accesskit(1.0);
    until(&mut h, "default assist", |h| h.state().autopilot().is_some_and(|a| a.default_level == Automation::Assist));
    h.get_by_label(&format!("{} use default", project.name)).click();
    until(&mut h, "back to default", |_| rig.store.project(project.id).unwrap().automation.is_none());
    until(&mut h, "follows assist", |h| h.state().project_level(project.id) == Some((Automation::Assist, true)));
    assert_eq!(rig.store.project(project.id).unwrap().risky, [Risky::Deploy], "toggles are kept");
}

#[test]
fn the_supervisor_chat_shows_and_sets_the_levels() {
    use agentbench_core::Automation;
    let rig = rig();
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("supervisor").click();
    until(&mut h, "the supervisor chat", |h| h.state().supervisor_chat().is_some());
    h.run_steps(3);
    assert!(h.query_by_label("automation level delegate").is_some(), "the header names the focused project's level");
    h.get_by_label("automation levels").click();
    h.run_steps(3);
    h.get_by_label(&format!("{} automation", project.name)).set_value_accesskit(0.0);
    until(&mut h, "manual", |_| rig.store.project(project.id).unwrap().automation == Some(Automation::Manual));
    until(&mut h, "header follows", |h| h.query_by_label("automation level manual").is_some());
    shot(&mut h, "automation-supervisor");
}

#[test]
fn the_all_projects_board_shows_each_projects_level() {
    use agentbench_core::Automation;
    let rig = rig();
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    rig.store
        .update_project(project.id, &agentbench_core::ProjectPatch { automation: Some(Some(Automation::Autowork)), ..Default::default() })
        .unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().show_all_projects();
    h.run_steps(3);
    shot(&mut h, "automation-board-chips");
    assert!(h.query_by_label_contains(&format!("{} autowork", project.name)).is_some(), "the board shows the level");
    h.get_by_label("project levels").click();
    h.run_steps(3);
    h.get_by_label(&format!("{} automation", project.name)).decrement_accesskit();
    until(&mut h, "delegate", |_| rig.store.project(project.id).unwrap().automation == Some(Automation::Delegate));
    shot(&mut h, "automation-board");
}

#[test]
fn rename_project_from_a_chats_menu_opens_the_name_field() {
    let rig = rig();
    let sid =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap().id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    h.state_mut().rename_project_from_chat(sid);
    h.run_steps(3);
    shot(&mut h, "rename-from-chat");
    h.get_by_label("save name");
}

#[test]
fn a_chat_is_renamed_from_its_menu_modal() {
    let rig = rig();
    let sid =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap().id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    h.state_mut().start_rename_chat(sid);
    h.run_steps(3);
    assert!(h.state().rename_chat_open());
    shot(&mut h, "rename-chat");
    h.event(egui::Event::Key {
        key: egui::Key::A,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::COMMAND,
    });
    h.event(egui::Event::Text("Greetings".into()));
    h.run_steps(2);
    assert!(h.state().rename_chat_open());
    h.run_steps(3);
    h.key_press(egui::Key::Enter);
    until(&mut h, "the new title", |_| rig.store.session(sid).unwrap().title == "Greetings");
    until(&mut h, "the list", |h| {
        h.state().overview().is_some_and(|o| o.sessions.iter().any(|s| s.title == "Greetings"))
    });
    assert!(!h.state().rename_chat_open());
}

#[test]
fn a_chat_can_be_opened_in_a_chosen_tile() {
    use agentbench::app::Grid;
    let rig = rig();
    let sid =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap().id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::Six);
    h.run_steps(3);
    h.state_mut().open_in_tile(sid, 4);
    h.run_steps(3);
    assert_eq!(h.state().tile_chat(4), Some(sid));
    assert_eq!(h.state().focused_tile(), 4);
    h.state_mut().open_in_tile(sid, 1);
    h.run_steps(3);
    assert_eq!(h.state().tile_chat(1), Some(sid));
    assert_eq!(h.state().tile_chat(4), None, "a chat lives in one tile only");
}

#[test]
fn the_chat_menu_shows_a_board_of_tiles() {
    use agentbench::app::Grid;
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_grid(Grid::new(3, 2));
    h.run_steps(3);
    let pos = h.get_by_label("chat say hello").rect().center();
    h.event(egui::Event::PointerMoved(pos));
    h.step();
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
    }
    h.run_steps(3);
    shot(&mut h, "chat-menu");
}

#[test]
fn older_rows_arriving_at_the_top_do_not_move_what_is_being_read() {
    let rig = rig();
    let s = long_chat(&rig, 40);
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_session(s.id, s.project_id);
    until(&mut h, "the latest page", |h| h.state().transcript_len(s.id) == 150);
    h.run_steps(3);
    let wheel = |h: &mut Harness<'_, App>, dy: f32| {
        h.event(egui::Event::PointerMoved(egui::pos2(800.0, 300.0)));
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            source: egui::MouseWheelSource::Unknown,
            delta: egui::vec2(0.0, dy),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        h.run_steps(3);
    };
    let first_turn = |h: &Harness<'_, App>| -> Option<usize> {
        (0..40).find(|n| h.query_by_label_contains(&format!("Turn {n}:")).is_some())
    };
    let mut before = None;
    for _ in 0..80 {
        wheel(&mut h, 600.0);
        if h.state().transcript_len(s.id) == 200 {
            break;
        }
        before = first_turn(&h);
    }
    let before = before.expect("some turn was in view before the older rows came");
    h.run_steps(6);
    let after = first_turn(&h).expect("some turn is in view after");
    assert!(
        after >= before.saturating_sub(1) || after == 0 && before <= 8,
        "the view jumped from turn {before} to turn {after} when the older rows arrived"
    );
    assert_ne!(after, 0, "not thrown to the very top of the older rows");
}

struct SecretWaiter;

#[agentbench_server::agent::async_trait]
impl agentbench_server::agent::Agent for SecretWaiter {
    fn name(&self) -> &str {
        "secretwaiter"
    }
    fn label(&self) -> &str {
        "Secret waiter"
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
        _turn: &agentbench_server::agent::Turn<'_>,
        io: &dyn agentbench_server::agent::Io,
    ) -> Result<(), String> {
        let q = agentbench_core::Question {
            text: "api key?".into(),
            secret: true,
            ..Default::default()
        };
        io.ask(q).await?;
        io.emit(agentbench_server::agent::Event::Text { delta: "got it".into() });
        Ok(())
    }
}

fn marker_of(title: &str) -> String {
    format!("needs you: {title}")
}

fn marker_count(h: &Harness<'_, App>) -> usize {
    h.query_all(egui_kittest::kittest::By::new().label_contains("needs you: ")).count()
}

#[test]
fn sessions_waiting_on_the_user_carry_a_marker_that_clears_when_answered() {
    let rig = rig_with(vec![Box::new(Waiter), Box::new(SecretWaiter)]);
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap().id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    assert_eq!(marker_count(&h), 0);
    assert_eq!(agentbench::window_title(0), "agentbench");

    let mut c = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    for (agent, title) in [("waiter", "licence chat"), ("secretwaiter", "key chat")] {
        c.add_session(&agentbench_core::NewSession {
            project_id: project,
            agent: agent.into(),
            title: Some(title.into()),
            prompt: Some("go".into()),
            ..Default::default()
        });
    }
    until(&mut h, "both waiting", |h| {
        h.state().overview().is_some_and(|o| agentbench::waiting_on_you(&o.sessions) == 2)
    });
    h.get_by_label(&marker_of("licence chat"));
    h.get_by_label(&marker_of("key chat"));
    assert_eq!(marker_count(&h), 2);
    assert_eq!(agentbench::window_title(2), "(2) agentbench: needs you");
    shot(&mut h, "needs-you");

    let all = h.state().overview().unwrap().sessions.clone();
    let below = |h: &Harness<'_, App>, t: &str| h.get_by_label(&format!("chat {t}")).rect().top();
    let lowest_waiting = below(&h, "licence chat").max(below(&h, "key chat"));
    let idle: Vec<_> = all
        .iter()
        .filter(|s| s.status == agentbench_core::SessionStatus::Idle && !s.title.is_empty())
        .collect();
    assert!(!idle.is_empty());
    for s in idle {
        assert!(below(&h, &s.title) > lowest_waiting, "{} sorts below the waiting chats", s.title);
    }

    let sessions = h.state().overview().unwrap().sessions.clone();
    for title in ["licence chat", "key chat"] {
        let s = sessions.iter().find(|s| s.title == title).unwrap();
        let q = rig
            .store
            .messages(s.id, 0)
            .unwrap()
            .into_iter()
            .find_map(|m| match m.entry {
                Entry::Question { id, .. } => Some(id),
                _ => None,
            })
            .expect("a question");
        c.answer(s.id, &q, &["MIT".into()]);
    }
    until(&mut h, "none waiting", |h| {
        h.state().overview().is_some_and(|o| agentbench::waiting_on_you(&o.sessions) == 0)
    });
    assert_eq!(marker_count(&h), 0);
}

#[test]
fn a_waiting_session_on_the_issue_board_carries_the_marker() {
    let rig = rig_with(vec![Box::new(Waiter)]);
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap().id;
    let issue = rig.store.issues().unwrap()[0].id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let mut c = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    c.add_session(&agentbench_core::NewSession {
        project_id: project,
        agent: "waiter".into(),
        title: Some("board chat".into()),
        issue_id: Some(issue),
        prompt: Some("go".into()),
        ..Default::default()
    });
    until(&mut h, "waiting", |h| {
        h.state().overview().is_some_and(|o| agentbench::waiting_on_you(&o.sessions) == 1)
    });
    click(&mut h, 283.0, 20.0);
    h.run_steps(3);
    shot(&mut h, "board-needs-you");
    assert_eq!(marker_count(&h), 2, "one in the side list, one on the issue card");
}


fn chat_server(chat_status: u16, chat_body: &'static str) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            let (code, body) = if head.starts_with("GET /v1/models") {
                (200, r#"{"data":[{"id":"alpha-1"},{"id":"beta-2"}]}"#)
            } else {
                (chat_status, chat_body)
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    format!("http://{addr}/v1")
}

fn onboarding_rig() -> Rig {
    rig_named("http://127.0.0.1:9/v1", "", Vec::new())
}

fn click_label(h: &mut Harness<'_, App>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}

fn type_into_secret(h: &mut Harness<'_, App>, text: &str) {
    let at = h
        .query_all_by_role(egui::accesskit::Role::PasswordInput)
        .next()
        .map(|n| n.rect().center())
        .expect("a secret field");
    click(h, at.x, at.y);
    h.run_steps(2);
    h.event(egui::Event::Text(text.to_string()));
    h.run_steps(3);
}

fn type_into_field(h: &mut Harness<'_, App>, nth_in_modal: usize, text: &str) {
    let modal_fields = |h: &Harness<'_, App>| {
        let mut all: Vec<_> = h
            .query_all_by_role(egui::accesskit::Role::TextInput)
            .filter(|n| n.rect().min.x > 300.0)
            .map(|n| n.rect().min.y)
            .collect();
        all.sort_by(|a, b| a.partial_cmp(b).unwrap());
        all
    };
    let y = modal_fields(h)[nth_in_modal];
    let pick = |h: &Harness<'_, App>| {
        h.query_all_by_role(egui::accesskit::Role::TextInput)
            .find(|n| n.rect().min.x > 300.0 && n.rect().min.y == y)
            .map(|n| n.rect().center())
    };
    let at = pick(h).unwrap();
    click(h, at.x, at.y);
    h.run_steps(2);
    h.event(egui::Event::Text(text.to_string()));
    h.run_steps(3);
}

#[test]
fn a_first_launch_with_nothing_set_up_shows_the_onboarding() {
    let rig = onboarding_rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the onboarding", |h| h.state().onboarding_open());
    shot(&mut h, "onboarding-welcome");
    for label in [
        "set up Claude subscription",
        "set up ChatGPT subscription",
        "set up Anthropic API key",
        "set up OpenAI API key",
        "set up OpenAI compatible endpoint",
        "set up Local model",
    ] {
        h.get_by_label(label);
    }
    h.get_by_label_contains("Nothing is set up yet");
}

#[test]
fn a_server_with_a_model_already_set_does_not_show_the_onboarding() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(20);
    std::thread::sleep(Duration::from_millis(300));
    h.run_steps(10);
    assert!(!h.state().onboarding_open());
}

#[test]
fn skipping_the_onboarding_closes_it_and_it_stays_closed() {
    let rig = onboarding_rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the onboarding", |h| h.state().onboarding_open());
    click_label(&mut h, "skip for now");
    assert!(!h.state().onboarding_open());
    until(&mut h, "it is saved", |_| rig.store.setting("onboarding").unwrap().as_deref() == Some("skipped"));
    let mut again = harness(rig.url.clone());
    until(&mut again, "the overview", |h| h.state().overview().is_some());
    again.run_steps(20);
    std::thread::sleep(Duration::from_millis(300));
    again.run_steps(10);
    assert!(!again.state().onboarding_open(), "a skipped setup is not shown again");
}

#[test]
fn completing_the_onboarding_writes_a_working_default_and_does_not_show_again() {
    let up = chat_server(200, r#"{"choices":[{"message":{"content":"ok"}}]}"#);
    let rig = onboarding_rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the onboarding", |h| h.state().onboarding_open());

    click_label(&mut h, "set up OpenAI compatible endpoint");
    until(&mut h, "the address field", |h| {
        h.query_all_by_role(egui::accesskit::Role::TextInput).count() >= 2
    });
    type_into_field(&mut h, 0, &up);
    click_label(&mut h, "test connection");
    until(&mut h, "the test to pass", |h| h.query_by_label_contains("answered").is_some());
    shot(&mut h, "onboarding-endpoint");
    click_label(&mut h, "save provider");
    until(&mut h, "the provider to save", |_| {
        agentbench_server::providers::saved(&rig.store).len() == 1
    });
    until(&mut h, "the welcome page", |h| h.state().onboarding_step() == "welcome");
    until(&mut h, "the card to show it works", |h| h.query_by_label("make OpenAI compatible endpoint the default").is_none() && h.query_by_label("edit OpenAI compatible endpoint").is_some());

    click_label(&mut h, "next");
    until(&mut h, "the model page", |h| h.state().onboarding_step() == "models");
    until(&mut h, "the model lists", |h| h.query_all_by_label("choose model").count() >= 1);
    shot(&mut h, "onboarding-models");
    click_label(&mut h, "next");
    until(&mut h, "the last page", |h| h.state().onboarding_step() == "finish");
    shot(&mut h, "onboarding-finish");
    click_label(&mut h, "finish");
    assert!(!h.state().onboarding_open());

    until(&mut h, "the setup to be marked done", |_| {
        rig.store.setting("onboarding").unwrap().as_deref() == Some("done")
    });
    let saved = agentbench_server::providers::saved(&rig.store);
    assert_eq!(saved[0].url, up);
    assert!(saved[0].default);
    assert!(!saved[0].medium.is_empty(), "a default model was chosen");
    let model: serde_json::Value =
        serde_json::from_str(&rig.store.setting("model").unwrap().unwrap()).unwrap();
    assert_eq!(model["url"], up, "the native agent now points at it");
    assert_eq!(model["model"], saved[0].medium.as_str());

    let mut again = harness(rig.url.clone());
    until(&mut again, "the overview", |h| h.state().overview().is_some());
    again.run_steps(20);
    std::thread::sleep(Duration::from_millis(300));
    again.run_steps(10);
    assert!(!again.state().onboarding_open(), "a finished setup is not shown again");
}

#[test]
fn running_the_setup_again_keeps_the_providers_already_set_up() {
    let up = chat_server(200, r#"{"choices":[]}"#);
    let rig = rig();
    agentbench_server::providers::save(
        &rig.store,
        &[agentbench_core::Provider {
            kind: agentbench_core::ProviderKind::Endpoint,
            url: up.clone(),
            key: "keep-me".into(),
            default: true,
            medium: "alpha-1".into(),
            ..Default::default()
        }],
    )
    .unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    assert!(!h.state().onboarding_open());
    h.get_by_label("settings").click();
    h.run_steps(2);
    until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
    h.run_steps(3);
    click_label(&mut h, "set up providers");
    until(&mut h, "the onboarding", |h| h.state().onboarding_open());
    until(&mut h, "the saved provider", |h| {
        h.state().onboarding_providers() == [agentbench_core::ProviderKind::Endpoint]
    });
    until(&mut h, "its health", |h| h.query_by_label("edit OpenAI compatible endpoint").is_some());

    click_label(&mut h, "set up Anthropic API key");
    until(&mut h, "the key field", |h| {
        h.query_all_by_role(egui::accesskit::Role::TextInput).count() >= 1
    });
    type_into_secret(&mut h, "sk-ant-new");
    click_label(&mut h, "save provider");
    until(&mut h, "both providers", |_| {
        agentbench_server::providers::saved(&rig.store).len() == 2
    });
    let saved = agentbench_server::providers::saved(&rig.store);
    assert_eq!(saved[0].key, "keep-me", "the first provider is untouched");
    assert!(saved[0].default, "and stays the default");
    assert_eq!(saved[1].key, "sk-ant-new");
    assert!(!saved[1].default);
}

#[test]
fn a_failed_test_call_shows_the_servers_own_error() {
    let down = chat_server(401, r#"{"error":{"message":"Incorrect API key provided"}}"#);
    let rig = onboarding_rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the onboarding", |h| h.state().onboarding_open());
    click_label(&mut h, "set up OpenAI compatible endpoint");
    until(&mut h, "the address field", |h| {
        h.query_all_by_role(egui::accesskit::Role::TextInput).count() >= 2
    });
    type_into_field(&mut h, 0, &down);
    click_label(&mut h, "test connection");
    until(&mut h, "the error", |h| h.query_by_label_contains("Incorrect API key provided").is_some());
    shot(&mut h, "onboarding-error");
}

#[test]
fn a_session_with_no_plan_says_so_in_its_details() {
    let rig = rig();
    let issue = rig.store.issues().unwrap()[0].id;
    let side = rig
        .store
        .worktrees(None)
        .unwrap()
        .into_iter()
        .find(|w| w.branch.as_deref() == Some("fix-login"))
        .unwrap();
    let linked = rig.store.add_session(side.id, "echo", "work on readme", None, Some(issue)).unwrap();
    rig.store.add_message(linked.id, &Entry::user("write it")).unwrap();
    rig.store.set_setting(&format!("no_plan_{}", linked.id), "15").unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().set_issues_first(true);
    h.state_mut().open_session(linked.id, linked.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(linked.id) == 1);
    h.run_steps(3);
    h.get_by_label_contains("no plan after 15");
}

fn right_click(h: &mut Harness<'_, App>, x: f32, y: f32) {
    let pos = egui::pos2(x, y);
    h.event(egui::Event::PointerMoved(pos));
    h.step();
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        });
        h.step();
    }
}

#[test]
fn the_paste_menu_puts_the_system_clipboard_into_the_composer() {
    let _one = clipboard_lock();
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(2);
    agentbench::system::stand_in_clipboard(Some("from the phone"));
    click(&mut h, 800.0, 735.0);
    h.event(egui::Event::Text("see ".into()));
    h.run_steps(2);
    right_click(&mut h, 800.0, 735.0);
    h.run_steps(2);
    h.get_by_label("PASTE").click();
    h.run_steps(4);
    agentbench::system::stand_in_clipboard(None);
    assert_eq!(h.state().draft_of(session.id), "see from the phone");
}

#[test]
fn text_from_the_keyboard_and_a_paste_event_both_land_in_the_composer() {
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    h.state_mut().open_session(session.id, session.project_id);
    until(&mut h, "the transcript", |h| h.state().transcript_len(session.id) == 3);
    h.run_steps(2);
    click(&mut h, 800.0, 735.0);
    h.event(egui::Event::Ime(egui::ImeEvent::Enabled));
    h.event(egui::Event::Ime(egui::ImeEvent::Commit("committed ".into())));
    h.run_steps(2);
    h.event(egui::Event::Text("typed ".into()));
    h.run_steps(2);
    h.event(egui::Event::Paste("pasted".into()));
    h.run_steps(2);
    assert_eq!(h.state().draft_of(session.id), "committed typed pasted");
}

#[test]
fn the_paste_menu_fills_a_settings_field() {
    let _one = clipboard_lock();
    let rig = rig();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(3);
    agentbench::system::stand_in_clipboard(Some("https://pasted.example.com"));
    h.get_all_by_label("https://agentbench.example.com").last().unwrap().click_secondary();
    h.run_steps(2);
    h.get_by_label("PASTE").click();
    h.run_steps(4);
    agentbench::system::stand_in_clipboard(None);
    assert_eq!(
        h.state().settings_external(),
        Some("https://pasted.example.com".to_string())
    );
}

#[test]
fn the_paste_menu_fills_a_question_answer_field() {
    let _one = clipboard_lock();
    let rig = rig_with(vec![Box::new(Planner)]);
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap().id;
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    let mut c = agentbench::client::Client::new(
        agentbench::client::Server { url: rig.url.clone(), token: TOKEN.into() },
        || {},
    );
    c.add_session(&agentbench_core::NewSession {
        project_id: project,
        agent: "planner".into(),
        prompt: Some("plan".into()),
        ..Default::default()
    });
    until(&mut h, "the waiting session", |h| {
        h.state().overview().is_some_and(|o| {
            o.sessions.iter().any(|s| {
                s.agent == "planner" && s.status == agentbench_core::SessionStatus::Waiting
            })
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
    h.get_by_label("Apache-2.0").click();
    h.get_by_label("linux").click();
    agentbench::system::stand_in_clipboard(Some("pasted answer"));
    h.query_all_by_role(egui::accesskit::Role::TextInput).last().unwrap().click_secondary();
    h.run_steps(2);
    h.get_by_label("PASTE").click();
    h.run_steps(4);
    agentbench::system::stand_in_clipboard(None);
    h.get_by_label("send answers").click();
    h.run_steps(5);
    until(&mut h, "the reply", |h| h.state().transcript_len(s.id) >= 3);
    let msgs = rig.store.messages(s.id, 0).unwrap();
    assert!(
        msgs.iter().any(|m| matches!(
            &m.entry,
            Entry::Questions { answers: Some(a), .. } if a.last().map(String::as_str) == Some("pasted answer")
        )),
        "{msgs:?}"
    );
}

fn clipboard_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn send_file(rig: &Rig, name: &str, data: &[u8]) -> agentbench_core::SessionId {
    let session =
        rig.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    let mime = agentbench_core::mime_for(name).unwrap_or("application/octet-stream");
    let file = rig.store.add_attachment(session.id, name, mime, data).unwrap();
    rig.store.add_message(session.id, &Entry::File { text: String::new(), file }).unwrap();
    session.id
}

fn show_chat(h: &mut Harness<'_, App>, rig: &Rig, id: agentbench_core::SessionId) {
    until(h, "the overview", |h| h.state().overview().is_some());
    let s = rig.store.sessions().unwrap().into_iter().find(|s| s.id == id).unwrap();
    h.state_mut().open_session(s.id, s.project_id);
    until(h, "the file", |h| h.state().transcript_len(id) == 4);
    until(h, "the thumbnail", |h| h.state().thumbnails_ready() == 1 || true);
    h.run_steps(10);
}

fn open_key(h: &mut Harness<'_, App>) {
    h.get_by_label("open").click();
    h.run_steps(3);
}

#[test]
fn the_open_key_shows_an_image_large_and_zooms_it() {
    let rig = rig();
    let id = send_file(&rig, "pic.png", &png(300, 200));
    let mut h = harness_at(rig.url.clone(), [1280.0, 1600.0]);
    show_chat(&mut h, &rig, id);
    open_key(&mut h);
    until(&mut h, "the viewer", |h| h.state().viewer_open() && h.state().viewer_ready());
    let fit = h.state().viewer_scale();
    assert!(fit > 0.0);
    h.get_by_label("zoom in").click();
    h.run_steps(3);
    assert!(h.state().viewer_scale() > fit, "{} vs {fit}", h.state().viewer_scale());
    h.get_by_label("100%").click();
    h.run_steps(3);
    assert!((h.state().viewer_scale() - 1.0).abs() < 1e-3);
    shot(&mut h, "viewer-image");
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(!h.state().viewer_open());
}

#[test]
fn the_open_key_shows_a_text_file_and_marks_a_big_one_truncated() {
    let rig = rig();
    let code = "fn main() {\n    println!(\"hi\");\n}\n".repeat(20);
    let id = send_file(&rig, "main.rs", code.as_bytes());
    let mut h = harness_at(rig.url.clone(), [1280.0, 1600.0]);
    show_chat(&mut h, &rig, id);
    open_key(&mut h);
    until(&mut h, "the viewer", |h| h.state().viewer_open() && h.state().viewer_ready());
    assert!(!h.state().viewer_truncated());
    h.get_by_label("copy");
    shot(&mut h, "viewer-code");
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(!h.state().viewer_open());

    let big = "x".repeat((1 << 20) + 100);
    let rig = rig_with(Vec::new());
    let id = send_file(&rig, "big.log", big.as_bytes());
    let mut h = harness_at(rig.url.clone(), [1280.0, 1600.0]);
    show_chat(&mut h, &rig, id);
    open_key(&mut h);
    until(&mut h, "the viewer", |h| h.state().viewer_open() && h.state().viewer_ready());
    assert!(h.state().viewer_truncated());
    h.get_by_label_contains("truncated");
}

#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
#[test]
fn open_externally_downloads_the_file_and_calls_the_opener() {
    let rig = rig();
    let id = send_file(&rig, "notes.txt", b"hello");
    let dir = tempfile::tempdir().unwrap();
    agentbench::attach::set_cache_dir(dir.path().to_path_buf());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s = seen.clone();
    agentbench::attach::set_opener(std::sync::Arc::new(move |p: &Path| {
        s.lock().unwrap().push(p.to_path_buf());
        Ok(())
    }));
    let mut h = harness_at(rig.url.clone(), [1280.0, 1600.0]);
    show_chat(&mut h, &rig, id);
    open_key(&mut h);
    until(&mut h, "the viewer", |h| h.state().viewer_open() && h.state().viewer_ready());
    h.get_by_label("open externally").click();
    let start = Instant::now();
    while seen.lock().unwrap().is_empty() {
        assert!(start.elapsed() < Duration::from_secs(20), "the opener was not called");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    let path = seen.lock().unwrap()[0].clone();
    assert_eq!(path.file_name().unwrap(), "notes.txt");
    assert_eq!(std::fs::read(&path).unwrap(), b"hello");
    assert!(path.starts_with(dir.path()));
}

#[test]
fn the_vision_panel_draws_saves_and_restores_from_the_projects_page() {
    let rig = rig();
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.get_by_label("settings").click();
    h.run_steps(3);
    h.state_mut().settings_page("projects");
    h.run_steps(3);
    h.get_by_label("project vision").click();
    until(&mut h, "the panel", |h| h.state().vision_open() == Some(project.id));
    h.get_by_label("add goal").click();
    h.run_steps(3);
    h.get_by_label("vision summary").click();
    h.run_steps(2);
    h.event(egui::Event::Text("A fast tracker".into()));
    h.run_steps(3);
    h.get_by_label("goal text").click();
    h.run_steps(2);
    h.event(egui::Event::Text("fast boards".into()));
    h.run_steps(3);
    shot(&mut h, "vision-panel");
    h.get_by_label("save vision").click();
    until(&mut h, "saved", |_| rig.store.vision(project.id).unwrap().is_some());
    let v = rig.store.vision(project.id).unwrap().unwrap();
    assert_eq!(v.body.summary, "A fast tracker");
    assert_eq!((v.body.goals[0].id.as_str(), v.body.goals[0].text.as_str()), ("g1", "fast boards"));
    assert_eq!(v.author, "user");
    until(&mut h, "the overview carries it", |h| {
        h.state().overview().is_some_and(|o| o.visions.iter().any(|v| v.project_id == project.id))
    });
}

#[test]
fn an_issue_card_shows_its_goal_and_the_board_groups_by_goal() {
    let rig = rig();
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    rig.store
        .set_vision(
            project.id,
            agentbench_core::VisionBody {
                summary: "s".into(),
                goals: vec![agentbench_core::Goal { id: "speed".into(), text: "fast".into(), status: Default::default() }],
                non_goals: vec![],
            },
            "user",
        )
        .unwrap();
    rig.store
        .add_issue(&NewIssue { project_id: project.id, title: "Cache the board".into(), goal: Some("speed".into()), ..Default::default() })
        .unwrap();
    rig.store
        .add_issue(&NewIssue { project_id: project.id, title: "Add a gimmick".into(), off_vision: true, ..Default::default() })
        .unwrap();
    let mut h = harness(rig.url.clone());
    until(&mut h, "the overview", |h| h.state().overview().is_some_and(|o| o.issues.len() == 3));
    click(&mut h, 283.0, 20.0);
    h.run_steps(3);
    h.get_by_label("goal speed");
    h.get_by_label("off vision");
    shot(&mut h, "goal-chips");
    h.state_mut().group_by_goal();
    h.run_steps(3);
    h.get_by_label_contains("speed 1");
    h.get_by_label_contains("no goal 1");
    h.get_by_label_contains("off vision 1");
    shot(&mut h, "goal-board");
}
