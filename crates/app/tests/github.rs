use agentbench::{App, GhTab};
use agentbench_core::{IssueState, NewIssue, PostMode, ProjectPatch, Tracker, Trust};
use agentbench_server::github::outbox::{Ask, propose};
use agentbench_server::store::{MirrorIssue, MirrorPr};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT as _, Queryable};
use serde_json::json;
use std::sync::OnceLock;

mod common;
use common::*;

const STUB: &str = r#"#!/bin/sh
D="$(dirname "$0")"
echo "$@" >> "$D/calls"
case "$1 $2" in
  "auth status") exit 0 ;;
  "api rate_limit") echo 4321; exit 0 ;;
  "api repos/o/r") echo true; exit 0 ;;
  "api --method") echo '{"id": 99}'; exit 0 ;;
  "pr merge") exit 0 ;;
esac
echo "gh: Service Unavailable (HTTP 503)" >&2
exit 1
"#;

fn stub_gh() -> &'static std::path::Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let d = tempfile::tempdir().unwrap();
        let gh = d.path().join("gh");
        std::fs::write(&gh, STUB).unwrap();
        std::process::Command::new("chmod").arg("755").arg(&gh).status().unwrap();
        d
    });
    unsafe { std::env::set_var("AGENTBENCH_GH", dir.path().join("gh")) };
    dir.path()
}

struct Gh {
    rig: Rig,
    project: i64,
    comment_row: i64,
}

fn mirror(rig: &Rig, project: i64, n: i64, title: &str, author: &str, trust: Trust, labels: &[&str]) -> i64 {
    rig.store
        .mirror_issue(
            project,
            &MirrorIssue {
                number: n,
                title: title.into(),
                body: format!("Steps to reproduce issue {n}."),
                state: IssueState::Backlog,
                priority: 0,
                labels: labels.iter().map(|l| l.to_string()).collect(),
                author: author.into(),
                url: format!("https://github.com/o/r/issues/{n}"),
                updated_at: 1,
                trust,
            },
        )
        .unwrap()
}

fn ask(project: i64, kind: &str, number: i64, text: &str, params: serde_json::Value, by: &str) -> Ask {
    Ask { project, kind: kind.into(), number, text: text.into(), params, asked_by: by.into(), trusted: true }
}

fn gh_rig() -> Gh {
    stub_gh();
    let rig = rig();
    let p = rig.store.projects().unwrap().into_iter().find(|p| !p.builtin()).unwrap();
    rig.store
        .update_project(p.id, &ProjectPatch { tracker: Some(Tracker::Github { repo: "o/r".into() }), post_mode: Some(PostMode::Draft), ..Default::default() })
        .unwrap();
    mirror(&rig, p.id, 41, "Crash when the config file is empty", "alice", Trust::Trusted, &["bug", "priority:high"]);
    mirror(&rig, p.id, 42, "Please add a dark mode toggle to the settings page with a very long title that must wrap on a phone", "stranger-with-a-long-login", Trust::Untrusted, &["enhancement"]);
    let linked = mirror(&rig, p.id, 43, "Sync drops comments", "kieran", Trust::Trusted, &["ready"]);
    rig.store
        .add_issue(&NewIssue { project_id: p.id, title: "Tidy the internal notes".into(), ..Default::default() })
        .unwrap();
    rig.store
        .mirror_pr(
            p.id,
            &MirrorPr {
                number: 44,
                title: "Keep comments when syncing".into(),
                body: "Closes #43\n\nKeeps comments across a sync.".into(),
                author: "agentbench".into(),
                association: String::new(),
                head_ref: "issue-43".into(),
                base_ref: "main".into(),
                head_sha: "abc123".into(),
                state: "open".into(),
                draft: false,
                url: "https://github.com/o/r/pull/44".into(),
                created_at: 1,
                updated_at: 2,
                trust: Trust::Trusted,
            },
        )
        .unwrap();
    rig.store.link_pr(p.id, 44, Some(43)).unwrap();
    let check = agentbench_core::PrCheck { name: "test".into(), status: "completed".into(), conclusion: Some("failure".into()), run_id: Some(5), url: "u".into() };
    rig.store.set_pr_checks(p.id, 44, "abc123", &[check]).unwrap();
    let (review, _) = propose(
        &rig.store,
        ask(p.id, "review", 44, "The sync now keeps comments, but the test for an edited comment fails.", json!({"event": "request_changes", "sha": "abc123", "comments": [{"path": "src/sync.rs", "line": 12, "body": "This drops edited comments."}]}), "reviewer"),
    )
    .unwrap();
    rig.store.set_pr_review(p.id, 44, "abc123", "request_changes", review, 0).unwrap();
    let (comment_row, _) = propose(&rig.store, ask(p.id, "comment", 41, "Thanks for the report. Can you share the config file?", json!({}), "triage")).unwrap();
    propose(&rig.store, ask(p.id, "label_add", 42, "", json!({"label": "needs-info"}), "triage")).unwrap();
    propose(&rig.store, ask(p.id, "issue_state", 42, "", json!({"state": "closed", "reason": "not_planned"}), "triage")).unwrap();
    let _ = linked;
    Gh { rig, project: p.id, comment_row }
}

fn open_github(h: &mut Harness<'_, App>, project: i64, tab: GhTab) {
    until(h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_project_view(project);
    h.state_mut().show_issues();
    h.state_mut().set_github_tab(tab);
    until(h, "the outbox and pull requests", |h| {
        h.state().github_view(project).is_some_and(|v| v.outbox.is_some() && v.pulls.is_some())
    });
}

fn off_screen(h: &Harness<'_, App>, size: [f32; 2]) -> Vec<String> {
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(size[0], size[1]));
    h.root()
        .children_recursive()
        .filter(|n| n.accesskit_node().bounding_box().is_some())
        .filter(|n| {
            let r = n.rect();
            r.right() > screen.right() + 0.5 || r.left() < screen.left() - 0.5
        })
        .map(|n| format!("{:?} {:?} {:?}", n.accesskit_node().role(), n.accesskit_node().label(), n.rect()))
        .collect()
}

const SIZES: [[f32; 2]; 2] = [[1280.0, 800.0], [390.0, 844.0]];

fn tab_chip_rects(h: &Harness<'_, App>) -> Vec<(String, egui::Rect)> {
    h.root()
        .children_recursive()
        .filter(|n| n.accesskit_node().bounding_box().is_some())
        .filter_map(|n| {
            let label = n.accesskit_node().label()?;
            let first = label.split_whitespace().next()?;
            matches!(first, "issues" | "outbox" | "pull" | "board").then(|| (label.clone(), n.rect()))
        })
        .collect()
}

#[test]
fn the_github_tabs_fit_a_phone() {
    let g = gh_rig();
    g.rig
        .store
        .update_project(g.project, &ProjectPatch { tracker: Some(Tracker::Github { repo: "o/a-repository-name-long-enough-to-fill-a-phone-row".into() }), ..Default::default() })
        .unwrap();
    let failed = g.rig.store.outbox(Some(g.project)).unwrap().into_iter().find(|i| i.kind == "label_add").unwrap().id;
    g.rig.store.outbox_set_approver(failed, "reviewer").unwrap();
    g.rig.store.outbox_result(failed, "failed", None, Some("gh: Service Unavailable (HTTP 503)"), 2, 0).unwrap();
    g.rig.store.outbox_set_approver(g.comment_row, "user").unwrap();
    for size in [[390.0, 844.0], [412.0, 900.0], [1280.0, 800.0]] {
        for tab in [GhTab::Issues, GhTab::Outbox, GhTab::Pulls] {
            let mut h = harness_at(g.rig.url.clone(), size);
            open_github(&mut h, g.project, tab);
            h.run_steps(4);
            shot(&mut h, &format!("github-fit-{:?}-{}", tab, size[0] as u32).to_lowercase());
            let bad: Vec<String> = h
                .root()
                .children_recursive()
                .filter(|n| n.accesskit_node().bounding_box().is_some())
                .filter(|n| {
                    let r = n.rect();
                    r.left() < -4.0 || r.right() > size[0] + 4.0
                })
                .map(|n| format!("{:?} {:?} {:?}", n.accesskit_node().role(), n.accesskit_node().label(), n.rect()))
                .collect();
            assert!(bad.is_empty(), "{tab:?} at {size:?}: {bad:?}");
            let toggle = h.get_by_label("project scope toggle").rect();
            assert!(toggle.width() >= 120.0, "{tab:?} at {size:?}: the scope toggle is squeezed to {} px wide, so its labels wrap", toggle.width());
            let chips = tab_chip_rects(&h);
            assert!(!chips.is_empty(), "{tab:?} at {size:?}: no tab chips found");
            for (label, r) in &chips {
                assert!(!toggle.intersects(*r), "{tab:?} at {size:?}: the scope toggle {toggle:?} sits over the tab chip {label:?} {r:?}");
            }
            if tab == GhTab::Outbox {
                let by = |h: &Harness<'_, App>, who: &str| h.query_all_by_label_contains(&format!("approved by {who}")).count();
                assert_eq!(by(&h, "reviewer"), 0, "{size:?}: the failed row still carries the approver tag");
                assert!(by(&h, "user") >= 1, "{size:?}: a proposed row with an approver lost its tag too");
                assert!(
                    h.query_all_by_label_contains("gh: Service Unavailable (HTTP 503)").count() >= 1,
                    "{size:?}: the failed row does not show its error, so it is not the row being read"
                );
            }
        }
    }
}

#[test]
fn a_github_project_lists_its_issues_with_trust_labels_and_the_internal_group() {
    let g = gh_rig();
    for size in SIZES {
        let mut h = harness_at(g.rig.url.clone(), size);
        open_github(&mut h, g.project, GhTab::Issues);
        h.run_steps(4);
        shot(&mut h, &format!("github-issues-{}", size[0]));
        h.get_by_label("github project");
        h.get_by_label("github issue #41 Crash when the config file is empty");
        assert!(h.query_all_by_label("outside contributor").count() >= 1, "the stranger's issue carries the badge");
        assert!(h.query_all_by_label("trusted").count() >= 2);
        h.get_by_label("pr #44");
        assert!(h.query_by_label("1 to approve").is_some() || h.query_by_label("2 to approve").is_some());
        assert!(h.query_all_by_label_contains("Tidy the internal notes").any(|n| n.accesskit_node().label().is_some_and(|l| l.starts_with("internal issue #"))));
        let off = off_screen(&h, size);
        assert!(off.is_empty(), "at {size:?}: {off:?}");
    }
}

#[test]
fn the_outbox_shows_what_will_be_posted_and_approve_posts_the_edited_text() {
    let g = gh_rig();
    for size in SIZES {
        let mut h = harness_at(g.rig.url.clone(), size);
        open_github(&mut h, g.project, GhTab::Outbox);
        h.run_steps(4);
        shot(&mut h, &format!("github-outbox-{}", size[0]));
        h.get_by_label("outbox comment on #41");
        h.get_by_label("outbox add label needs-info to #42");
        h.get_by_label("outbox close #42 as not planned");
        h.get_by_label("outbox request changes review on pull request #44");
        h.get_by_label("approve all");
        h.get_by_label("pause all posting");
        let off = off_screen(&h, size);
        assert!(off.is_empty(), "at {size:?}: {off:?}");
    }
    let mut h = harness_at(g.rig.url.clone(), [1280.0, 800.0]);
    open_github(&mut h, g.project, GhTab::Outbox);
    let field = format!("text of row {}", g.comment_row);
    h.get_by_label(&field).click();
    h.run_steps(2);
    h.get_by_label(&field).type_text(" Thank you.");
    h.run_steps(3);
    h.get_by_label(&format!("approve row {}", g.comment_row)).click();
    let store = g.rig.store.clone();
    let row = g.comment_row;
    until(&mut h, "the edited row is posted", |_| store.outbox_item(row).unwrap().status == "done");
    let item = store.outbox_item(row).unwrap();
    assert!(item.text.ends_with("Thank you."), "{}", item.text);
    assert_eq!(item.approved_by, "user");
    let calls = std::fs::read_to_string(stub_gh().join("calls")).unwrap();
    assert!(calls.contains("api --method POST repos/o/r/issues/41/comments"), "{calls}");
    h.state_mut().set_github_tab(GhTab::Outbox);
    let pid = g.project;
    until(&mut h, "the sent row with its link", |h| {
        h.state().github_view(pid).and_then(|v| v.outbox.as_ref()).is_some_and(|o| o.items.iter().any(|i| i.url.as_deref() == Some("https://github.com/o/r/issues/41#issuecomment-99")))
    });
}

#[test]
fn reject_pause_and_approve_all_reach_the_server() {
    let g = gh_rig();
    let mut h = harness_at(g.rig.url.clone(), [390.0, 844.0]);
    open_github(&mut h, g.project, GhTab::Outbox);
    h.get_by_label("pause all posting").click();
    let store = g.rig.store.clone();
    until(&mut h, "paused", |_| agentbench_server::github::outbox::is_paused(&store));
    let label = store.outbox(Some(g.project)).unwrap().into_iter().find(|i| i.kind == "label_add").unwrap().id;
    h.get_by_label(&format!("reject row {label}")).click();
    until(&mut h, "rejected", |_| store.outbox_item(label).unwrap().status == "rejected");
    h.get_by_label("approve all").click();
    until(&mut h, "every proposed row approved", |_| store.outbox(None).unwrap().iter().all(|i| i.status != "proposed"));
    assert!(store.outbox(None).unwrap().iter().all(|i| i.status != "done"), "paused: nothing is posted");
}

#[test]
fn the_pull_requests_tab_shows_checks_verdict_and_summary_and_merge_confirms() {
    let g = gh_rig();
    for size in SIZES {
        let mut h = harness_at(g.rig.url.clone(), size);
        open_github(&mut h, g.project, GhTab::Pulls);
        h.run_steps(4);
        shot(&mut h, &format!("github-pulls-{}", size[0]));
        h.get_by_label("pull request #44");
        h.get_by_label("checks failed");
        h.get_by_label("review: changes asked");
        h.get_by_label("diff on github");
        let off = off_screen(&h, size);
        assert!(off.is_empty(), "at {size:?}: {off:?}");
    }
    let mut h = harness_at(g.rig.url.clone(), [1280.0, 800.0]);
    open_github(&mut h, g.project, GhTab::Pulls);
    h.get_by_label("review pull request #44").click();
    let store = g.rig.store.clone();
    let pid = g.project;
    until(&mut h, "the review is asked", |_| store.pull_request(pid, 44).unwrap().unwrap().review_asked > 0);
    h.get_by_label("merge pull request #44").click();
    h.run_steps(4);
    shot(&mut h, "github-merge-confirm");
    h.get_by_label("merge").click();
    until(&mut h, "the refusal", |h| h.state().notice().is_some_and(|(ok, t)| !ok && t.contains("#44")));
    let calls = std::fs::read_to_string(stub_gh().join("calls")).unwrap();
    assert!(!calls.contains("pr merge"), "{calls}");
    assert_eq!(store.pull_request(pid, 44).unwrap().unwrap().state, "open", "the server refuses: red checks and no owner approval");
}

#[test]
fn the_project_page_holds_the_github_settings_and_sync_status() {
    let g = gh_rig();
    for size in SIZES {
        let mut h = harness_at(g.rig.url.clone(), [size[0], 1600.0f32.max(size[1])]);
        until(&mut h, "the overview", |h| h.state().overview().is_some());
        h.state_mut().open_project_view(g.project);
        let pid = g.project;
        until(&mut h, "the github status", |h| h.state().github_view(pid).is_some_and(|v| v.status.is_some() && v.settings.is_some()));
        h.run_steps(4);
        shot(&mut h, &format!("github-settings-{}", size[0]));
        h.get_by_label("github repo");
        h.get_by_label("poll seconds");
        h.get_by_label("public address");
        h.get_by_label("create webhook");
        h.get_by_label("github project");
        let st = h.state().github_view(pid).unwrap().status.clone().unwrap();
        assert_eq!((st.authenticated, st.rate_limit_remaining, st.write_access), (true, Some(4321), Some(true)));
        let off = off_screen(&h, [size[0], 1600.0f32.max(size[1])]);
        assert!(off.is_empty(), "at {size:?}: {off:?}");
    }
    let mut h = harness_at(g.rig.url.clone(), [1280.0, 1600.0]);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_project_view(g.project);
    let pid = g.project;
    until(&mut h, "the github status", |h| h.state().github_view(pid).is_some_and(|v| v.settings.is_some()));
    h.get_by_label("public address").click();
    h.run_steps(2);
    h.get_by_label("public address").type_text("https://bench.example.com");
    h.run_steps(2);
    h.key_press(egui::Key::Tab);
    h.run_steps(3);
    let store = g.rig.store.clone();
    until(&mut h, "the public address is saved", |_| agentbench_server::github::webhook::base(&store).as_deref() == Some("https://bench.example.com"));
    h.get_by_label("make secret").click();
    until(&mut h, "the secret shown once", |h| h.state().github_view(pid).is_some_and(|v| v.secret.is_some()));
    let secret = h.state().github_view(pid).unwrap().secret.clone().unwrap();
    assert_eq!(secret.url.as_deref(), Some(format!("https://bench.example.com/api/github/webhook/{pid}").as_str()));
    assert_eq!(agentbench_server::github::webhook::secret(&store, pid).as_deref(), Some(secret.secret.as_str()));
}

#[test]
fn an_older_server_shows_no_outbox_or_pull_requests() {
    let mut overview: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/api/v9/overview.json")).unwrap(),
    )
    .unwrap();
    let p = overview["projects"].as_array_mut().unwrap().iter_mut().find(|p| !p["home"].as_bool().unwrap_or(false) && !p["supervisor"].as_bool().unwrap_or(false)).unwrap();
    p["tracker"] = json!({"kind": "github", "repo": "o/r"});
    let pid = p["id"].as_i64().unwrap();
    overview["api_version"] = json!(9);
    let url = canned_server(overview.to_string());
    let mut h = harness_at(url, [1280.0, 800.0]);
    until(&mut h, "the overview", |h| h.state().overview().is_some());
    h.state_mut().open_project_view(pid);
    h.state_mut().show_issues();
    h.state_mut().set_github_tab(GhTab::Outbox);
    h.run_steps(6);
    assert!(h.state().github_mode(pid));
    assert!(h.query_by_label_contains("OUTBOX").is_none() && h.query_by_label_contains("outbox").is_none());
    assert!(h.query_by_label("pull requests 0").is_none() && h.query_by_label("pull requests 1").is_none());
    assert_eq!(h.state().github_tab(), GhTab::Issues);
    assert!(h.state().github_view(pid).is_none_or(|v| v.outbox.is_none()));
}
