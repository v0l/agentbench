pub use agent_native::quota::*;
use agent_native::native::{Model, Shared};
use std::time::Duration;

#[cfg(test)]
pub(crate) use agent_native::testing::quota as fake;

fn pause_key(id: agentbench_core::SessionId) -> String {
    format!("quota_pause_{id}")
}

pub fn paused_until(store: &crate::store::Store, id: agentbench_core::SessionId) -> Option<i64> {
    store.setting(&pause_key(id)).ok().flatten()?.parse().ok()
}

pub fn set_paused(store: &crate::store::Store, id: agentbench_core::SessionId, until: Option<i64>) {
    let _ = match until {
        Some(t) => store.set_setting(&pause_key(id), &t.to_string()),
        None => store.delete_setting(&pause_key(id)),
    };
}


pub async fn poll_once(runner: &crate::runner::Runner, model: &Model) {
    runner.quotas.set_base(model);
    runner.quotas.set_start_model(
        &model.level("medium").map_or_else(|| model.model.clone(), |m| m.model),
    );
    for target in servers(model) {
        let host = agentbench_core::host_of(&target.base);
        let Some(body) = agent_native::quota::status(&target.base, &target.key, "", Duration::from_secs(5)).await else {
            continue;
        };
        let now = now();
        for q in readings(&body, &host, &target.models, now) {
            let block = q.exhausted_until(now);
            let name = q.model.clone();
            runner.observe_quota(q);
            if let Some(t) = block {
                runner.quotas.block(&name, t);
            }
        }
    }
    runner.expire_quotas();
    runner.wake_paused();
}

pub fn spawn_poll(runner: crate::runner::Runner, model: Shared) {
    tokio::spawn(async move {
        loop {
            poll_once(&runner, &model.get()).await;
            tokio::time::sleep(POLL_EVERY).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_native::native::Level;
    use serde_json::json;

    fn two_models() -> String {
        json!({
            "summary": {"used_pct": 100, "window": "7d", "resets_at": 1_900_000_000},
            "models": {
                "code-x-high": {"summary": {"used_pct": 100, "window": "7d", "resets_at": 1_900_000_000, "exhausted": true, "next_available_at": 1_900_000_000}, "windows": []},
                "code-high": {"summary": {"used_pct": 17, "window": "7d", "resets_at": 1_900_000_000, "exhausted": false, "next_available_at": null}, "windows": []}
            }})
        .to_string()
    }

    fn levels(url: &str) -> Model {
        Model {
            url: url.into(),
            model: "code".into(),
            high: Level { model: "code-x-high".into(), ..Level::default() },
            medium: Level { model: "code-high".into(), ..Level::default() },
            ..Model::default()
        }
    }

    #[tokio::test]
    async fn only_the_exhausted_model_is_blocked_when_the_server_reports_models() {
        let (url, paths) = fake::server(vec![], Some(two_models()));
        let runner = crate::runner::Runner::new(crate::store::Store::memory().unwrap(), vec![]);
        let model = levels(&url);
        assert_eq!(servers(&model).len(), 1);
        poll_once(&runner, &model).await;
        assert_eq!(paths.lock().unwrap().len(), 1, "one call covers every model");
        let now = 1_800_000_000_000;
        assert!(runner.quotas.exhausted_until("code-x-high", now).is_some());
        assert!(runner.quotas.exhausted_until("code-high", now).is_none());
        assert!(runner.quotas.exhausted_until("code", now).is_none());
        assert_eq!(runner.quotas.start_model(), "code-high");
        let summary = runner.quotas.summary(now);
        assert!(summary.contains("code-x-high: 7d 100%, exhausted until"), "{summary}");
        assert!(summary.contains("code-high: 7d 17%"), "{summary}");
    }


    #[tokio::test]
    async fn a_server_without_models_still_reads_its_top_level_summary() {
        let body = json!({"summary": {"used_pct": 40, "window": "5h", "resets_at": 1_900_000_000}}).to_string();
        let (url, _) = fake::server(vec![], Some(body));
        let runner = crate::runner::Runner::new(crate::store::Store::memory().unwrap(), vec![]);
        poll_once(&runner, &levels(&url)).await;
        let all = runner.quotas.all();
        assert_eq!(all.len(), 3);
        assert!(all.iter().all(|q| q.used_pct == 40.0 && q.window == "5h"));
        let old = json!({"windows": [{"window": "5h", "used_pct": 62, "resets_at": 1_900_000_000}]});
        let q = for_model(&old, "h", "code", 0).unwrap();
        assert_eq!((q.model.as_str(), q.used_pct), ("code", 62.0));
    }

}
