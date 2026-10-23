use crate::{Config, DEFAULT_LISTEN, home, home_file, store::Store, token};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub struct Local {
    pub home: PathBuf,
    pub listen: SocketAddr,
    pub token: String,
}

impl Local {
    pub fn open(home_dir: Option<PathBuf>) -> Result<Local, String> {
        let home = home_dir.unwrap_or_else(home);
        let config = Config::load(&home_file(&home, "agentbench.toml", "foreman.toml"))?;
        let store =
            Store::open(&home_file(&home, "agentbench.db", "foreman.db")).map_err(|e| e.0)?;
        let token = token(&store).map_err(|e| e.0)?;
        let listen = config
            .listen
            .unwrap_or_else(|| DEFAULT_LISTEN.parse().expect("default listen address"));
        Ok(Local { home, listen, token })
    }

    pub fn url(&self) -> String {
        let host = match self.listen {
            SocketAddr::V4(a) if a.ip().is_unspecified() => format!("127.0.0.1:{}", a.port()),
            SocketAddr::V6(a) if a.ip().is_unspecified() => format!("[::1]:{}", a.port()),
            a => a.to_string(),
        };
        format!("http://{host}")
    }

    pub fn running(&self) -> bool {
        let target = match self.listen {
            SocketAddr::V4(a) if a.ip().is_unspecified() => {
                SocketAddr::from(([127, 0, 0, 1], a.port()))
            }
            SocketAddr::V6(a) if a.ip().is_unspecified() => {
                SocketAddr::from(([0u16, 0, 0, 0, 0, 0, 0, 1], a.port()))
            }
            a => a,
        };
        TcpStream::connect_timeout(&target, Duration::from_millis(300)).is_ok()
    }

    pub fn log_path(&self) -> PathBuf {
        self.home.join("server.log")
    }

    pub fn spawn(&self, program: &Path, args: &[&str]) -> Result<std::process::Child, String> {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .map_err(|e| format!("{}: {e}", self.log_path().display()))?;
        let err = log.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .env("AGENTBENCH_HOME", &self.home)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        cmd.spawn().map_err(|e| format!("could not start {}: {e}", program.display()))
    }

    pub fn ensure(&self, program: &Path, args: &[&str]) -> Result<Option<u32>, String> {
        if self.running() {
            return Ok(None);
        }
        let mut child = self.spawn(program, args)?;
        let pid = child.id();
        let start = Instant::now();
        let mut up = false;
        while start.elapsed() < Duration::from_secs(10) {
            if self.running() {
                up = true;
                break;
            }
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        if up {
            Ok(Some(pid))
        } else {
            let tail = std::fs::read_to_string(self.log_path()).unwrap_or_default();
            let tail: Vec<&str> = tail.lines().rev().take(3).collect();
            Err(format!(
                "the agentbench server did not come up on {}: {}",
                self.listen,
                tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(dir: &Path, listen: &str) -> Local {
        std::fs::write(dir.join("agentbench.toml"), format!("listen = \"{listen}\"\n")).unwrap();
        Local::open(Some(dir.to_path_buf())).unwrap()
    }

    #[test]
    fn opening_makes_the_token_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = local(dir.path(), "127.0.0.1:7421");
        let b = Local::open(Some(dir.path().to_path_buf())).unwrap();
        assert_eq!(a.token, b.token);
        assert_eq!(a.token.len(), 48);
        assert_eq!(a.url(), "http://127.0.0.1:7421");
    }

    #[test]
    fn a_wildcard_listen_is_reached_on_loopback() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(local(dir.path(), "0.0.0.0:7422").url(), "http://127.0.0.1:7422");
    }

    #[test]
    fn running_sees_a_listener() {
        let dir = tempfile::tempdir().unwrap();
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mine = local(dir.path(), &format!("127.0.0.1:{port}"));
        assert!(mine.running());
        drop(l);
        let gone = local(dir.path(), "127.0.0.1:1");
        assert!(!gone.running(), "nothing listens on port 1");
    }

    #[test]
    fn ensure_starts_a_daemon_and_leaves_a_running_one_alone() {
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mine = local(dir.path(), &format!("127.0.0.1:{port}"));
        let script = format!(
            "exec python3 -c 'import socket,time;s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);s.bind((\"127.0.0.1\",{port}));s.listen();time.sleep(20)'"
        );
        let pid = mine.ensure(Path::new("sh"), &["-c", &script, "sh"]).unwrap().expect("started");
        assert_eq!(
            mine.ensure(Path::new("sh"), &["-c", "exit 1", "sh"]).unwrap(),
            None,
            "already up"
        );
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }

    #[test]
    fn ensure_reports_a_daemon_that_dies() {
        let start = Instant::now();
        let dir = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mine = local(dir.path(), &format!("127.0.0.1:{port}"));
        let e =
            mine.ensure(Path::new("sh"), &["-c", "echo port in use; exit 1", "sh"]).unwrap_err();
        assert!(e.contains("port in use"), "{e}");
        assert!(start.elapsed() < Duration::from_secs(3), "a dead child is noticed at once");
    }
}
