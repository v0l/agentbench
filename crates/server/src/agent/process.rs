use super::{Agent, Decoder, Event, Io, Turn, async_trait, brief, resolve, with_files};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

const STDERR_KEPT: usize = 4000;
const LISTING_PATIENCE: Duration = Duration::from_secs(20);
const TERM_GRACE: Duration = Duration::from_secs(3);

/// Run `program args` for a short listing and return its stdout and stderr together.
pub(super) async fn listing(program: &Path, args: &[&str]) -> Result<String, String> {
    let run = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(LISTING_PATIENCE, run)
        .await
        .map_err(|_| format!("{} took too long to list models", program.display()))?
        .map_err(|e| format!("could not start {}: {e}", program.display()))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub env: Vec<(String, String)>,
}

#[async_trait]
pub trait Cli: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn program(&self) -> &Path;
    fn resumable(&self) -> bool;
    fn launch(&self, turn: &Turn, brief: &str) -> Launch;
    async fn models(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    fn decoder(&self) -> Box<dyn Decoder>;
}

#[async_trait]
impl<T: Cli> Agent for T {
    fn name(&self) -> &str {
        Cli::name(self)
    }

    fn label(&self) -> &str {
        Cli::label(self)
    }

    fn available(&self) -> bool {
        resolve(self.program()).is_some()
    }

    fn about(&self) -> String {
        self.program().display().to_string()
    }

    fn resumable(&self) -> bool {
        Cli::resumable(self)
    }

    async fn models(&self) -> Result<Vec<String>, String> {
        Cli::models(self).await
    }

    async fn run(&self, turn: &Turn<'_>, io: &dyn Io) -> Result<(), String> {
        let prompt = with_files(turn.prompt, turn.files);
        let told = Turn {
            prompt: &prompt,
            files: &[],
            cwd: turn.cwd,
            model: turn.model,
            effort: turn.effort,
            resume: turn.resume,
            history: turn.history,
            context: turn.context,
        };
        let launch = self.launch(&told, &brief(turn.context));
        run_process(self, launch, turn, io).await
    }
}

async fn run_process<T: Cli + ?Sized>(
    cli: &T,
    launch: Launch,
    turn: &Turn<'_>,
    io: &dyn Io,
) -> Result<(), String> {
    let mut cmd = Command::new(&launch.program);
    cmd.args(&launch.args)
        .current_dir(turn.cwd)
        .envs(turn.context.env())
        .envs(launch.env.iter().cloned())
        .stdin(if launch.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let mut child =
        cmd.spawn().map_err(|e| format!("could not start {}: {e}", launch.program.display()))?;
    if let (Some(text), Some(mut stdin)) = (launch.stdin, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = stdin.write_all(text.as_bytes()).await;
        });
    }
    let stderr = child.stderr.take().map(|err| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = err.take(64 * 1024).read_to_end(&mut buf).await;
            let text = String::from_utf8_lossy(&buf).trim().to_string();
            let start = text.len().saturating_sub(STDERR_KEPT);
            text[text.floor_char_boundary(start)..].to_string()
        })
    });
    let mut lines = BufReader::new(child.stdout.take().expect("stdout piped")).lines();
    let mut decoder = cli.decoder();
    let mut errored = false;
    loop {
        let line = tokio::select! {
            _ = io.stop().cancelled() => {
                kill_tree(&mut child).await;
                return Err("stopped".into());
            }
            line = lines.next_line() => line,
        };
        let Ok(Some(line)) = line else { break };
        for event in decoder.line(&line) {
            errored |= matches!(event, Event::Error(_));
            io.emit(event);
        }
    }
    let exit = tokio::select! {
        _ = io.stop().cancelled() => {
            kill_tree(&mut child).await;
            return Err("stopped".into());
        }
        exit = child.wait() => exit.ok(),
    };
    let stderr = match stderr {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };
    if exit.is_some_and(|s| s.success()) || errored {
        return if errored { Err(String::new()) } else { Ok(()) };
    }
    let code = exit.and_then(|s| s.code()).map(|c| format!(" (exit {c})")).unwrap_or_default();
    let detail = if stderr.is_empty() { "no output".to_string() } else { stderr };
    Err(format!("{} failed{code}: {detail}", cli.name()))
}

/// Stop a child and everything it started: TERM to its process group, then KILL if it lingers.
pub(super) async fn kill_tree(child: &mut Child) {
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
pub(super) async fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("kill").args([signal, &format!("-{pid}")]).status().await;
}
