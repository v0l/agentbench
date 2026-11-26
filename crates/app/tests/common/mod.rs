#![allow(dead_code)]

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

pub const TOKEN: &str = "test-token";

pub struct Rig {
    pub url: String,
    pub store: Store,
    pub _dir: tempfile::TempDir,
    pub _rt: tokio::runtime::Runtime,
}

pub fn repo(dir: &Path) {
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

pub fn worktree(main: &Path, branch: &str) {
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

pub fn rig() -> Rig {
    rig_with(Vec::new())
}

pub fn rig_with(extra: Vec<Box<dyn agentbench_server::agent::Agent>>) -> Rig {
    rig_on("http://127.0.0.1:9/v1", extra)
}

pub fn rig_with_models(
    extra: Vec<Box<dyn agentbench_server::agent::Agent>>,
    models: PathBuf,
) -> Rig {
    rig_model_full(
        agentbench_server::agent::native::Model {
            url: "http://127.0.0.1:9/v1".into(),
            model: "test-model".into(),
            ..Default::default()
        },
        extra,
        Some(models),
    )
}

/// A model server that lists `n` models, for as many requests as come.
pub fn model_server(n: usize) -> String {
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

pub fn rig_on(model_url: &str, extra: Vec<Box<dyn agentbench_server::agent::Agent>>) -> Rig {
    rig_named(model_url, "test-model", extra)
}

pub fn rig_named(
    model_url: &str,
    model_name: &str,
    extra: Vec<Box<dyn agentbench_server::agent::Agent>>,
) -> Rig {
    let model = agentbench_server::agent::native::Model {
        url: model_url.to_string(),
        model: model_name.to_string(),
        ..Default::default()
    };
    rig_model(model, extra)
}

pub fn rig_model(
    model: agentbench_server::agent::native::Model,
    extra: Vec<Box<dyn agentbench_server::agent::Agent>>,
) -> Rig {
    rig_model_full(model, extra, None)
}

fn rig_model_full(
    model: agentbench_server::agent::native::Model,
    extra: Vec<Box<dyn agentbench_server::agent::Agent>>,
    models: Option<PathBuf>,
) -> Rig {
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
    let rt = tokio::runtime::Runtime::new().unwrap();
    let models = models.unwrap_or_else(|| dir.path().join("models"));
    let mut extra = Some(extra);
    let mut echo = Some(echo);
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let addr = listener.local_addr().unwrap();
    {
        let s = store.clone();
        let model = model.clone();
        let (extra, echo) = (extra.take().unwrap(), echo.take().unwrap());
        rt.spawn(async move {
            let config = agentbench_server::Config {
                model,
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

fn hold_gpu_lock() {
    if std::env::var_os("AGENTBENCH_UI_LOCK_INHERITED").is_some() {
        return;
    }
    static LOCK: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| {
        let path = std::env::temp_dir().join("agentbench-ui-tests.lock");
        let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).expect("open gpu lock");
        file.lock().expect("take gpu lock");
        file
    });
}

pub fn share_one_device() {
    SHARE_ONE_DEVICE.store(true, std::sync::atomic::Ordering::SeqCst);
}

static SHARE_ONE_DEVICE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn gpu_setup() -> eframe::egui_wgpu::WgpuSetup {
    if SHARE_ONE_DEVICE.load(std::sync::atomic::Ordering::SeqCst) {
        return shared_gpu_setup();
    }
    hold_gpu_lock();
    let mut setup = egui_kittest::wgpu::default_wgpu_setup();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(new) = &mut setup {
        new.instance_descriptor.flags = eframe::egui_wgpu::wgpu::InstanceFlags::empty();
    }
    setup
}

fn shared_gpu_setup() -> eframe::egui_wgpu::WgpuSetup {
    hold_gpu_lock();
    static SHARED: std::sync::OnceLock<eframe::egui_wgpu::WgpuSetupExisting> = std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| {
            let mut setup = egui_kittest::wgpu::default_wgpu_setup();
            if let eframe::egui_wgpu::WgpuSetup::CreateNew(new) = &mut setup {
                new.instance_descriptor.flags = eframe::egui_wgpu::wgpu::InstanceFlags::empty();
            }
            let state = egui_kittest::wgpu::create_render_state(setup, eframe::egui_wgpu::RendererOptions::PREDICTABLE);
            eframe::egui_wgpu::WgpuSetupExisting {
                instance: state.instance.clone(),
                adapter: state.adapter.clone(),
                device: state.device.clone(),
                queue: state.queue.clone(),
            }
        })
        .clone()
        .into()
}

pub fn build_lock() -> std::sync::MutexGuard<'static, ()> {
    static BUILD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    BUILD.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn harness(url: String) -> Harness<'static, App> {
    harness_at(url, [1280.0, 800.0])
}

pub fn harness_at(url: String, size: [f32; 2]) -> Harness<'static, App> {
    harness_cfg(vec![("url".into(), url)], Cfg { size, ..Cfg::default() })
}

#[derive(Clone, Copy, Debug)]
pub struct Cfg {
    pub size: [f32; 2],
    pub scale: f32,
    pub insets: [f32; 4],
    pub ppp: f32,
}

impl Default for Cfg {
    fn default() -> Self {
        Cfg { size: [1280.0, 800.0], scale: 1.0, insets: [0.0; 4], ppp: 1.0 }
    }
}

pub fn harness_cfg(storage: Vec<(String, String)>, cfg: Cfg) -> Harness<'static, App> {
    let _build = build_lock();
    let mut h = Harness::builder().with_size(cfg.size).with_pixels_per_point(cfg.ppp).wgpu_setup(gpu_setup()).build_eframe(move |cc| {
        let mut mem = Mem::default();
        mem.0.insert("token".into(), TOKEN.into());
        mem.0.insert("font_size".into(), (agentbench::fonts::BASE * cfg.scale).to_string());
        for (k, v) in storage {
            mem.0.insert(k, v);
        }
        App::new(&cc.egui_ctx, Some(&mem), None)
    });
    let [left, top, right, bottom] = cfg.insets;
    if cfg.insets != [0.0; 4] {
        h.input_mut().safe_area_insets =
            Some(egui::SafeAreaInsets(egui::epaint::MarginF32 { left, right, top, bottom }));
    }
    h
}

#[derive(Default)]
pub struct Mem(pub std::collections::HashMap<String, String>);

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

pub fn until(h: &mut Harness<'_, App>, what: &str, ok: impl Fn(&Harness<'_, App>) -> bool) {
    let start = Instant::now();
    while !ok(h) {
        assert!(start.elapsed() < Duration::from_secs(60), "timed out waiting for {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(30));
    }
    h.run_steps(3);
}

pub fn until_label(h: &mut Harness<'_, App>, label: &str) {
    until(h, label, |h| h.query_by_label(label).is_some());
}

pub fn click_until(h: &mut Harness<'_, App>, label: &str, what: &str, ok: impl Fn() -> bool) {
    for _ in 0..8 {
        until_label(h, label);
        h.get_by_label(label).click();
        for _ in 0..20 {
            h.step();
            if ok() {
                h.run_steps(3);
                return;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    panic!("clicking {label} never changed {what}");
}

pub fn open_home_row(h: &mut Harness<'_, App>) {
    h.state_mut().blank_chat_in_home();
    h.run_steps(3);
}

pub fn shot(h: &mut Harness<'_, App>, name: &str) {
    if let Ok(dir) = std::env::var("AGENTBENCH_SHOTS") {
        let img = h.render().expect("rendered");
        img.save(Path::new(&dir).join(format!("{name}.png"))).unwrap();
    }
}


pub fn click(h: &mut Harness<'_, App>, x: f32, y: f32) {
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


pub fn two_quota_server(used_pct: u32, resets_in_secs: i64) -> String {
    let resets = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + resets_in_secs;
    let entry = format!(r#"{{"summary":{{"used_pct":{used_pct},"window":"7d","resets_at":{resets},"exhausted":false}}}}"#);
    let body = format!(r#"{{"models":{{"code-high":{entry},"code-x-high":{entry}}}}}"#);
    serve_json(body)
}

pub fn quota_server(used_pct: u32, resets_in_secs: i64) -> String {
    let resets = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + resets_in_secs;
    serve_json(format!(r#"{{"window":"5h","used_pct":{used_pct},"resets_at":{resets}}}"#))
}

fn serve_json(body: String) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
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


pub fn canned_server(overview: String) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let overview = overview.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let (code, body) = if head.starts_with("GET /api/overview") {
                    ("200 OK", overview)
                } else if head.starts_with("GET /api/changes") {
                    std::thread::sleep(Duration::from_secs(5));
                    ("200 OK", r#"{"cursor":1,"changes":[]}"#.to_string())
                } else {
                    ("404 Not Found", r#"{"error":"no such route"}"#.to_string())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {code}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    addr
}


pub fn recorded_overview() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/api/v1/overview.json");
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}


pub fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([40, 160, 220, 255]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// A long chat: markdown, code blocks, tables, tool calls with long output and thinking.
pub fn long_chat(rig: &Rig, turns: usize) -> agentbench_core::Session {
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


pub fn lsp(
    name: &str,
    dir: &str,
    health: agentbench_core::LspHealth,
    detail: &str,
) -> agentbench_core::Lsp {
    agentbench_core::Lsp { name: name.into(), dir: dir.into(), health, detail: detail.into() }
}

/// Give the "say hello" chat a model, a context and three language servers in different states.
pub fn give_usage(rig: &Rig) -> agentbench_core::Session {
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


pub fn click_settled(h: &mut Harness<'_, App>, label: &str) {
    let start = Instant::now();
    let mut last = None;
    let mut still = 0;
    while still < 5 {
        assert!(start.elapsed() < Duration::from_secs(60), "{label} never held still");
        h.step();
        std::thread::sleep(Duration::from_millis(20));
        let now = h.query_by_label(label).map(|n| n.rect());
        if now.is_some() && now == last {
            still += 1;
        } else {
            still = 0;
        }
        last = now;
    }
    click_label(h, label);
}

pub fn click_label(h: &mut Harness<'_, App>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(3);
}


pub fn add_project_request(rig: &Rig, dir: &std::path::Path) -> agentbench_core::Id {
    let request = agentbench_core::AddProject {
        mode: agentbench_core::AddMode::Import,
        path: dir.to_string_lossy().into_owned(),
        name: "newcomer".into(),
        remote: Some("https://github.com/v0l/newcomer".into()),
        branch: Some("main".into()),
        head: Some("abcdef1234".into()),
        free_bytes: None,
        requester: None,
    };
    let proposal = agentbench_core::Proposal {
        id: 0,
        kind: agentbench_core::ProposalKind::AddProject,
        summary: "add newcomer".into(),
        issues: vec![],
        sessions: vec![],
        projects: vec![],
        changes: vec![agentbench_core::Change::AddProject { request }],
        state: agentbench_core::ProposalState::Pending,
        created_at: 0,
    };
    rig.store.add_proposal("add_project:test", &proposal).unwrap()
}


pub fn crowd_the_supervisor(rig: &Rig) {
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


#[derive(Debug)]
pub struct Dropped {
    pub path: std::path::PathBuf,
    pub bytes: Vec<u8>,
}


impl egui::DroppedFile for Dropped {
    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn bytes(&self) -> Result<Vec<u8>, String> {
        Ok(self.bytes.clone())
    }
}


pub fn drop_file(h: &mut Harness<'_, App>, name: &str, bytes: Vec<u8>) {
    h.input_mut().dropped_files.push(std::sync::Arc::new(Dropped { path: name.into(), bytes }));
    h.step();
}

pub const LONG_TITLES: [&str; 8] = [
    "Merge JSON mode for the structured output path of the native agent, with the grammar cache",
    "payment-gateway sandbox credentials rotate every night and the integration tests keep failing",
    "Windows test pass for the installer, the updater and the service wrapper on a clean machine",
    "Perf: close the gaps in the qwen3-moe and qwen3-next gguf decode path on the Spark",
    "Keep the directory as data, not as code, so the fleet can be described in one file",
    "Preempt by swapping KV caches between the two Spark nodes during decode under load",
    "Review the migration plan for the ledger and its rollback before the Friday deploy window",
    "Fix on main: the retry storm that happens after a deploy of the api gateway to production",
];

pub const LONG_BRANCHES: [&str; 3] = [
    "issue-817-perf-pass-gaps-qwen3-moe-qwen3-next-gguf",
    "issue-402-something-extremely-long-without-any-break-at-all-so-it-cannot-wrap",
    "payment-gateway-sandbox-master-merge-candidate-for-release-v0-9-2",
];

pub fn real_world_rig() -> Rig {
    use agentbench_core::SessionStatus;
    let model = agentbench_server::agent::native::Model {
        url: two_quota_server(64, 7 * 86_400 - 3600),
        model: "code-high".into(),
        high: agentbench_server::agent::native::Level {
            model: "code-x-high".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let rig = rig_model(model, Vec::new());
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    let main = rig.store.main_worktree(project.id).unwrap();
    for branch in LONG_BRANCHES {
        worktree_row(&rig, project.id, branch);
    }
    let trees = rig.store.worktrees(Some(project.id)).unwrap();
    let parent = rig.store.add_session(main.id, "native", "Merge JSON mode, working, 14 children", None, None).unwrap();
    rig.store
        .add_usage(parent.id, &agentbench_core::Usage { cost_usd: 947.94, turns: 1, ..Default::default() })
        .unwrap();
    for i in 0..25 {
        let tree = &trees[i % trees.len()];
        let s = rig.store.add_session(tree.id, "native", LONG_TITLES[i % LONG_TITLES.len()], None, None).unwrap();
        rig.store
            .set_status(s.id, if i < 9 { SessionStatus::Running } else { SessionStatus::Idle })
            .unwrap();
        if i < 14 {
            rig.store.set_parent(s.id, Some(parent.id)).unwrap();
        }
    }
    rig.store.set_status(parent.id, SessionStatus::Running).unwrap();
    rig
}

fn worktree_row(rig: &Rig, project: agentbench_core::Id, branch: &str) {
    let main = rig.store.worktrees(Some(project)).unwrap().into_iter().find(|w| w.main).unwrap();
    let dir = std::path::PathBuf::from(&main.path);
    let target = dir.parent().unwrap().join(format!("demo-{}", &branch[..12.min(branch.len())]));
    let ok = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["worktree", "add", "-q", "-b", branch])
        .arg(&target)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    agentbench_server::projects::sync(&rig.store, project, None).unwrap();
}
