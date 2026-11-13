use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const FLAG: &str = "__sandbox";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub workspace: PathBuf,
    pub scratch: PathBuf,
    pub tool_dirs: Vec<PathBuf>,
    pub command: String,
    #[serde(default)]
    pub shared: Vec<Shared>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    pub path: PathBuf,
    pub write: bool,
}

#[cfg(all(feature = "sandbox", any(target_os = "linux", target_os = "macos")))]
mod enforce {
    use super::Plan;
    use nono::{AccessMode, CapabilitySet, Sandbox};
    use std::path::Path;

    #[cfg(target_os = "linux")]
    const SYSTEM: &[&str] =
        &["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/opt", "/nix", "/proc"];

    #[cfg(target_os = "macos")]
    const SYSTEM: &[&str] = &[
        "/usr",
        "/bin",
        "/sbin",
        "/opt",
        "/System",
        "/Library",
        "/Applications",
        "/private/etc",
        "/private/var/db",
        "/etc",
    ];

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const SYSTEM: &[&str] = &[];

    const DEVICES: &str = "/dev";

    pub fn supported() -> bool {
        Sandbox::is_supported()
    }

    pub fn why_not() -> String {
        Sandbox::support_info().details
    }

    pub fn capabilities(plan: &Plan) -> nono::Result<CapabilitySet> {
        let mut caps = CapabilitySet::new();
        for dir in SYSTEM {
            if Path::new(dir).is_dir() {
                caps = caps.allow_path(dir, AccessMode::Read)?;
            }
        }
        if Path::new(DEVICES).is_dir() {
            caps = caps.allow_path(DEVICES, AccessMode::ReadWrite)?;
        }
        for dir in &plan.tool_dirs {
            if dir.is_dir() {
                caps = caps.allow_path(dir, AccessMode::Read)?;
            }
        }
        caps = caps.allow_path(&plan.scratch, AccessMode::ReadWrite)?;
        caps = caps.allow_path(&plan.workspace, AccessMode::ReadWrite)?;
        for share in &plan.shared {
            let mode = if share.write { AccessMode::ReadWrite } else { AccessMode::Read };
            caps = match share.path.is_dir() {
                true => caps.allow_path(&share.path, mode)?,
                false if share.path.exists() => caps.allow_file(&share.path, mode)?,
                false => caps,
            };
        }
        platform_rules(caps)
    }

    #[cfg(target_os = "macos")]
    fn platform_rules(caps: CapabilitySet) -> nono::Result<CapabilitySet> {
        const RULES: [&str; 5] = [
            "(allow file-read-metadata (literal \"/var\"))",
            "(allow file-read-metadata (literal \"/private/var\"))",
            "(allow file-read* (subpath \"/private/var/select\"))",
            "(allow file-read-metadata (regex #\"^/private/var/folders(/[^/]+(/[^/]+(/T)?)?)?$\"))",
            "(allow file-read* file-write* (regex #\"^/private/var/folders/[^/]+/[^/]+/T/xcrun_db\"))",
        ];
        RULES.iter().try_fold(caps, |caps, rule| caps.platform_rule(*rule))
    }

    #[cfg(not(target_os = "macos"))]
    fn platform_rules(caps: CapabilitySet) -> nono::Result<CapabilitySet> {
        Ok(caps)
    }

    pub fn apply(plan: &Plan) -> Result<(), String> {
        let caps = capabilities(plan).map_err(|e| e.to_string())?;
        Sandbox::apply_auto(&caps).map(|_| ()).map_err(|e| e.to_string())
    }
}

#[cfg(all(feature = "sandbox", any(target_os = "linux", target_os = "macos")))]
pub use enforce::{capabilities, supported, why_not};

#[cfg(not(all(feature = "sandbox", any(target_os = "linux", target_os = "macos"))))]
pub fn supported() -> bool {
    false
}

#[cfg(not(all(feature = "sandbox", any(target_os = "linux", target_os = "macos"))))]
pub fn why_not() -> String {
    "this build has no sandbox".to_string()
}

pub fn path_of(plan: &Plan) -> String {
    let mut parts: Vec<String> = plan.tool_dirs.iter().map(|d| d.display().to_string()).collect();
    parts.extend(["/usr/local/bin", "/usr/bin", "/bin"].map(String::from));
    parts.join(if cfg!(windows) { ";" } else { ":" })
}

pub fn shell() -> PathBuf {
    for candidate in ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash", "/bin/sh"] {
        if Path::new(candidate).is_file() {
            return PathBuf::from(candidate);
        }
    }
    PathBuf::from("/bin/sh")
}

pub fn helper_args(plan: &Plan) -> Result<Vec<String>, String> {
    let json = serde_json::to_string(plan).map_err(|e| e.to_string())?;
    Ok(vec![FLAG.to_string(), json])
}

#[cfg(all(feature = "sandbox", any(target_os = "linux", target_os = "macos")))]
pub fn run_helper() -> Option<i32> {
    use std::os::unix::process::CommandExt;
    let mut args = std::env::args();
    args.next();
    if args.next().as_deref() != Some(FLAG) {
        return None;
    }
    let fail = |msg: String| {
        eprintln!("sandbox: {msg}");
        Some(126)
    };
    let Some(json) = args.next() else { return fail("no plan".into()) };
    let plan: Plan = match serde_json::from_str(&json) {
        Ok(p) => p,
        Err(e) => return fail(format!("bad plan: {e}")),
    };
    if let Err(e) = enforce::apply(&plan) {
        return fail(e);
    }
    let error = std::process::Command::new(shell())
        .arg("-c")
        .arg(&plan.command)
        .current_dir(&plan.workspace)
        .env_clear()
        .env("PATH", path_of(&plan))
        .env("HOME", &plan.workspace)
        .env("TMPDIR", &plan.scratch)
        .env("LANG", "C.UTF-8")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .envs(plan.env.iter().cloned())
        .exec();
    fail(format!("{}: {error}", shell().display()))
}

#[cfg(not(all(feature = "sandbox", any(target_os = "linux", target_os = "macos"))))]
pub fn run_helper() -> Option<i32> {
    None
}
