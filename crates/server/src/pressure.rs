use crate::store::Store;
use agentbench_core::{HostPressure, PressureReadings, PressureState, SessionId};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

pub const SETTING: &str = "autopilot_pressure";
pub const DEVICES: &str = "autopilot_pressure_devices";
pub const SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_secs(10);
const GB: f64 = 1024.0 * 1024.0 * 1024.0;

pub const PAUSE_TEXT: &str = "the host is under heavy IO or memory pressure: pause test and build runs, commit what you have, resume when told";
pub const RESUME_TEXT: &str = "the host pressure has eased: you can resume test and build runs";

pub const NOTICEABLE_STALL_PCT: f32 = 5.0;
const NORMAL_PERCENTILE: f32 = 0.95;

#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    pub stall_pct: f32,
    pub swap_full_pct: f32,
    pub history_ms: i64,
    pub resident_ms: i64,
    pub start_grace_ms: i64,
    pub hold_ms: i64,
    pub critical_hold_ms: i64,
    pub cooldown_ms: i64,
    pub step_ms: i64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            stall_pct: 0.0,
            swap_full_pct: 85.0,
            history_ms: 86_400_000,
            resident_ms: 600_000,
            start_grace_ms: 60_000,
            hold_ms: 60_000,
            critical_hold_ms: 30_000,
            cooldown_ms: 300_000,
            step_ms: 120_000,
        }
    }
}

pub fn enabled(store: &Store) -> bool {
    !matches!(
        store.setting(SETTING).ok().flatten().map(|v| v.trim().to_lowercase()).as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

pub fn limits(store: &Store) -> Limits {
    let d = Limits::default();
    let f = |k: &str, v: f32| crate::autopilot::number(store, &format!("{SETTING}_{k}"), v);
    let secs = |k: &str, v: i64| crate::autopilot::number(store, &format!("{SETTING}_{k}"), v / 1000) * 1000;
    Limits {
        stall_pct: f("stall_pct", d.stall_pct),
        swap_full_pct: f("swap_full_pct", d.swap_full_pct),
        history_ms: secs("history_secs", d.history_ms),
        resident_ms: secs("resident_secs", d.resident_ms),
        start_grace_ms: secs("start_grace_secs", d.start_grace_ms),
        hold_ms: secs("hold_secs", d.hold_ms),
        critical_hold_ms: secs("critical_hold_secs", d.critical_hold_ms),
        cooldown_ms: secs("cooldown_secs", d.cooldown_ms),
        step_ms: secs("recover_secs", d.step_ms).max(1000),
    }
}

pub fn devices(store: &Store) -> Vec<String> {
    let set: Vec<String> = store
        .setting(DEVICES)
        .ok()
        .flatten()
        .unwrap_or_default()
        .split(',')
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .collect();
    if set.is_empty() { crate::host::default_devices() } else { set }
}

pub const LSP_MAX: &str = "lsp_max_servers";
pub const LSP_IDLE: &str = "lsp_idle_stop_secs";
pub const LSP_MEMORY: &str = "lsp_memory_mb";

pub fn lsp_config(store: &Store) -> agent_core::pool::Config {
    use agent_core::pool::{Config, DEFAULT_IDLE, DEFAULT_MAX, DEFAULT_MEMORY_MB};
    let n = |k: &str, d: u64| crate::autopilot::number(store, k, d);
    let memory = n(LSP_MEMORY, DEFAULT_MEMORY_MB);
    Config {
        max: n(LSP_MAX, DEFAULT_MAX as u64).max(1) as usize,
        idle: std::time::Duration::from_secs(n(LSP_IDLE, DEFAULT_IDLE.as_secs()).max(1)),
        memory_mb: (memory > 0).then_some(memory),
        ..Config::default()
    }
}

pub fn pressure_cap(configured: u32) -> u32 {
    (configured / 2).max(1).min(configured)
}

fn gb(bytes: u64) -> f64 {
    bytes as f64 / GB
}

fn gb_text(bytes: u64) -> String {
    let g = gb(bytes);
    if g >= 10.0 { format!("{g:.0} GB") } else { format!("{g:.1} GB") }
}

struct Hit {
    key: &'static str,
    text: String,
}

#[derive(Clone, Copy)]
enum Stall {
    Memory,
    Io,
}

impl Stall {
    const ALL: [Stall; 2] = [Stall::Memory, Stall::Io];

    fn key(self) -> &'static str {
        match self {
            Stall::Memory => "memory",
            Stall::Io => "io",
        }
    }

    fn some(self, r: &PressureReadings) -> (Option<f32>, Option<f32>) {
        match self {
            Stall::Memory => (r.memory_some10, r.memory_some60),
            Stall::Io => (r.io_some10, r.io_some60),
        }
    }
}

fn swap_text(used: u64, device: &Option<String>) -> String {
    match device {
        Some(d) => format!("swap {} (swap on {d})", gb_text(used)),
        None => format!("swap {}", gb_text(used)),
    }
}

fn swapping(r: &PressureReadings) -> bool {
    r.swap_in_rate.unwrap_or(0.0) + r.swap_out_rate.unwrap_or(0.0) > 0.0
}

fn swap_context(r: &PressureReadings) -> Option<String> {
    if !swapping(r) {
        return None;
    }
    let rate = r.swap_in_rate.unwrap_or(0.0).max(r.swap_out_rate.unwrap_or(0.0));
    let mut parts = vec![format!("swapping {rate:.0} pages/s")];
    parts.extend(r.swap_used.map(|s| swap_text(s, &r.swap_device)));
    if let Some(d) = r.disks.iter().find(|d| Some(&d.device) == r.swap_device.as_ref()) {
        parts.push(format!("swap disk queue {:.1} ms, writes {:.1} ms", d.queue_ms, d.write_ms));
    }
    Some(parts.join(", "))
}

fn with_context(texts: &[String], r: &PressureReadings) -> String {
    let hits = texts.join(", ");
    match swap_context(r) {
        Some(ctx) => format!("{hits}; {ctx}"),
        None => hits,
    }
}

fn swap_full(r: &PressureReadings, l: &Limits) -> Option<f64> {
    let (used, total) = (r.swap_used?, r.swap_total?);
    let pct = used as f64 * 100.0 / total as f64;
    (l.swap_full_pct > 0.0 && total > 0 && pct > l.swap_full_pct as f64).then_some(pct)
}

fn memory_stall_rising(r: &PressureReadings) -> Option<f32> {
    match (r.memory_some10, r.memory_some60) {
        (Some(now), Some(minute)) if now > minute => Some(now),
        _ => None,
    }
}

fn percentile(mut v: Vec<f32>, p: f32) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let rank = ((p * v.len() as f32).ceil() as usize).clamp(1, v.len());
    Some(v[rank - 1])
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Changed { from: PressureState, to: PressureState, reason: String, cap: u32 },
    Step { cap: u32 },
}

#[derive(Default)]
pub struct Monitor {
    enabled: bool,
    state: PressureState,
    since: i64,
    reason: String,
    readings: PressureReadings,
    held: HashMap<&'static str, i64>,
    calm_since: Option<i64>,
    critical_quiet_since: Option<i64>,
    recover: Option<(u32, i64)>,
    started: Option<i64>,
    history: HashMap<&'static str, VecDeque<(i64, f32)>>,
    resident: VecDeque<(i64, u64)>,
    last_cap: Option<u32>,
    last_at: i64,
    pub(crate) notified: bool,
    pub(crate) messaged: HashSet<SessionId>,
}

pub type Shared = Arc<Mutex<Monitor>>;

impl Monitor {
    pub fn state(&self) -> PressureState {
        self.state
    }

    pub fn disable(&mut self) {
        *self = Monitor::default();
    }

    pub fn effective_cap(&self, configured: u32, now: i64, l: &Limits) -> u32 {
        if !self.enabled {
            return configured;
        }
        match self.state {
            PressureState::Critical => 0,
            PressureState::Pressure => pressure_cap(configured),
            _ => match self.recover {
                Some((base, at)) => {
                    let steps = ((now - at).max(0) / l.step_ms.max(1)) as u32;
                    base.saturating_add(steps).min(configured)
                }
                None => configured,
            },
        }
    }

    fn sustained(&self, hits: &[Hit], hold: i64, now: i64) -> Vec<String> {
        hits.iter()
            .filter(|h| self.held.get(h.key).is_some_and(|at| *at < now && now - at >= hold))
            .map(|h| h.text.clone())
            .collect()
    }

    fn normal(&self, key: &'static str, l: &Limits, now: i64) -> Option<f32> {
        let covered = self.started.is_some_and(|s| now - s >= l.history_ms);
        let history = self.history.get(key).filter(|_| covered)?;
        percentile(history.iter().map(|(_, v)| *v).collect(), NORMAL_PERCENTILE)
    }

    fn stall_line(&self, key: &'static str, l: &Limits, now: i64) -> f32 {
        if l.stall_pct > 0.0 {
            return l.stall_pct;
        }
        self.normal(key, l, now).map_or(NOTICEABLE_STALL_PCT, |n| n.max(NOTICEABLE_STALL_PCT))
    }

    fn stall_hits(&self, r: &PressureReadings, l: &Limits, now: i64) -> Vec<Hit> {
        Stall::ALL
            .into_iter()
            .filter_map(|s| {
                let line = self.stall_line(s.key(), l, now);
                match s.some(r) {
                    (Some(ten), Some(sixty)) if ten > line && sixty > line => {
                        Some(Hit { key: s.key(), text: format!("{} stall {ten:.0} percent", s.key()) })
                    }
                    _ => None,
                }
            })
            .collect()
    }

    fn largest_worker(&self) -> Option<u64> {
        self.resident.iter().map(|(_, v)| *v).max().filter(|v| *v > 0)
    }

    fn critical_hits(&self, r: &PressureReadings, l: &Limits) -> Vec<Hit> {
        let mut c = Vec::new();
        if let (Some(m), Some(w)) = (r.mem_available, self.largest_worker())
            && m < w
        {
            c.push(Hit { key: "c_memory", text: format!("{} available, under the largest worker's {}", gb_text(m), gb_text(w)) });
        }
        if let (Some(pct), Some(stall)) = (swap_full(r, l), memory_stall_rising(r)) {
            c.push(Hit { key: "c_swap_full", text: format!("swap {pct:.0} percent full while memory stall rises to {stall:.1} percent") });
        }
        c
    }

    fn remember(&mut self, r: &PressureReadings, l: &Limits, now: i64) {
        for s in Stall::ALL {
            let Some(v) = s.some(r).0 else { continue };
            let history = self.history.entry(s.key()).or_default();
            while history.front().is_some_and(|(at, _)| now - at > l.history_ms) {
                history.pop_front();
            }
            history.push_back((now, v));
        }
    }

    fn enter(&mut self, to: PressureState, reason: String, now: i64) -> (PressureState, PressureState) {
        let from = self.state;
        self.state = to;
        self.since = now;
        self.reason = reason;
        (from, to)
    }

    pub fn observe(&mut self, r: PressureReadings, l: &Limits, configured: u32, now: i64) -> Vec<Event> {
        if !self.enabled {
            self.enabled = true;
            self.since = now;
        }
        let started = *self.started.get_or_insert(now);
        if let Some(w) = r.worker_resident {
            while self.resident.front().is_some_and(|(at, _)| now - at > l.resident_ms) {
                self.resident.pop_front();
            }
            self.resident.push_back((now, w));
        }
        let mut p = self.stall_hits(&r, l, now);
        if now - started < l.start_grace_ms && swapping(&r) {
            p.retain(|h| h.key != Stall::Memory.key());
        }
        let c = self.critical_hits(&r, l);
        self.remember(&r, l, now);
        self.readings = r;
        let keys: HashSet<&'static str> = p.iter().chain(&c).map(|h| h.key).collect();
        self.held.retain(|k, _| keys.contains(k));
        for k in keys {
            self.held.entry(k).or_insert(now);
        }
        self.calm_since = if p.is_empty() && c.is_empty() { self.calm_since.or(Some(now)) } else { None };
        self.critical_quiet_since = if c.is_empty() { self.critical_quiet_since.or(Some(now)) } else { None };
        let p_held = self.sustained(&p, l.hold_ms, now);
        let c_held = self.sustained(&c, l.critical_hold_ms, now);
        let texts = |hits: &[Hit]| hits.iter().map(|h| h.text.clone()).collect::<Vec<_>>();
        let mut events = Vec::new();
        let before = self.last_cap.unwrap_or(configured);
        let changed = if !c_held.is_empty() && self.state != PressureState::Critical {
            self.recover = None;
            let reason = with_context(&c_held, &self.readings);
            Some(self.enter(PressureState::Critical, reason, now))
        } else if !p_held.is_empty() && self.state == PressureState::Ok {
            self.recover = None;
            let reason = with_context(&p_held, &self.readings);
            Some(self.enter(PressureState::Pressure, reason, now))
        } else if self.state == PressureState::Critical
            && self.critical_quiet_since.is_some_and(|at| now - at >= l.hold_ms)
        {
            let reason = if p.is_empty() { "critical readings eased".to_string() } else { with_context(&texts(&p), &self.readings) };
            Some(self.enter(PressureState::Pressure, reason, now))
        } else if self.state == PressureState::Pressure && self.calm_since.is_some_and(|at| now - at >= l.cooldown_ms) {
            self.recover = Some((pressure_cap(configured), now));
            let mins = l.cooldown_ms / 60_000;
            Some(self.enter(PressureState::Ok, format!("calm for {mins} min"), now))
        } else {
            None
        };
        match self.state {
            PressureState::Critical if !c.is_empty() => self.reason = with_context(&texts(&c), &self.readings),
            PressureState::Pressure if !p.is_empty() && changed.is_none() => self.reason = with_context(&texts(&p), &self.readings),
            PressureState::Ok if changed.is_none() => {
                let quiet = p.is_empty() && c.is_empty();
                self.reason = match (quiet, swap_context(&self.readings), self.readings.swap_used.filter(|s| *s > 0)) {
                    (true, Some(ctx), _) => format!("{ctx}, no tasks stalled"),
                    (true, None, Some(s)) => format!("swap {} used, idle", gb_text(s)),
                    _ if self.recover.is_some() => self.reason.clone(),
                    _ => String::new(),
                }
            }
            _ => {}
        }
        let cap = self.effective_cap(configured, now, l);
        if let Some((from, to)) = changed {
            events.push(Event::Changed { from, to, reason: self.reason.clone(), cap });
        } else if self.state == PressureState::Ok && self.recover.is_some() && cap > before {
            events.push(Event::Step { cap });
        }
        if self.state == PressureState::Ok && self.recover.is_some() && cap >= configured {
            self.recover = None;
        }
        if self.state == PressureState::Ok && self.recover.is_none() {
            self.notified = false;
        }
        self.last_cap = Some(cap);
        self.last_at = now;
        events
    }

    pub fn cap(&self, configured: u32, l: &Limits) -> u32 {
        self.effective_cap(configured, self.last_at, l)
    }

    pub fn current(&self, configured: u32, l: &Limits) -> HostPressure {
        self.snapshot(configured, self.last_at, l)
    }

    pub fn snapshot(&self, configured: u32, now: i64, l: &Limits) -> HostPressure {
        HostPressure {
            enabled: self.enabled,
            state: self.state,
            reason: self.reason.clone(),
            since: self.since,
            readings: self.readings.clone(),
            effective_cap: self.effective_cap(configured, now, l),
            configured_cap: configured,
            ..HostPressure::default()
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use agentbench_core::DiskReading;

    const GIB: u64 = 1024 * 1024 * 1024;

    pub(crate) fn calm() -> PressureReadings {
        PressureReadings {
            known: true,
            cpu_some10: Some(2.0),
            cpu_some60: Some(2.0),
            memory_some10: Some(0.0),
            memory_some60: Some(0.0),
            memory_full10: Some(0.0),
            memory_full60: Some(0.0),
            io_some10: Some(1.0),
            io_some60: Some(1.0),
            io_full10: Some(1.0),
            io_full60: Some(1.0),
            mem_available: Some(40 * GIB),
            swap_used: Some(0),
            swap_total: None,
            swap_device: Some("nvme1n1p1".into()),
            swap_in_rate: None,
            swap_out_rate: None,
            load_per_core: Some(0.2),
            disks: vec![],
            worker_resident: None,
        }
    }

    pub(crate) fn busy() -> PressureReadings {
        PressureReadings { io_full10: Some(20.0), io_some10: Some(22.0), io_some60: Some(22.0), ..calm() }
    }

    pub(crate) fn heavy() -> PressureReadings {
        PressureReadings { mem_available: Some(2 * GIB), worker_resident: Some(6 * GIB), ..calm() }
    }

    pub(crate) fn stalled(pct: f32) -> PressureReadings {
        PressureReadings { memory_some10: Some(pct), memory_some60: Some(pct), io_some10: Some(pct), io_some60: Some(pct), ..calm() }
    }

    const S: i64 = 1000;

    fn run(m: &mut Monitor, r: &PressureReadings, from: i64, to: i64) -> Vec<Event> {
        let l = Limits::default();
        let mut out = Vec::new();
        let mut t = from;
        while t <= to {
            out.extend(m.observe(r.clone(), &l, 8, t));
            t += 10 * S;
        }
        out
    }

    fn observe_all(m: &mut Monitor, l: &Limits, rs: &[PressureReadings], from: i64) -> Vec<Event> {
        let mut out = Vec::new();
        for (i, r) in rs.iter().enumerate() {
            out.extend(m.observe(r.clone(), l, 5, from + i as i64 * 10 * S));
        }
        out
    }

    fn on_disk(r: PressureReadings, queue_ms: f32, write_ms: f32, bytes_rate: f32) -> PressureReadings {
        let disk = DiskReading { device: "nvme1n1p1".into(), path: "/mnt/data".into(), write_ms, queue_ms, bytes_rate };
        PressureReadings { swap_device: Some("nvme1n1p1".into()), disks: vec![disk], ..r }
    }

    pub(crate) fn live_disk_queue_reading() -> PressureReadings {
        let r = PressureReadings {
            memory_some10: Some(0.44),
            memory_some60: Some(0.44),
            memory_full10: Some(0.3),
            memory_full60: Some(0.3),
            io_some10: Some(0.1),
            io_some60: Some(0.1),
            io_full10: Some(0.1),
            io_full60: Some(0.1),
            load_per_core: Some(0.11),
            swap_in_rate: Some(40.0),
            swap_out_rate: Some(0.0),
            swap_used: Some(17_000_000_000),
            swap_total: Some(51_500_000_000),
            mem_available: Some(24_000_000_000),
            ..calm()
        };
        on_disk(r, 1.7, 0.6, 2_000_000.0)
    }

    #[test]
    fn the_live_disk_queue_of_1_7_ms_against_0_5_ms_with_no_stall_is_ok() {
        let l = Limits::default();
        let mut m = Monitor::default();
        let normal: Vec<PressureReadings> = (0..12).map(|_| on_disk(live_disk_queue_reading(), 0.5, 0.6, 2_000_000.0)).collect();
        observe_all(&mut m, &l, &normal, 0);
        let ev = observe_all(&mut m, &l, &vec![live_disk_queue_reading(); 60], 120 * S);
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(m.state(), PressureState::Ok);
        assert_eq!(m.effective_cap(5, 720 * S, &l), 5);
        assert_eq!(
            m.snapshot(5, 720 * S, &l).reason,
            "swapping 40 pages/s, swap 16 GB (swap on nvme1n1p1), swap disk queue 1.7 ms, writes 0.6 ms, no tasks stalled"
        );
    }

    #[test]
    fn thirty_percent_stall_for_sixty_seconds_is_pressure_whatever_the_disk_says() {
        let l = Limits::default();
        for (queue, write) in [(0.1, 0.1), (0.5, 0.6), (40.0, 30.0)] {
            for stalled in [
                PressureReadings { io_some10: Some(30.0), io_some60: Some(30.0), io_full10: Some(2.0), ..calm() },
                PressureReadings { memory_some10: Some(30.0), memory_some60: Some(30.0), ..calm() },
            ] {
                let mut m = Monitor::default();
                observe_all(&mut m, &l, &vec![on_disk(calm(), queue, write, 1_000_000.0); 12], 0);
                assert_eq!(m.state(), PressureState::Ok);
                observe_all(&mut m, &l, &vec![on_disk(stalled.clone(), queue, write, 1_000_000.0); 6], 120 * S);
                assert_eq!(m.state(), PressureState::Ok, "50 seconds is not sustained");
                observe_all(&mut m, &l, &[on_disk(stalled, queue, write, 1_000_000.0)], 180 * S);
                assert_eq!(m.state(), PressureState::Pressure, "disk {queue} ms");
                assert_eq!(m.effective_cap(5, 180 * S, &l), 2);
            }
        }
    }

    #[test]
    fn a_slow_disk_with_zero_stall_is_ok() {
        let l = Limits::default();
        let still = PressureReadings {
            memory_some10: Some(0.0),
            memory_some60: Some(0.0),
            io_some10: Some(0.0),
            io_some60: Some(0.0),
            io_full10: Some(0.0),
            swap_in_rate: Some(3000.0),
            swap_out_rate: Some(3000.0),
            ..live_disk_queue_reading()
        };
        let mut m = Monitor::default();
        observe_all(&mut m, &l, &vec![on_disk(still.clone(), 0.5, 0.5, 1_000_000.0); 12], 0);
        let ev = observe_all(&mut m, &l, &vec![on_disk(still, 80.0, 60.0, 90_000_000.0); 60], 120 * S);
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(m.state(), PressureState::Ok);
    }

    #[test]
    fn memory_under_the_largest_workers_resident_set_is_critical() {
        let l = Limits::default();
        let gb_ = 1_000_000_000u64;
        let nearly_gone = PressureReadings { mem_available: Some(8 * gb_), worker_resident: Some(10 * gb_), ..calm() };
        let mut m = Monitor::default();
        observe_all(&mut m, &l, &vec![PressureReadings { worker_resident: Some(10 * gb_), ..calm() }; 7], 0);
        observe_all(&mut m, &l, &vec![nearly_gone; 4], 70 * S);
        assert_eq!(m.state(), PressureState::Critical);
        assert_eq!(m.effective_cap(5, 100 * S, &l), 0);
        assert!(m.reason.starts_with("7.5 GB available, under the largest worker's 9.3 GB"), "{}", m.reason);
    }

    #[test]
    fn the_largest_worker_is_remembered_after_its_build_ends() {
        let l = Limits::default();
        let mut m = Monitor::default();
        observe_all(&mut m, &l, &[PressureReadings { worker_resident: Some(12 * GIB), ..calm() }], 0);
        let after = PressureReadings { mem_available: Some(9 * GIB), worker_resident: Some(GIB), ..calm() };
        observe_all(&mut m, &l, &vec![after.clone(); 4], 10 * S);
        assert_eq!(m.state(), PressureState::Critical, "9 GB free is under the 12 GB build seen a minute ago");
        let mut later = Monitor::default();
        observe_all(&mut later, &l, &[PressureReadings { worker_resident: Some(12 * GIB), ..calm() }], 0);
        observe_all(&mut later, &l, &vec![after; 6], 610 * S);
        assert_eq!(later.state(), PressureState::Ok, "the build is out of the window");
    }

    #[test]
    fn one_bad_sample_does_not_scale_back() {
        let mut m = Monitor::default();
        let l = Limits::default();
        run(&mut m, &calm(), 0, 30 * S);
        run(&mut m, &heavy(), 40 * S, 40 * S);
        run(&mut m, &stalled(40.0), 50 * S, 50 * S);
        run(&mut m, &PressureReadings { worker_resident: Some(GIB), ..calm() }, 60 * S, 200 * S);
        assert_eq!(m.state(), PressureState::Ok);
        assert_eq!(m.effective_cap(8, 200 * S, &l), 8);
    }

    #[test]
    fn sixty_seconds_of_io_stall_halves_the_cap() {
        let mut m = Monitor::default();
        let l = Limits::default();
        assert!(run(&mut m, &busy(), 0, 50 * S).is_empty());
        let ev = run(&mut m, &busy(), 60 * S, 60 * S);
        assert_eq!(ev, vec![Event::Changed { from: PressureState::Ok, to: PressureState::Pressure, reason: "io stall 22 percent".into(), cap: 4 }]);
        assert_eq!(m.effective_cap(8, 60 * S, &l), 4);
        assert_eq!(m.effective_cap(1, 60 * S, &l), 1);
        assert_eq!(m.effective_cap(3, 60 * S, &l), 1);
    }

    #[test]
    fn the_ten_and_sixty_second_stall_both_have_to_be_over_the_line() {
        let mut m = Monitor::default();
        run(&mut m, &PressureReadings { io_some10: Some(40.0), io_some60: Some(4.0), ..calm() }, 0, 300 * S);
        assert_eq!(m.state(), PressureState::Ok, "a burst the minute average does not carry");
        run(&mut m, &PressureReadings { io_some10: Some(4.0), io_some60: Some(40.0), ..calm() }, 310 * S, 600 * S);
        assert_eq!(m.state(), PressureState::Ok, "a stall that has already ended");
    }

    #[test]
    fn thirty_seconds_of_critical_readings_stop_all_starts() {
        let mut m = Monitor::default();
        let l = Limits::default();
        run(&mut m, &heavy(), 0, 20 * S);
        assert_eq!(m.state(), PressureState::Ok);
        let ev = run(&mut m, &heavy(), 30 * S, 30 * S);
        assert!(matches!(&ev[..], [Event::Changed { to: PressureState::Critical, cap: 0, .. }]), "{ev:?}");
        assert_eq!(m.effective_cap(8, 30 * S, &l), 0);
        assert_eq!(m.snapshot(8, 30 * S, &l).reason, "2.0 GB available, under the largest worker's 6.0 GB");
    }

    #[test]
    fn clearing_waits_five_minutes_then_adds_one_worker_every_two() {
        let mut m = Monitor::default();
        let l = Limits::default();
        run(&mut m, &busy(), 0, 60 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        let calm_from = 70 * S;
        run(&mut m, &calm(), calm_from, calm_from + 290 * S);
        assert_eq!(m.state(), PressureState::Pressure, "still in the cooldown");
        let ev = run(&mut m, &calm(), calm_from + 300 * S, calm_from + 300 * S);
        assert!(matches!(&ev[..], [Event::Changed { to: PressureState::Ok, cap: 4, .. }]), "{ev:?}");
        let cleared = calm_from + 300 * S;
        assert_eq!(m.effective_cap(8, cleared + 110 * S, &l), 4);
        assert_eq!(m.effective_cap(8, cleared + 120 * S, &l), 5);
        assert_eq!(m.effective_cap(8, cleared + 359 * S, &l), 6);
        let steps = run(&mut m, &calm(), cleared + 10 * S, cleared + 600 * S);
        assert_eq!(steps, (5..=8).map(|cap| Event::Step { cap }).collect::<Vec<_>>());
        assert_eq!(m.effective_cap(8, cleared + 2000 * S, &l), 8);
    }

    #[test]
    fn a_brief_calm_does_not_end_the_cooldown() {
        let mut m = Monitor::default();
        run(&mut m, &calm(), 0, 200 * S);
        run(&mut m, &busy(), 210 * S, 270 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        run(&mut m, &calm(), 280 * S, 460 * S);
        run(&mut m, &busy(), 470 * S, 470 * S);
        run(&mut m, &calm(), 480 * S, 710 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        run(&mut m, &calm(), 720 * S, 780 * S);
        assert_eq!(m.state(), PressureState::Ok);
    }

    #[test]
    fn a_host_whose_normal_stall_is_above_five_percent_needs_more_than_its_own_95th_percentile() {
        let l = Limits { history_ms: 2000 * S, ..Limits::default() };
        let level = |pct: f32| PressureReadings { io_some10: Some(pct), io_some60: Some(pct), ..calm() };
        let normal: Vec<PressureReadings> = (0..200).map(|i| level(6.0 + (i % 3) as f32)).collect();
        let mut m = Monitor::default();
        observe_all(&mut m, &l, &normal, 0);
        assert_eq!(m.state(), PressureState::Pressure, "before a whole window of its own samples the line is 5 percent");
        let mut m = Monitor::default();
        observe_all(&mut m, &l, &vec![calm(); 200], 0);
        let mut stalled = vec![level(9.0); 200];
        stalled.extend(normal.clone());
        observe_all(&mut m, &l, &stalled, 2000 * S);
        assert_eq!(m.state(), PressureState::Ok, "6 to 8 percent became this host's own normal over the window");
        observe_all(&mut m, &l, &vec![level(8.0); 30], 6000 * S);
        assert_eq!(m.state(), PressureState::Ok, "at its 95th percentile, not above it");
        observe_all(&mut m, &l, &vec![level(12.0); 7], 6300 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        assert_eq!(m.reason, "io stall 12 percent");
    }

    #[test]
    fn under_five_percent_is_never_pressure_from_history_alone() {
        let l = Limits { history_ms: 2000 * S, ..Limits::default() };
        let level = |pct: f32| PressureReadings { memory_some10: Some(pct), memory_some60: Some(pct), ..calm() };
        let mut m = Monitor::default();
        observe_all(&mut m, &l, &vec![level(0.0); 300], 0);
        assert!(observe_all(&mut m, &l, &vec![level(4.9); 60], 3000 * S).is_empty());
        assert_eq!(m.state(), PressureState::Ok);
    }

    #[test]
    fn a_users_stall_pct_replaces_the_derived_line() {
        let level = |pct: f32| PressureReadings { io_some10: Some(pct), io_some60: Some(pct), ..calm() };
        let high = Limits { stall_pct: 20.0, ..Limits::default() };
        let mut m = Monitor::default();
        observe_all(&mut m, &high, &vec![calm(); 12], 0);
        observe_all(&mut m, &high, &vec![level(15.0); 30], 120 * S);
        assert_eq!(m.state(), PressureState::Ok);
        observe_all(&mut m, &high, &vec![level(25.0); 7], 420 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        let low = Limits { stall_pct: 3.0, ..Limits::default() };
        let mut strict = Monitor::default();
        observe_all(&mut strict, &low, &vec![calm(); 12], 0);
        observe_all(&mut strict, &low, &vec![level(4.0); 7], 120 * S);
        assert_eq!(strict.state(), PressureState::Pressure, "the person's own line, even under five");
    }

    #[test]
    fn a_stall_longer_than_five_percent_of_the_window_becomes_the_hosts_normal() {
        let l = Limits::default();
        let mut m = Monitor::default();
        observe_all(&mut m, &l, &vec![calm(); 12], 0);
        observe_all(&mut m, &l, &vec![stalled(30.0); 360], 120 * S);
        assert_eq!(m.state(), PressureState::Pressure, "an hour of stall on a server up less than a day");
        let short = Limits { history_ms: 2000 * S, ..Limits::default() };
        let mut m = Monitor::default();
        observe_all(&mut m, &short, &vec![calm(); 200], 0);
        observe_all(&mut m, &short, &vec![stalled(30.0); 7], 2000 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        observe_all(&mut m, &short, &vec![stalled(30.0); 50], 2070 * S);
        assert_eq!(m.state(), PressureState::Ok, "past 5 percent of the window it is the host's normal, then the cooldown");
    }

    #[test]
    fn stale_swap_on_an_idle_host_stays_ok() {
        let mut m = Monitor::default();
        let r = PressureReadings { swap_used: Some(18 * GIB), mem_available: Some(34 * GIB), swap_in_rate: Some(0.0), swap_out_rate: Some(0.0), ..calm() };
        run(&mut m, &r, 0, 600 * S);
        assert_eq!(m.state(), PressureState::Ok);
    }

    pub(crate) fn live_idle_host() -> PressureReadings {
        PressureReadings {
            swap_used: Some(24_800_000_000),
            swap_total: Some(49_000_000_000),
            swap_in_rate: Some(0.0),
            swap_out_rate: Some(0.0),
            io_full10: Some(0.3),
            io_some10: Some(0.4),
            memory_some10: Some(0.0),
            memory_full10: Some(0.0),
            cpu_some60: Some(1.0),
            mem_available: Some(25_000_000_000),
            load_per_core: Some(0.12),
            ..calm()
        }
    }

    #[test]
    fn the_live_idle_host_with_creeping_stale_swap_stays_ok() {
        let mut m = Monitor::default();
        let l = Limits::default();
        let creep = [24_800_000_000u64, 24_800_000_000, 24_810_000_000];
        let mut used = 0;
        for i in 0..90u64 {
            used = creep[(i % 3) as usize] + (i / 3) * 10_000_000;
            let r = PressureReadings { swap_used: Some(used), ..live_idle_host() };
            let ev = m.observe(r, &l, 5, i as i64 * 10 * S);
            assert!(ev.is_empty(), "sample {i}: {ev:?}");
        }
        assert!(used > 25_000_000_000);
        assert_eq!(m.state(), PressureState::Ok);
        assert_eq!(m.effective_cap(5, 900 * S, &l), 5);
    }

    pub(crate) fn nearly_full_swap() -> PressureReadings {
        PressureReadings { swap_used: Some(44_600_000_000), memory_some10: Some(3.0), memory_some60: Some(1.0), ..live_idle_host() }
    }

    #[test]
    fn a_nearly_full_swap_is_critical_while_memory_stall_rises() {
        let mut m = Monitor::default();
        let l = Limits::default();
        run(&mut m, &nearly_full_swap(), 0, 20 * S);
        assert_eq!(m.state(), PressureState::Ok);
        let ev = run(&mut m, &nearly_full_swap(), 30 * S, 30 * S);
        assert!(matches!(&ev[..], [Event::Changed { to: PressureState::Critical, cap: 0, .. }]), "{ev:?}");
        assert_eq!(m.reason, "swap 91 percent full while memory stall rises to 3.0 percent");
        assert_eq!(m.effective_cap(8, 30 * S, &l), 0);
        let mut flat = Monitor::default();
        run(&mut flat, &PressureReadings { memory_some60: Some(3.0), ..nearly_full_swap() }, 0, 300 * S);
        assert_eq!(flat.state(), PressureState::Ok, "a full swap whose stall is not rising is old pages");
        let mut off = Monitor::default();
        let l0 = Limits { swap_full_pct: 0.0, ..Limits::default() };
        for i in 0..30 {
            off.observe(nearly_full_swap(), &l0, 8, i * 10 * S);
        }
        assert_eq!(off.state(), PressureState::Ok, "0 turns the headroom rule off");
        let mut under = Monitor::default();
        run(&mut under, &PressureReadings { swap_used: Some(41_000_000_000), ..nearly_full_swap() }, 0, 300 * S);
        assert_eq!(under.state(), PressureState::Ok, "84 percent is under the line");
    }

    #[test]
    fn an_ok_host_with_much_stale_swap_says_why_it_is_not_throttling() {
        let mut m = Monitor::default();
        let l = Limits::default();
        run(&mut m, &live_idle_host(), 0, 120 * S);
        let snap = m.snapshot(5, 120 * S, &l);
        assert_eq!(snap.state, PressureState::Ok);
        assert_eq!(snap.reason, "swap 23 GB used, idle");
        assert_eq!(snap.line(), None);
        run(&mut m, &calm(), 130 * S, 130 * S);
        assert_eq!(m.snapshot(5, 130 * S, &l).reason, "");
    }

    #[test]
    fn the_live_flapping_sequence_recovers_and_stays_recovered_on_unchanged_swap() {
        let mut m = Monitor::default();
        let l = Limits::default();
        let gb_ = 1_000_000_000u64;
        let mut t = 0;
        let mut used = 20 * gb_;
        while m.state() == PressureState::Ok && t < 300 * S {
            used += gb_ / 4;
            let r = PressureReadings { swap_used: Some(used), swap_out_rate: Some(400.0), memory_some10: Some(14.0), memory_some60: Some(14.0), ..live_idle_host() };
            m.observe(r, &l, 5, t);
            t += 10 * S;
        }
        assert_eq!(m.state(), PressureState::Pressure);
        assert_eq!(m.effective_cap(5, t, &l), 2);
        let settled = used;
        let mut cleared = None;
        let mut changes = Vec::new();
        for i in 0..240u64 {
            let creep = (i % 3 == 2) as u64 * 10_000_000 + (i / 3) * 1_000_000;
            let r = PressureReadings { swap_used: Some(settled + creep), ..live_idle_host() };
            for e in m.observe(r, &l, 5, t) {
                if let Event::Changed { to, .. } = e {
                    changes.push((t, to));
                    if to == PressureState::Ok {
                        cleared = Some(t);
                    }
                }
            }
            t += 10 * S;
        }
        let cleared = cleared.expect("pressure clears after the calm window");
        assert_eq!(changes, vec![(cleared, PressureState::Ok)], "no flap back into pressure");
        assert_eq!(m.state(), PressureState::Ok);
        assert_eq!(m.effective_cap(5, t, &l), 5);
        assert!(m.reason.ends_with("used, idle"), "{}", m.reason);
    }

    fn live_restart_host() -> PressureReadings {
        let r = PressureReadings {
            swap_in_rate: Some(148.25),
            swap_out_rate: Some(0.0),
            memory_some10: Some(0.0),
            memory_full10: Some(0.0),
            io_full10: Some(0.01),
            io_some10: Some(0.01),
            mem_available: Some(33_000_000_000),
            swap_used: Some(21_100_000_000),
            swap_total: Some(51_500_000_000),
            load_per_core: Some(0.15),
            ..calm()
        };
        on_disk(r, 1.2, 0.4, 600_000.0)
    }

    #[test]
    fn the_live_restart_host_paging_its_own_pages_back_in_stays_ok() {
        let mut m = Monitor::default();
        let l = Limits::default();
        for i in 0..90 {
            let ev = m.observe(live_restart_host(), &l, 5, i * 10 * S);
            assert!(ev.is_empty(), "sample {i}: {ev:?}");
        }
        assert_eq!(m.state(), PressureState::Ok);
        assert_eq!(m.effective_cap(5, 900 * S, &l), 5);
    }

    #[test]
    fn paging_in_either_direction_on_any_disk_is_never_the_trigger() {
        let l = Limits::default();
        for r in [
            PressureReadings { swap_in_rate: Some(3000.0), ..live_restart_host() },
            PressureReadings { swap_out_rate: Some(50000.0), ..live_restart_host() },
            on_disk(PressureReadings { swap_out_rate: Some(50000.0), ..live_restart_host() }, 90.0, 60.0, 120_000_000.0),
            PressureReadings { swap_out_rate: Some(50000.0), disks: vec![], ..live_restart_host() },
        ] {
            let mut m = Monitor::default();
            assert!(observe_all(&mut m, &l, &vec![r; 60], 0).is_empty());
            assert_eq!(m.state(), PressureState::Ok);
        }
    }

    #[test]
    fn the_reason_shows_swapping_and_the_swap_disk_only_as_context() {
        let l = Limits::default();
        let mut m = Monitor::default();
        observe_all(&mut m, &l, &vec![calm(); 12], 0);
        let r = PressureReadings { memory_some10: Some(30.0), memory_some60: Some(30.0), ..live_disk_queue_reading() };
        observe_all(&mut m, &l, &vec![r; 7], 120 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        assert_eq!(m.reason, "memory stall 30 percent; swapping 40 pages/s, swap 16 GB (swap on nvme1n1p1), swap disk queue 1.7 ms, writes 0.6 ms");
    }

    #[test]
    fn the_first_minute_after_start_is_ignored() {
        let l = Limits::default();
        let burst = PressureReadings { swap_in_rate: Some(30000.0), memory_some10: Some(30.0), memory_some60: Some(30.0), ..live_restart_host() };
        let mut m = Monitor::default();
        let mut samples = vec![burst.clone(); 6];
        samples.extend(vec![live_restart_host(); 60]);
        assert!(observe_all(&mut m, &l, &samples, 0).is_empty());
        assert_eq!(m.state(), PressureState::Ok);
        let mut later = Monitor::default();
        observe_all(&mut later, &l, &vec![live_restart_host(); 7], 0);
        observe_all(&mut later, &l, &vec![burst; 7], 70 * S);
        assert_eq!(later.state(), PressureState::Pressure, "after the first minute the same burst counts");
    }

    #[test]
    fn critical_needs_two_consecutive_samples() {
        let l = Limits { critical_hold_ms: 0, ..Limits::default() };
        let mut m = Monitor::default();
        m.observe(calm(), &l, 8, 0);
        assert!(m.observe(heavy(), &l, 8, 10 * S).is_empty(), "one sample");
        m.observe(PressureReadings { worker_resident: Some(GIB), ..calm() }, &l, 8, 20 * S);
        assert!(m.observe(heavy(), &l, 8, 30 * S).is_empty(), "one sample again");
        let ev = m.observe(heavy(), &l, 8, 40 * S);
        assert!(matches!(&ev[..], [Event::Changed { to: PressureState::Critical, cap: 0, .. }]), "{ev:?}");
    }

    #[test]
    fn unknown_readings_stay_ok() {
        let mut m = Monitor::default();
        run(&mut m, &PressureReadings::default(), 0, 600 * S);
        assert_eq!(m.state(), PressureState::Ok);
    }

    #[test]
    fn the_status_line_names_the_scaled_cap_and_the_reason() {
        let mut m = Monitor::default();
        let r = PressureReadings { swap_used: Some(20 * GIB), swap_out_rate: Some(5000.0), ..heavy() };
        let mut t = 0;
        while m.state() != PressureState::Critical && t < 300 * S {
            m.observe(r.clone(), &Limits::default(), 8, t);
            t += 10 * S;
        }
        let snap = m.snapshot(8, t, &Limits::default());
        assert_eq!(snap.state, PressureState::Critical);
        let line = snap.line().unwrap();
        assert_eq!(line, "starts paused, 0 of 8: 2.0 GB available, under the largest worker's 6.0 GB; swapping 5000 pages/s, swap 20 GB (swap on nvme1n1p1)");
        let p = HostPressure { state: PressureState::Pressure, effective_cap: 2, reason: "io stall 22 percent".into(), ..snap };
        assert_eq!(p.line().unwrap(), "scaled to 2 of 8: io stall 22 percent");
    }
}
