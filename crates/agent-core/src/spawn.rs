use super::sandbox::{self, Plan, Shared};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

pub const MAX_OUTPUT: usize = 30_000;
const TERM_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq)]
pub enum Confine {
    Sandbox { tool_dirs: Vec<PathBuf>, shared: Vec<Shared>, env: Vec<(String, String)> },
    Open { env: Vec<(String, String)> },
}

impl Default for Confine {
    fn default() -> Self {
        Confine::Sandbox { tool_dirs: Vec::new(), shared: Vec::new(), env: Vec::new() }
    }
}

impl Confine {
    pub fn sandboxed(&self) -> bool {
        matches!(self, Confine::Sandbox { .. })
    }

    pub fn widened(
        &self,
        on_path: &[PathBuf],
        readable: &[PathBuf],
        env: &[(String, String)],
    ) -> Confine {
        match self.clone() {
            Confine::Sandbox { mut tool_dirs, mut shared, env: mut base } => {
                for d in on_path {
                    if !tool_dirs.contains(d) {
                        tool_dirs.push(d.clone());
                    }
                }
                for f in readable {
                    if !shared.iter().any(|s| s.path == *f) {
                        shared.push(Shared { path: f.clone(), write: false });
                    }
                }
                base.extend(env.iter().cloned());
                Confine::Sandbox { tool_dirs, shared, env: base }
            }
            Confine::Open { env: mut base } => {
                base.extend(env.iter().cloned());
                Confine::Open { env: base }
            }
        }
    }
}

pub struct Scratch(PathBuf);

impl Scratch {
    pub fn new() -> Result<Scratch, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("egui-bench-run-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        dir.canonicalize().map(Scratch).map_err(|e| e.to_string())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Prepared {
    pub command: Command,
    pub scratch: Option<Scratch>,
}

pub fn quote(word: &str) -> String {
    if !word.is_empty()
        && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,+@%".contains(c))
    {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

pub fn line(program: &Path, args: &[String]) -> String {
    let mut words = vec![quote(&program.display().to_string())];
    words.extend(args.iter().map(|a| quote(a)));
    words.join(" ")
}

pub fn prepare(shell_line: &str, cwd: &Path, confine: &Confine) -> Result<Prepared, String> {
    if shell_line.trim().is_empty() {
        return Err("`command` is empty".into());
    }
    let mut prepared = match confine {
        Confine::Sandbox { tool_dirs, shared, env } => {
            if !sandbox::supported() {
                return Err(format!(
                    "unavailable: no sandbox on this machine ({})",
                    sandbox::why_not()
                ));
            }
            let scratch = Scratch::new()?;
            let plan = Plan {
                workspace: cwd.to_path_buf(),
                scratch: scratch.path().to_path_buf(),
                tool_dirs: tool_dirs.clone(),
                command: shell_line.to_string(),
                shared: shared.clone(),
                env: env.clone(),
            };
            let me = std::env::current_exe().map_err(|e| e.to_string())?;
            let mut command = Command::new(me);
            command.args(sandbox::helper_args(&plan)?);
            Prepared { command, scratch: Some(scratch) }
        }
        Confine::Open { env } => {
            let mut command = Command::new(sandbox::shell());
            command
                .arg("-c")
                .arg(shell_line)
                .envs(env.iter().cloned())
                .env("NO_COLOR", "1")
                .env("GIT_PAGER", "cat")
                .env("PAGER", "cat");
            Prepared { command, scratch: None }
        }
    };
    prepared.command.current_dir(cwd).kill_on_drop(true);
    own_group(&mut prepared.command);
    no_console(&mut prepared.command);
    Ok(prepared)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Exited(i32),
    Signal,
    TimedOut,
}

pub struct Output {
    pub text: String,
    pub status: Status,
}

pub async fn run(
    mut prepared: Prepared,
    stdin: Option<&str>,
    timeout: Duration,
    stop: &CancellationToken,
) -> Result<Output, String> {
    let cmd = &mut prepared.command;
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let input = input.to_string();
        tokio::spawn(async move {
            let _ = pipe.write_all(input.as_bytes()).await;
        });
    }
    let drain = |pipe: Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf).await;
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let status = tokio::select! {
        s = child.wait() => match s.map_err(|e| e.to_string())?.code() {
            Some(c) => Status::Exited(c),
            None => Status::Signal,
        },
        _ = tokio::time::sleep(timeout) => {
            kill_tree(&mut child).await;
            Status::TimedOut
        }
        _ = stop.cancelled() => {
            kill_tree(&mut child).await;
            return Err("stopped".into());
        }
    };
    let mut text = String::new();
    for reader in [out, err] {
        let Ok(Ok(bytes)) = tokio::time::timeout(Duration::from_millis(500), reader).await else {
            continue;
        };
        text.push_str(&String::from_utf8_lossy(&bytes));
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
    }
    drop(prepared.scratch);
    Ok(Output { text: clip(text), status })
}

pub fn report(
    mut shown: String,
    out: Output,
    took: Duration,
    timeout: u64,
) -> Result<String, String> {
    shown.push_str(&out.text);
    let took = took.as_secs_f32();
    let end = match out.status {
        Status::Exited(code) => format!("\n[exit {code} in {took:.1}s]"),
        Status::Signal => format!("\n[killed by a signal after {took:.1}s]"),
        Status::TimedOut => {
            format!("\n[killed after {timeout}s: too slow, pass a larger timeout_s]")
        }
    };
    shown.push_str(&end);
    match out.status {
        Status::Exited(0) => Ok(shown),
        _ => Err(shown),
    }
}

pub fn clip(text: String) -> String {
    if text.len() <= MAX_OUTPUT {
        return text;
    }
    let keep = MAX_OUTPUT / 2;
    let head = text.floor_char_boundary(keep);
    let tail = text.ceil_char_boundary(text.len() - keep);
    format!("{}\n[... {} bytes left out ...]\n{}", &text[..head], tail - head, &text[tail..])
}

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
    let _ = Command::new("kill")
        .args([signal, "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

#[cfg(not(unix))]
pub async fn signal_group(pid: u32, _signal: &str) {
    let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &pid.to_string()]).status().await;
}

#[cfg(unix)]
fn own_group(cmd: &mut Command) {
    cmd.process_group(0);
}

#[cfg(not(unix))]
fn own_group(_cmd: &mut Command) {}

#[cfg(windows)]
fn no_console(cmd: &mut Command) {
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn no_console(_cmd: &mut Command) {}

pub fn resolve(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(program)).find(|p| p.is_file())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn open() -> Confine {
        Confine::Open { env: vec![("GREETING".into(), "hi".into())] }
    }

    #[tokio::test]
    async fn a_command_gets_stdin_its_env_and_reports_its_exit() {
        let dir = tempfile::tempdir().unwrap();
        let p = prepare("cat; echo $GREETING >&2; exit 3", dir.path(), &open()).unwrap();
        let out = run(p, Some("hello\n"), Duration::from_secs(10), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.status, Status::Exited(3));
        assert!(out.text.contains("hello") && out.text.contains("hi"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_slow_command_is_killed_with_what_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let p = prepare("sleep 30 & sleep 30", dir.path(), &open()).unwrap();
        let started = std::time::Instant::now();
        let out =
            run(p, None, Duration::from_millis(300), &CancellationToken::new()).await.unwrap();
        assert_eq!(out.status, Status::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn stopping_ends_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let p = prepare("sleep 30", dir.path(), &open()).unwrap();
        let stop = CancellationToken::new();
        let later = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            later.cancel();
        });
        assert_eq!(run(p, None, Duration::from_secs(30), &stop).await.err().unwrap(), "stopped");
    }

    #[test]
    fn long_output_keeps_its_head_and_tail() {
        let big = format!("{}{}", "a".repeat(40_000), "z".repeat(40_000));
        let cut = clip(big);
        assert!(cut.len() < 31_000);
        assert!(cut.starts_with('a') && cut.ends_with('z'));
        assert!(cut.contains("bytes left out"));
    }

    #[test]
    fn words_are_quoted_for_the_shell_only_when_they_need_it() {
        assert_eq!(quote("--stdio"), "--stdio");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(quote(""), "''");
        assert_eq!(line(Path::new("/usr/bin/x y"), &["--a".into()]), "'/usr/bin/x y' --a");
    }

    #[test]
    fn widening_adds_folders_files_and_variables_once() {
        let base = Confine::default();
        let d = PathBuf::from("/opt/tool");
        let f = PathBuf::from("/x/config.toml");
        let w = base.widened(&[d.clone(), d.clone()], &[f.clone(), f], &[("A".into(), "1".into())]);
        let Confine::Sandbox { tool_dirs, shared, env } = w else { panic!() };
        assert_eq!(tool_dirs, [d]);
        assert_eq!(shared.len(), 1);
        assert!(!shared[0].write);
        assert_eq!(env, [("A".to_string(), "1".to_string())]);
    }
}
