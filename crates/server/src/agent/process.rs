use super::{Agent, Decoder, Event, Io, Turn, brief, resolve};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

const STDERR_KEPT: usize = 4000;

/// Run `program args` for a short listing and return its stdout and stderr together.
pub(super) fn listing(program: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", program.display()))?;
    let read = |from: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = String::new();
            if let Some(mut r) = from {
                let _ = r.read_to_string(&mut text);
            }
            text
        })
    };
    let out = read(child.stdout.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let err = read(child.stderr.take().map(|r| Box::new(r) as Box<dyn Read + Send>));
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} took too long to list models", program.display()));
            }
        }
    }
    let mut text = out.join().unwrap_or_default();
    text.push_str(&err.join().unwrap_or_default());
    Ok(text)
}

pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub env: Vec<(String, String)>,
}

pub trait Cli: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str;
    fn program(&self) -> &Path;
    fn resumable(&self) -> bool;
    fn launch(&self, turn: &Turn, brief: &str) -> Launch;
    fn models(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    fn decoder(&self) -> Box<dyn Decoder>;
}

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

    fn models(&self) -> Result<Vec<String>, String> {
        Cli::models(self)
    }

    fn run(&self, turn: &Turn, io: &dyn Io) -> Result<(), String> {
        let launch = self.launch(turn, &brief(turn.context));
        run_process(self, launch, turn, io)
    }
}

fn run_process<T: Cli + ?Sized>(
    cli: &T,
    launch: Launch,
    turn: &Turn,
    io: &dyn Io,
) -> Result<(), String> {
    let mut cmd = Command::new(&launch.program);
    cmd.args(&launch.args)
        .current_dir(turn.cwd)
        .envs(turn.context.env())
        .envs(launch.env.iter().cloned())
        .stdin(if launch.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child =
        cmd.spawn().map_err(|e| format!("could not start {}: {e}", launch.program.display()))?;
    if let (Some(text), Some(mut stdin)) = (launch.stdin, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    let stderr = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut err).take(64 * 1024).read_to_end(&mut buf);
            let text = String::from_utf8_lossy(&buf).trim().to_string();
            let start = text.len().saturating_sub(STDERR_KEPT);
            text[text.floor_char_boundary(start)..].to_string()
        })
    });
    let (lines_tx, lines) = channel::<String>();
    let stdout = child.stdout.take().expect("stdout piped");
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut decoder = cli.decoder();
    let mut errored = false;
    loop {
        if io.stopped() {
            kill_tree(&mut child);
            let _ = child.wait();
            return Err("stopped".into());
        }
        match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                for event in decoder.line(&line) {
                    errored |= matches!(event, Event::Error(_));
                    io.emit(event);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    let exit = child.wait().ok();
    let stderr = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
    if exit.is_some_and(|s| s.success()) || errored {
        return if errored { Err(String::new()) } else { Ok(()) };
    }
    let code = exit.and_then(|s| s.code()).map(|c| format!(" (exit {c})")).unwrap_or_default();
    let detail = if stderr.is_empty() { "no output".to_string() } else { stderr };
    Err(format!("{} failed{code}: {detail}", cli.name()))
}

fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
        for _ in 0..30 {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = Command::new("kill").args(["-KILL", &format!("-{pid}")]).status();
    }
    let _ = child.kill();
}
