use serde_json::Value;
use std::io::{Read, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use web_time::Instant;

pub const NEED_MIB: u64 = 16 * 1024;
pub const LOAD_WAIT: Duration = Duration::from_secs(300);
pub const RESTART_AFTER: Duration = Duration::from_secs(2);
pub const STOP_GRACE: Duration = Duration::from_secs(5);
pub const PROGRAM: &str = "inference-bench-server";
pub const INSTALL: &str = "cargo install --locked --git https://github.com/v0l/inference-bench inference-bench-server";
pub const DUPLEX_MAX_CTX: usize = 4096;
pub const KEY_ENV: &str = "INFERENCE_BENCH_API_KEYS";
const LOG_TAIL: usize = 6;
const CACHE_FOR: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
pub struct Gpu {
    pub index: u32,
    pub name: String,
    pub total_mib: u64,
    pub free_mib: u64,
}

pub fn gb(mib: u64) -> String {
    format!("{:.1} GB", mib as f64 / 1024.0)
}

pub fn parse_smi(text: &str) -> Vec<Gpu> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            let [index, name, total, free] = f.as_slice() else { return None };
            Some(Gpu {
                index: index.parse().ok()?,
                name: name.to_string(),
                total_mib: total.parse().ok()?,
                free_mib: free.parse().ok()?,
            })
        })
        .collect()
}

pub fn smi_gpus() -> Result<Vec<Gpu>, String> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=index,name,memory.total,memory.free", "--format=csv,noheader,nounits"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| "no NVIDIA GPU found here (nvidia-smi is missing); the voice model needs one".to_string())?;
    Ok(parse_smi(&String::from_utf8_lossy(&out.stdout)))
}

fn refusal(g: &Gpu, need: u64) -> String {
    format!(
        "GPU {} ({}) has {} free of {}; the voice model needs about {}. Free some memory or pick another card.",
        g.index,
        g.name,
        gb(g.free_mib),
        gb(g.total_mib),
        gb(need)
    )
}

pub fn choose_gpu(gpus: &[Gpu], picked: Option<u32>, need: u64) -> Result<(Gpu, String), String> {
    if gpus.is_empty() {
        return Err(format!("no NVIDIA GPU found here; the voice model needs one with about {} free", gb(need)));
    }
    if let Some(i) = picked {
        let g = gpus.iter().find(|g| g.index == i).ok_or_else(|| format!("GPU {i} is not on this machine"))?;
        if g.free_mib < need {
            return Err(refusal(g, need));
        }
        return Ok((g.clone(), "picked in settings".into()));
    }
    let best = gpus.iter().max_by_key(|g| g.free_mib).expect("one GPU at least");
    if best.free_mib < need {
        return Err(format!(
            "no GPU has {} free for the voice model; the most is {} on GPU {} ({})",
            gb(need),
            gb(best.free_mib),
            best.index,
            best.name
        ));
    }
    Ok((best.clone(), format!("it has the most free memory, {}", gb(best.free_mib))))
}

pub fn folder_problem(dir: &Path, name: &str) -> Option<String> {
    if !dir.is_dir() {
        return Some(format!("no folder at {}; download {name} there or pick its folder", dir.display()));
    }
    let missing: Vec<&str> =
        ["config.json", "model.safetensors"].into_iter().filter(|f| !dir.join(f).is_file()).collect();
    (!missing.is_empty()).then(|| format!("{} has no {}", dir.display(), missing.join(" and no ")))
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelSpec {
    pub id: String,
    pub path: PathBuf,
    pub kind: String,
    pub preload: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Host {
    #[default]
    Never,
    Fallback,
    Always,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub models: Vec<ModelSpec>,
    pub gpu: Option<u32>,
    pub program: String,
    pub need_mib: u64,
    pub host: Host,
}

pub fn route_of(kind: &str) -> Option<&'static str> {
    match kind {
        "duplex" => Some("/v1/duplex"),
        "transcription" => Some("/v1/audio/transcriptions"),
        "speech" => Some("/v1/audio/speech"),
        _ => None,
    }
}

fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub fn config_toml(port: u16, models: &[ModelSpec]) -> String {
    let mut t = format!(
        "bind = \"127.0.0.1:{port}\"\napi_keys_env = \"{KEY_ENV}\"\nlazy_load = false\nmax_loaded_models = {}\n\n[duplex]\nsessions_per_gpu = 1\nmax_ctx = {DUPLEX_MAX_CTX}\n",
        models.len().max(1)
    );
    for m in models {
        t.push_str(&format!(
            "\n[[models]]\nid = {}\nmodel = {}\npreload = {}\n",
            quoted(&m.id),
            quoted(&m.path.display().to_string()),
            m.preload
        ));
    }
    t
}

pub fn find_program(setting: &str, path: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Result<PathBuf, String> {
    let setting = setting.trim();
    if !setting.is_empty() {
        let p = PathBuf::from(setting);
        return if p.is_file() {
            Ok(p)
        } else {
            Err(format!("no voice engine at {setting}; install it with: {INSTALL}"))
        };
    }
    let mut dirs: Vec<PathBuf> = path.map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    if let Some(h) = home {
        dirs.push(h.join(".cargo/bin"));
    }
    dirs.into_iter()
        .map(|d| d.join(PROGRAM))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("the voice engine is not installed; install it with: {INSTALL}"))
}

pub fn program_version(p: &Path) -> Result<String, String> {
    let bad = |why: String| format!("{} {why}; reinstall it with: {INSTALL}", p.display());
    let mut child = Command::new(p)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| bad(format!("does not run ({e})")))?;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(bad("did not answer --version".into()));
            }
        }
    }
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    let out = out.trim().to_string();
    if out.contains("inference-bench") {
        Ok(out)
    } else {
        Err(bad(format!("answered --version with {out:?}")))
    }
}

pub fn lookup(setting: &str) -> Result<(PathBuf, String), String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let p = find_program(setting, std::env::var_os("PATH").as_deref(), home.as_deref())?;
    let v = program_version(&p)?;
    Ok((p, v))
}

fn random_key() -> String {
    let mut bytes = [0u8; 24];
    let got = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_ok();
    if !got {
        use std::hash::{BuildHasher, Hasher};
        for chunk in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(Instant::now().elapsed().as_nanos());
            chunk.copy_from_slice(&h.finish().to_le_bytes()[..chunk.len()]);
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn free_port() -> Result<u16, String> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free port for the voice engine: {e}"))?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

pub fn get(port: u16, path: &str, key: &str) -> Result<(u16, Value), String> {
    let mut s = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {key}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    let code = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or("no answer")?;
    let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let json = match (body.find(['{', '[']), body.rfind(['}', ']'])) {
        (Some(a), Some(b)) if b >= a => serde_json::from_str(&body[a..=b]).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    Ok((code, json))
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

pub fn log_tail(path: &Path, from: u64) -> String {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let _ = f.seek(std::io::SeekFrom::Start(from));
    let mut text = String::new();
    let _ = f.read_to_string(&mut text);
    let lines: Vec<String> =
        text.lines().map(|l| strip_ansi(l).trim().to_string()).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(LOG_TAIL)..].join(" | ")
}

#[derive(Clone, Debug, PartialEq)]
enum Load {
    Waiting,
    Ready(Duration),
    Failed(String),
}

fn missing_route<'a>(caps: &Value, models: &'a [ModelSpec]) -> Option<&'a str> {
    let routes = caps["routes"].as_array().cloned().unwrap_or_default();
    models
        .iter()
        .find(|m| route_of(&m.kind).is_some_and(|path| !routes.iter().any(|r| r["path"] == path)))
        .map(|m| m.kind.as_str())
}

fn models_ready(listed: &Value, models: &[ModelSpec]) -> bool {
    let data = listed["data"].as_array().cloned().unwrap_or_default();
    models.iter().filter(|m| m.preload).all(|m| {
        data.iter().any(|d| d["id"] == m.id.as_str() && d["type"] == m.kind.as_str() && d["loaded"] != false)
    })
}

fn watch(port: u16, key: String, models: Vec<ModelSpec>, load: Arc<Mutex<Load>>, stop: Arc<AtomicBool>) {
    let start = Instant::now();
    let set = |l: Load| *load.lock().unwrap_or_else(|e| e.into_inner()) = l;
    let (mut healthy, mut capable) = (false, false);
    while !stop.load(Ordering::Relaxed) && start.elapsed() < LOAD_WAIT {
        if !healthy {
            healthy = matches!(get(port, "/health", &key), Ok((200, _)));
        }
        if healthy && !capable {
            match get(port, "/v1/capabilities", &key) {
                Ok((200, caps)) => match missing_route(&caps, &models) {
                    None => capable = true,
                    Some(kind) => {
                        return set(Load::Failed(format!(
                            "this inference-bench-server cannot serve {kind} models; update it with: {INSTALL}"
                        )));
                    }
                },
                Ok((401 | 403, _)) => return set(Load::Failed("the voice engine refused the key agentbench gave it".into())),
                _ => {}
            }
        }
        if capable && let Ok((200, listed)) = get(port, "/v1/models", &key) && models_ready(&listed, &models) {
            return set(Load::Ready(start.elapsed()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    if !stop.load(Ordering::Relaxed) {
        set(Load::Failed(format!("the voice model did not load in {} s", LOAD_WAIT.as_secs())));
    }
}

struct Running {
    child: Child,
    port: u16,
    key: String,
    gpu: Option<Gpu>,
    started: Instant,
    load: Arc<Mutex<Load>>,
    stop: Arc<AtomicBool>,
    log_from: u64,
}

impl Running {
    fn load(&self) -> Load {
        self.load.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn halt(mut child: Child, grace: Duration) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let until = Instant::now() + grace;
    while Instant::now() < until {
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    #[cfg(unix)]
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Clone, Debug, PartialEq)]
pub enum Want {
    Off,
    Idle(Plan),
    Use(Plan),
}

#[derive(Default)]
pub struct Engine {
    running: Option<Running>,
    plan: Option<Plan>,
    restarts: u32,
    restart_at: Option<Instant>,
    failed: Option<String>,
    last_used: Option<Instant>,
    using: bool,
    dir: Option<PathBuf>,
    fake_gpus: Option<Vec<Gpu>>,
    gpu_cache: Option<(Instant, Result<Vec<Gpu>, String>)>,
    program_cache: Option<(String, Instant, Result<(PathBuf, String), String>)>,
    pub starts: usize,
    pub load_time: Option<Duration>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop.store(true, Ordering::Relaxed);
            halt(r.child, STOP_GRACE);
        }
    }
}

impl Engine {
    pub fn in_dir(dir: PathBuf) -> Engine {
        let mut e = Engine::default();
        e.dir = Some(dir);
        e
    }

    #[doc(hidden)]
    pub fn set_test(&mut self, dir: PathBuf, gpus: Vec<Gpu>) {
        self.dir = Some(dir);
        self.fake_gpus = Some(gpus);
        self.gpu_cache = None;
    }

    pub fn dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(|| std::env::temp_dir().join("agentbench-voice-engine"))
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir().join("engine.log")
    }

    pub fn gpus(&mut self) -> Result<Vec<Gpu>, String> {
        if let Some(g) = &self.fake_gpus {
            return Ok(g.clone());
        }
        if let Some((at, g)) = &self.gpu_cache
            && at.elapsed() < CACHE_FOR
        {
            return g.clone();
        }
        let g = smi_gpus();
        self.gpu_cache = Some((Instant::now(), g.clone()));
        g
    }

    pub fn program(&mut self, setting: &str) -> Result<(PathBuf, String), String> {
        if let Some((s, at, r)) = &self.program_cache
            && s == setting
            && at.elapsed() < CACHE_FOR
        {
            return r.clone();
        }
        let r = lookup(setting);
        self.program_cache = Some((setting.to_string(), Instant::now(), r.clone()));
        r
    }

    pub fn pid(&self) -> Option<u32> {
        self.running.as_ref().map(|r| r.child.id())
    }

    pub fn failed(&self) -> Option<&str> {
        self.failed.as_deref()
    }

    pub fn gpu(&self) -> Option<&Gpu> {
        self.running.as_ref().and_then(|r| r.gpu.as_ref())
    }

    pub fn on_host(&self) -> bool {
        self.running.as_ref().is_some_and(|r| r.gpu.is_none())
    }

    pub fn endpoint(&self) -> Option<(String, String)> {
        let r = self.running.as_ref()?;
        matches!(r.load(), Load::Ready(_)).then(|| (format!("http://127.0.0.1:{}", r.port), r.key.clone()))
    }

    pub fn loading(&self) -> bool {
        self.running.as_ref().is_some_and(|r| r.load() == Load::Waiting) || self.restart_at.is_some()
    }

    pub fn progress(&self, now: Instant) -> Option<String> {
        if self.restart_at.is_some() {
            return Some("restarting the voice model".into());
        }
        let r = self.running.as_ref()?;
        (r.load() == Load::Waiting)
            .then(|| format!("loading the voice model, {} s", now.saturating_duration_since(r.started).as_secs()))
    }

    pub fn stop(&mut self) {
        self.restart_at = None;
        if let Some(r) = self.running.take() {
            r.stop.store(true, Ordering::Relaxed);
            let child = r.child;
            let _ = std::thread::Builder::new()
                .name("agentbench-engine-stop".into())
                .spawn(move || halt(child, STOP_GRACE));
        }
    }

    pub fn stop_now(&mut self) {
        self.restart_at = None;
        if let Some(r) = self.running.take() {
            r.stop.store(true, Ordering::Relaxed);
            halt(r.child, STOP_GRACE);
        }
    }

    pub fn sync(&mut self, want: Want, idle: Duration, now: Instant) -> Option<String> {
        let using = matches!(want, Want::Use(_));
        let resumed = using && !std::mem::replace(&mut self.using, using);
        let plan = match &want {
            Want::Off => {
                self.stop();
                self.plan = None;
                self.failed = None;
                self.restarts = 0;
                return None;
            }
            Want::Idle(p) | Want::Use(p) => p,
        };
        if self.plan.as_ref() != Some(plan) || (resumed && self.failed.is_some()) {
            self.stop();
            self.plan = Some(plan.clone());
            self.failed = None;
            self.restarts = 0;
        }
        if let Some(n) = self.check() {
            return Some(n);
        }
        if let Want::Idle(_) = want {
            let unused = self.last_used.map_or(Duration::MAX, |t| now.saturating_duration_since(t));
            if (self.running.is_some() || self.restart_at.is_some()) && !idle.is_zero() && unused >= idle {
                self.stop();
            }
            return None;
        }
        self.last_used = Some(now);
        if self.failed.is_some() || self.running.is_some() {
            return None;
        }
        if self.restart_at.is_some_and(|at| now < at) {
            return None;
        }
        self.restart_at = None;
        match self.start(&plan.clone()) {
            Ok(()) => None,
            Err(e) => {
                self.failed = Some(e.clone());
                Some(e)
            }
        }
    }

    fn check(&mut self) -> Option<String> {
        let r = self.running.as_mut()?;
        if let Ok(Some(status)) = r.child.try_wait() {
            let r = self.running.take().expect("running");
            r.stop.store(true, Ordering::Relaxed);
            let tail = log_tail(&self.log_path(), r.log_from);
            let tail = if tail.is_empty() { "it wrote nothing".to_string() } else { format!("its log ends: {tail}") };
            if self.restarts == 0 {
                self.restarts = 1;
                self.restart_at = Some(Instant::now() + RESTART_AFTER);
                return Some(format!(
                    "the voice model stopped ({status}); starting it again in {} s",
                    RESTART_AFTER.as_secs()
                ));
            }
            let why = format!("the voice model stopped again ({status}) and is left off; {tail}");
            self.failed = Some(why.clone());
            return Some(why);
        }
        match r.load() {
            Load::Failed(why) => {
                let from = r.log_from;
                self.stop();
                let tail = log_tail(&self.log_path(), from);
                let why = if tail.is_empty() { why } else { format!("{why}; its log ends: {tail}") };
                self.failed = Some(why.clone());
                Some(why)
            }
            Load::Ready(t) => {
                self.load_time.get_or_insert(t);
                None
            }
            Load::Waiting => None,
        }
    }

    fn start(&mut self, plan: &Plan) -> Result<(), String> {
        for m in &plan.models {
            if let Some(p) = folder_problem(&m.path, &m.id) {
                return Err(p);
            }
        }
        let (program, _) = self.program(&plan.program)?;
        let gpu = match plan.host {
            Host::Always => None,
            Host::Never => Some(choose_gpu(&self.gpus()?, plan.gpu, plan.need_mib)?.0),
            Host::Fallback => match self.gpus().and_then(|g| choose_gpu(&g, plan.gpu, plan.need_mib)) {
                Ok((g, _)) => Some(g),
                Err(_) if plan.gpu.is_none() => None,
                Err(e) => return Err(e),
            },
        };
        let port = free_port()?;
        let key = random_key();
        let dir = self.dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let config = dir.join("engine.toml");
        std::fs::write(&config, config_toml(port, &plan.models)).map_err(|e| format!("{}: {e}", config.display()))?;
        let log_path = self.log_path();
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("{}: {e}", log_path.display()))?;
        match &gpu {
            Some(g) => writeln!(log, "agentbench starts {} on GPU {} ({})", program.display(), g.index, g.name),
            None => writeln!(log, "agentbench starts {} on the CPU", program.display()),
        }
        .map_err(|e| format!("{}: {e}", log_path.display()))?;
        let log_from = log.metadata().map(|m| m.len()).unwrap_or(0);
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(&program);
        cmd.arg("--config")
            .arg(&config)
            .env("CUDA_VISIBLE_DEVICES", gpu.as_ref().map(|g| g.index.to_string()).unwrap_or_default())
            .env("CUDA_DEVICE_ORDER", "PCI_BUS_ID")
            .env(KEY_ENV, &key)
            .env("NO_COLOR", "1")
            .env_remove("INFERENCE_BENCH_SERVER_CONFIG")
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            let parent = libc::getpid();
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", program.display()))?;
        let load = Arc::new(Mutex::new(Load::Waiting));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (load, stop, key, models) = (load.clone(), stop.clone(), key.clone(), plan.models.clone());
            std::thread::Builder::new()
                .name("agentbench-engine-watch".into())
                .spawn(move || watch(port, key, models, load, stop))
                .map_err(|e| e.to_string())?;
        }
        self.starts += 1;
        self.running = Some(Running { child, port, key, gpu, started: Instant::now(), load, stop, log_from });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(index: u32, free_gb: u64) -> Gpu {
        Gpu { index, name: format!("Card {index}"), total_mib: 96 * 1024, free_mib: free_gb * 1024 }
    }

    #[test]
    fn nvidia_smi_lines_become_gpus() {
        let g = parse_smi("0, NVIDIA RTX PRO 6000, 97887, 2234\n1, NVIDIA RTX PRO 6000, 97887, 97245\nbad line\n");
        assert_eq!(g.len(), 2);
        assert_eq!(g[1], Gpu { index: 1, name: "NVIDIA RTX PRO 6000".into(), total_mib: 97887, free_mib: 97245 });
    }

    #[test]
    fn the_card_with_the_most_free_memory_is_picked_and_says_why() {
        let (g, why) = choose_gpu(&[card(0, 2), card(1, 90)], None, NEED_MIB).unwrap();
        assert_eq!(g.index, 1);
        assert!(why.contains("most free") && why.contains("90.0 GB"), "{why}");
    }

    #[test]
    fn a_busy_card_is_refused_with_the_numbers() {
        let e = choose_gpu(&[card(0, 2), card(1, 90)], Some(0), NEED_MIB).unwrap_err();
        assert!(e.contains("GPU 0") && e.contains("2.0 GB free of 96.0 GB") && e.contains("16.0 GB"), "{e}");
        let e = choose_gpu(&[card(0, 2), card(1, 3)], None, NEED_MIB).unwrap_err();
        assert!(e.contains("3.0 GB on GPU 1"), "never a silent pick of a full card: {e}");
        assert!(choose_gpu(&[], None, NEED_MIB).unwrap_err().contains("no NVIDIA GPU"));
        assert!(choose_gpu(&[card(0, 90)], Some(3), NEED_MIB).unwrap_err().contains("GPU 3"));
    }

    #[test]
    fn the_config_loads_a_list_of_models_on_loopback_with_the_key_in_the_environment() {
        let models = vec![
            ModelSpec { id: "Haru-4B".into(), path: "/m/Haru \"4B\"".into(), kind: "duplex".into(), preload: true },
            ModelSpec { id: "other".into(), path: "/m/other".into(), kind: "speech".into(), preload: false },
        ];
        let t = config_toml(4100, &models);
        assert!(t.contains("bind = \"127.0.0.1:4100\""), "{t}");
        assert!(t.contains(&format!("api_keys_env = \"{KEY_ENV}\"")));
        assert!(t.contains("sessions_per_gpu = 1") && t.contains("max_ctx = 4096"));
        assert!(t.contains("max_loaded_models = 2"));
        assert_eq!(t.matches("[[models]]").count(), 2);
        assert!(t.contains("model = \"/m/Haru \\\"4B\\\"\""), "{t}");
        assert!(t.contains("model = \"/m/other\"\npreload = false"), "{t}");
        assert!(!t.contains("api_keys ="), "no key is written to disk");
    }

    #[test]
    fn the_engine_is_found_from_the_setting_then_path_then_cargo_bin() {
        let dir = tempfile::tempdir().unwrap();
        let (a, home) = (dir.path().join("a"), dir.path().join("home"));
        std::fs::create_dir_all(home.join(".cargo/bin")).unwrap();
        std::fs::create_dir_all(&a).unwrap();
        let missing = find_program("", Some(a.as_os_str()), Some(&home)).unwrap_err();
        assert!(missing.contains(INSTALL), "{missing}");
        std::fs::write(home.join(".cargo/bin").join(PROGRAM), "").unwrap();
        assert_eq!(find_program("", Some(a.as_os_str()), Some(&home)).unwrap(), home.join(".cargo/bin").join(PROGRAM));
        std::fs::write(a.join(PROGRAM), "").unwrap();
        assert_eq!(find_program("", Some(a.as_os_str()), Some(&home)).unwrap(), a.join(PROGRAM));
        let set = dir.path().join("mine");
        std::fs::write(&set, "").unwrap();
        assert_eq!(find_program(set.to_str().unwrap(), Some(a.as_os_str()), Some(&home)).unwrap(), set);
        assert!(find_program("/no/such/engine", None, None).unwrap_err().contains("/no/such/engine"));
    }

    #[cfg(unix)]
    #[test]
    fn an_engine_that_does_not_answer_its_version_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(PROGRAM);
        std::fs::write(&p, "#!/bin/sh\necho something else\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(program_version(&p).unwrap_err().contains(INSTALL));
        std::fs::write(&p, "#!/bin/sh\necho inference-bench-server 0.9.0\n").unwrap();
        assert_eq!(program_version(&p).unwrap(), "inference-bench-server 0.9.0");
    }

    #[test]
    fn the_log_tail_is_the_last_lines_of_this_run_without_colour() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("engine.log");
        std::fs::write(&p, "old run\n").unwrap();
        let from = std::fs::metadata(&p).unwrap().len();
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        for i in 0..10 {
            writeln!(f, "\u{1b}[31mline {i}\u{1b}[0m").unwrap();
        }
        let t = log_tail(&p, from);
        assert_eq!(t, "line 4 | line 5 | line 6 | line 7 | line 8 | line 9");
    }

    #[test]
    fn the_models_are_ready_when_each_is_listed_loaded_with_its_type() {
        let m = vec![ModelSpec { id: "Haru-4B".into(), path: "/m".into(), kind: "duplex".into(), preload: true }];
        let listed = |ty: &str, loaded: bool| serde_json::json!({"data": [{"id": "Haru-4B", "type": ty, "loaded": loaded}]});
        assert!(models_ready(&listed("duplex", true), &m));
        assert!(!models_ready(&listed("duplex", false), &m));
        assert!(!models_ready(&listed("chat", true), &m));
        assert_eq!(missing_route(&serde_json::json!({"routes": [{"path": "/v1/duplex"}]}), &m), None);
        assert_eq!(missing_route(&serde_json::json!({"routes": [{"path": "/v1/chat/completions"}]}), &m), Some("duplex"));
    }

    #[test]
    fn speech_models_need_their_routes_and_only_the_preloaded_ones_must_be_loaded() {
        let m = vec![
            ModelSpec { id: "whisper-base.en".into(), path: "/w".into(), kind: "transcription".into(), preload: true },
            ModelSpec { id: "whisper-small.en".into(), path: "/s".into(), kind: "transcription".into(), preload: false },
            ModelSpec { id: "phonon".into(), path: "/p".into(), kind: "speech".into(), preload: true },
        ];
        let caps = serde_json::json!({"routes": [{"path": "/v1/audio/transcriptions"}]});
        assert_eq!(missing_route(&caps, &m), Some("speech"));
        let caps = serde_json::json!({"routes": [{"path": "/v1/audio/transcriptions"}, {"path": "/v1/audio/speech"}]});
        assert_eq!(missing_route(&caps, &m), None);
        let listed = serde_json::json!({"data": [
            {"id": "whisper-base.en", "type": "transcription", "loaded": true},
            {"id": "whisper-small.en", "type": "transcription", "loaded": false},
            {"id": "phonon", "type": "speech", "loaded": true},
        ]});
        assert!(models_ready(&listed, &m));
    }

    #[test]
    fn a_model_folder_says_plainly_what_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let p = folder_problem(dir.path(), "Haru-4B").unwrap();
        assert!(p.contains("config.json") && p.contains("model.safetensors"), "{p}");
        std::fs::write(dir.path().join("config.json"), "{}").unwrap();
        std::fs::write(dir.path().join("model.safetensors"), "").unwrap();
        assert_eq!(folder_problem(dir.path(), "Haru-4B"), None);
        let gone = folder_problem(&dir.path().join("nope"), "Haru-4B").unwrap();
        assert!(gone.contains("no folder") && gone.contains("Haru-4B"), "{gone}");
    }
}
