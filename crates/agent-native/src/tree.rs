use tokio::process::Child;
#[cfg(unix)]
use {std::time::Duration, tokio::process::Command};

#[cfg(unix)]
const TERM_GRACE: Duration = Duration::from_secs(3);

/// Stop a child and everything it started: TERM to its process group, then KILL if it lingers.
pub async fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        signal_group(pid, "-TERM").await;
        if tokio::time::timeout(TERM_GRACE, child.wait()).await.is_ok() {
            return;
        }
        signal_group(pid, "-KILL").await;
    }
    let _ = child.kill().await;
}

#[cfg(unix)]
pub async fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("kill").args([signal, &format!("-{pid}")]).status().await;
}
