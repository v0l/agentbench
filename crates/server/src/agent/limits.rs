use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const POLL: Duration = Duration::from_millis(200);
pub const DEFAULT_MEMORY_MB: u64 = 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Limits {
    pub memory_mb: Option<u64>,
}

impl Limits {
    pub fn from_args(args: &Value) -> Limits {
        let memory_mb = match args.get("memory_mb").and_then(Value::as_f64) {
            None => Some(DEFAULT_MEMORY_MB),
            Some(m) if m >= 1.0 => Some(m as u64),
            Some(_) => None,
        };
        Limits { memory_mb }
    }

    pub async fn exceeded(&self, root: Option<u32>) -> u64 {
        let (Some(mb), Some(root)) = (self.memory_mb, root) else {
            return std::future::pending().await;
        };
        let cap = mb << 20;
        let Ok(mut sys) = System::new() else {
            return std::future::pending().await;
        };
        loop {
            tokio::time::sleep(POLL).await;
            sys.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing().with_memory(),
            );
            let used = tree_memory(&sys, root);
            if used > cap {
                return used >> 20;
            }
        }
    }
}

fn tree_memory(sys: &System, root: u32) -> u64 {
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if let Some(parent) = p.parent() {
            children.entry(parent).or_default().push(*pid);
        }
    }
    let mut total = 0;
    let mut stack = vec![Pid::from_u32(root)];
    while let Some(pid) = stack.pop() {
        if let Some(p) = sys.process(pid) {
            total += p.memory();
        }
        stack.extend(children.get(&pid).into_iter().flatten());
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_memory_limit_defaults_to_1g_and_zero_turns_it_off() {
        assert_eq!(Limits::from_args(&json!({})).memory_mb, Some(1024));
        assert_eq!(Limits::from_args(&json!({"memory_mb": 0})).memory_mb, None);
        assert_eq!(Limits::from_args(&json!({"memory_mb": 512})).memory_mb, Some(512));
    }

    #[cfg(unix)]
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

    #[tokio::test]
    async fn no_limit_never_fires() {
        let fired = tokio::time::timeout(Duration::from_millis(500), Limits::default().exceeded(Some(1))).await;
        assert!(fired.is_err());
    }
}
