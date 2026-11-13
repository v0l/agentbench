mod command;

pub use agent_native::*;
pub use command::JsonCommand;

#[cfg(test)]
mod gate_tests {
    use agent_native::native::{confirm_project, gated};
    use agent_native::{TestIo, context};
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn a_proposal_asks_the_user_and_runs_only_what_they_allowed() {
        use agentbench_core::{Automation, ProjectPatch};
        let dir = tempfile::tempdir().unwrap();
        let s = crate::store::Store::memory().unwrap();
        let (p, _) = crate::store::testing::project(&s, "/a");
        let sup = crate::projects::ensure_supervisor(&s, &dir.path().join("sup")).unwrap();
        let boss = s.add_session(s.main_worktree(sup.id).unwrap().id, "native", "boss", None, None).unwrap();
        s.update_project(p.id, &ProjectPatch { automation: Some(Some(Automation::Manual)), ..Default::default() }).unwrap();
        let mut c = context();
        c.session = boss.id;
        let api = Arc::new(crate::tools::StoreApi::new(s.clone()));
        let file = json!({"project": p.id, "title": "found it"});
        let ask = |answer: &str| {
            let io = TestIo::new(api.clone());
            io.answers.lock().unwrap().push(answer.to_string());
            io
        };
        let count = || s.issues().unwrap().len();

        let e = gated("file_issue", &file, &c, &ask("No")).await.unwrap_err();
        assert!(e.contains("said no") && e.contains("needs assist"), "{e}");
        assert!(gated("file_issue", &file, &c, &TestIo::new(api.clone())).await.is_err(), "no answer");
        assert_eq!(count(), 0);

        gated("file_issue", &file, &c, &ask("Do it this once")).await.unwrap();
        assert_eq!(count(), 1);
        assert_eq!(s.project(p.id).unwrap().automation, Some(Automation::Manual));
        assert!(gated("file_issue", &file, &c, &ask("No")).await.is_err(), "once is once");

        let name = s.project(p.id).unwrap().name;
        gated("file_issue", &file, &c, &ask(&format!("Raise {name} to assist"))).await.unwrap();
        assert_eq!(s.project(p.id).unwrap().automation, Some(Automation::Assist));
        gated("file_issue", &file, &c, &TestIo::new(api.clone())).await.unwrap();
        assert_eq!(count(), 3);

        let up = json!({"project": p.id, "level": "ship"});
        gated("set_automation", &up, &c, &ask("Do it this once")).await.unwrap();
        assert_eq!(s.project(p.id).unwrap().automation, Some(Automation::Ship), "a raise confirmed in chat persists");
        assert!(!agent_native::native::AGENTBENCH_TOOLS.contains(&"approve_action"), "the model cannot approve its own proposals");
    }

    #[tokio::test]
    async fn another_project_is_entered_only_after_the_user_says_so() {
        let s = crate::store::Store::memory().unwrap();
        let (_, main) = crate::store::testing::project(&s, "/a");
        let other = s.add_project("other", "/b", Some("main"), None).unwrap();
        let me = s.add_session(main.id, "native", "me", None, None).unwrap();
        let mut c = context();
        c.session = me.id;
        let api = Arc::new(crate::tools::StoreApi::new(s.clone()));
        let args = json!({"project": other.id, "prompt": "go"});
        let ask = |answer: &str| {
            let io = TestIo::new(api.clone());
            io.answers.lock().unwrap().push(answer.to_string());
            io
        };

        assert!(confirm_project(&args, &c, &ask("Deny")).await.is_err());
        assert!(confirm_project(&args, &c, &ask("anything else")).await.is_err());
        assert!(confirm_project(&args, &c, &TestIo::new(api.clone())).await.is_err(), "no answer");

        confirm_project(&args, &c, &ask("Allow once")).await.unwrap();
        assert!(s.setting("start_session_projects").unwrap().is_none());

        confirm_project(&args, &c, &ask("Always allow in other")).await.unwrap();
        assert_eq!(
            s.setting("start_session_projects").unwrap().unwrap(),
            format!("[{}]", other.id)
        );
        // Now it does not ask: with no answer queued it still passes.
        confirm_project(&args, &c, &TestIo::new(api.clone())).await.unwrap();

        let s2 = crate::store::Store::memory().unwrap();
        let (_, main2) = crate::store::testing::project(&s2, "/a");
        let other2 = s2.add_project("other", "/b", Some("main"), None).unwrap();
        let me2 = s2.add_session(main2.id, "native", "me", None, None).unwrap();
        c.session = me2.id;
        let api2 = Arc::new(crate::tools::StoreApi::new(s2.clone()));
        let io = TestIo::new(api2.clone());
        io.answers.lock().unwrap().push("Always allow, any project".into());
        confirm_project(&json!({"project": "other"}), &c, &io).await.unwrap();
        assert_eq!(s2.setting("start_session_any_project").unwrap().as_deref(), Some("1"));
        let _ = other2;
        // Naming the session's own project never asks.
        confirm_project(&json!({"project": main2.project_id}), &c, &TestIo::new(api2))
            .await
            .unwrap();
    }
}
