use serde_json::Value;
use std::time::Duration;

const POLL: Duration = Duration::from_secs(1);
pub const DEFAULT_MEMORY_MB: u64 = 1024;
pub const DEFAULT_BACKGROUND_MEMORY_MB: u64 = 8192;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    pub memory_mb: Option<u64>,
}

impl Limits {
    pub fn from_args(args: &Value) -> Limits {
        Limits::with_default(args, DEFAULT_MEMORY_MB)
    }

    pub fn background_from_args(args: &Value) -> Limits {
        Limits::with_default(args, DEFAULT_BACKGROUND_MEMORY_MB)
    }

    fn with_default(args: &Value, default_mb: u64) -> Limits {
        let memory_mb = match args.get("memory_mb").and_then(Value::as_f64) {
            None => Some(default_mb),
            Some(m) if m >= 1.0 => Some(m as u64),
            Some(_) => None,
        };
        Limits { memory_mb }
    }

    pub async fn exceeded(&self, root: Option<u32>) -> u64 {
        let (Some(mb), Some(root), true) = (self.memory_mb, root, cfg!(target_os = "linux")) else {
            return std::future::pending().await;
        };
        let cap = mb << 20;
        loop {
            tokio::time::sleep(POLL).await;
            let used = tokio::task::spawn_blocking(move || tree_memory(root, proc_children, proc_rss))
                .await
                .unwrap_or(0);
            if used > cap {
                return used >> 20;
            }
        }
    }
}

fn tree_memory(root: u32, children: impl Fn(u32) -> Vec<u32>, rss: impl Fn(u32) -> u64) -> u64 {
    let mut total = 0;
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        total += rss(pid);
        stack.extend(children(pid));
    }
    total
}

#[cfg(not(target_os = "linux"))]
fn proc_children(_pid: u32) -> Vec<u32> {
    Vec::new()
}

#[cfg(not(target_os = "linux"))]
fn proc_rss(_pid: u32) -> u64 {
    0
}

#[cfg(target_os = "linux")]
fn proc_children(pid: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for task in tasks.flatten() {
        if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
            out.extend(text.split_whitespace().filter_map(|p| p.parse::<u32>().ok()));
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn proc_rss(pid: u32) -> u64 {
    let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/statm")) else {
        return 0;
    };
    let pages = text.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()).unwrap_or(0);
    pages * page_size()
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    static SIZE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if n > 0 {
            n as u64
        } else {
            4096
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn a_memory_limit_defaults_to_1g_and_zero_turns_it_off() {
        assert_eq!(Limits::from_args(&json!({})).memory_mb, Some(1024));
        assert_eq!(Limits::from_args(&json!({"memory_mb": 0})).memory_mb, None);
        assert_eq!(Limits::from_args(&json!({"memory_mb": 512})).memory_mb, Some(512));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_process_tree_over_its_memory_limit_is_reported() {
        let mut child = tokio::process::Command::new("bash")
            .arg("-c")
            .arg("python3 -c 'b = bytearray(300 << 20); import time; time.sleep(30)'")
            .spawn()
            .unwrap();
        let limits = Limits { memory_mb: Some(100) };
        let used = tokio::time::timeout(Duration::from_secs(10), limits.exceeded(child.id()))
            .await
            .expect("the limit was never noticed");
        assert!(used >= 100, "{used}");
        let _ = child.kill().await;
    }

    #[cfg(target_os = "linux")]
    fn self_cpu() -> Duration {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
        let tv = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
        tv(ru.ru_utime) + tv(ru.ru_stime)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn thirty_capped_commands_keep_the_server_cpu_low() {
        if std::env::var_os("LIMITER_CPU_CHILD").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "limits::tests::thirty_capped_commands_keep_the_server_cpu_low", "--nocapture"])
                .env("LIMITER_CPU_CHILD", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        let mut children = Vec::new();
        for _ in 0..30 {
            children.push(tokio::process::Command::new("sleep").arg("60").spawn().unwrap());
        }
        let before = self_cpu();
        let wall = std::time::Instant::now();
        let limits = Limits { memory_mb: Some(1024) };
        let mut watchers = Vec::new();
        for c in &children {
            let id = c.id();
            watchers.push(tokio::spawn(async move { limits.exceeded(id).await }));
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
        let cpu = self_cpu() - before;
        let pct = cpu.as_secs_f64() / wall.elapsed().as_secs_f64() * 100.0;
        eprintln!("limiter cpu: {pct:.1}% of one core for 30 commands");
        for w in watchers {
            w.abort();
        }
        for mut c in children {
            let _ = c.kill().await;
        }
        assert!(pct < 10.0, "{pct:.1}%");
    }

    #[test]
    fn the_walk_only_reads_descendants_of_the_root() {
        let tree: HashMap<u32, Vec<u32>> =
            HashMap::from([(10, vec![11, 12]), (11, vec![13]), (20, vec![21]), (1, vec![10, 20])]);
        let read = std::cell::RefCell::new(Vec::new());
        let total = tree_memory(
            10,
            |p| {
                read.borrow_mut().push(p);
                tree.get(&p).cloned().unwrap_or_default()
            },
            |p| {
                read.borrow_mut().push(p);
                p as u64
            },
        );
        assert_eq!(total, 10 + 11 + 12 + 13);
        let mut seen = read.into_inner();
        seen.sort();
        seen.dedup();
        assert_eq!(seen, vec![10, 11, 12, 13]);
    }

    #[tokio::test]
    async fn no_limit_never_fires() {
        let fired = tokio::time::timeout(Duration::from_millis(500), Limits::default().exceeded(Some(1))).await;
        assert!(fired.is_err());
    }
}
