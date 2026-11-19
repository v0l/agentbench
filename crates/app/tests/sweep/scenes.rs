use crate::common::*;
use agentbench::App;
use agentbench_core::{Entry, Id, NewIssue, SessionId, SessionStatus};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::sync::OnceLock;

pub struct Rich {
    pub rig: Rig,
    pub calm: Rig,
    pub project: Id,
    pub chat: SessionId,
    pub question: SessionId,
    pub faulty: SessionId,
    pub files: SessionId,
    pub supervisor: SessionId,
    pub deep_issue: Id,
}

pub struct Built {
    pub h: Harness<'static, App>,
    pub _keep: Vec<Box<dyn std::any::Any>>,
}

pub struct Scene {
    pub id: u32,
    pub name: &'static str,
    pub build: fn(&Rich, Cfg) -> Built,
}

pub fn rich() -> &'static Rich {
    static RICH: OnceLock<Rich> = OnceLock::new();
    RICH.get_or_init(fixture)
}

const TITLES: [&str; 20] = [
    "Preempt by swapping KV caches between the two Spark nodes during decode",
    "JSON mode for the structured output path",
    "issue-402-something-extremely-long-without-any-break-at-all-so-it-cannot-wrap",
    "Fix the login loop",
    "bench / spark-gates",
    "Review the migration plan for the ledger",
    "Tests on the Spark",
    "Write a README",
    "Swap the allocator",
    "Why does the retry storm happen after a deploy of the api",
    "master",
    "Same 2h ago",
    LONG_TITLES[0],
    LONG_TITLES[1],
    LONG_TITLES[2],
    LONG_TITLES[3],
    LONG_TITLES[4],
    LONG_TITLES[5],
    LONG_TITLES[6],
    LONG_TITLES[7],
];

fn fixture() -> Rich {
    let calm = rig();
    let rig = rig_on(&quota_server(83, 7800), Vec::new());
    let project = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    let trees = rig.store.worktrees(Some(project.id)).unwrap();
    rig.store
        .set_vision(
            project.id,
            agentbench_core::VisionBody {
                summary: "A fast tracker that agents and people share".into(),
                goals: vec![agentbench_core::Goal {
                    id: "speed".into(),
                    text: "fast boards on a phone".into(),
                    status: Default::default(),
                }],
                non_goals: vec!["a hosted service".into()],
            },
            "user",
        )
        .unwrap();
    let issues: Vec<Id> = (0..10)
        .map(|i| {
            rig.store
                .add_issue(&NewIssue {
                    project_id: project.id,
                    title: format!(
                        "{} {}",
                        ["Allocator", "Retry storm", "Ledger rows", "Quota chip"][i % 4],
                        TITLES[(i * 5) % TITLES.len()]
                    ),
                    body: "The service drops every second request under load.\n\n- reproduce\n- fix\n- test".into(),
                    state: Some(agentbench_core::IssueState::ALL[i % 5]),
                    labels: vec!["bug".into(), "mobile".into(), "a-long-label-name".into()][..1 + i % 3].to_vec(),
                    goal: (i % 3 == 0).then(|| "speed".into()),
                    ..Default::default()
                })
                .unwrap()
                .id
        })
        .collect();
    let deep_issue = rig
        .store
        .add_issue(&NewIssue {
            project_id: project.id,
            title: "Build the roof once the base and the walls are done".into(),
            depends_on: vec![issues[0], issues[1]],
            ..Default::default()
        })
        .unwrap()
        .id;
    let statuses = [
        SessionStatus::Running,
        SessionStatus::Idle,
        SessionStatus::Waiting,
        SessionStatus::Failed,
        SessionStatus::Stopped,
    ];
    let mut first = None;
    for i in 0..30 {
        let tree = &trees[i % trees.len()];
        let s = rig
            .store
            .add_session(
                tree.id,
                if i % 2 == 0 { "native" } else { "echo" },
                TITLES[i % TITLES.len()],
                None,
                (i % 7 == 0).then_some(issues[i % issues.len()]),
            )
            .unwrap();
        rig.store.set_status(s.id, statuses[i % statuses.len()]).unwrap();
        match first {
            None => first = Some(s.id),
            Some(parent) if i % 5 == 0 => rig.store.set_parent(s.id, Some(parent)).unwrap(),
            _ => {}
        }
        if i % 9 == 4 {
            rig.store.set_released(s.id, true).unwrap();
        }
    }
    let first = first.unwrap();
    rig.store
        .add_usage(
            first,
            &agentbench_core::Usage {
                cost_usd: 6.57,
                turns: 3,
                context: 84_000,
                window: 128_000,
                model: "qwen3-coder".into(),
                host: "gpu.lan:11434".into(),
                effort: "medium".into(),
                lsp: ["rust", "typescript", "go", "bash", "lua", "java", "gcad", "cuda"]
                    .into_iter()
                    .map(|n| lsp(n, ".", agentbench_core::LspHealth::Ready, ""))
                    .collect(),
                ..Default::default()
            },
        )
        .unwrap();

    let main = rig.store.main_worktree(project.id).unwrap();
    let chat = long_chat(&rig, 3);
    rig.store
        .add_message(
            chat.id,
            &Entry::Tool {
                call_id: "plan".into(),
                name: "todo_write".into(),
                input: serde_json::json!({"items": [
                    {"text": "read the billing worker and its callers", "status": "done"},
                    {"text": "write the ledger row for every credit", "status": "in_progress"},
                    {"text": "run the tests for the billing crate", "status": "pending"},
                ]}),
                output: Some("ok".into()),
                error: false,
            },
        )
        .unwrap();
    rig.store.set_status(chat.id, SessionStatus::Idle).unwrap();

    let question = rig.store.add_session(main.id, "native", "needs a licence", None, None).unwrap();
    rig.store.add_message(question.id, &Entry::user("pick a licence for the new crate")).unwrap();
    rig.store
        .add_message(
            question.id,
            &Entry::Question {
                id: "q1".into(),
                question: agentbench_core::Question {
                    text: "Which licence should the new crate use, given it links the GPL parts?".into(),
                    options: vec![
                        "GPL-3.0-only".into(),
                        "Apache-2.0 with the runtime exception".into(),
                        "MIT".into(),
                    ],
                    multiple: false,
                    secret: false,
                },
                answer: None,
            },
        )
        .unwrap();
    rig.store.set_status(question.id, SessionStatus::Waiting).unwrap();

    let faulty = rig.store.add_session(main.id, "native", "a failing run", None, None).unwrap();
    for e in [
        Entry::user("deploy it"),
        Entry::Notice { text: "retrying in 4s after a 529 from the provider".into() },
        Entry::Warning { text: "the context is at 94%, the oldest turns will be folded".into() },
        Entry::Diagnostics {
            text: "src/billing.rs: 2 errors after the edit".into(),
            broken: vec!["src/billing.rs:88: expected `;`".into()],
        },
        Entry::Error {
            text: "the provider answered 401: the token was refused. Check the key in the settings, then send the message again.".into(),
        },
    ] {
        rig.store.add_message(faulty.id, &e).unwrap();
    }
    rig.store.set_status(faulty.id, SessionStatus::Failed).unwrap();

    let files = rig.store.add_session(main.id, "native", "files and pictures", None, None).unwrap();
    let pic = rig.store.add_attachment(files.id, "screenshot-of-the-error.png", "image/png", &png(300, 200)).unwrap();
    let log = rig.store.add_attachment(files.id, "build-output-2026-11-18-with-a-long-name.log", "text/plain", &[b'x'; 5000]).unwrap();
    rig.store
        .add_message(
            files.id,
            &Entry::User {
                text: "here is what I see".into(),
                attachments: vec![pic.clone(), log.clone()],
                from: vec![],
            },
        )
        .unwrap();
    rig.store
        .add_message(files.id, &Entry::File { text: "the report you asked for".into(), file: log })
        .unwrap();
    rig.store.add_message(files.id, &Entry::Assistant { text: "Looking at the picture now.".into() }).unwrap();

    let sup_project = rig.store.supervisor_project().unwrap().unwrap();
    let sup_tree = rig.store.main_worktree(sup_project.id).unwrap();
    let supervisor = match rig.store.sessions().unwrap().into_iter().find(|s| s.project_id == sup_project.id) {
        Some(s) => s,
        None => rig.store.add_session(sup_tree.id, "native", "Supervisor", None, None).unwrap(),
    };
    for e in [
        Entry::user("what is running"),
        Entry::Assistant { text: "The worker is on the drawer fix. Two issues wait on it.".into() },
    ] {
        rig.store.add_message(supervisor.id, &e).unwrap();
    }
    rig.store.set_setting("autopilot", "1").unwrap();
    rig.store.set_setting("autopilot_cap", "6").unwrap();
    rig.store.add_worker(first, issues[0], project.id).unwrap();
    rig.store.log_autopilot(Some(project.id), Some(issues[0]), "started a worker on the drawer fix").unwrap();
    rig.store.log_autopilot(None, None, "judged the allocator branch, sent back for tests").unwrap();
    rig.store
        .update_project(
            project.id,
            &agentbench_core::ProjectPatch {
                automation: Some(Some(agentbench_core::Automation::FullAuto)),
                ..Default::default()
            },
        )
        .unwrap();
    for i in 0..3 {
        let proposal = agentbench_core::Proposal {
            id: 0,
            kind: agentbench_core::ProposalKind::Stale,
            summary: format!("Close stale issue number {i} that nobody has touched since the spring"),
            issues: vec![issues[i]],
            sessions: vec![],
            projects: vec![project.id],
            changes: vec![],
            state: agentbench_core::ProposalState::Pending,
            created_at: 0,
        };
        rig.store.add_proposal(&format!("sweep:{i}"), &proposal).unwrap();
    }
    let dir = rig._dir.path().join("newcomer");
    repo(&dir);
    add_project_request(&rig, &dir);

    Rich {
        rig,
        calm,
        project: project.id,
        chat: chat.id,
        question: question.id,
        faulty: faulty.id,
        files: files.id,
        supervisor: supervisor.id,
        deep_issue,
    }
}

fn storage(url: &str) -> Vec<(String, String)> {
    vec![("url".into(), url.into())]
}

fn open(url: &str, cfg: Cfg) -> Harness<'static, App> {
    let mut h = harness_cfg(storage(url), cfg);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.run_steps(3);
    h
}

fn open_rich(r: &Rich, cfg: Cfg) -> Harness<'static, App> {
    let mut h = open(&r.rig.url, cfg);
    until(&mut h, "the quota", |h| {
        h.state().overview().and_then(|o| o.usage.as_ref()).is_some_and(|u| !u.quotas.is_empty())
    });
    until(&mut h, "the sessions", |h| h.state().overview().is_some_and(|o| o.sessions.len() >= 30));
    h
}

fn built(h: Harness<'static, App>) -> Built {
    Built { h, _keep: Vec::new() }
}

fn chat_of(r: &Rich, cfg: Cfg, id: SessionId, rows: usize) -> Harness<'static, App> {
    let mut h = open_rich(r, cfg);
    h.state_mut().open_session(id, r.project);
    until(&mut h, "the transcript", |h| h.state().transcript_len(id) >= rows);
    h.run_steps(4);
    h
}

fn supervisor(r: &Rich, cfg: Cfg) -> Harness<'static, App> {
    let mut h = open_rich(r, cfg);
    until(&mut h, "the supervisor", |h| h.state().supervisor_chat().is_some());
    let sid = h.state().supervisor_chat().unwrap();
    assert_eq!(sid, r.supervisor);
    h.state_mut().open_supervisor_chat(sid);
    until(&mut h, "the transcript", |h| h.state().transcript_len(r.supervisor) >= 2);
    until(&mut h, "the autopilot", |h| h.state().autopilot().is_some_and(|a| a.enabled));
    h.run_steps(4);
    h
}

fn s_home(r: &Rich, cfg: Cfg) -> Built {
    built(open_rich(r, cfg))
}

fn s_drawer(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    if cfg.size[0] < 720.0 {
        h.get_by_label("projects").click();
        h.run_steps(5);
    }
    built(h)
}

fn s_drawer_few(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open(&r.calm.url, cfg);
    if cfg.size[0] < 720.0 {
        h.get_by_label("projects").click();
        h.run_steps(5);
    }
    Built { h, _keep: Vec::new() }
}

fn s_chat(r: &Rich, cfg: Cfg) -> Built {
    built(chat_of(r, cfg, r.chat, 10))
}

fn s_chat_top(r: &Rich, cfg: Cfg) -> Built {
    let mut h = chat_of(r, cfg, r.chat, 10);
    h.key_press(egui::Key::Home);
    h.run_steps(4);
    built(h)
}

fn s_question(r: &Rich, cfg: Cfg) -> Built {
    built(chat_of(r, cfg, r.question, 2))
}

fn s_errors(r: &Rich, cfg: Cfg) -> Built {
    built(chat_of(r, cfg, r.faulty, 5))
}

fn s_files(r: &Rich, cfg: Cfg) -> Built {
    built(chat_of(r, cfg, r.files, 3))
}

fn s_composer_files(r: &Rich, cfg: Cfg) -> Built {
    let mut h = chat_of(r, cfg, r.files, 3);
    drop_file(&mut h, "photo.png", png(120, 80));
    drop_file(&mut h, "notes.txt", b"hello".to_vec());
    h.run_steps(8);
    built(h)
}

fn s_composer_draft(r: &Rich, cfg: Cfg) -> Built {
    let mut h = chat_of(r, cfg, r.files, 3);
    let draft = (0..30).map(|i| format!("line {i} of a long dictation")).collect::<Vec<_>>();
    let at = h
        .query_all_by_role(egui::accesskit::Role::MultilineTextInput)
        .last()
        .or_else(|| h.query_all_by_role(egui::accesskit::Role::TextInput).last())
        .map(|n| n.rect().center())
        .expect("the composer");
    click(&mut h, at.x, at.y);
    h.run_steps(2);
    h.event(egui::Event::Text(draft.join("\n")));
    h.run_steps(8);
    built(h)
}

fn s_chat_menu(r: &Rich, cfg: Cfg) -> Built {
    let mut h = chat_of(r, cfg, r.chat, 10);
    if let Some(n) = h.query_by_label("more") {
        n.click();
        h.run_steps(4);
    }
    built(h)
}

fn s_header_menu(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    if let Some(n) = h.query_by_label("more") {
        n.click();
        h.run_steps(4);
    }
    built(h)
}

fn s_issues(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().set_issues_first(true);
    h.run_steps(4);
    built(h)
}

fn s_issue_detail(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().set_issues_first(true);
    h.state_mut().open_issue_pane(r.deep_issue);
    h.run_steps(5);
    built(h)
}

fn s_board_all(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().show_all_projects();
    h.run_steps(4);
    built(h)
}

fn s_settings(page: &'static str, r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().open_settings_page(page);
    until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
    h.run_steps(4);
    built(h)
}

fn s_settings_general(r: &Rich, cfg: Cfg) -> Built {
    s_settings("general", r, cfg)
}

fn s_settings_projects(r: &Rich, cfg: Cfg) -> Built {
    s_settings("projects", r, cfg)
}

fn s_settings_agent(r: &Rich, cfg: Cfg) -> Built {
    s_settings("agent", r, cfg)
}

fn s_settings_speech(r: &Rich, cfg: Cfg) -> Built {
    s_settings("speech", r, cfg)
}

fn s_pairing(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().open_settings_page("general");
    until(&mut h, "the settings", |h| h.state().settings_loaded() == Some(true));
    h.state_mut().settings_set_external("https://agentbench.harkin.network");
    h.run_steps(5);
    built(h)
}

fn s_vision(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().open_vision(r.project);
    h.run_steps(6);
    built(h)
}

fn s_add_workspace(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().open_add_workspace_form();
    h.run_steps(4);
    built(h)
}

fn s_help(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().open_help();
    h.run_steps(4);
    built(h)
}

fn s_new_chat(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().open_new_chat_form();
    h.run_steps(4);
    built(h)
}

fn s_file_issue(r: &Rich, cfg: Cfg) -> Built {
    let mut h = open_rich(r, cfg);
    h.state_mut().open_file_issue_form(r.project);
    h.run_steps(4);
    built(h)
}

fn s_confirm_close(r: &Rich, cfg: Cfg) -> Built {
    let mut h = chat_of(r, cfg, r.chat, 10);
    h.state_mut().ask_to_close_chat(r.chat);
    h.run_steps(4);
    built(h)
}

fn s_rename(r: &Rich, cfg: Cfg) -> Built {
    let mut h = chat_of(r, cfg, r.chat, 10);
    h.state_mut().start_rename_chat(r.chat);
    h.run_steps(4);
    built(h)
}

fn s_supervisor(r: &Rich, cfg: Cfg) -> Built {
    built(supervisor(r, cfg))
}

fn s_supervisor_levels(r: &Rich, cfg: Cfg) -> Built {
    let mut h = supervisor(r, cfg);
    h.get_by_label("automation levels").click();
    h.run_steps(5);
    built(h)
}

fn s_supervisor_expanded(r: &Rich, cfg: Cfg) -> Built {
    let mut h = supervisor(r, cfg);
    h.get_by_label("expand supervisor panels").click();
    h.run_steps(5);
    built(h)
}

fn s_supervisor_cards(r: &Rich, cfg: Cfg) -> Built {
    let mut h = supervisor(r, cfg);
    until(&mut h, "the triage", |h| {
        h.state().overview().and_then(|o| o.triage.as_ref()).is_some_and(|t| t.proposals.len() >= 3)
    });
    h.run_steps(4);
    built(h)
}

fn two(r: &Rich, cfg: Cfg) -> (Harness<'static, App>, Rig) {
    let second = rig();
    let chat = second.store.sessions().unwrap().into_iter().find(|s| s.title == "say hello").unwrap();
    second.store.set_status(chat.id, SessionStatus::Waiting).unwrap();
    let servers = serde_json::json!([{"slot": 1, "name": "workstation", "url": second.url, "token": TOKEN}]);
    let mut h = harness_cfg(
        vec![
            ("url".into(), r.calm.url.clone()),
            ("server_name".into(), "desktop".into()),
            ("servers".into(), servers.to_string()),
        ],
        cfg,
    );
    until(&mut h, "both overviews", |h| {
        h.state().overview().is_some_and(|o| o.projects.iter().filter(|p| p.name == "demo").count() == 2)
    });
    h.run_steps(4);
    (h, second)
}

fn s_workspaces(r: &Rich, cfg: Cfg) -> Built {
    let (h, second) = two(r, cfg);
    Built { h, _keep: vec![Box::new(second)] }
}

fn s_workspace_drawer(r: &Rich, cfg: Cfg) -> Built {
    let (mut h, second) = two(r, cfg);
    if cfg.size[0] < 720.0 {
        h.get_by_label("projects").click();
        h.run_steps(5);
    }
    Built { h, _keep: vec![Box::new(second)] }
}

fn s_out_of_date(r: &Rich, cfg: Cfg) -> Built {
    let _ = r;
    let mut o = recorded_overview();
    o["api_version"] = 9.into();
    o["min_client_api"] = 9.into();
    let mut h = harness_cfg(storage(&canned_server(o.to_string())), cfg);
    until(&mut h, "the banner", |h| {
        h.query_by_label_contains("This app is older than the server").is_some()
    });
    built(h)
}

fn s_offline(r: &Rich, cfg: Cfg) -> Built {
    let _ = r;
    let mut h = harness_cfg(storage("127.0.0.1:9"), cfg);
    h.run_steps(10);
    std::thread::sleep(std::time::Duration::from_millis(400));
    h.run_steps(10);
    built(h)
}

fn s_onboarding(r: &Rich, cfg: Cfg) -> Built {
    let _ = r;
    let fresh = rig_named("http://127.0.0.1:9/v1", "", Vec::new());
    let mut h = harness_cfg(storage(&fresh.url), cfg);
    until(&mut h, "the onboarding", |h| h.state().onboarding_open());
    Built { h, _keep: vec![Box::new(fresh)] }
}

fn s_viewer(r: &Rich, cfg: Cfg) -> Built {
    let mut h = chat_of(r, cfg, r.files, 3);
    if let Some(n) = h.query_all_by_label("open").next() {
        n.click();
    }
    h.run_steps(8);
    built(h)
}

pub const SCENES: &[Scene] = &[
    Scene { id: 1, name: "home", build: s_home },
    Scene { id: 2, name: "drawer-30-sessions", build: s_drawer },
    Scene { id: 3, name: "drawer-few", build: s_drawer_few },
    Scene { id: 4, name: "chat-long", build: s_chat },
    Scene { id: 5, name: "chat-top", build: s_chat_top },
    Scene { id: 6, name: "chat-question", build: s_question },
    Scene { id: 7, name: "chat-errors", build: s_errors },
    Scene { id: 8, name: "chat-files", build: s_files },
    Scene { id: 9, name: "composer-files", build: s_composer_files },
    Scene { id: 10, name: "composer-draft", build: s_composer_draft },
    Scene { id: 11, name: "chat-menu", build: s_chat_menu },
    Scene { id: 12, name: "header-menu", build: s_header_menu },
    Scene { id: 13, name: "issues", build: s_issues },
    Scene { id: 14, name: "issue-detail", build: s_issue_detail },
    Scene { id: 15, name: "board-all", build: s_board_all },
    Scene { id: 16, name: "settings-general", build: s_settings_general },
    Scene { id: 17, name: "settings-projects", build: s_settings_projects },
    Scene { id: 18, name: "settings-agent", build: s_settings_agent },
    Scene { id: 19, name: "settings-speech", build: s_settings_speech },
    Scene { id: 20, name: "pairing", build: s_pairing },
    Scene { id: 21, name: "vision", build: s_vision },
    Scene { id: 22, name: "add-workspace", build: s_add_workspace },
    Scene { id: 23, name: "help", build: s_help },
    Scene { id: 24, name: "new-chat", build: s_new_chat },
    Scene { id: 25, name: "file-issue", build: s_file_issue },
    Scene { id: 26, name: "confirm-close", build: s_confirm_close },
    Scene { id: 27, name: "rename-chat", build: s_rename },
    Scene { id: 28, name: "supervisor-autopilot", build: s_supervisor },
    Scene { id: 29, name: "supervisor-levels", build: s_supervisor_levels },
    Scene { id: 30, name: "supervisor-expanded", build: s_supervisor_expanded },
    Scene { id: 31, name: "supervisor-cards", build: s_supervisor_cards },
    Scene { id: 32, name: "workspaces-needs-you", build: s_workspaces },
    Scene { id: 33, name: "workspaces-drawer", build: s_workspace_drawer },
    Scene { id: 34, name: "out-of-date", build: s_out_of_date },
    Scene { id: 35, name: "offline", build: s_offline },
    Scene { id: 36, name: "onboarding", build: s_onboarding },
    Scene { id: 37, name: "viewer", build: s_viewer },
];
