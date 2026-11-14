//! Fetching one file off the network, with progress, written beside its target and renamed so a
//! download that stops half way is not taken for the file.

use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// Where a download has got to, for a status line.
pub struct Progress<'a> {
    pub name: &'a str,
    pub got_mb: u64,
    pub total_mb: u64,
}

/// Fetch `url` to `target` unless it is already there. `name` is what progress reports. Blocks
/// for as long as the download takes, so call it off the UI thread.
pub fn get(
    url: &str,
    target: &Path,
    name: &str,
    progress: &dyn Fn(Progress<'_>),
) -> Result<(), String> {
    if target.exists() {
        return Ok(());
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_body(Some(Duration::from_secs(120)))
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut res = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("{url}: answered {}", res.status()));
    }
    let total = res
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let mut part = target.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let mut out = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let mut body = res.body_mut().with_config().limit(u64::MAX).reader();
    let mut buf = vec![0u8; 1 << 16];
    let (mut got, mut shown) = (0u64, 0u64);
    loop {
        let n = body.read(&mut buf).map_err(|e| format!("{url}: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| format!("{}: {e}", part.display()))?;
        got += n as u64;
        if got - shown > 1 << 20 {
            shown = got;
            progress(Progress { name, got_mb: got >> 20, total_mb: total >> 20 });
        }
    }
    drop(out);
    if total != 0 && got != total {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{url}: stopped after {got} of {total} bytes"));
    }
    std::fs::rename(&part, target).map_err(|e| format!("{}: {e}", target.display()))
}
