use super::limits::Limits;
use super::spawn::{self, Confine, Scratch};
use std::collections::HashMap;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub type Owner = String;

const KEPT: usize = 256 * 1024;
const RUNNING_MAX: usize = 8;
pub const TAIL: usize = 32 * 1024;

pub enum Until {
    Exit,
    Pattern(regex::Regex),
}

impl Until {
    pub fn pattern(text: &str) -> Result<Until, String> {
        if text.trim().eq_ignore_ascii_case("exit") {
            return Ok(Until::Exit);
        }
        regex::Regex::new(text).map(Until::Pattern).map_err(|e| format!("bad `until` regex: {e}"))
    }
}

fn tail_of(text: &str) -> (String, Option<String>) {
    if text.len() <= TAIL {
        return (text.to_string(), None);
    }
    let mut from = text.len() - TAIL;
    while !text.is_char_boundary(from) {
        from += 1;
    }
    if let Some(nl) = text[from..].find('\n') {
        from += nl + 1;
    }
    let lines = text.lines().count();
    let shown = text[from..].lines().count();
    (
        text[from..].to_string(),
        Some(format!("[{lines} lines of new output, showing the last {shown}]\n")),
    )
}

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    dropped: usize,
    read: usize,
    exit: Option<Option<i32>>,
    killed: Option<String>,
    stopped: bool,
}

struct Job {
    command: String,
    started: Instant,
    pid: Option<u32>,
    out: Arc<Mutex<Output>>,
    woke: Arc<Notify>,
    _scratch: Option<Arc<Scratch>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    pub handle: String,
    pub command: String,
    pub running: bool,
    pub exit: Option<i32>,
    pub seconds: u64,
    pub dropped: usize,
    pub text: String,
}

#[derive(Clone, Default)]
pub struct Jobs(Arc<Mutex<HashMap<(Owner, String), Job>>>);

mod scope {
    use std::process::ExitStatus;
    use tokio::process::Command;

    #[cfg(target_os = "linux")]
    pub fn available() -> bool {
        static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OK.get_or_init(|| {
            std::process::Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "-p", "MemoryMax=64M", "--", "true"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn available() -> bool {
        false
    }

    pub fn wrap(cmd: Command, mb: u64) -> Command {
        let inner = cmd.as_std();
        let mut wrapped = Command::new("systemd-run");
        wrapped
            .args(["--user", "--scope", "--quiet", "-p"])
            .arg(format!("MemoryMax={mb}M"))
            .args(["-p", "MemorySwapMax=0", "-p", "CPUWeight=30", "-p", "IOWeight=30", "--"])
            .arg(inner.get_program())
            .args(inner.get_args());
        for (k, v) in inner.get_envs() {
            match v {
                Some(v) => wrapped.env(k, v),
                None => wrapped.env_remove(k),
            };
        }
        if let Some(dir) = inner.get_current_dir() {
            wrapped.current_dir(dir);
        }
        wrapped.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        #[cfg(unix)]
        wrapped.process_group(0);
        wrapped
    }

    #[cfg(unix)]
    pub fn killed_by_signal(status: &Option<ExitStatus>) -> Option<i32> {
        use std::os::unix::process::ExitStatusExt;
        status.as_ref().and_then(|s| s.signal()).filter(|s| *s == 9)
    }

    #[cfg(not(unix))]
    pub fn killed_by_signal(_status: &Option<ExitStatus>) -> Option<i32> {
        None
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn exit_code(s: Option<ExitStatus>) -> Option<i32> {
    s.map(|s| s.code().unwrap_or(-1))
}

impl Jobs {
    pub fn start(
        &self,
        session: &str,
        cwd: &Path,
        command: &str,
        confine: &Confine,
    ) -> Result<String, String> {
        self.start_limited(session, cwd, command, confine, Limits::default())
    }

    pub fn start_limited(
        &self,
        session: &str,
        cwd: &Path,
        command: &str,
        confine: &Confine,
        limits: Limits,
    ) -> Result<String, String> {
        self.start_with(session, cwd, command, confine, limits, scope::available())
    }

    fn start_with(
        &self,
        session: &str,
        cwd: &Path,
        command: &str,
        confine: &Confine,
        limits: Limits,
        use_scope: bool,
    ) -> Result<String, String> {
        if command.trim().is_empty() {
            return Err("`command` is empty".into());
        }
        let running = lock(&self.0)
            .iter()
            .filter(|((s, _), j)| s == session && lock(&j.out).exit.is_none())
            .count();
        if running >= RUNNING_MAX {
            return Err(format!(
                "{running} background commands are already running; bash_kill some"
            ));
        }
        let spawn::Prepared { command: mut cmd, scratch } = spawn::prepare(command, cwd, confine)?;
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let scoped = limits.memory_mb.is_some() && use_scope;
        let mut cmd = match limits.memory_mb {
            Some(mb) if scoped => scope::wrap(cmd, mb),
            _ => cmd,
        };
        let mut child = cmd.spawn().map_err(|e| format!("bash: {e}"))?;
        let out = Arc::new(Mutex::new(Output::default()));
        let woke = Arc::new(Notify::new());
        let mut readers = Vec::new();
        for pipe in [
            child
                .stdout
                .take()
                .map(|p| Box::new(p) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child.stderr.take().map(|p| Box::new(p) as _),
        ]
        .into_iter()
        .flatten()
        {
            let (out, woke) = (out.clone(), woke.clone());
            readers.push(tokio::spawn(async move {
                let mut pipe = pipe;
                let mut buf = [0u8; 8192];
                while let Ok(n) = pipe.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    let mut o = lock(&out);
                    o.bytes.extend_from_slice(&buf[..n]);
                    if o.bytes.len() > KEPT {
                        let over = o.bytes.len() - KEPT;
                        o.bytes.drain(..over);
                        o.dropped += over;
                    }
                    drop(o);
                    woke.notify_waiters();
                }
            }));
        }
        let pid = child.id();
        let handle = {
            let jobs = lock(&self.0);
            let n = jobs.keys().filter(|(s, _)| s == session).count() + 1;
            format!("job-{n}")
        };
        let (watch_out, watch_woke) = (out.clone(), woke.clone());
        let watchdog = if scoped { Limits::default() } else { limits };
        tokio::spawn(async move {
            let mut over = None;
            let status = tokio::select! {
                s = child.wait() => s.ok(),
                used = watchdog.exceeded(pid) => {
                    over = Some(used);
                    if let Some(pid) = pid {
                        spawn::signal_group(pid, "-KILL").await;
                    }
                    child.wait().await.ok()
                }
            };
            let cap = limits.memory_mb.unwrap_or_default();
            let by_cap = over.or_else(|| scoped.then(|| scope::killed_by_signal(&status)).flatten().map(|_| cap));
            for r in readers {
                let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
            }
            if let Some(used) = by_cap.filter(|_| !lock(&watch_out).stopped) {
                lock(&watch_out).killed =
                    Some(format!("killed: used {} MB, over memory_mb {cap}", used.max(cap)));
            }
            lock(&watch_out).exit = Some(exit_code(status));
            watch_woke.notify_waiters();
        });
        lock(&self.0).insert(
            (session.to_string(), handle.clone()),
            Job {
                command: command.to_string(),
                started: Instant::now(),
                pid,
                out,
                woke,
                _scratch: scratch.map(Arc::new),
            },
        );
        Ok(handle)
    }

    fn find(&self, session: &str, handle: &str) -> Result<Job, String> {
        let jobs = lock(&self.0);
        jobs.get(&(session.to_string(), handle.to_string()))
            .map(|j| Job {
                command: j.command.clone(),
                started: j.started,
                pid: j.pid,
                out: j.out.clone(),
                woke: j.woke.clone(),
                _scratch: j._scratch.clone(),
            })
            .ok_or_else(|| {
                let mut names: Vec<_> =
                    jobs.keys().filter(|(s, _)| s == session).map(|(_, h)| h.clone()).collect();
                names.sort();
                if names.is_empty() {
                    format!("no background command called {handle:?}; none are running")
                } else {
                    format!(
                        "no background command called {handle:?}; there are {}",
                        names.join(", ")
                    )
                }
            })
    }

    pub fn list(&self, session: &str) -> String {
        let jobs = lock(&self.0);
        let mut rows: Vec<_> = jobs.iter().filter(|((s, _), _)| s == session).collect();
        rows.sort_by(|a, b| a.0.1.cmp(&b.0.1));
        if rows.is_empty() {
            return "no background commands".into();
        }
        rows.iter()
            .map(|((_, h), j)| {
                let state = match lock(&j.out).exit {
                    None => "running".to_string(),
                    Some(Some(c)) => format!("exited {c}"),
                    Some(None) => "ended".to_string(),
                };
                format!("{h}: {state}, {}s: {}", j.started.elapsed().as_secs(), j.command)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn snapshot(&self, session: &str) -> Vec<Snapshot> {
        let jobs = lock(&self.0);
        let mut rows: Vec<_> = jobs.iter().filter(|((s, _), _)| s == session).collect();
        rows.sort_by_key(|((_, h), _)| {
            h.strip_prefix("job-").and_then(|n| n.parse::<usize>().ok()).unwrap_or(usize::MAX)
        });
        rows.into_iter()
            .map(|((_, h), j)| {
                let o = lock(&j.out);
                Snapshot {
                    handle: h.clone(),
                    command: j.command.clone(),
                    running: o.exit.is_none(),
                    exit: o.exit.flatten(),
                    seconds: j.started.elapsed().as_secs(),
                    dropped: o.dropped,
                    text: String::from_utf8_lossy(&o.bytes).into_owned(),
                }
            })
            .collect()
    }

    pub async fn output(
        &self,
        session: &str,
        handle: &str,
        wait: Duration,
        stop: &CancellationToken,
    ) -> Result<String, String> {
        self.output_until(session, handle, wait, None, stop).await
    }

    pub async fn output_until(
        &self,
        session: &str,
        handle: &str,
        wait: Duration,
        until: Option<&Until>,
        stop: &CancellationToken,
    ) -> Result<String, String> {
        let job = self.find(session, handle)?;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = job.woke.notified();
            {
                let o = lock(&job.out);
                if o.exit.is_some() {
                    break;
                }
                let fresh = o.read < o.dropped + o.bytes.len();
                let done = match until {
                    None => fresh,
                    Some(Until::Exit) => false,
                    Some(Until::Pattern(re)) => {
                        let start = o.read.max(o.dropped) - o.dropped;
                        String::from_utf8_lossy(&o.bytes[start..]).lines().any(|l| re.is_match(l))
                    }
                };
                if done {
                    break;
                }
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep_until(deadline) => break,
                _ = stop.cancelled() => return Err("stopped".into()),
            }
        }
        let mut o = lock(&job.out);
        let start = o.read.max(o.dropped);
        let skipped = start - o.read;
        let full = String::from_utf8_lossy(&o.bytes[start - o.dropped..]).into_owned();
        let (text, counted) = tail_of(&full);
        o.read = o.dropped + o.bytes.len();
        let state = match o.exit {
            None => format!("[{handle} still running after {}s]", job.started.elapsed().as_secs()),
            Some(Some(c)) => format!("[{handle} exited {c}]"),
            Some(None) => format!("[{handle} ended]"),
        };
        let state = match &o.killed {
            Some(why) => format!("[{why}]\n{state}"),
            None => state,
        };
        let mut lost = if skipped > 0 {
            format!("[{skipped} bytes of older output were dropped]\n")
        } else {
            String::new()
        };
        lost.push_str(&counted.unwrap_or_default());
        Ok(format!(
            "{lost}{text}{}{state}",
            if text.ends_with('\n') || text.is_empty() { "" } else { "\n" }
        ))
    }

    pub async fn kill_all(&self, session: &str) {
        let handles: Vec<String> =
            lock(&self.0).keys().filter(|(s, _)| s == session).map(|(_, h)| h.clone()).collect();
        for h in handles {
            let _ = self.kill(session, &h).await;
        }
    }

    pub async fn kill(&self, session: &str, handle: &str) -> Result<String, String> {
        let job = self.find(session, handle)?;
        if lock(&job.out).exit.is_some() {
            return Ok(format!("{handle} had already finished"));
        }
        if let Some(pid) = job.pid {
            lock(&job.out).stopped = true;
            spawn::signal_group(pid, "-KILL").await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while lock(&job.out).exit.is_none() && tokio::time::Instant::now() < deadline {
            let _ = tokio::time::timeout(Duration::from_millis(100), job.woke.notified()).await;
        }
        Ok(format!("{handle} stopped"))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const SID: &str = "chat-1";

    fn open() -> Confine {
        Confine::Open { env: Vec::new() }
    }

    #[tokio::test]
    async fn a_background_job_over_its_memory_cap_is_killed_and_says_so() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let cmd = "python3 -c 'b = bytearray(300 << 20); import time; time.sleep(30)'";
        let limits = Limits { memory_mb: Some(64) };
        for scoped in [true, false] {
            if scoped && !scope::available() {
                continue;
            }
            let h = jobs.start_with(SID, dir.path(), cmd, &open(), limits, scoped).unwrap();
            let out = jobs
                .output_until(SID, &h, Duration::from_secs(30), Some(&Until::Exit), &stop)
                .await
                .unwrap();
            assert!(out.contains("killed: used") && out.contains("over memory_mb 64"), "{scoped} {out}");
        }
    }

    #[tokio::test]
    async fn a_background_job_under_its_memory_cap_finishes_with_its_output() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let limits = Limits { memory_mb: Some(512) };
        let h = jobs.start_limited(SID, dir.path(), "echo fine", &open(), limits).unwrap();
        let out = jobs
            .output_until(SID, &h, Duration::from_secs(30), Some(&Until::Exit), &stop)
            .await
            .unwrap();
        assert!(out.contains("fine") && out.contains("exited 0") && !out.contains("killed"), "{out}");
    }

    #[tokio::test]
    async fn a_background_job_with_no_memory_cap_is_left_alone() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let cmd = "python3 -c 'b = bytearray(300 << 20); print(len(b))'";
        let h = jobs.start_limited(SID, dir.path(), cmd, &open(), Limits::default()).unwrap();
        let out = jobs
            .output_until(SID, &h, Duration::from_secs(30), Some(&Until::Exit), &stop)
            .await
            .unwrap();
        assert!(out.contains("314572800") && out.contains("exited 0"), "{out}");
    }

    #[tokio::test]
    async fn a_background_command_is_read_from_and_stopped() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let h = jobs.start(SID, dir.path(), "echo up; sleep 30", &open()).unwrap();
        assert_eq!(h, "job-1");
        let first = jobs.output(SID, &h, Duration::from_secs(5), &stop).await.unwrap();
        assert!(first.contains("up") && first.contains("still running"), "{first}");
        let again = jobs.output(SID, &h, Duration::from_millis(50), &stop).await.unwrap();
        assert!(!again.contains("up"), "only new output is returned: {again}");
        assert!(jobs.list(SID).contains("running"));
        jobs.kill(SID, &h).await.unwrap();
        let last = jobs.output(SID, &h, Duration::from_secs(1), &stop).await.unwrap();
        assert!(last.contains("exited"), "{last}");
        assert!(jobs.list(SID).contains("exited"));
    }

    #[tokio::test]
    async fn until_exit_waits_for_the_exit_not_for_output() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let h = jobs.start(SID, dir.path(), "echo chatty; sleep 1; echo end", &open()).unwrap();
        let began = Instant::now();
        let out = jobs
            .output_until(SID, &h, Duration::from_secs(600), Some(&Until::Exit), &stop)
            .await
            .unwrap();
        assert!(out.contains("exited 0") && out.contains("end"), "{out}");
        assert!(began.elapsed() < Duration::from_secs(10), "{:?}", began.elapsed());
    }

    #[tokio::test]
    async fn until_a_pattern_returns_on_the_matching_line_not_earlier_ones() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let cmd = "echo compiling; sleep 1; echo more; sleep 1; echo 'test result: FAILED'; sleep 30";
        let h = jobs.start(SID, dir.path(), cmd, &open()).unwrap();
        let until = Until::pattern("test result:|panicked").unwrap();
        let began = Instant::now();
        let out = jobs
            .output_until(SID, &h, Duration::from_secs(60), Some(&until), &stop)
            .await
            .unwrap();
        assert!(out.contains("test result: FAILED") && out.contains("still running"), "{out}");
        assert!(out.contains("compiling") && out.contains("more"), "{out}");
        assert!(began.elapsed() >= Duration::from_secs(2), "returned on an earlier line");
        assert!(began.elapsed() < Duration::from_secs(20));
        jobs.kill(SID, &h).await.unwrap();
    }

    #[tokio::test]
    async fn until_a_pattern_returns_on_exit_when_nothing_matches() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let h = jobs.start(SID, dir.path(), "echo fine; exit 2", &open()).unwrap();
        let until = Until::pattern("never-printed").unwrap();
        let out = jobs
            .output_until(SID, &h, Duration::from_secs(60), Some(&until), &stop)
            .await
            .unwrap();
        assert!(out.contains("exited 2"), "{out}");
    }

    #[tokio::test]
    async fn stop_cancels_an_until_wait() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let h = jobs.start(SID, dir.path(), "sleep 30", &open()).unwrap();
        let canceller = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            canceller.cancel();
        });
        let began = Instant::now();
        let err = jobs
            .output_until(SID, &h, Duration::from_secs(600), Some(&Until::Exit), &stop)
            .await
            .unwrap_err();
        assert_eq!(err, "stopped");
        assert!(began.elapsed() < Duration::from_secs(5));
        jobs.kill(SID, &h).await.unwrap();
    }

    #[tokio::test]
    async fn long_output_is_returned_as_a_capped_tail_with_a_line_count() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let h = jobs.start(SID, dir.path(), "seq 1 40000; echo last-line", &open()).unwrap();
        let out = jobs
            .output_until(SID, &h, Duration::from_secs(30), Some(&Until::Exit), &stop)
            .await
            .unwrap();
        assert!(out.len() <= TAIL + 512, "{}", out.len());
        assert!(out.contains("last-line") && out.contains("exited 0"), "tail kept");
        assert!(out.contains("40001 lines") && out.contains("showing the last"), "{}", &out[..200]);
        assert!(!out.contains("\n1\n"), "head dropped");
    }

    #[tokio::test]
    async fn a_snapshot_shows_all_output_without_consuming_it() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let h = jobs.start(SID, dir.path(), "echo up; sleep 30", &open()).unwrap();
        jobs.output(SID, &h, Duration::from_secs(5), &stop).await.unwrap();
        let seen = jobs.snapshot(SID);
        assert_eq!(seen.len(), 1);
        assert!(seen[0].running && seen[0].text.contains("up"), "{seen:?}");
        assert_eq!(seen[0].handle, "job-1");
        assert_eq!(jobs.snapshot(SID)[0].text, seen[0].text);
        assert!(jobs.snapshot("other").is_empty());
        jobs.kill(SID, &h).await.unwrap();
        assert!(!jobs.snapshot(SID)[0].running);
    }

    #[tokio::test]
    async fn a_finished_command_reports_its_exit_code() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let h = jobs.start(SID, dir.path(), "echo bye >&2; exit 3", &open()).unwrap();
        let out = jobs.output(SID, &h, Duration::from_secs(5), &stop).await.unwrap();
        assert!(out.contains("bye"), "{out}");
        let out = if out.contains("exited") {
            out
        } else {
            jobs.output(SID, &h, Duration::from_secs(5), &stop).await.unwrap()
        };
        assert!(out.contains("exited 3"), "{out}");
    }

    #[tokio::test]
    async fn kill_all_stops_every_job_of_a_chat() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let a = jobs.start(SID, dir.path(), "sleep 30", &open()).unwrap();
        let b = jobs.start(SID, dir.path(), "sleep 30", &open()).unwrap();
        jobs.kill_all(SID).await;
        for h in [a, b] {
            let out = jobs.output(SID, &h, Duration::from_secs(2), &stop).await.unwrap();
            assert!(out.contains("exited") || out.contains("ended"), "{out}");
        }
    }

    #[tokio::test]
    async fn jobs_belong_to_their_session() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let h = jobs.start(SID, dir.path(), "sleep 30", &open()).unwrap();
        let other = "chat-2";
        let stop = CancellationToken::new();
        assert!(jobs.output(other, &h, Duration::ZERO, &stop).await.is_err());
        assert_eq!(jobs.list(other), "no background commands");
        jobs.kill(SID, &h).await.unwrap();
    }
}
