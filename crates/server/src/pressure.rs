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

#[derive(Clone, Debug, PartialEq)]
pub struct Limits {
    pub io_full: f32,
    pub memory_some: f32,
    pub mem_available_gb: f32,
    pub swap_gb: f32,
    pub cpu_some: f32,
    pub load_per_core: f32,
    pub write_ms: f32,
    pub critical_io_full: f32,
    pub critical_memory_full: f32,
    pub critical_mem_available_gb: f32,
    pub critical_swap_gb: f32,
    pub swap_pages: f32,
    pub critical_swap_pages: f32,
    pub hold_ms: i64,
    pub critical_hold_ms: i64,
    pub cooldown_ms: i64,
    pub step_ms: i64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            io_full: 15.0,
            memory_some: 10.0,
            mem_available_gb: 12.0,
            swap_gb: 4.0,
            cpu_some: 40.0,
            load_per_core: 0.7,
            write_ms: 0.0,
            critical_io_full: 30.0,
            critical_memory_full: 5.0,
            critical_mem_available_gb: 5.0,
            critical_swap_gb: 0.0,
            swap_pages: 100.0,
            critical_swap_pages: 1000.0,
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
        io_full: f("io_full", d.io_full),
        memory_some: f("memory_some", d.memory_some),
        mem_available_gb: f("mem_available_gb", d.mem_available_gb),
        swap_gb: f("swap_gb", d.swap_gb),
        cpu_some: f("cpu_some", d.cpu_some),
        load_per_core: f("load_per_core", d.load_per_core),
        write_ms: f("write_ms", d.write_ms),
        critical_io_full: f("critical_io_full", d.critical_io_full),
        critical_memory_full: f("critical_memory_full", d.critical_memory_full),
        critical_mem_available_gb: f("critical_mem_available_gb", d.critical_mem_available_gb),
        critical_swap_gb: f("critical_swap_gb", d.critical_swap_gb),
        swap_pages: f("swap_pages", d.swap_pages),
        critical_swap_pages: f("critical_swap_pages", d.critical_swap_pages),
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

fn swap_text(used: u64, device: &Option<String>) -> String {
    match device {
        Some(d) => format!("swap {} (swap on {d})", gb_text(used)),
        None => format!("swap {}", gb_text(used)),
    }
}

fn swap_rate(r: &PressureReadings) -> f32 {
    r.swap_in_rate.unwrap_or(0.0).max(r.swap_out_rate.unwrap_or(0.0))
}

fn swapping_text(r: &PressureReadings) -> String {
    let used = r.swap_used.map(|s| format!(", {}", swap_text(s, &r.swap_device))).unwrap_or_default();
    format!("swapping {:.0} pages/s{used}", swap_rate(r))
}

pub struct SwapTrend {
    pub growing: bool,
    pub grew_over_gb: bool,
}

fn above(v: Option<f32>, limit: f32) -> Option<f32> {
    v.filter(|v| limit > 0.0 && *v > limit)
}

fn hits(r: &PressureReadings, l: &Limits, trend: &SwapTrend) -> (Vec<Hit>, Vec<Hit>) {
    let mut p = Vec::new();
    let mut c = Vec::new();
    let pct = |key, name: &str, v: f32| Hit { key, text: format!("{name} {v:.0}%") };
    if let Some(v) = above(r.io_full10, l.io_full) {
        p.push(pct("io_full", "io full", v));
    }
    if let Some(v) = above(r.memory_some10, l.memory_some) {
        p.push(pct("memory_some", "memory some", v));
    }
    if let Some(m) = r.mem_available.filter(|m| gb(*m) < l.mem_available_gb as f64) {
        p.push(Hit { key: "mem_available", text: format!("{} available", gb_text(m)) });
    }
    let rate = swap_rate(r);
    if l.swap_pages > 0.0 && rate > l.swap_pages {
        p.push(Hit { key: "swap", text: swapping_text(r) });
    } else if let Some(s) = r.swap_used.filter(|s| trend.growing && gb(*s) > l.swap_gb as f64) {
        p.push(Hit { key: "swap", text: swap_text(s, &r.swap_device) });
    }
    if let Some(v) = above(r.cpu_some60, l.cpu_some) {
        p.push(pct("cpu_some", "cpu some", v));
    }
    if let Some(v) = above(r.load_per_core, l.load_per_core) {
        p.push(Hit { key: "load", text: format!("load {v:.2} per core") });
    }
    if l.write_ms > 0.0
        && let Some(d) = r.disks.iter().filter(|d| d.write_ms > l.write_ms).max_by(|a, b| a.write_ms.total_cmp(&b.write_ms))
    {
        p.push(Hit { key: "write_ms", text: format!("{} writes {:.0} ms", d.device, d.write_ms) });
    }
    if let Some(v) = above(r.io_full10, l.critical_io_full) {
        c.push(pct("c_io_full", "io full", v));
    }
    if let Some(v) = above(r.memory_full10, l.critical_memory_full) {
        c.push(pct("c_memory_full", "memory full", v));
    }
    if let Some(m) = r.mem_available.filter(|m| gb(*m) < l.critical_mem_available_gb as f64) {
        c.push(Hit { key: "c_mem_available", text: format!("{} available", gb_text(m)) });
    }
    if l.critical_swap_pages > 0.0 && rate > l.critical_swap_pages {
        c.push(Hit { key: "c_swap", text: swapping_text(r) });
    } else if let Some(s) = r.swap_used.filter(|s| l.critical_swap_gb > 0.0 && gb(*s) > l.critical_swap_gb as f64) {
        let trouble = trend.grew_over_gb
            || r.mem_available.is_some_and(|m| gb(m) < l.mem_available_gb as f64)
            || above(r.memory_some10, 5.0).is_some();
        if trouble {
            c.push(Hit { key: "c_swap", text: swap_text(s, &r.swap_device) });
        }
    }
    (p, c)
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
    swaps: VecDeque<(i64, u64)>,
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
            .filter(|h| self.held.get(h.key).is_some_and(|at| now - at >= hold))
            .map(|h| h.text.clone())
            .collect()
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
        if let Some(s) = r.swap_used {
            while self.swaps.front().is_some_and(|(at, _)| now - at > l.hold_ms) {
                self.swaps.pop_front();
            }
            self.swaps.push_back((now, s));
        }
        let growing = r.swap_used.is_some_and(|s| {
            self.swaps.iter().rev().skip(1).map(|(_, v)| *v).min().is_some_and(|low| s > low)
        });
        let grew_over_gb = r.swap_used.is_some_and(|s| {
            self.swaps.iter().rev().skip(1).map(|(_, v)| *v).min().is_some_and(|low| s.saturating_sub(low) as f64 > GB)
        });
        let (p, c) = hits(&r, l, &SwapTrend { growing, grew_over_gb });
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
        let mut events = Vec::new();
        let before = self.last_cap.unwrap_or(configured);
        let changed = if !c_held.is_empty() && self.state != PressureState::Critical {
            self.recover = None;
            Some(self.enter(PressureState::Critical, c_held.join(", "), now))
        } else if !p_held.is_empty() && self.state == PressureState::Ok {
            self.recover = None;
            Some(self.enter(PressureState::Pressure, p_held.join(", "), now))
        } else if self.state == PressureState::Critical
            && self.critical_quiet_since.is_some_and(|at| now - at >= l.hold_ms)
        {
            let reason = if p.is_empty() { "critical readings eased".to_string() } else { p.iter().map(|h| h.text.clone()).collect::<Vec<_>>().join(", ") };
            Some(self.enter(PressureState::Pressure, reason, now))
        } else if self.state == PressureState::Pressure && self.calm_since.is_some_and(|at| now - at >= l.cooldown_ms) {
            self.recover = Some((pressure_cap(configured), now));
            let mins = l.cooldown_ms / 60_000;
            Some(self.enter(PressureState::Ok, format!("calm for {mins} min"), now))
        } else {
            None
        };
        match self.state {
            PressureState::Critical if !c.is_empty() => self.reason = c.iter().map(|h| h.text.clone()).collect::<Vec<_>>().join(", "),
            PressureState::Pressure if !p.is_empty() && changed.is_none() => {
                self.reason = p.iter().map(|h| h.text.clone()).collect::<Vec<_>>().join(", ")
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
            mem_available: Some(40 * 1024 * 1024 * 1024),
            swap_used: Some(0),
            swap_device: Some("nvme1n1p1".into()),
            swap_in_rate: None,
            swap_out_rate: None,
            load_per_core: Some(0.2),
            disks: vec![],
        }
    }

    pub(crate) fn busy() -> PressureReadings {
        PressureReadings { io_full10: Some(22.0), io_some10: Some(22.0), ..calm() }
    }

    pub(crate) fn heavy() -> PressureReadings {
        PressureReadings { io_full10: Some(38.0), memory_full10: Some(7.0), ..calm() }
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

    #[test]
    fn one_bad_sample_does_not_scale_back() {
        let mut m = Monitor::default();
        let l = Limits::default();
        run(&mut m, &calm(), 0, 30 * S);
        run(&mut m, &heavy(), 40 * S, 40 * S);
        run(&mut m, &calm(), 50 * S, 200 * S);
        assert_eq!(m.state(), PressureState::Ok);
        assert_eq!(m.effective_cap(8, 200 * S, &l), 8);
    }

    #[test]
    fn sixty_seconds_of_io_pressure_halves_the_cap() {
        let mut m = Monitor::default();
        let l = Limits::default();
        assert!(run(&mut m, &busy(), 0, 50 * S).is_empty());
        let ev = run(&mut m, &busy(), 60 * S, 60 * S);
        assert_eq!(ev, vec![Event::Changed { from: PressureState::Ok, to: PressureState::Pressure, reason: "io full 22%".into(), cap: 4 }]);
        assert_eq!(m.effective_cap(8, 60 * S, &l), 4);
        assert_eq!(m.effective_cap(1, 60 * S, &l), 1);
        assert_eq!(m.effective_cap(3, 60 * S, &l), 1);
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
        assert!(m.snapshot(8, 30 * S, &l).reason.contains("io full 38%"));
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
        run(&mut m, &busy(), 0, 60 * S);
        run(&mut m, &calm(), 70 * S, 250 * S);
        run(&mut m, &busy(), 260 * S, 260 * S);
        run(&mut m, &calm(), 270 * S, 500 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        run(&mut m, &calm(), 510 * S, 570 * S);
        assert_eq!(m.state(), PressureState::Ok);
    }

    #[test]
    fn swap_counts_only_while_it_grows_and_names_its_device() {
        let mut m = Monitor::default();
        let gib = 1024 * 1024 * 1024u64;
        let flat = PressureReadings { swap_used: Some(6 * gib), ..calm() };
        run(&mut m, &flat, 0, 200 * S);
        assert_eq!(m.state(), PressureState::Ok, "a flat swap is not pressure");
        let mut t = 210 * S;
        let mut used = 6 * gib;
        while m.state() == PressureState::Ok && t < 400 * S {
            used += gib / 4;
            m.observe(PressureReadings { swap_used: Some(used), ..calm() }, &Limits::default(), 8, t);
            t += 10 * S;
        }
        assert_eq!(m.state(), PressureState::Pressure);
        assert!(m.reason.contains("swap on nvme1n1p1"), "{}", m.reason);
    }

    #[test]
    fn stale_swap_on_an_idle_host_stays_ok() {
        let mut m = Monitor::default();
        let gib = 1024 * 1024 * 1024u64;
        let r = PressureReadings { swap_used: Some(18 * gib), mem_available: Some(34 * gib), swap_in_rate: Some(0.0), swap_out_rate: Some(0.0), ..calm() };
        run(&mut m, &r, 0, 600 * S);
        assert_eq!(m.state(), PressureState::Ok);
        let l = Limits { critical_swap_gb: 16.0, ..Limits::default() };
        for t in 0..60 {
            m.observe(r.clone(), &l, 8, 700 * S + t * 10 * S);
        }
        assert_eq!(m.state(), PressureState::Ok, "a person's limit still needs evidence");
    }

    #[test]
    fn heavy_paging_out_is_critical_with_the_used_size_in_the_reason() {
        let mut m = Monitor::default();
        let gib = 1024 * 1024 * 1024u64;
        let r = PressureReadings { swap_used: Some(18 * gib), mem_available: Some(34 * gib), swap_in_rate: Some(0.0), swap_out_rate: Some(5000.0), ..calm() };
        run(&mut m, &r, 0, 30 * S);
        assert_eq!(m.state(), PressureState::Critical);
        assert!(m.reason.contains("swap 18 GB"), "{}", m.reason);
    }

    #[test]
    fn growing_swap_with_low_memory_is_pressure() {
        let mut m = Monitor::default();
        let gib = 1024 * 1024 * 1024u64;
        let l = Limits::default();
        for i in 0..24u64 {
            let r = PressureReadings { swap_used: Some(6 * gib + i * gib / 3), mem_available: Some(8 * gib), ..calm() };
            m.observe(r, &l, 8, (i * 10) as i64 * S);
        }
        assert_eq!(m.state(), PressureState::Pressure);
    }

    #[test]
    fn a_first_reading_cannot_be_critical_from_swap_alone() {
        let mut m = Monitor::default();
        let gib = 1024 * 1024 * 1024u64;
        let l = Limits { critical_swap_gb: 16.0, ..Limits::default() };
        let r = PressureReadings { swap_used: Some(18 * gib), mem_available: Some(34 * gib), ..calm() };
        let ev = m.observe(r, &l, 8, 0);
        assert!(ev.is_empty());
        assert_eq!(m.state(), PressureState::Ok);
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
        let gib = 1024 * 1024 * 1024u64;
        let l = Limits::default();
        let mut used = 18 * gib + gib / 2;
        let mut t = 0;
        while m.state() == PressureState::Ok && t < 200 * S {
            used += gib / 8;
            let r = PressureReadings { io_full10: Some(22.0), swap_used: Some(used), swap_out_rate: Some(5000.0), ..calm() };
            m.observe(r, &l, 8, t);
            t += 10 * S;
        }
        let snap = m.snapshot(8, t, &l);
        assert_eq!(snap.state, PressureState::Critical);
        let line = snap.line().unwrap();
        assert!(line.starts_with("starts paused, 0 of 8: swapping 5000 pages/s, swap 19 GB (swap on nvme1n1p1)"), "{line}");
        let p = HostPressure { state: PressureState::Pressure, effective_cap: 2, reason: "io full 22%, swap 19 GB".into(), ..snap };
        assert_eq!(p.line().unwrap(), "scaled to 2 of 8: io full 22%, swap 19 GB");
    }
}
