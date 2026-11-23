use crate::store::Store;

pub const DAY_MS: i64 = 86_400_000;

fn day_key(now: i64) -> String {
    format!("untrusted_turns_{}", now.div_euclid(DAY_MS))
}

pub fn turns_today(store: &Store, now: i64) -> u64 {
    store.setting(&day_key(now)).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(0)
}
