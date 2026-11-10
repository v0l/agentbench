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
        .or_else(|| body.get("quota").and_then(found))?;
    if best.next_available_at.is_none() {
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
    Some(Quota {
        host: host.to_string(),
        window: header(h, "x-yalr-quota-window").unwrap_or("quota").to_string(),
        used_pct,
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

pub async fn fetch(base: &str, key: &str, host: &str, timeout: Duration) -> Option<Quota> {
    let mut req = crate::http::client().get(format!("{}/quota", base.trim_end_matches('/'))).timeout(timeout);
    if !key.trim().is_empty() {
        req = req.bearer_auth(key.trim());
    }
    let resp = req.send().await.ok()?.error_for_status().ok()?;
    let body: Value = resp.json().await.ok()?;
    from_status(&body, host, now())
}

pub const POLL_EVERY: Duration = Duration::from_secs(180);

pub fn servers(model: &crate::agent::native::Model) -> Vec<(String, String)> {
    let mut all = vec![(model.url.clone(), model.key.clone())];
    for level in [&model.low, &model.medium, &model.high] {
        if !level.url.trim().is_empty() {
            all.push((level.url.clone(), level.key.clone()));
        }
    }
    all.retain(|(u, _)| !u.trim().is_empty());
    all.iter_mut().for_each(|(u, _)| *u = base_of(u));
    all.sort();
    all.dedup_by(|a, b| a.0 == b.0);
    all
}

pub async fn poll_once(runner: &crate::runner::Runner, model: &crate::agent::native::Model) {
    for (base, key) in servers(model) {
        let host = agentbench_core::host_of(&base);
        let now = now();
        if let Some(q) = fetch(&base, &key, &host, Duration::from_secs(5)).await {
            let block = q.exhausted_until(now);
            runner.observe_quota(q);
            if let Some(t) = block {
                runner.quotas.block(t);
            }
        }
    }
    runner.expire_quotas();
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
    blocked: i64,
}

#[derive(Clone, Default)]
pub struct Quotas(Arc<Mutex<Held>>);

pub struct Hint {
    pub text: String,
}

impl Quotas {
    fn map(&self) -> std::sync::MutexGuard<'_, Held> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn block(&self, until: i64) {
        let mut held = self.map();
        held.blocked = held.blocked.max(until);
    }

    pub fn unblock(&self, now: i64) {
        let mut held = self.map();
        if held.blocked <= now {
            held.blocked = 0;
        }
    }

    pub fn blocked_until(&self, now: i64) -> Option<i64> {
        Some(self.map().blocked).filter(|t| *t > now)
    }

    pub fn observe(&self, quota: Quota, now: i64) -> Option<Hint> {
        let mut held = self.map();
        let host = quota.host.clone();
        let seen = held.announced.get(&host).copied();
        let level = quota.level(now);
        let announced = match seen {
            Some((resets, announced)) if resets == quota.resets_at => announced,
            _ => 0,
        };
        let hint = (level > announced).then(|| Hint { text: quota.hint(now) });
        held.announced.insert(host.clone(), (quota.resets_at, announced.max(level)));
        held.quotas.insert(host, quota);
        hint
    }

    pub fn set(&self, quota: Quota) {
        self.map().quotas.insert(quota.host.clone(), quota);
    }

    pub fn all(&self) -> Vec<Quota> {
        let mut all: Vec<Quota> = self.map().quotas.values().cloned().collect();
        all.sort_by(|a, b| a.host.cmp(&b.host));
        all
    }

    pub fn exhausted_until(&self, now: i64) -> Option<i64> {
        self.all()
            .iter()
            .filter_map(|q| q.exhausted_until(now))
            .chain(self.blocked_until(now))
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
        for h in &gone {
            held.announced.remove(h);
        }
        gone.iter().filter_map(|h| held.quotas.remove(h)).collect()
    }

    pub fn summary(&self, now: i64) -> String {
        let mut out: String = self
            .all()
            .iter()
            .filter(|q| !q.expired(now))
            .map(|q| format!("- {}: {}\n", q.host, q.label(now)))
            .collect();
        if let Some(t) = self.blocked_until(now) {
            out.push_str(&format!(
                "- exhausted: no new workers start until {} (in {})\n",
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
                let reply = if path.ends_with("/quota") {
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
        assert_eq!(held.exhausted_until(1_000), Some(9_000));
        assert_eq!(held.exhausted_until(9_000), None);
        held.block(12_000);
        assert_eq!(held.exhausted_until(9_500), Some(12_000));
        held.unblock(12_000);
        assert_eq!(held.exhausted_until(12_000), None);
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
}
