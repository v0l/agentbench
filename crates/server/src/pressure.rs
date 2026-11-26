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
    pub swap_grow_gb: f32,
    pub swap_full_pct: f32,
    pub disk_ratio: f32,
    pub disk_floor_ms: f32,
    pub swap_share_pct: f32,
    pub baseline_ms: i64,
    pub start_grace_ms: i64,
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
            swap_gb: 0.0,
            cpu_some: 40.0,
            load_per_core: 0.7,
            write_ms: 0.0,
            critical_io_full: 30.0,
            critical_memory_full: 5.0,
            critical_mem_available_gb: 5.0,
            critical_swap_gb: 0.0,
            swap_pages: 0.0,
            critical_swap_pages: 0.0,
            swap_grow_gb: 0.0,
            swap_full_pct: 85.0,
            disk_ratio: 3.0,
            disk_floor_ms: 1.0,
            swap_share_pct: 50.0,
            baseline_ms: 600_000,
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
        swap_grow_gb: f("swap_grow_gb", d.swap_grow_gb),
        swap_full_pct: f("swap_full_pct", d.swap_full_pct),
        disk_ratio: f("disk_ratio", d.disk_ratio),
        disk_floor_ms: f("disk_floor_ms", d.disk_floor_ms),
        swap_share_pct: f("swap_share_pct", d.swap_share_pct),
        baseline_ms: secs("baseline_secs", d.baseline_ms),
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
    pub grew: u64,
    pub window_secs: i64,
    pub disk: Option<SwapDisk>,
}

const PAGE_BYTES: f32 = 4096.0;
const BASELINE_SAMPLES: usize = 6;
const MB: f32 = 1_000_000.0;

#[derive(Clone, Copy, Debug)]
struct DiskSample {
    at: i64,
    queue_ms: f32,
    write_ms: f32,
    bytes_rate: f32,
    idle: bool,
}

#[derive(Clone, Debug)]
pub struct SwapDisk {
    device: String,
    queue_ms: f32,
    write_ms: f32,
    queue_base: f32,
    write_base: f32,
    peak: f32,
}

fn median(mut v: Vec<f32>) -> f32 {
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

fn swapping(r: &PressureReadings) -> bool {
    r.swap_in_rate.unwrap_or(0.0) + r.swap_out_rate.unwrap_or(0.0) > 0.0
}

fn swap_bytes(r: &PressureReadings) -> f32 {
    (r.swap_in_rate.unwrap_or(0.0) + r.swap_out_rate.unwrap_or(0.0)) * PAGE_BYTES
}

fn rising(v: f32, base: f32, l: &Limits) -> bool {
    v > base + l.disk_floor_ms
}

fn slow(v: f32, base: f32, l: &Limits) -> bool {
    l.disk_ratio > 0.0 && v > base * l.disk_ratio && rising(v, base, l)
}

impl SwapDisk {
    fn slow(&self, l: &Limits) -> Option<String> {
        let text = |what: &str, v: f32, base: f32| format!("swap disk {what} {v:.1} ms against {base:.1} ms normal");
        if slow(self.queue_ms, self.queue_base, l) {
            Some(text("queue", self.queue_ms, self.queue_base))
        } else if slow(self.write_ms, self.write_base, l) {
            Some(text("writes", self.write_ms, self.write_base))
        } else {
            None
        }
    }

    fn share(&self, r: &PressureReadings) -> f32 {
        if self.peak > 0.0 { swap_bytes(r) * 100.0 / self.peak } else { 0.0 }
    }

    fn crowded(&self, r: &PressureReadings, l: &Limits) -> Option<String> {
        let share = self.share(r);
        (l.swap_share_pct > 0.0 && share > l.swap_share_pct && rising(self.queue_ms, self.queue_base, l)).then(|| {
            format!(
                "swapping takes {share:.0} percent of {} ({:.1} MB/s of {:.1} MB/s peak), queue {:.1} ms against {:.1} ms normal",
                self.device,
                swap_bytes(r) / MB,
                self.peak / MB,
                self.queue_ms,
                self.queue_base
            )
        })
    }
}

fn harmed(r: &PressureReadings, l: &Limits, trend: &SwapTrend) -> bool {
    swapping(r)
        && (above(r.memory_some10, l.memory_some).is_some() || trend.disk.as_ref().is_some_and(|d| d.slow(l).is_some()))
}

fn growing(r: &PressureReadings, l: &Limits, trend: &SwapTrend) -> bool {
    l.swap_grow_gb > 0.0 && gb(trend.grew) >= l.swap_grow_gb as f64 && harmed(r, l, trend)
}

fn grew_text(used: u64, r: &PressureReadings, trend: &SwapTrend) -> String {
    format!("swap grew {} in {} s to {}", gb_text(trend.grew), trend.window_secs, swap_text(used, &r.swap_device).trim_start_matches("swap "))
}

fn swap_full(r: &PressureReadings, l: &Limits) -> Option<f64> {
    let (used, total) = (r.swap_used?, r.swap_total?);
    let pct = used as f64 * 100.0 / total as f64;
    (l.swap_full_pct > 0.0 && total > 0 && pct > l.swap_full_pct as f64).then_some(pct)
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
        if swapping(r) {
            p.push(Hit { key: "memory_some", text: format!("memory stall {v:.0} percent while swapping") });
        } else {
            p.push(pct("memory_some", "memory some", v));
        }
    }
    if let Some(m) = r.mem_available.filter(|m| gb(*m) < l.mem_available_gb as f64) {
        p.push(Hit { key: "mem_available", text: format!("{} available", gb_text(m)) });
    }
    let rate = swap_rate(r);
    let disk = trend.disk.as_ref().filter(|_| swapping(r));
    if l.swap_pages > 0.0 && rate > l.swap_pages {
        p.push(Hit { key: "swap", text: swapping_text(r) });
    } else if let Some(text) = disk.and_then(|d| d.slow(l).or_else(|| d.crowded(r, l))) {
        p.push(Hit { key: "swap", text });
    } else if let Some(s) = r.swap_used.filter(|s| growing(r, l, trend) && gb(*s) > l.swap_gb as f64) {
        p.push(Hit { key: "swap", text: grew_text(s, r, trend) });
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
    if let Some(pct) = swap_full(r, l) {
        c.push(Hit { key: "c_swap_full", text: format!("swap {pct:.0} percent full") });
    }
    if l.critical_swap_pages > 0.0 && rate > l.critical_swap_pages {
        c.push(Hit { key: "c_swap", text: swapping_text(r) });
    } else if let Some(s) = r.swap_used.filter(|s| l.critical_swap_gb > 0.0 && gb(*s) > l.critical_swap_gb as f64) {
        let trouble = harmed(r, l, trend) || r.mem_available.is_some_and(|m| gb(m) < l.mem_available_gb as f64);
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
    started: Option<i64>,
    swap_disk: Option<(String, VecDeque<DiskSample>)>,
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

    fn swap_disk(&mut self, r: &PressureReadings, l: &Limits, now: i64) -> Option<SwapDisk> {
        let name = r.swap_device.clone()?;
        let d = r.disks.iter().find(|d| d.device == name)?;
        if self.swap_disk.as_ref().is_none_or(|(n, _)| *n != name) {
            self.swap_disk = Some((name.clone(), VecDeque::new()));
        }
        let history = &mut self.swap_disk.as_mut()?.1;
        while history.front().is_some_and(|s| now - s.at > l.baseline_ms) {
            history.pop_front();
        }
        let idle: Vec<&DiskSample> = history.iter().filter(|s| s.idle).collect();
        let peak = history.iter().map(|s| s.bytes_rate).fold(d.bytes_rate, f32::max);
        let disk = (idle.len() >= BASELINE_SAMPLES).then(|| SwapDisk {
            device: name,
            queue_ms: d.queue_ms,
            write_ms: d.write_ms,
            queue_base: median(idle.iter().map(|s| s.queue_ms).collect()),
            write_base: median(idle.iter().map(|s| s.write_ms).collect()),
            peak,
        });
        let idle = disk.as_ref().is_none_or(|d| d.slow(l).is_none());
        history.push_back(DiskSample { at: now, queue_ms: d.queue_ms, write_ms: d.write_ms, bytes_rate: d.bytes_rate, idle });
        disk
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
        let grew = r.swap_used.map_or(0, |s| {
            self.swaps.iter().rev().skip(1).map(|(_, v)| *v).min().map_or(0, |low| s.saturating_sub(low))
        });
        let trend = SwapTrend { grew, window_secs: l.hold_ms / 1000, disk: self.swap_disk(&r, l, now) };
        let started = *self.started.get_or_insert(now);
        let (mut p, mut c) = hits(&r, l, &trend);
        if now - started < l.start_grace_ms {
            let paging = swapping(&r);
            let own_page_in = |h: &Hit| matches!(h.key, "swap" | "c_swap") || (paging && h.key == "memory_some");
            p.retain(|h| !own_page_in(h));
            c.retain(|h| !own_page_in(h));
        }
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
            PressureState::Ok if changed.is_none() => {
                self.reason = match self.readings.swap_used.filter(|s| gb(*s) > l.swap_gb as f64) {
                    Some(s) if p.is_empty() && c.is_empty() => format!("swap {} used, idle", gb_text(s)),
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
            swap_total: None,
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
    fn a_users_growth_rule_still_needs_measured_harm_and_names_its_device() {
        let mut m = Monitor::default();
        let gib = 1024 * 1024 * 1024u64;
        let l = Limits { swap_gb: 4.0, swap_grow_gb: 1.0, memory_some: 20.0, ..Limits::default() };
        let mut used = 6 * gib;
        for i in 0..30 {
            used += gib / 4;
            m.observe(PressureReadings { swap_used: Some(used), swap_out_rate: Some(50.0), ..calm() }, &l, 8, i * 10 * S);
        }
        assert_eq!(m.state(), PressureState::Ok, "growth with nothing waiting is not pressure");
        let mut t = 300 * S;
        while m.state() == PressureState::Ok && t < 600 * S {
            used += gib / 4;
            m.observe(PressureReadings { swap_used: Some(used), swap_out_rate: Some(50.0), memory_some10: Some(25.0), ..calm() }, &l, 8, t);
            t += 10 * S;
        }
        assert_eq!(m.state(), PressureState::Pressure);
        assert!(m.reason.contains("swap grew 1.5 GB in 60 s to "), "{}", m.reason);
        assert!(m.reason.contains("swap on nvme1n1p1"), "{}", m.reason);
    }

    #[test]
    fn swap_growth_without_paging_out_is_not_pressure() {
        let mut m = Monitor::default();
        let gib = 1024 * 1024 * 1024u64;
        for i in 0..40u64 {
            let r = PressureReadings { swap_used: Some(6 * gib + i * gib / 4), swap_out_rate: Some(0.0), ..calm() };
            m.observe(r, &Limits::default(), 8, i as i64 * 10 * S);
        }
        assert_eq!(m.state(), PressureState::Ok);
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

    #[test]
    fn swap_over_the_headroom_line_is_critical_and_says_how_full() {
        let mut m = Monitor::default();
        let l = Limits::default();
        let r = PressureReadings { swap_used: Some(44_600_000_000), ..live_idle_host() };
        run(&mut m, &r, 0, 20 * S);
        assert_eq!(m.state(), PressureState::Ok);
        let ev = run(&mut m, &r, 30 * S, 30 * S);
        assert!(matches!(&ev[..], [Event::Changed { to: PressureState::Critical, cap: 0, .. }]), "{ev:?}");
        assert_eq!(m.reason, "swap 91 percent full");
        assert_eq!(m.effective_cap(8, 30 * S, &l), 0);
        let mut off = Monitor::default();
        let l0 = Limits { swap_full_pct: 0.0, ..Limits::default() };
        for i in 0..30 {
            off.observe(r.clone(), &l0, 8, i * 10 * S);
        }
        assert_eq!(off.state(), PressureState::Ok, "0 turns the headroom rule off");
        let mut under = Monitor::default();
        run(&mut under, &PressureReadings { swap_used: Some(41_000_000_000), ..live_idle_host() }, 0, 300 * S);
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
            m.observe(PressureReadings { swap_used: Some(used), swap_out_rate: Some(400.0), memory_some10: Some(14.0), ..live_idle_host() }, &l, 5, t);
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

    #[test]
    fn heavy_paging_alone_is_not_critical_but_a_users_page_limit_is() {
        let gib = 1024 * 1024 * 1024u64;
        let r = PressureReadings { swap_used: Some(18 * gib), mem_available: Some(34 * gib), swap_in_rate: Some(0.0), swap_out_rate: Some(5000.0), ..calm() };
        let mut m = Monitor::default();
        run(&mut m, &r, 0, 600 * S);
        assert_eq!(m.state(), PressureState::Ok);
        let mut set = Monitor::default();
        let l = Limits { critical_swap_pages: 1000.0, ..Limits::default() };
        for i in 0..12 {
            set.observe(r.clone(), &l, 8, i * 10 * S);
        }
        assert_eq!(set.state(), PressureState::Critical);
        assert!(set.reason.contains("swap 18 GB"), "{}", set.reason);
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

    fn on_disk(r: PressureReadings, queue_ms: f32, write_ms: f32, bytes_rate: f32) -> PressureReadings {
        let disk = DiskReading { device: "nvme1n1p1".into(), path: "/mnt/data".into(), write_ms, queue_ms, bytes_rate };
        PressureReadings { swap_device: Some("nvme1n1p1".into()), disks: vec![disk], ..r }
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

    fn observe_all(m: &mut Monitor, l: &Limits, rs: &[PressureReadings], from: i64) -> Vec<Event> {
        let mut out = Vec::new();
        for (i, r) in rs.iter().enumerate() {
            out.extend(m.observe(r.clone(), l, 5, from + i as i64 * 10 * S));
        }
        out
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
    fn the_same_page_rate_on_a_slow_swap_disk_with_memory_stall_is_pressure() {
        let mut m = Monitor::default();
        let l = Limits::default();
        observe_all(&mut m, &l, &vec![live_restart_host(); 12], 0);
        let slow = on_disk(PressureReadings { memory_some10: Some(14.0), ..live_restart_host() }, 6.1, 0.4, 600_000.0);
        observe_all(&mut m, &l, &vec![slow; 8], 120 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        assert!(m.reason.contains("memory stall 14 percent while swapping"), "{}", m.reason);
        assert!(m.reason.contains("swap disk queue 6.1 ms against 1.2 ms normal"), "{}", m.reason);
    }

    #[test]
    fn a_slow_swap_disk_alone_is_pressure_only_while_swapping() {
        let l = Limits::default();
        let slow = on_disk(live_restart_host(), 6.1, 0.4, 600_000.0);
        let mut m = Monitor::default();
        observe_all(&mut m, &l, &vec![live_restart_host(); 12], 0);
        observe_all(&mut m, &l, &vec![slow.clone(); 8], 120 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        assert_eq!(m.reason, "swap disk queue 6.1 ms against 1.2 ms normal");
        let mut still = Monitor::default();
        let quiet = PressureReadings { swap_in_rate: Some(0.0), ..slow };
        observe_all(&mut still, &l, &vec![live_restart_host(); 12], 0);
        observe_all(&mut still, &l, &vec![quiet; 30], 120 * S);
        assert_eq!(still.state(), PressureState::Ok, "a slow disk with no swapping is not swap pressure");
    }

    #[test]
    fn a_fast_and_a_slow_swap_disk_differ_only_through_their_measured_latency() {
        let l = Limits::default();
        let paging = PressureReadings { swap_in_rate: Some(3000.0), ..live_restart_host() };
        let fast: Vec<PressureReadings> = (0..60).map(|i| on_disk(paging.clone(), if i < 12 { 0.1 } else { 0.4 }, 0.1, 12_000_000.0)).collect();
        let mut m = Monitor::default();
        assert!(observe_all(&mut m, &l, &fast, 0).is_empty());
        assert_eq!(m.state(), PressureState::Ok, "jitter under the floor on a fast disk");
        let mut slow_disk = Monitor::default();
        let idle: Vec<PressureReadings> = (0..12).map(|_| on_disk(paging.clone(), 4.0, 3.0, 100_000_000.0)).collect();
        observe_all(&mut slow_disk, &l, &idle, 0);
        assert_eq!(slow_disk.state(), PressureState::Ok, "a slow disk at its own baseline");
        observe_all(&mut slow_disk, &l, &vec![on_disk(paging.clone(), 9.0, 3.0, 12_000_000.0); 7], 120 * S);
        assert_eq!(slow_disk.state(), PressureState::Ok, "over the floor but under three times normal");
        observe_all(&mut slow_disk, &l, &vec![on_disk(paging, 15.0, 3.0, 12_000_000.0); 8], 190 * S);
        assert_eq!(slow_disk.state(), PressureState::Pressure);
        assert_eq!(slow_disk.reason, "swap disk queue 15.0 ms against 4.0 ms normal");
    }

    #[test]
    fn a_page_rate_alone_in_either_direction_is_never_pressure() {
        let l = Limits::default();
        for r in [
            PressureReadings { swap_in_rate: Some(3000.0), ..live_restart_host() },
            PressureReadings { swap_out_rate: Some(50000.0), ..live_restart_host() },
            PressureReadings { swap_out_rate: Some(50000.0), disks: vec![], ..live_restart_host() },
        ] {
            let mut m = Monitor::default();
            assert!(observe_all(&mut m, &l, &vec![r; 60], 0).is_empty());
            assert_eq!(m.state(), PressureState::Ok);
        }
    }

    #[test]
    fn swapping_that_takes_most_of_the_disk_with_a_rising_queue_is_pressure() {
        let l = Limits::default();
        let mut m = Monitor::default();
        let idle: Vec<PressureReadings> = (0..12).map(|_| on_disk(live_restart_host(), 2.0, 1.0, 100_000_000.0)).collect();
        observe_all(&mut m, &l, &idle, 0);
        let crowded = on_disk(PressureReadings { swap_out_rate: Some(20000.0), ..live_restart_host() }, 3.5, 1.0, 90_000_000.0);
        observe_all(&mut m, &l, &vec![crowded.clone(); 8], 120 * S);
        assert_eq!(m.state(), PressureState::Pressure);
        assert!(m.reason.starts_with("swapping takes 83 percent of nvme1n1p1 (82.5 MB/s of 100.0 MB/s peak), queue 3.5 ms against 2.0 ms normal"), "{}", m.reason);
        let mut flat = Monitor::default();
        observe_all(&mut flat, &l, &idle, 0);
        observe_all(&mut flat, &l, &vec![on_disk(crowded, 2.5, 1.0, 90_000_000.0); 30], 120 * S);
        assert_eq!(flat.state(), PressureState::Ok, "a high share with a flat queue is the disk coping");
    }

    #[test]
    fn the_first_minute_after_start_is_ignored() {
        let l = Limits { critical_swap_pages: 1000.0, ..Limits::default() };
        let burst = on_disk(PressureReadings { swap_in_rate: Some(30000.0), memory_some10: Some(30.0), ..live_restart_host() }, 40.0, 9.0, 120_000_000.0);
        let mut m = Monitor::default();
        let mut samples = vec![burst.clone(); 6];
        samples.extend(vec![live_restart_host(); 60]);
        assert!(observe_all(&mut m, &l, &samples, 0).is_empty());
        assert_eq!(m.state(), PressureState::Ok);
        let mut later = Monitor::default();
        observe_all(&mut later, &l, &vec![live_restart_host(); 7], 0);
        observe_all(&mut later, &l, &vec![burst; 6], 70 * S);
        assert_eq!(later.state(), PressureState::Critical, "after the first minute the same burst counts");
    }

    #[test]
    fn critical_needs_two_consecutive_samples() {
        let l = Limits { critical_hold_ms: 0, ..Limits::default() };
        let mut m = Monitor::default();
        m.observe(calm(), &l, 8, 0);
        assert!(m.observe(heavy(), &l, 8, 10 * S).is_empty(), "one sample");
        m.observe(calm(), &l, 8, 20 * S);
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
        let gib = 1024 * 1024 * 1024u64;
        let l = Limits { critical_swap_pages: 1000.0, ..Limits::default() };
        let mut used = 18 * gib + gib / 2;
        let mut t = 0;
        while m.state() != PressureState::Critical && t < 300 * S {
            used += gib / 8;
            let r = PressureReadings { io_full10: Some(22.0), swap_used: Some(used), swap_out_rate: Some(5000.0), ..calm() };
            m.observe(r, &l, 8, t);
            t += 10 * S;
        }
        let snap = m.snapshot(8, t, &l);
        assert_eq!(snap.state, PressureState::Critical);
        let line = snap.line().unwrap();
        assert!(line.starts_with("starts paused, 0 of 8: swapping 5000 pages/s, swap 20 GB (swap on nvme1n1p1)"), "{line}");
        let p = HostPressure { state: PressureState::Pressure, effective_cap: 2, reason: "io full 22%, swap 19 GB".into(), ..snap };
        assert_eq!(p.line().unwrap(), "scaled to 2 of 8: io full 22%, swap 19 GB");
    }
}
