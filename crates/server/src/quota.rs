use agentbench_core::{Quota, Usage};
use reqwest::header::HeaderMap;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq, Deserialize)]
pub struct Price {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct Prices(HashMap<String, Price>);

const SHIPPED: &[(&str, Price)] = &[
    ("claude-opus-4-1", Price { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-opus-4-0", Price { input: 15.0, output: 75.0, cache_read: 1.5, cache_write: 18.75 }),
    ("claude-opus-4-5", Price { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 }),
    ("claude-opus-4-6", Price { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 }),
    ("claude-opus", Price { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25 }),
    ("claude-sonnet", Price { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 }),
    ("claude-haiku-4", Price { input: 1.0, output: 5.0, cache_read: 0.1, cache_write: 1.25 }),
    ("claude-3-5-haiku", Price { input: 0.8, output: 4.0, cache_read: 0.08, cache_write: 1.0 }),
    ("claude-3-haiku", Price { input: 0.25, output: 1.25, cache_read: 0.03, cache_write: 0.3 }),
];

impl Prices {
    pub fn new(table: impl IntoIterator<Item = (String, Price)>) -> Prices {
        Prices(table.into_iter().collect())
    }

    pub fn of(&self, model: &str) -> Option<Price> {
        let model = model.trim().to_ascii_lowercase();
        let own = self
            .0
            .iter()
            .filter(|(k, _)| model.starts_with(&k.to_ascii_lowercase()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, p)| *p);
        own.or_else(|| {
            let name = model.rsplit('/').next().unwrap_or(&model);
            SHIPPED
                .iter()
                .filter(|(k, _)| name.starts_with(k))
                .max_by_key(|(k, _)| k.len())
                .map(|(_, p)| *p)
        })
    }

    pub fn cost(&self, model: &str, input: u64, output: u64, cache_read: u64, cache_write: u64) -> f64 {
        let Some(p) = self.of(model) else { return 0.0 };
        (input as f64 * p.input
            + output as f64 * p.output
            + cache_read as f64 * p.cache_read
            + cache_write as f64 * p.cache_write)
            / 1_000_000.0
    }
}

pub fn now() -> i64 {
    crate::store::now()
}

#[derive(Clone, Debug, Default)]
pub struct Clock(Option<Arc<std::sync::atomic::AtomicI64>>);

impl Clock {
    pub fn fake(at: i64) -> Clock {
        Clock(Some(Arc::new(std::sync::atomic::AtomicI64::new(at))))
    }

    pub fn now(&self) -> i64 {
        match &self.0 {
            Some(t) => t.load(std::sync::atomic::Ordering::SeqCst),
            None => now(),
        }
    }

    pub async fn sleep(&self, d: Duration) {
        match &self.0 {
            Some(t) => {
                t.fetch_add(d.as_millis() as i64, std::sync::atomic::Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
            None => tokio::time::sleep(d).await,
        }
    }
}

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

pub fn base_of(url: &str) -> String {
    let base = url.trim().trim_end_matches('/');
    base.strip_suffix("/chat/completions").unwrap_or(base).to_string()
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub fn rfc3339_ms(text: &str) -> Option<i64> {
    let t = text.trim();
    let (date, rest) = t.split_once(['T', ' '])?;
    let mut d = date.split('-');
    let (y, m, day) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
    );
    let split = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (clock, zone) = rest.split_at(split);
    let mut c = clock.split(':');
    let h = c.next()?.parse::<i64>().ok()?;
    let mi = c.next()?.parse::<i64>().ok()?;
    let sec = c.next().map(|s| s.parse::<f64>()).transpose().ok()?.unwrap_or(0.0);
    let offset = match zone.as_bytes().first() {
        Some(b'+' | b'-') => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let digits: String = zone[1..].chars().filter(|c| c.is_ascii_digit()).collect();
            let (oh, om) = (digits.get(..2)?.parse::<i64>().ok()?, digits.get(2..4).map_or(Some(0), |s| s.parse::<i64>().ok())?);
            sign * (oh * 60 + om)
        }
        _ => 0,
    };
    let secs = days_from_civil(y, m, day) * 86_400 + h * 3600 + mi * 60 - offset * 60;
    Some(secs * 1000 + (sec * 1000.0) as i64)
}

pub fn time_ms(v: &Value, now: i64) -> Option<i64> {
    let number = |n: f64| {
        if n >= 1e11 {
            n as i64
        } else if n >= 1e9 {
            (n * 1000.0) as i64
        } else {
            now + (n * 1000.0) as i64
        }
    };
    match v {
        Value::Number(n) => n.as_f64().map(number),
        Value::String(s) => s.trim().parse::<f64>().ok().map(number).or_else(|| rfc3339_ms(s)),
        _ => None,
    }
}

fn percent(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches('%').parse().ok(),
        _ => None,
    }
}

fn quota_of(v: &Value, host: &str, now: i64) -> Option<Quota> {
    let used_pct = percent(v.get("used_pct")?)?;
    Some(Quota {
        host: host.to_string(),
        model: v.get("model").and_then(Value::as_str).unwrap_or_default().trim().to_string(),
        window: v.get("window").and_then(Value::as_str).unwrap_or("quota").to_string(),
        used_pct,
        resets_at: v.get("resets_at").and_then(|t| time_ms(t, now)).unwrap_or(0),
        next_available_at: v.get("next_available_at").and_then(|t| time_ms(t, now)),
        updated_at: now,
    })
}

pub fn from_usage(usage: &Value, host: &str, now: i64) -> Option<Quota> {
    quota_of(usage.get("quota")?, host, now)
}

fn model_entry(entry: &Value, host: &str, model: &str, now: i64) -> Option<Quota> {
    let summary = entry.get("summary")?;
    let mut q = quota_of(summary, host, now)?;
    q.model = model.to_string();
    if summary.get("exhausted").and_then(Value::as_bool) == Some(true) {
        q.used_pct = q.used_pct.max(100.0);
    }
    Some(q)
}

pub fn from_models(body: &Value, host: &str, now: i64) -> Vec<Quota> {
    let Some(models) = body.get("models").and_then(Value::as_object) else { return Vec::new() };
    let mut all: Vec<Quota> =
        models.iter().filter_map(|(name, e)| model_entry(e, host, name, now)).collect();
    all.sort_by(|a, b| a.model.cmp(&b.model));
    all
}

pub fn for_model(body: &Value, host: &str, model: &str, now: i64) -> Option<Quota> {
    let named = body.pointer(&format!("/models/{}", model.replace('~', "~0").replace('/', "~1")));
    if let Some(q) = named.and_then(|e| model_entry(e, host, model, now)) {
        return Some(q);
    }
    from_status(body, host, now).map(|q| Quota { model: model.to_string(), ..q })
}

pub fn from_status(body: &Value, host: &str, now: i64) -> Option<Quota> {
    let found = |v: &Value| quota_of(v, host, now).map(|q| Quota {
        next_available_at: q
            .next_available_at
            .or_else(|| v.get("next_available_at").and_then(|t| time_ms(t, now))),
        ..q
    });
    let top = body.get("next_available_at").and_then(|t| time_ms(t, now));
    let windows = body.get("windows").and_then(Value::as_array);
    let mut best = windows
        .into_iter()
        .flatten()
        .filter_map(found)
        .max_by(|a, b| a.used_pct.total_cmp(&b.used_pct))
        .or_else(|| found(body))
        .or_else(|| body.get("summary").and_then(found))
        .or_else(|| body.get("quota").and_then(found))?;
    if top.is_some() {
        best.next_available_at = top;
    }
    Some(best)
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)?.to_str().ok()
}

pub fn from_headers(h: &HeaderMap, host: &str, now: i64) -> Option<Quota> {
    let used = header(h, "x-yalr-quota-used-pct")?;
    let used_pct = percent(&Value::String(used.into()))?;
    let resets = header(h, "x-yalr-quota-resets-at")
        .and_then(|t| time_ms(&Value::String(t.into()), now))
        .unwrap_or(0);
    let exhausted = header(h, "x-yalr-quota-exhausted").is_some_and(|v| v.trim().eq_ignore_ascii_case("true"));
    Some(Quota {
        host: host.to_string(),
        model: header(h, "x-yalr-quota-model").unwrap_or_default().trim().to_string(),
        window: header(h, "x-yalr-quota-window").unwrap_or("quota").to_string(),
        used_pct: if exhausted { used_pct.max(100.0) } else { used_pct },
        resets_at: resets,
        next_available_at: header(h, "x-yalr-quota-next-available-at")
            .and_then(|t| time_ms(&Value::String(t.into()), now)),
        updated_at: now,
    })
}

pub fn cost_from_headers(h: &HeaderMap) -> Option<f64> {
    header(h, "x-yalr-cost-usd")?.trim().parse().ok()
}

pub fn usage_of(v: &Value, host: &str, now: i64) -> Option<Usage> {
    let u = v.get("usage").filter(|u| !u.is_null())?;
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cached =
        u.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        input_tokens: n("prompt_tokens").saturating_sub(cached),
        output_tokens: n("completion_tokens"),
        cache_read_tokens: cached,
        cost_usd: u.get("cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
        turns: 1,
        quota: from_usage(u, host, now),
        ..Usage::default()
    })
}

pub fn cache_write_of(v: &Value) -> u64 {
    let Some(u) = v.get("usage") else { return 0 };
    u.pointer("/prompt_tokens_details/cache_write_tokens")
        .or_else(|| u.get("cache_creation_input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

pub fn apply_headers(usage: &mut Usage, h: &HeaderMap, host: &str, now: i64) {
    if usage.cost_usd <= 0.0
        && let Some(c) = cost_from_headers(h)
    {
        usage.cost_usd = c;
    }
    if usage.quota.is_none() {
        usage.quota = from_headers(h, host, now);
    }
}

pub fn retry_after_ms(h: &HeaderMap, body: &str) -> Option<i64> {
    if let Some(v) = header(h, "retry-after") {
        if let Ok(secs) = v.trim().parse::<f64>() {
            return Some((secs * 1000.0) as i64);
        }
    }
    let lower = body.to_ascii_lowercase();
    let at = lower.find("retry after:")? + "retry after:".len();
    let digits: String =
        lower[at..].trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: i64 = digits.parse().ok()?;
    let rest = lower[at..].trim_start()[digits.len()..].trim_start();
    Some(if rest.starts_with("ms") { n } else { n * 1000 })
}

async fn status(base: &str, key: &str, model: &str, timeout: Duration) -> Option<Value> {
    let mut url = format!("{}/quota", base.trim_end_matches('/'));
    if !model.is_empty() {
        let escaped: String = model
            .bytes()
            .map(|b| match b {
                b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect();
        url.push_str(&format!("?model={escaped}"));
    }
    let mut req = crate::http::client().get(url).timeout(timeout);
    if !key.trim().is_empty() {
        req = req.bearer_auth(key.trim());
    }
    let resp = req.send().await.ok()?.error_for_status().ok()?;
    resp.json().await.ok()
}

pub async fn fetch(base: &str, key: &str, host: &str, model: &str, timeout: Duration) -> Option<Quota> {
    let body = status(base, key, model, timeout).await?;
    if model.is_empty() {
        from_status(&body, host, now())
    } else {
        for_model(&body, host, model, now())
    }
}

pub const POLL_EVERY: Duration = Duration::from_secs(180);

pub struct Target {
    pub base: String,
    pub key: String,
    pub models: Vec<String>,
}

pub fn servers(model: &crate::agent::native::Model) -> Vec<Target> {
    let mut all: Vec<(String, String, String)> = Vec::new();
    let mut add = |url: &str, key: &str, name: &str| {
        if !url.trim().is_empty() && !name.trim().is_empty() {
            all.push((base_of(url), key.to_string(), name.trim().to_string()));
        }
    };
    add(&model.url, &model.key, &model.model);
    for name in agentbench_core::LEVELS {
        if let Some(m) = model.level(name) {
            add(&m.url, &m.key, &m.model);
        }
    }
    all.sort();
    all.dedup_by(|a, b| a.0 == b.0 && a.2 == b.2);
    let mut out: Vec<Target> = Vec::new();
    for (base, key, name) in all {
        match out.iter_mut().find(|t| t.base == base) {
            Some(t) => t.models.push(name),
            None => out.push(Target { base, key, models: vec![name] }),
        }
    }
    out
}

pub fn readings(body: &Value, host: &str, models: &[String], now: i64) -> Vec<Quota> {
    let listed = from_models(body, host, now);
    if !listed.is_empty() {
        return listed.into_iter().filter(|q| models.contains(&q.model)).collect();
    }
    match from_status(body, host, now) {
        Some(q) => models.iter().map(|m| Quota { model: m.clone(), ..q.clone() }).collect(),
        None => Vec::new(),
    }
}

pub async fn poll_once(runner: &crate::runner::Runner, model: &crate::agent::native::Model) {
    runner.quotas.set_base(model);
    runner.quotas.set_start_model(
        &model.level("medium").map_or_else(|| model.model.clone(), |m| m.model),
    );
    for target in servers(model) {
        let host = agentbench_core::host_of(&target.base);
        let Some(body) = status(&target.base, &target.key, "", Duration::from_secs(5)).await else {
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

pub fn spawn_poll(runner: crate::runner::Runner, model: crate::agent::native::Shared) {
    tokio::spawn(async move {
        loop {
            poll_once(&runner, &model.get()).await;
            tokio::time::sleep(POLL_EVERY).await;
        }
    });
}

#[derive(Default)]
struct Held {
    quotas: HashMap<String, Quota>,
    announced: HashMap<String, (i64, u8)>,
    reset_told: HashMap<String, i64>,
    sent: HashMap<(agentbench_core::SessionId, String), (i64, bool)>,
    base: Option<crate::agent::native::Model>,
    blocked: HashMap<String, i64>,
    start_model: String,
}

const SAME_WINDOW_DRIFT_MS: i64 = 5 * 60_000;
const NOTICE_EVERY_MS: i64 = 30 * 60_000;

#[derive(Clone, Default)]
pub struct Quotas(Arc<Mutex<Held>>);

pub struct Hint {
    pub text: String,
    pub model: String,
    pub exhausted: bool,
}

fn key_of(q: &Quota) -> String {
    format!("{}\n{}", q.host, q.model)
}

fn window_key(q: &Quota) -> String {
    format!("{}\n{}\n{}", q.host, q.model, q.window)
}

fn same_window(old: i64, new: i64, now: i64) -> bool {
    if old == 0 || new == 0 {
        return true;
    }
    now < old && (new - old).abs() <= SAME_WINDOW_DRIFT_MS
}

impl Quotas {
    fn map(&self) -> std::sync::MutexGuard<'_, Held> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_start_model(&self, model: &str) {
        self.map().start_model = model.trim().to_string();
    }

    pub fn set_base(&self, model: &crate::agent::native::Model) {
        self.map().base = Some(model.clone());
    }

    pub fn resolves_to(&self, chosen: Option<&str>, model: &str) -> bool {
        if model.is_empty() {
            return true;
        }
        match &self.map().base {
            Some(base) => base.choose(chosen.unwrap_or_default()).model == model,
            None => chosen == Some(model),
        }
    }

    pub fn may_notify(&self, session: agentbench_core::SessionId, model: &str, exhausted: bool, now: i64) -> bool {
        let mut held = self.map();
        let key = (session, model.to_string());
        let allowed = match held.sent.get(&key) {
            None => true,
            Some((at, was_exhausted)) => {
                now - at >= NOTICE_EVERY_MS || (exhausted && !was_exhausted)
            }
        };
        if allowed {
            held.sent.insert(key, (now, exhausted));
        }
        allowed
    }

    pub fn start_model(&self) -> String {
        self.map().start_model.clone()
    }

    pub fn block(&self, model: &str, until: i64) {
        let mut held = self.map();
        let at = held.blocked.entry(model.to_string()).or_insert(0);
        *at = (*at).max(until);
    }

    pub fn unblock(&self, now: i64) {
        self.map().blocked.retain(|_, t| *t > now);
    }

    pub fn blocked_until(&self, model: &str, now: i64) -> Option<i64> {
        self.map()
            .blocked
            .iter()
            .filter(|(m, _)| m.is_empty() || model.is_empty() || m.as_str() == model)
            .map(|(_, t)| *t)
            .filter(|t| *t > now)
            .max()
    }

    pub fn observe(&self, quota: Quota, now: i64) -> Option<Hint> {
        let mut held = self.map();
        if quota.exhausted_until(now).is_none() {
            held.blocked.remove(&quota.model);
        }
        let key = key_of(&quota);
        let wkey = window_key(&quota);
        let seen = held.announced.get(&wkey).copied();
        let level = quota.level(now);
        let announced = match seen {
            Some((resets, announced)) if same_window(resets, quota.resets_at, now) => announced,
            _ => 0,
        };
        let hint = (level > announced).then(|| Hint {
            text: quota.hint(now),
            model: quota.model.clone(),
            exhausted: quota.used_pct >= 100.0 || quota.exhausted_until(now).is_some(),
        });
        let kept = if level == 0 { 0 } else { announced.max(level) };
        let anchor = match seen {
            Some((resets, _)) if resets != 0 && same_window(resets, quota.resets_at, now) => resets,
            _ => quota.resets_at,
        };
        held.announced.insert(wkey, (anchor, kept));
        held.quotas.insert(key, quota);
        hint
    }

    pub fn set(&self, quota: Quota) {
        self.map().quotas.insert(key_of(&quota), quota);
    }

    pub fn all(&self) -> Vec<Quota> {
        let mut all: Vec<Quota> = self.map().quotas.values().cloned().collect();
        all.sort_by(|a, b| (&a.host, &a.model).cmp(&(&b.host, &b.model)));
        all
    }

    pub fn exhausted_until(&self, model: &str, now: i64) -> Option<i64> {
        self.all()
            .iter()
            .filter(|q| q.applies_to(model))
            .filter_map(|q| q.exhausted_until(now))
            .chain(self.blocked_until(model, now))
            .max()
    }

    pub fn expire(&self, now: i64) -> Vec<Quota> {
        let mut held = self.map();
        let gone: Vec<String> = held
            .quotas
            .iter()
            .filter(|(_, q)| q.expired(now))
            .map(|(h, _)| h.clone())
            .collect();
        let gone: Vec<Quota> = gone.iter().filter_map(|h| held.quotas.remove(h)).collect();
        gone.into_iter()
            .filter(|q| {
                let wkey = window_key(q);
                match held.reset_told.get(&wkey) {
                    Some(told) if q.resets_at <= told + SAME_WINDOW_DRIFT_MS => false,
                    _ => {
                        held.reset_told.insert(wkey, q.resets_at);
                        true
                    }
                }
            })
            .collect()
    }

    pub fn summary(&self, now: i64) -> String {
        let mut out: String = self
            .all()
            .iter()
            .filter(|q| !q.expired(now))
            .map(|q| match q.model.is_empty() {
                true => format!("- {}: {}\n", q.host, q.label(now)),
                false => format!("- {} {}\n", q.host, q.label(now)),
            })
            .collect();
        let mut blocked: Vec<(String, i64)> =
            self.map().blocked.iter().filter(|(_, t)| **t > now).map(|(m, t)| (m.clone(), *t)).collect();
        blocked.sort();
        for (model, t) in blocked {
            let who = if model.is_empty() { String::new() } else { format!(" on {model}") };
            out.push_str(&format!(
                "- exhausted{who}: no new workers start{who} until {} (in {})\n",
                agentbench_core::clock_hm(t),
                agentbench_core::span(t - now)
            ));
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    pub(crate) struct Reply {
        pub status: u16,
        pub headers: Vec<(&'static str, String)>,
        pub body: String,
    }

    pub(crate) fn ok(body: String) -> Reply {
        Reply { status: 200, headers: vec![], body }
    }

    pub(crate) fn server(
        completions: Vec<Reply>,
        quota: Option<String>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let paths = Arc::new(Mutex::new(Vec::new()));
        let log = paths.clone();
        let queue = Mutex::new(completions.into_iter().collect::<std::collections::VecDeque<_>>());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { return };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let path = loop {
                    let n = s.read(&mut chunk).unwrap_or(0);
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(split) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..split]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= split + 4 + len {
                            break head.split_whitespace().nth(1).unwrap_or_default().to_string();
                        }
                    }
                    if n == 0 {
                        break String::new();
                    }
                };
                log.lock().unwrap().push(path.clone());
                let reply = if path.split('?').next().unwrap_or_default().ends_with("/quota") {
                    match &quota {
                        Some(b) => ok(b.clone()),
                        None => Reply { status: 404, headers: vec![], body: "{}".into() },
                    }
                } else {
                    match queue.lock().unwrap().pop_front() {
                        Some(r) => r,
                        None => return,
                    }
                };
                let kind = if reply.body.starts_with("data:") {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let extra: String =
                    reply.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                let _ = write!(
                    s,
                    "HTTP/1.1 {} X\r\ncontent-type: {kind}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
            }
        });
        (url, paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_hint_is_sent_once_per_window_at_75_and_again_at_90_and_cleared_on_reset() {
        let held = Quotas::default();
        let q = |pct: f64, resets: i64| Quota {
            host: "h".into(),
            window: "5h".into(),
            used_pct: pct,
            resets_at: resets,
            ..Quota::default()
        };
        let reset = 5 * 3_600_000;
        assert!(held.observe(q(50.0, reset), 1000).is_none());
        assert!(held.observe(q(76.0, reset), 2000).is_some());
        assert!(held.observe(q(78.0, reset), 3000).is_none());
        assert!(held.observe(q(91.0, reset), 4000).is_some());
        assert!(held.observe(q(95.0, reset), 5000).is_none());
        assert_eq!(held.expire(reset).len(), 1);
        assert!(held.all().is_empty());
        let next = reset + 5 * 3_600_000;
        assert!(held.observe(q(80.0, next), reset + 10).is_some());
        assert!(held.observe(q(80.0, next), reset + 20).is_none());
    }

    #[test]
    fn the_hint_comes_again_when_the_window_rolls_over_without_an_expiry_pass() {
        let held = Quotas::default();
        let q = |resets: i64| Quota {
            host: "h".into(),
            window: "5h".into(),
            used_pct: 80.0,
            resets_at: resets,
            ..Quota::default()
        };
        assert!(held.observe(q(10_000), 1).is_some());
        assert!(held.observe(q(10_000), 2).is_none());
        assert!(held.observe(q(20_000), 10_001).is_some());
    }

    #[test]
    fn an_exhausted_quota_blocks_until_its_reset() {
        let held = Quotas::default();
        held.set(Quota {
            host: "h".into(),
            window: "5h".into(),
            used_pct: 100.0,
            resets_at: 9_000,
            ..Quota::default()
        });
        assert_eq!(held.exhausted_until("", 1_000), Some(9_000));
        assert_eq!(held.exhausted_until("", 9_000), None);
        held.block("", 12_000);
        assert_eq!(held.exhausted_until("", 9_500), Some(12_000));
        held.unblock(12_000);
        assert_eq!(held.exhausted_until("", 12_000), None);
    }

    #[test]
    fn usage_reads_cost_and_quota_from_a_response() {
        let v = json!({"usage": {
            "prompt_tokens": 100, "completion_tokens": 7, "cost_usd": 0.0123,
            "prompt_tokens_details": {"cached_tokens": 40},
            "quota": {"window": "5h", "used_pct": 62.5, "resets_at": "2023-11-14T22:13:20Z"}}});
        let u = usage_of(&v, "h", 0).unwrap();
        assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_tokens), (60, 7, 40));
        assert_eq!(u.cost_usd, 0.0123);
        let q = u.quota.unwrap();
        assert_eq!((q.window.as_str(), q.used_pct, q.resets_at), ("5h", 62.5, 1_700_000_000_000));
        assert!(usage_of(&json!({"choices": []}), "h", 0).is_none());
    }

    #[test]
    fn headers_fill_in_what_the_body_left_out() {
        let mut h = HeaderMap::new();
        h.insert("x-yalr-cost-usd", "0.5".parse().unwrap());
        h.insert("x-yalr-quota-window", "5h".parse().unwrap());
        h.insert("x-yalr-quota-used-pct", "81".parse().unwrap());
        h.insert("x-yalr-quota-resets-at", "1700000000".parse().unwrap());
        let mut u = Usage::default();
        apply_headers(&mut u, &h, "h", 0);
        assert_eq!(u.cost_usd, 0.5);
        assert_eq!(u.quota.as_ref().map(|q| (q.used_pct, q.resets_at)), Some((81.0, 1_700_000_000_000)));
        let mut own = Usage { cost_usd: 2.0, ..Usage::default() };
        apply_headers(&mut own, &h, "h", 0);
        assert_eq!(own.cost_usd, 2.0);
    }

    #[test]
    fn the_shipped_table_prices_claude_and_leaves_local_models_free() {
        let p = Prices::default();
        let c = p.cost("claude-sonnet-4-5", 1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert!((c - (3.0 + 15.0 + 0.3 + 3.75)).abs() < 1e-9);
        assert_eq!(p.cost("qwen3-coder", 5000, 5000, 0, 0), 0.0);
        assert!(p.of("anthropic/claude-opus-4-5-20251101").is_some());
    }

    #[test]
    fn a_configured_price_beats_the_shipped_one_by_longest_prefix() {
        let p = Prices::new([
            ("claude-sonnet".into(), Price { input: 1.0, ..Price::default() }),
            ("claude-sonnet-4-5".into(), Price { input: 2.0, ..Price::default() }),
            ("local".into(), Price { output: 7.0, ..Price::default() }),
        ]);
        assert_eq!(p.cost("claude-sonnet-4-5", 1_000_000, 0, 0, 0), 2.0);
        assert_eq!(p.cost("claude-sonnet-4", 1_000_000, 0, 0, 0), 1.0);
        assert_eq!(p.cost("local-7b", 0, 1_000_000, 0, 0), 7.0);
    }

    #[test]
    fn prices_load_from_toml() {
        let p: Prices = toml::from_str("[claude-x]\ninput = 1.5\noutput = 2.5\n").unwrap();
        assert_eq!(p.of("claude-x").unwrap().output, 2.5);
    }

    #[test]
    fn times_read_as_seconds_millis_or_iso() {
        let now = 1_000;
        assert_eq!(time_ms(&json!(1_700_000_000), now), Some(1_700_000_000_000));
        assert_eq!(time_ms(&json!(1_700_000_000_000i64), now), Some(1_700_000_000_000));
        assert_eq!(time_ms(&json!("2023-11-14T22:13:20Z"), now), Some(1_700_000_000_000));
        assert_eq!(time_ms(&json!("2023-11-15T00:13:20+02:00"), now), Some(1_700_000_000_000));
        assert_eq!(time_ms(&json!(60), now), Some(61_000));
    }

    #[test]
    fn retry_after_reads_the_header_or_the_message() {
        let mut h = HeaderMap::new();
        assert_eq!(retry_after_ms(&h, "Rate limit exceeded. Retry after: 387042000ms"), Some(387_042_000));
        h.insert("retry-after", "30".parse().unwrap());
        assert_eq!(retry_after_ms(&h, ""), Some(30_000));
        assert_eq!(retry_after_ms(&HeaderMap::new(), "nope"), None);
    }

    #[test]
    fn a_status_body_with_windows_picks_the_fullest() {
        let body = json!({"windows":[
            {"window":"5h","used_pct":62,"resets_at":1_700_000_000},
            {"window":"7d","used_pct":10,"resets_at":1_700_900_000}],
            "next_available_at": 1_700_000_500});
        let q = from_status(&body, "h", 0).unwrap();
        assert_eq!((q.window.as_str(), q.used_pct), ("5h", 62.0));
        assert_eq!(q.next_available_at, Some(1_700_000_500_000));
    }

    #[test]
    fn the_hint_and_label_name_the_reset() {
        let q = Quota {
            window: "5h".into(),
            used_pct: 78.0,
            resets_at: 14 * 3_600_000 + 30 * 60_000,
            ..Quota::default()
        };
        let now = 12 * 3_600_000 + 20 * 60_000;
        assert_eq!(q.hint(now), "Quota 5h at 78%, resets at 14:30 UTC (in 2h 10m)");
        assert_eq!(q.label(now), "5h 78%, resets 14:30 UTC");
        assert_eq!(q.level(now), 1);
        assert_eq!(Quota { used_pct: 91.0, ..q.clone() }.level(now), 2);
        assert_eq!(q.level(q.resets_at), 0);
    }

    fn two_models() -> String {
        json!({
            "summary": {"used_pct": 100, "window": "7d", "resets_at": 1_900_000_000},
            "models": {
                "code-x-high": {"summary": {"used_pct": 100, "window": "7d", "resets_at": 1_900_000_000, "exhausted": true, "next_available_at": 1_900_000_000}, "windows": []},
                "code-high": {"summary": {"used_pct": 17, "window": "7d", "resets_at": 1_900_000_000, "exhausted": false, "next_available_at": null}, "windows": []}
            }})
        .to_string()
    }

    fn levels(url: &str) -> crate::agent::native::Model {
        use crate::agent::native::{Level, Model};
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

    fn fine(model: &str) -> Quota {
        Quota { model: model.into(), window: "5h".into(), used_pct: 35.0, resets_at: 1_900_000_000_000, ..Quota::default() }
    }

    #[test]
    fn a_pause_takes_the_earliest_time_the_model_is_free_not_the_7d_reset() {
        let body = json!({
            "next_available_at": 1_900_001_800,
            "windows": [
                {"window": "7d", "used_pct": 100, "resets_at": 1_900_500_000, "next_available_at": 1_900_500_000},
                {"window": "5h", "used_pct": 96, "resets_at": 1_900_003_600}
            ]});
        let q = from_status(&body, "h", 1_800_000_000_000).unwrap();
        assert_eq!(q.exhausted_until(1_800_000_000_000), Some(1_900_001_800_000));
    }

    #[test]
    fn a_fresh_reading_that_is_not_exhausted_clears_that_models_block() {
        let quotas = Quotas::default();
        let now = 1_800_000_000_000;
        quotas.block("code-high", now + 3 * 86_400_000);
        quotas.block("code-x-high", now + 3 * 86_400_000);
        assert!(quotas.exhausted_until("code-high", now).is_some());
        quotas.observe(fine("code-high"), now);
        assert_eq!(quotas.exhausted_until("code-high", now), None);
        assert!(quotas.exhausted_until("code-x-high", now).is_some(), "other models keep their block");
    }

    #[test]
    fn a_retry_after_block_holds_without_a_newer_reading() {
        let quotas = Quotas::default();
        let now = 1_800_000_000_000;
        quotas.block("code-high", now + 60_000);
        assert_eq!(quotas.exhausted_until("code-high", now), Some(now + 60_000));
        assert_eq!(quotas.exhausted_until("code-high", now + 60_001), None);
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

    #[test]
    fn a_response_names_its_model_from_the_body_or_the_header() {
        let v = json!({"usage": {"quota": {"model": "code-x-high", "window": "7d", "used_pct": 50, "resets_at": 1_900_000_000}}});
        assert_eq!(from_usage(&v["usage"], "h", 0).unwrap().model, "code-x-high");
        let mut h = HeaderMap::new();
        h.insert("x-yalr-quota-used-pct", "20".parse().unwrap());
        h.insert("x-yalr-quota-model", "code-high".parse().unwrap());
        assert_eq!(from_headers(&h, "h", 0).unwrap().model, "code-high");
        h.insert("x-yalr-quota-exhausted", "true".parse().unwrap());
        h.insert("x-yalr-quota-next-available-at", "1900000000".parse().unwrap());
        let q = from_headers(&h, "h", 0).unwrap();
        assert_eq!((q.used_pct, q.next_available_at), (100.0, Some(1_900_000_000_000)));
    }

    #[test]
    fn the_hint_and_label_name_the_model() {
        let q = Quota { model: "code-x-high".into(), window: "7d".into(), used_pct: 100.0, resets_at: 16 * 3_600_000, ..Quota::default() };
        assert_eq!(q.label(0), "code-x-high: 7d 100%, exhausted until 16:00 UTC");
        assert!(q.hint(0).starts_with("Quota code-x-high 7d at 100%"));
    }
}
