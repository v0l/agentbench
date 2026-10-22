use egui_kittest::Harness;
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
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("demo");
    repo(&main);
    let store = Store::open(&dir.path().join("f.db")).unwrap();
    store.set_setting("token", TOKEN).unwrap();
    worktree(&main, "request-log");
    worktree(&main, "fix-login");
    let project = foreman_server::projects::add(&store, &main, None).unwrap();
    store
        .add_issue(&NewIssue {
            project_id: project.id,
            title: "Write a README".into(),
            ..Default::default()
        })
        .unwrap();
    let main_tree = store.main_worktree(project.id).unwrap();
    let session = store.add_session(main_tree.id, "echo", "say hello", None, None).unwrap();
    store.add_message(session.id, &Entry::User { text: "say hello".into() }).unwrap();
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
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let s = store.clone();
    rt.spawn(async move {
        foreman_server::serve(s, vec![Box::new(echo)], addr, std::future::pending()).await.unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    Rig { url: addr.to_string(), store, _dir: dir, _rt: rt }
}

fn harness(url: String) -> Harness<'static, App> {
    Harness::builder().with_size([1280.0, 800.0]).wgpu().build_eframe(move |cc| {
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
    assert_eq!(msgs[3].entry, Entry::User { text: "ping".into() });
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
fn without_a_local_server_the_window_asks_where_to_connect() {
    let h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_eframe(|cc| App::new(&cc.egui_ctx, None, None));
    assert!(!h.state().is_local());
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
