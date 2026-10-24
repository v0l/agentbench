//! Fetching one file off the network, with progress, written beside its target and renamed so a
//! download that stops half way is not taken for the file.

use std::path::Path;

/// Where a download has got to, for a status line.
pub struct Progress<'a> {
    pub name: &'a str,
    pub got_mb: u64,
    pub total_mb: u64,
}

/// Fetch `url` to `target` unless it is already there. `name` is what progress reports.
pub async fn get(
    http: &reqwest::Client,
    url: &str,
    target: &Path,
    name: &str,
    progress: &impl Fn(Progress<'_>),
) -> Result<(), String> {
    if target.exists() {
        return Ok(());
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let mut resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("{url}: {e}"))?;
    let total = resp.content_length().unwrap_or(0);
    let mut part = target.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let mut out = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let (mut got, mut shown) = (0u64, 0u64);
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{url}: {e}"))? {
        std::io::Write::write_all(&mut out, &chunk)
            .map_err(|e| format!("{}: {e}", part.display()))?;
        got += chunk.len() as u64;
        if got - shown > 1 << 20 || got == total {
            shown = got;
            progress(Progress { name, got_mb: got >> 20, total_mb: total >> 20 });
        }
    }
    drop(out);
    std::fs::rename(&part, target).map_err(|e| format!("{}: {e}", target.display()))
}
