use crate::agent::native::{Level, Model};
use crate::store::{Store, StoreError};

pub const URL: &str = "AGENTBENCH_MODEL_URL";
pub const MODEL: &str = "AGENTBENCH_MODEL";
pub const KEY: &str = "AGENTBENCH_MODEL_KEY";
pub const LOW: &str = "AGENTBENCH_MODEL_LOW";
pub const MED: &str = "AGENTBENCH_MODEL_MED";
pub const HIGH: &str = "AGENTBENCH_MODEL_HIGH";

pub fn model_from(
    store: &Store,
    fallback: &Model,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, StoreError> {
    if store.setting("model")?.is_some() {
        return Ok(None);
    }
    let get = |name: &str| env(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let (url, model) = (get(URL), get(MODEL));
    if url.is_none() && model.is_none() {
        return Ok(None);
    }
    let mut m = fallback.clone();
    if let Some(url) = url {
        m.url = url;
    }
    if let Some(model) = model {
        m.model = model;
    }
    if let Some(key) = get(KEY) {
        m.key = key;
    }
    let level = |name: &str, old: &Level| match get(name) {
        Some(model) => Level { model, ..old.clone() },
        None => old.clone(),
    };
    m.low = level(LOW, &m.low);
    m.medium = level(MED, &m.medium);
    m.high = level(HIGH, &m.high);
    let text = serde_json::to_string(&m).map_err(|e| StoreError(e.to_string()))?;
    store.set_setting("model", &text)?;
    let named = |l: &Level| if l.model.is_empty() { "none".to_string() } else { l.model.clone() };
    Ok(Some(format!(
        "model set from the environment: {} at {}, key {}, low {}, med {}, high {}",
        if m.model.is_empty() { "(server default)" } else { &m.model },
        m.url,
        if m.key.is_empty() { "not set" } else { "set" },
        named(&m.low),
        named(&m.medium),
        named(&m.high),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |name| map.get(name).cloned()
    }

    fn saved(store: &Store) -> Model {
        serde_json::from_str(&store.setting("model").unwrap().unwrap()).unwrap()
    }

    #[test]
    fn the_environment_seeds_the_model_once_and_never_prints_the_key() {
        let store = Store::memory().unwrap();
        let vars = env(&[
            (URL, "http://10.0.0.5:8001/v1"),
            (MODEL, "qwen"),
            (KEY, "sk-very-secret"),
            (LOW, "qwen-small"),
            (HIGH, "qwen-large"),
        ]);
        let said = model_from(&store, &Model::default(), &vars).unwrap().expect("seeded");
        assert!(!said.contains("sk-very-secret"), "{said}");
        assert!(said.contains("key set") && said.contains("qwen") && said.contains("low qwen-small"), "{said}");
        let m = saved(&store);
        assert_eq!((m.url.as_str(), m.model.as_str(), m.key.as_str()), ("http://10.0.0.5:8001/v1", "qwen", "sk-very-secret"));
        assert_eq!((m.low.model.as_str(), m.medium.model.as_str(), m.high.model.as_str()), ("qwen-small", "", "qwen-large"));
        let again = env(&[(URL, "http://other/v1"), (MODEL, "other")]);
        assert_eq!(model_from(&store, &Model::default(), &again).unwrap(), None);
        assert_eq!(saved(&store).model, "qwen", "a second start keeps the first seed");
    }

    #[test]
    fn a_model_saved_from_the_window_is_never_overwritten() {
        let store = Store::memory().unwrap();
        let mine = Model { url: "http://mine/v1".into(), model: "mine".into(), key: "k".into(), ..Model::default() };
        store.set_setting("model", &serde_json::to_string(&mine).unwrap()).unwrap();
        let vars = env(&[(URL, "http://env/v1"), (MODEL, "env"), (KEY, "sk-env")]);
        assert_eq!(model_from(&store, &Model::default(), &vars).unwrap(), None);
        assert_eq!(saved(&store), mine);
    }

    #[test]
    fn no_model_variables_leave_nothing_saved() {
        let store = Store::memory().unwrap();
        assert_eq!(model_from(&store, &Model::default(), env(&[(KEY, "sk-only")])).unwrap(), None);
        assert_eq!(model_from(&store, &Model::default(), env(&[(URL, "  ")])).unwrap(), None);
        assert_eq!(store.setting("model").unwrap(), None);
    }

    #[test]
    fn only_a_model_name_keeps_the_fallback_server() {
        let store = Store::memory().unwrap();
        let toml = Model { url: "http://toml/v1".into(), ..Model::default() };
        let said = model_from(&store, &toml, env(&[(MODEL, "m")])).unwrap().unwrap();
        assert!(said.contains("key not set"), "{said}");
        assert_eq!((saved(&store).url.as_str(), saved(&store).model.as_str()), ("http://toml/v1", "m"));
    }
}
