//! Commands left running in the background: a dev server, a watcher, a long build.
//!
//! `bash` waits for its command, so an agent cannot start a server and carry on. With
//! `background` it gets a handle instead, reads what the command has printed since it last
//! looked with `bash_output`, and stops it with `bash_kill`. Each session sees only its own jobs.
//! A job outlives the turn that started it and dies with the server.

use foreman_core::SessionId;
use std::collections::HashMap;
use std::path::Path;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::Notify;

/// Output kept for a job; the oldest bytes go first.
const KEPT: usize = 256 * 1024;
/// Jobs a session may have running at once.
const RUNNING_MAX: usize = 8;

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    /// How many bytes have been dropped from the front, so offsets stay absolute.
    dropped: usize,
    /// Offset the agent has read up to.
    read: usize,
    exit: Option<Option<i32>>,
}

struct Job {
    command: String,
    started: Instant,
    pid: Option<u32>,
    out: Arc<Mutex<Output>>,
    woke: Arc<Notify>,
}

#[derive(Clone, Default)]
pub struct Jobs(Arc<Mutex<HashMap<(SessionId, String), Job>>>);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn exit_code(s: Option<ExitStatus>) -> Option<i32> {
    s.map(|s| s.code().unwrap_or(-1))
}

impl Jobs {
    /// Start `command` in `cwd` and answer with its handle.
    pub fn start(
        &self,
        session: SessionId,
        cwd: &Path,
        command: &str,
        env: Vec<(String, String)>,
    ) -> Result<String, String> {
        if command.trim().is_empty() {
            return Err("`command` is empty".into());
        }
        let running = lock(&self.0)
            .iter()
            .filter(|((s, _), j)| *s == session && lock(&j.out).exit.is_none())
            .count();
        if running >= RUNNING_MAX {
            return Err(format!(
                "{running} background commands are already running; bash_kill some"
            ));
        }
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(command)
            .current_dir(cwd)
            .envs(env)
            .env("NO_COLOR", "1")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
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
            let n = jobs.keys().filter(|(s, _)| *s == session).count() + 1;
            format!("job-{n}")
        };
        let (watch_out, watch_woke) = (out.clone(), woke.clone());
        tokio::spawn(async move {
            let status = child.wait().await.ok();
            for r in readers {
                let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
            }
            lock(&watch_out).exit = Some(exit_code(status));
            watch_woke.notify_waiters();
        });
        lock(&self.0).insert(
            (session, handle.clone()),
            Job { command: command.to_string(), started: Instant::now(), pid, out, woke },
        );
        Ok(handle)
    }

    fn find(&self, session: SessionId, handle: &str) -> Result<Job, String> {
        let jobs = lock(&self.0);
        jobs.get(&(session, handle.to_string()))
            .map(|j| Job {
                command: j.command.clone(),
                started: j.started,
                pid: j.pid,
                out: j.out.clone(),
                woke: j.woke.clone(),
            })
            .ok_or_else(|| {
                let mut names: Vec<_> =
                    jobs.keys().filter(|(s, _)| *s == session).map(|(_, h)| h.clone()).collect();
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

    /// The jobs of a session, one line each.
    pub fn list(&self, session: SessionId) -> String {
        let jobs = lock(&self.0);
        let mut rows: Vec<_> = jobs.iter().filter(|((s, _), _)| *s == session).collect();
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

    /// What a job printed since it was last read. Waits up to `wait` for something new when
    /// nothing is waiting and the job is still going.
    pub async fn output(
        &self,
        session: SessionId,
        handle: &str,
        wait: Duration,
        stop: &tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        let job = self.find(session, handle)?;
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let notified = job.woke.notified();
            {
                let o = lock(&job.out);
                if o.read < o.dropped + o.bytes.len() || o.exit.is_some() {
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
        let text = String::from_utf8_lossy(&o.bytes[start - o.dropped..]).into_owned();
        o.read = o.dropped + o.bytes.len();
        let state = match o.exit {
            None => format!("[{handle} still running after {}s]", job.started.elapsed().as_secs()),
            Some(Some(c)) => format!("[{handle} exited {c}]"),
            Some(None) => format!("[{handle} ended]"),
        };
        let lost = if skipped > 0 {
            format!("[{skipped} bytes of older output were dropped]\n")
        } else {
            String::new()
        };
        Ok(format!(
            "{lost}{text}{}{state}",
            if text.ends_with('\n') || text.is_empty() { "" } else { "\n" }
        ))
    }

    /// Stop a job and everything it started.
    pub async fn kill(&self, session: SessionId, handle: &str) -> Result<String, String> {
        let job = self.find(session, handle)?;
        if lock(&job.out).exit.is_some() {
            return Ok(format!("{handle} had already finished"));
        }
        #[cfg(unix)]
        if let Some(pid) = job.pid {
            super::process::signal_group(pid, "-KILL").await;
        }
        #[cfg(not(unix))]
        if let Some(pid) = job.pid {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .status()
                .await;
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
    use tokio_util::sync::CancellationToken;

    fn sid() -> SessionId {
        SessionId::default()
    }

    #[tokio::test]
    async fn a_background_command_is_read_from_and_stopped() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let h = jobs.start(sid(), dir.path(), "echo up; sleep 30", Vec::new()).unwrap();
        assert_eq!(h, "job-1");
        let first = jobs.output(sid(), &h, Duration::from_secs(5), &stop).await.unwrap();
        assert!(first.contains("up") && first.contains("still running"), "{first}");
        let again = jobs.output(sid(), &h, Duration::from_millis(50), &stop).await.unwrap();
        assert!(!again.contains("up"), "only new output is returned: {again}");
        assert!(jobs.list(sid()).contains("running"));
        jobs.kill(sid(), &h).await.unwrap();
        let last = jobs.output(sid(), &h, Duration::from_secs(1), &stop).await.unwrap();
        assert!(last.contains("exited"), "{last}");
        assert!(jobs.list(sid()).contains("exited"));
    }

    #[tokio::test]
    async fn a_finished_command_reports_its_exit_code() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let stop = CancellationToken::new();
        let h = jobs.start(sid(), dir.path(), "echo bye >&2; exit 3", Vec::new()).unwrap();
        let out = jobs.output(sid(), &h, Duration::from_secs(5), &stop).await.unwrap();
        assert!(out.contains("bye"), "{out}");
        let out = if out.contains("exited") {
            out
        } else {
            jobs.output(sid(), &h, Duration::from_secs(5), &stop).await.unwrap()
        };
        assert!(out.contains("exited 3"), "{out}");
    }

    #[tokio::test]
    async fn jobs_belong_to_their_session() {
        let jobs = Jobs::default();
        let dir = tempfile::tempdir().unwrap();
        let h = jobs.start(sid(), dir.path(), "sleep 30", Vec::new()).unwrap();
        let other = SessionId(uuid::Uuid::new_v4());
        let stop = CancellationToken::new();
        assert!(jobs.output(other, &h, Duration::ZERO, &stop).await.is_err());
        assert_eq!(jobs.list(other), "no background commands");
        jobs.kill(sid(), &h).await.unwrap();
    }
}
