//! A picture of a web page, taken with a headless browser the machine already has.
//!
//! The picture goes back to the model the way `read_file` shows an image, so an agent can look at
//! the UI it just built. No browser is bundled: Chrome, Chromium or Edge is found on the path.

use super::media::{self, Media};
use egui_bench::agent::grants::Access;
use egui_bench::agent::workspace::Workspace;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 800;
const SIZE_MAX: u32 = 4000;
/// How long the page may run its scripts before the picture is taken.
const SETTLE: u64 = 2000;
const SETTLE_MAX: u64 = 20_000;

const BROWSERS: [&str; 7] = [
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "chrome",
    "microsoft-edge",
    "microsoft-edge-stable",
];

#[cfg(target_os = "macos")]
const APPS: [&str; 3] = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
];
#[cfg(not(target_os = "macos"))]
const APPS: [&str; 0] = [];

fn browser() -> Option<PathBuf> {
    BROWSERS
        .iter()
        .map(std::path::Path::new)
        .find_map(super::resolve)
        .or_else(|| APPS.iter().map(PathBuf::from).find(|p| p.is_file()))
}

/// The address to open: web addresses as they are, a file by its absolute path.
fn address(ws: &Workspace, target: &str) -> Result<String, String> {
    let target = target.trim();
    if let Some((scheme, _)) = target.split_once("://") {
        return match scheme.to_ascii_lowercase().as_str() {
            "http" | "https" | "file" => Ok(target.to_string()),
            other => Err(format!(
                "{other}:// addresses are not supported, use http, https or a file path"
            )),
        };
    }
    if target.starts_with("localhost") || target.starts_with("127.0.0.1") {
        return Ok(format!("http://{target}"));
    }
    let file = ws.resolve_for(target, Access::Read)?;
    if !file.is_file() {
        return Err(format!("{} is not a file", ws.shown(&file)));
    }
    let abs = file.canonicalize().map_err(|e| format!("{target}: {e}"))?;
    Ok(format!("file://{}", abs.display()))
}

fn size(args: &Value, key: &str, default: u32) -> u32 {
    args.get(key).and_then(Value::as_u64).map_or(default, |n| (n as u32).clamp(100, SIZE_MAX))
}

/// Take the picture. Answers the image as media for the model, and keeps a copy at `save_to`
/// when asked.
pub async fn take(
    ws: &Workspace,
    args: &Value,
    stop: &tokio_util::sync::CancellationToken,
) -> Result<Media, String> {
    let target = args
        .get("url")
        .or_else(|| args.get("path"))
        .and_then(Value::as_str)
        .ok_or("missing `url`")?;
    let url = address(ws, target)?;
    let browser = browser()
        .ok_or("screenshot needs Chrome, Chromium or Edge, and none was found on the path")?;
    let (w, h) = (size(args, "width", WIDTH), size(args, "height", HEIGHT));
    let settle = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(SETTLE).min(SETTLE_MAX);
    let dir = std::env::temp_dir().join(format!("agentbench-shot-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let result = shoot(&browser, &url, &dir, w, h, settle, stop).await;
    let media = result.and_then(|png| {
        let shown = format!("screenshot of {target}");
        if let Some(save) = args.get("save_to").and_then(Value::as_str) {
            let dest = ws.resolve_for(save, Access::Write)?;
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::copy(&png, &dest).map_err(|e| format!("{save}: {e}"))?;
        }
        let mut m = media::attachment(&png, "screenshot.png")?;
        m.note =
            format!("{shown} at {w}x{h}: {}", m.note.split_once(": ").map_or(&*m.note, |x| x.1));
        Ok(m)
    });
    let _ = std::fs::remove_dir_all(&dir);
    media
}

async fn shoot(
    browser: &std::path::Path,
    url: &str,
    dir: &std::path::Path,
    w: u32,
    h: u32,
    settle: u64,
    stop: &tokio_util::sync::CancellationToken,
) -> Result<PathBuf, String> {
    let png = dir.join("shot.png");
    let mut cmd = Command::new(browser);
    cmd.arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--hide-scrollbars")
        .arg("--disable-extensions")
        .arg("--no-sandbox")
        .arg(format!("--user-data-dir={}", dir.join("profile").display()))
        .arg(format!("--window-size={w},{h}"))
        .arg(format!("--virtual-time-budget={settle}"))
        .arg(format!("--screenshot={}", png.display()))
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("{}: {e}", browser.display()))?;
    let waited = tokio::select! {
        r = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output()) => r,
        _ = stop.cancelled() => return Err("stopped".into()),
    };
    let out = waited
        .map_err(|_| "the browser took over 60s to load the page".to_string())?
        .map_err(|e| e.to_string())?;
    if png.is_file() {
        return Ok(png);
    }
    let said = String::from_utf8_lossy(&out.stderr);
    let said = said.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
    Err(format!("the browser took no picture of {url}: {}", said.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_web_pages_or_files() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("a.html"), "x").unwrap();
        assert_eq!(address(&ws, "https://example.com/x").unwrap(), "https://example.com/x");
        assert_eq!(address(&ws, "localhost:3000").unwrap(), "http://localhost:3000");
        assert!(address(&ws, "ftp://example.com").is_err());
        let file = address(&ws, "a.html").unwrap();
        assert!(file.starts_with("file://") && file.ends_with("/a.html"), "{file}");
        assert!(address(&ws, "missing.html").is_err());
    }

    #[tokio::test]
    async fn a_page_is_photographed_when_a_browser_is_installed() {
        if browser().is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.html"),
            "<body style='background:#246'><h1>hello</h1></body>",
        )
        .unwrap();
        let ws = Workspace::open(dir.path()).unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let args = serde_json::json!({"url": "a.html", "width": 400, "height": 300});
        let m = take(&ws, &args, &stop).await.unwrap();
        assert!(m.note.contains("screenshot of a.html"), "{}", m.note);
        assert_eq!(m.parts.iter().filter(|p| p["type"] == "image_url").count(), 1);
    }
}
