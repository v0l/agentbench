use agentbench_core::{DiskReading, Host, PressureReadings};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use sysinfo::{Disks, System};

pub fn sample() -> Host {
    let mut host = Host {
        load: System::load_average().map(|l| l.one as f32).unwrap_or_default(),
        cores: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
        ..Host::default()
    };
    if let Ok(mut sys) = System::new() {
        sys.refresh_memory();
        host.mem_total = sys.total_memory();
        host.mem_used = sys.total_memory().saturating_sub(sys.available_memory());
    }
    if let Ok(disks) = Disks::new_with_refreshed_list() {
        let root = disks
            .iter()
            .find(|d| d.mount_point() == std::path::Path::new("/"))
            .or_else(|| disks.iter().max_by_key(|d| d.total_space()));
        if let Some(d) = root {
            host.disk_total = d.total_space();
            host.disk_free = d.available_space();
        }
    }
    host
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DiskCounters {
    ios: u64,
    writes: u64,
    write_ms: u64,
    weighted_ms: u64,
    sectors: u64,
}

pub struct Reader {
    root: PathBuf,
    cores: u32,
    live: bool,
    last: HashMap<String, (i64, DiskCounters)>,
    vm: VecDeque<(i64, u64, u64)>,
    workers_of: Option<u32>,
}

const SWAP_WINDOW_MS: i64 = 60_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

fn vmstat_swap(text: &str) -> Option<(u64, u64)> {
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.trim().parse::<u64>().ok());
    Some((get("pswpin ")?, get("pswpout ")?))
}

impl Reader {
    pub fn system() -> Reader {
        Reader {
            root: PathBuf::from("/"),
            cores: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1),
            live: cfg!(target_os = "linux"),
            last: HashMap::new(),
            vm: VecDeque::new(),
            workers_of: Some(std::process::id()),
        }
    }

    pub fn at(root: &Path, cores: u32) -> Reader {
        Reader { root: root.to_path_buf(), cores: cores.max(1), live: true, last: HashMap::new(), vm: VecDeque::new(), workers_of: None }
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.join(path.trim_start_matches('/'))).ok()
    }

    pub fn sample(&mut self, devices: &[String]) -> PressureReadings {
        self.sample_at(devices, now_ms())
    }

    fn swap_rates(&mut self, text: &str, now: i64) -> (Option<f32>, Option<f32>) {
        let Some((pin, pout)) = vmstat_swap(text) else { return (None, None) };
        while self.vm.front().is_some_and(|(at, _, _)| now - at > SWAP_WINDOW_MS) {
            self.vm.pop_front();
        }
        let rates = self.vm.front().filter(|(at, _, _)| now > *at).map(|(at, i, o)| {
            let secs = (now - at) as f32 / 1000.0;
            (pin.saturating_sub(*i) as f32 / secs, pout.saturating_sub(*o) as f32 / secs)
        });
        self.vm.push_back((now, pin, pout));
        (rates.map(|r| r.0), rates.map(|r| r.1))
    }

    pub fn sample_at(&mut self, devices: &[String], now: i64) -> PressureReadings {
        let mut r = PressureReadings::default();
        if !self.live {
            return r;
        }
        if let Some(text) = self.read("/proc/pressure/cpu") {
            let p = psi(&text);
            (r.cpu_some10, r.cpu_some60) = (p.some10, p.some60);
            r.known = true;
        }
        if let Some(text) = self.read("/proc/pressure/memory") {
            let p = psi(&text);
            (r.memory_some10, r.memory_some60, r.memory_full10, r.memory_full60) = (p.some10, p.some60, p.full10, p.full60);
            r.known = true;
        }
        if let Some(text) = self.read("/proc/pressure/io") {
            let p = psi(&text);
            (r.io_some10, r.io_some60, r.io_full10, r.io_full60) = (p.some10, p.some60, p.full10, p.full60);
            r.known = true;
        }
        if let Some(text) = self.read("/proc/meminfo") {
            let kb = meminfo(&text);
            let get = |k: &str| kb.get(k).map(|v| v * 1024);
            r.mem_available = get("MemAvailable");
            if let (Some(total), Some(free)) = (get("SwapTotal"), get("SwapFree")) {
                r.swap_used = Some(total.saturating_sub(free));
                r.swap_total = Some(total);
            }
            r.known |= r.mem_available.is_some();
        }
        if let Some(load) = self.read("/proc/loadavg").and_then(|t| t.split_whitespace().next()?.parse::<f32>().ok()) {
            r.load_per_core = Some(load / self.cores as f32);
            r.known = true;
        }
        if let Some(text) = self.read("/proc/vmstat") {
            (r.swap_in_rate, r.swap_out_rate) = self.swap_rates(&text, now);
        }
        let mounts = self.read("/proc/self/mountinfo").map(|t| mountinfo(&t)).unwrap_or_default();
        r.swap_device = self.read("/proc/swaps").and_then(|t| swap_device(&t, &mounts));
        if let Some(stats) = self.read("/proc/diskstats") {
            let mut wanted = devices.to_vec();
            wanted.extend(r.swap_device.clone());
            r.disks = self.disks(&stats, &mounts, &wanted, now);
        }
        if let Some(root) = self.workers_of {
            r.worker_resident = Some(agent_core::limits::largest_child(root));
        }
        r
    }

    fn disks(&mut self, stats: &str, mounts: &[Mount], devices: &[String], now: i64) -> Vec<DiskReading> {
        let rows = diskstats(stats);
        let mut out: Vec<DiskReading> = Vec::new();
        for path in devices {
            let Some(row) = find_row(&rows, mounts, path) else { continue };
            if out.iter().any(|d| d.device == row.name) {
                continue;
            }
            let (at, prev) = self.last.insert(row.name.clone(), (now, row.counters)).unwrap_or((now, DiskCounters::default()));
            let d = |a: u64, b: u64| a.saturating_sub(b) as f32;
            let writes = d(row.counters.writes, prev.writes);
            let ios = d(row.counters.ios, prev.ios);
            let secs = (now - at) as f32 / 1000.0;
            out.push(DiskReading {
                device: row.name.clone(),
                path: path.clone(),
                write_ms: if writes > 0.0 { d(row.counters.write_ms, prev.write_ms) / writes } else { 0.0 },
                queue_ms: if ios > 0.0 { d(row.counters.weighted_ms, prev.weighted_ms) / ios } else { 0.0 },
                bytes_rate: if secs > 0.0 { d(row.counters.sectors, prev.sectors) * 512.0 / secs } else { 0.0 },
            });
        }
        out
    }
}

pub fn default_devices() -> Vec<String> {
    let mut out = vec![std::env::var("TMPDIR").ok().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "/tmp".into())];
    if let Some(parent) = std::env::var("CARGO_TARGET_DIR")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .and_then(|t| Path::new(&t).parent().map(|p| p.display().to_string()))
    {
        out.push(parent);
    }
    out
}

#[derive(Default)]
struct Psi {
    some10: Option<f32>,
    some60: Option<f32>,
    full10: Option<f32>,
    full60: Option<f32>,
}

fn psi(text: &str) -> Psi {
    let mut p = Psi::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let kind = words.next();
        let mut avg10 = None;
        let mut avg60 = None;
        for w in words {
            if let Some(v) = w.strip_prefix("avg10=") {
                avg10 = v.parse().ok();
            } else if let Some(v) = w.strip_prefix("avg60=") {
                avg60 = v.parse().ok();
            }
        }
        match kind {
            Some("some") => (p.some10, p.some60) = (avg10, avg60),
            Some("full") => (p.full10, p.full60) = (avg10, avg60),
            _ => {}
        }
    }
    p
}

fn meminfo(text: &str) -> HashMap<String, u64> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.split_whitespace().next()?.parse().ok()?))
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
struct Mount {
    dev: (u32, u32),
    point: String,
    source: String,
}

fn unescape(s: &str) -> String {
    s.replace("\\040", " ").replace("\\011", "\t").replace("\\134", "\\")
}

fn mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|l| {
            let (head, tail) = l.split_once(" - ")?;
            let f: Vec<&str> = head.split_whitespace().collect();
            let (maj, min) = f.get(2)?.split_once(':')?;
            Some(Mount {
                dev: (maj.parse().ok()?, min.parse().ok()?),
                point: unescape(f.get(4)?),
                source: tail.split_whitespace().nth(1).unwrap_or_default().to_string(),
            })
        })
        .collect()
}

fn mount_of<'a>(mounts: &'a [Mount], path: &str) -> Option<&'a Mount> {
    let path = std::fs::canonicalize(path).map(|p| p.display().to_string()).unwrap_or_else(|_| path.to_string());
    mounts
        .iter()
        .filter(|m| {
            m.point == "/"
                || path == m.point
                || path.strip_prefix(&m.point).is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|m| m.point.len())
}

fn short(source: &str) -> String {
    source.rsplit('/').next().unwrap_or(source).to_string()
}

fn swap_device(text: &str, mounts: &[Mount]) -> Option<String> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let used: u64 = f.get(3)?.parse().ok()?;
            let name = unescape(f.first()?);
            let device = if f.get(1) == Some(&"partition") {
                short(&name)
            } else {
                mount_of(mounts, &name).map(|m| short(&m.source)).unwrap_or(name)
            };
            Some((used, device))
        })
        .max_by_key(|(used, _)| *used)
        .map(|(_, d)| d)
}

struct Row {
    dev: (u32, u32),
    name: String,
    counters: DiskCounters,
}

fn diskstats(text: &str) -> Vec<Row> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let n = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
            Some(Row {
                dev: (n(0)? as u32, n(1)? as u32),
                name: f.get(2)?.to_string(),
                counters: DiskCounters {
                    ios: n(3)? + n(7)?,
                    writes: n(7)?,
                    write_ms: n(10)?,
                    weighted_ms: n(13)?,
                    sectors: n(5)? + n(9)?,
                },
            })
        })
        .collect()
}

fn find_row<'a>(rows: &'a [Row], mounts: &[Mount], wanted: &str) -> Option<&'a Row> {
    if let Some(row) = rows.iter().find(|r| r.name == short(wanted) && !wanted.contains('/')) {
        return Some(row);
    }
    let m = mount_of(mounts, wanted)?;
    rows.iter().find(|r| r.dev == m.dev).or_else(|| rows.iter().find(|r| r.name == short(&m.source)))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn the_host_reports_memory_disk_and_cores() {
        let h = super::sample();
        assert!(h.mem_total > 0 && h.mem_used <= h.mem_total);
        assert!(h.disk_total > 0 && h.disk_free <= h.disk_total);
        assert!(h.cores >= 1);
        assert!(h.mem_percent() <= 100 && h.disk_percent() <= 100);
    }

    pub(crate) fn fake_proc(dir: &Path, files: &[(&str, &str)]) {
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    const MOUNTS: &str = "22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw\n\
        40 22 259:5 / /mnt/data rw,relatime shared:2 - ext4 /dev/nvme1n1p1 rw\n\
        41 22 0:30 / /proc rw - proc proc rw\n";

    fn desktop(dir: &Path, written: u64, write_ms: u64) {
        fake_proc(
            dir,
            &[
                ("proc/pressure/cpu", "some avg10=12.50 avg60=41.00 avg300=30.00 total=1\nfull avg10=0.00 avg60=0.00 avg300=0.00 total=0\n"),
                ("proc/pressure/memory", "some avg10=3.10 avg60=2.00 avg300=1.00 total=1\nfull avg10=1.50 avg60=1.00 avg300=0.50 total=1\n"),
                ("proc/pressure/io", "some avg10=22.00 avg60=21.00 avg300=20.00 total=1\nfull avg10=21.00 avg60=19.50 avg300=18.00 total=1\n"),
                ("proc/meminfo", "MemTotal:       65000000 kB\nMemAvailable:    9375000 kB\nSwapTotal:      50331648 kB\nSwapFree:       30331648 kB\n"),
                ("proc/loadavg", "24.00 21.00 20.00 3/2000 12345\n"),
                ("proc/swaps", "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n/mnt/data/swapfile                      file\t\t50331644\t20000000\t-2\n"),
                ("proc/self/mountinfo", MOUNTS),
                (
                    "proc/diskstats",
                    &format!(
                        " 259       2 nvme0n1p2 100 0 800 50 100 0 800 80 0 100 130 0 0 0 0\n 259       5 nvme1n1p1 1000 0 8000 500 {written} 0 9000 {write_ms} 3 900 {} 0 0 0 0\n",
                        write_ms + 500
                    ),
                ),
            ],
        );
    }

    #[test]
    fn the_reader_parses_pressure_memory_swap_load_and_the_swap_device() {
        let dir = tempfile::tempdir().unwrap();
        desktop(dir.path(), 1000, 15_000);
        let r = Reader::at(dir.path(), 48).sample(&["/mnt/data/cargo-target".into()]);
        assert!(r.known);
        assert_eq!((r.io_full10, r.io_some60, r.memory_full10, r.cpu_some60), (Some(21.0), Some(21.0), Some(1.5), Some(41.0)));
        assert_eq!(r.mem_available, Some(9_375_000 * 1024));
        assert_eq!(r.swap_used, Some(20_000_000 * 1024));
        assert_eq!(r.swap_total, Some(50_331_648 * 1024));
        assert_eq!(r.load_per_core, Some(0.5));
        assert_eq!(r.swap_device.as_deref(), Some("nvme1n1p1"));
        assert_eq!(r.disks.len(), 1);
        assert_eq!(r.disks[0].device, "nvme1n1p1");
        assert_eq!(r.disks[0].write_ms, 15.0);
    }

    #[test]
    fn disk_latency_is_measured_between_samples() {
        let dir = tempfile::tempdir().unwrap();
        desktop(dir.path(), 1000, 1000);
        let mut reader = Reader::at(dir.path(), 48);
        reader.sample(&["/mnt/data".into(), "/mnt/data/x".into(), "/".into()]);
        desktop(dir.path(), 1100, 2500);
        let r = reader.sample(&["/mnt/data".into(), "/".into()]);
        let data = r.disks.iter().find(|d| d.device == "nvme1n1p1").unwrap();
        assert_eq!(data.write_ms, 15.0);
        assert_eq!(r.disks.len(), 2, "{:?}", r.disks);
    }

    #[test]
    fn a_swap_partition_is_named_by_itself() {
        let text = "Filename Type Size Used Priority\n/dev/sda2 partition 100 50 -2\n/mnt/data/swapfile file 100 10 -3\n";
        assert_eq!(swap_device(text, &mountinfo(MOUNTS)).as_deref(), Some("sda2"));
    }

    #[test]
    fn swap_rates_come_from_vmstat_deltas_over_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = Reader::at(dir.path(), 4);
        let vm = |i: u64, o: u64| format!("pgfault 5\npswpin {i}\npswpout {o}\npswpoutx 9\n");
        fake_proc(dir.path(), &[("proc/vmstat", &vm(1000, 2000))]);
        let first = reader.sample_at(&[], 0);
        assert_eq!((first.swap_in_rate, first.swap_out_rate), (None, None));
        fake_proc(dir.path(), &[("proc/vmstat", &vm(1000, 152_000))]);
        let r = reader.sample_at(&[], 30_000);
        assert_eq!((r.swap_in_rate, r.swap_out_rate), (Some(0.0), Some(5000.0)));
    }

    #[test]
    fn the_swap_device_is_always_read_with_its_byte_rate() {
        let dir = tempfile::tempdir().unwrap();
        desktop(dir.path(), 1000, 1000);
        let mut reader = Reader::at(dir.path(), 48);
        reader.sample_at(&["/".into()], 0);
        let stats = " 259       2 nvme0n1p2 100 0 800 50 100 0 800 80 0 100 130 0 0 0 0\n 259       5 nvme1n1p1 1000 0 28000 500 1100 0 9000 2500 3 900 3000 0 0 0 0\n";
        fake_proc(dir.path(), &[("proc/diskstats", stats)]);
        let r = reader.sample_at(&["/".into()], 10_000);
        let swap = r.disks.iter().find(|d| d.device == "nvme1n1p1").expect("the swap device is read");
        assert_eq!(swap.bytes_rate, 20_000.0 * 512.0 / 10.0);
        assert_eq!(swap.write_ms, 15.0);
    }

    #[test]
    fn a_missing_proc_reads_unknown_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let r = Reader::at(dir.path(), 4).sample(&default_devices());
        assert!(!r.known);
        assert_eq!(r, PressureReadings::default());
    }
}
