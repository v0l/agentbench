#![cfg(any(target_os = "linux", target_os = "macos"))]

use agent_core::sandbox;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if let Some(code) = sandbox::run_helper() {
        std::process::exit(code);
    }
    let mut failed = Vec::new();
    let cases: [(&str, fn()); 13] = [
        ("a command writes inside the workspace and its scratch", writes_inside),
        ("a command cannot write outside the workspace", cannot_write_outside),
        ("a command cannot read what is outside the workspace", cannot_read_outside),
        ("a command cannot follow a link out", cannot_follow_a_link),
        ("the environment is scrubbed", environment_is_scrubbed),
        ("pipes, globs and exit codes work", shell_works),
        ("without a plan the helper refuses to run", refuses_without_a_plan),
        ("a shared folder is readable and writable only as far as shared", shared_folders),
        ("a share does not widen to its neighbours", a_share_stays_where_it_was_put),
        ("names still resolve, since the network is open", names_resolve),
        ("a compiler runs without the system temp folder being opened", a_compiler_runs),
        ("variables the plan names reach the command", plan_env_reaches_the_command),
        ("a language server answers from inside the sandbox", a_language_server_runs_confined),
    ];
    for (name, case) in cases {
        match std::panic::catch_unwind(case) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed.push(name);
            }
        }
    }
    if !failed.is_empty() {
        eprintln!("{} failed: {failed:?}", failed.len());
        std::process::exit(1);
    }
}

fn refused(text: &str) -> bool {
    text.contains("Permission denied") || text.contains("Operation not permitted")
}

struct Rig {
    root: PathBuf,
    workspace: PathBuf,
    scratch: PathBuf,
    outside: PathBuf,
}

impl Rig {
    fn new(name: &str) -> Rig {
        let base = std::env::temp_dir();
        let root = base
            .canonicalize()
            .unwrap_or(base)
            .join(format!("egui-bench-sandbox-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let make = |sub: &str| {
            let dir = root.join(sub);
            fs::create_dir_all(&dir).unwrap();
            dir.canonicalize().unwrap()
        };
        let rig = Rig {
            workspace: make("work"),
            scratch: make("scratch"),
            outside: make("outside"),
            root,
        };
        fs::write(rig.outside.join("secret.txt"), "TOPSECRET").unwrap();
        rig
    }

    fn plan(&self, command: &str, shared: &[(&Path, bool)]) -> String {
        let shared: Vec<_> =
            shared.iter().map(|(p, w)| serde_json::json!({ "path": p, "write": w })).collect();
        serde_json::json!({
            "workspace": self.workspace,
            "scratch": self.scratch,
            "tool_dirs": [],
            "command": command,
            "shared": shared,
        })
        .to_string()
    }

    fn run(&self, command: &str) -> (i32, String) {
        self.run_with(command, &[])
    }

    fn run_with(&self, command: &str, shared: &[(&Path, bool)]) -> (i32, String) {
        let out = Command::new(std::env::current_exe().unwrap())
            .arg(sandbox::FLAG)
            .arg(self.plan(command, shared))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code().unwrap_or(-1), text)
    }

    fn usable(&self) -> bool {
        let (code, text) = self.run("true");
        if code != 0 {
            eprintln!("no sandbox here, skipping: {text}");
        }
        code == 0
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn writes_inside() {
    let rig = Rig::new("inside");
    if !rig.usable() {
        return;
    }
    let (code, out) =
        rig.run("echo hi > out.txt && cat out.txt && echo t > $TMPDIR/t && cat $TMPDIR/t");
    assert_eq!(code, 0, "{out}");
    assert_eq!(out, "hi\nt\n");
    assert_eq!(fs::read_to_string(rig.workspace.join("out.txt")).unwrap(), "hi\n");
}

fn cannot_write_outside() {
    let rig = Rig::new("write");
    if !rig.usable() {
        return;
    }
    let target = rig.outside.join("new.txt");
    let (_, out) = rig.run(&format!("echo x > {}", target.display()));
    assert!(refused(&out), "{out}");
    assert!(!target.exists());
    let (_, out) = rig.run("echo x > /etc/egui-bench-sandbox-test");
    assert!(!Path::new("/etc/egui-bench-sandbox-test").exists(), "{out}");
}

fn cannot_read_outside() {
    let rig = Rig::new("read");
    if !rig.usable() {
        return;
    }
    let (_, out) = rig.run(&format!("cat {}", rig.outside.join("secret.txt").display()));
    assert!(refused(&out), "{out}");
    assert!(!out.contains("TOPSECRET"));
    if let Some(home) = std::env::var_os("HOME") {
        let (_, out) = rig.run(&format!("ls {}", Path::new(&home).display()));
        assert!(refused(&out), "{out}");
    }
}

fn cannot_follow_a_link() {
    let rig = Rig::new("link");
    if !rig.usable() {
        return;
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(&rig.outside, rig.workspace.join("door")).unwrap();
    let (_, out) = rig.run("cat door/secret.txt");
    assert!(!out.contains("TOPSECRET"), "{out}");
}

fn environment_is_scrubbed() {
    let rig = Rig::new("env");
    if !rig.usable() {
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .arg(sandbox::FLAG)
        .arg(rig.plan("env", &[]))
        .env("SECRET_TOKEN", "leaked")
        .env("OPENAI_API_KEY", "sk-leaked")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("leaked"), "{text}");
    assert!(text.contains("TMPDIR="));
    assert!(text.contains(&format!("HOME={}", rig.workspace.display())));
}

fn shell_works() {
    let rig = Rig::new("shell");
    if !rig.usable() {
        return;
    }
    let (code, out) = rig.run("printf 'a\\nb\\n' > l.txt; ls *.txt | wc -l; exit 3");
    assert_eq!(code, 3, "{out}");
    assert_eq!(out.trim(), "1");
}

fn refuses_without_a_plan() {
    let out = Command::new(std::env::current_exe().unwrap()).arg(sandbox::FLAG).output().unwrap();
    assert_eq!(out.status.code(), Some(126));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no plan"));
}

fn shared_folders() {
    let rig = Rig::new("shared");
    if !rig.usable() {
        return;
    }
    let (code, out) = rig.run(&format!("cat {}", rig.outside.join("secret.txt").display()));
    assert!(refused(&out) && code != 0 || !out.contains("TOPSECRET"), "{out}");

    let read_only = [(rig.outside.as_path(), false)];
    let (_, out) =
        rig.run_with(&format!("cat {}", rig.outside.join("secret.txt").display()), &read_only);
    assert_eq!(out, "TOPSECRET", "a read only share must be readable: {out}");
    let target = rig.outside.join("new.txt");
    let (_, out) = rig.run_with(&format!("echo x > {}", target.display()), &read_only);
    assert!(refused(&out), "{out}");
    assert!(!target.exists(), "a read only share must not be writable");

    let writable = [(rig.outside.as_path(), true)];
    let (code, out) = rig.run_with(&format!("echo x > {}", target.display()), &writable);
    assert_eq!(code, 0, "{out}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "x\n");
}

fn a_share_stays_where_it_was_put() {
    let rig = Rig::new("neighbour");
    if !rig.usable() {
        return;
    }
    let shared = rig.root.join("shared");
    let neighbour = rig.root.join("neighbour");
    fs::create_dir_all(&shared).unwrap();
    fs::create_dir_all(&neighbour).unwrap();
    fs::write(shared.join("a.txt"), "A").unwrap();
    fs::write(neighbour.join("b.txt"), "B").unwrap();
    let share = [(shared.as_path(), true)];
    let (_, out) = rig.run_with(&format!("cat {}", shared.join("a.txt").display()), &share);
    assert_eq!(out, "A");
    let (_, out) = rig.run_with(&format!("cat {}", neighbour.join("b.txt").display()), &share);
    assert!(refused(&out) && !out.contains('B'), "{out}");
    let (_, out) = rig.run_with(&format!("ls {}", rig.root.display()), &share);
    assert!(refused(&out), "{out}");
    let (_, out) =
        rig.run_with(&format!("echo x > {}", rig.root.join("top.txt").display()), &share);
    assert!(refused(&out), "{out}");
}

fn names_resolve() {
    let rig = Rig::new("dns");
    if !rig.usable() {
        return;
    }
    if !network_is_up() {
        eprintln!("no network here, skipping");
        return;
    }
    let (_, out) =
        rig.run("curl -sS -m 10 -o /dev/null -w '%{http_code}' https://example.com 2>&1");
    assert_eq!(out.trim(), "200", "names must still resolve inside the sandbox: {out}");
}

fn plan_env_reaches_the_command() {
    let rig = Rig::new("planenv");
    if !rig.usable() {
        return;
    }
    let plan = serde_json::json!({
        "workspace": rig.workspace,
        "scratch": rig.scratch,
        "tool_dirs": [],
        "command": "echo $CARGO_HOME; echo ${SECRET_TOKEN:-none}",
        "shared": [],
        "env": [["CARGO_HOME", "/opt/cargo"]],
    })
    .to_string();
    let out = Command::new(std::env::current_exe().unwrap())
        .arg(sandbox::FLAG)
        .arg(plan)
        .env("SECRET_TOKEN", "leaked")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/opt/cargo\nnone\n");
}

fn a_language_server_runs_confined() {
    use agent_core::code::{Code, Settings};
    use agent_core::workspace::Workspace;
    use agent_core::{CancellationToken, runtime};
    if !sandbox::supported() {
        return;
    }
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("rust-analyzer").is_file()))
        .unwrap_or(false);
    if !found {
        eprintln!("rust-analyzer is not installed, skipping");
        return;
    }
    let rig = Rig::new("lsp");
    fs::write(
        rig.workspace.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(rig.workspace.join("src")).unwrap();
    fs::write(
        rig.workspace.join("src/lib.rs"),
        "pub struct Shape { pub size: f64 }\n\nimpl Shape {\n    pub fn area(&self) -> f64 { self.size * self.size }\n}\n\npub fn twice(s: &Shape) -> f64 { 2.0 * s.area() }\n",
    )
    .unwrap();
    let ws = Workspace::open(&rig.workspace).unwrap();
    let code = std::sync::Arc::new(Code::new(Settings::default()));
    let stop = CancellationToken::new();
    let out = runtime()
        .block_on(code.run("code_find", &serde_json::json!({"name": "Shape::area"}), &ws, &stop))
        .unwrap();
    assert_eq!(code.launches(), 1);
    assert!(out.contains("method Shape::area  src/lib.rs:4"), "{out}");
    assert!(out.contains("fn twice"), "the users are found too: {out}");
    let (_, home) = rig.run(&format!("ls {} 2>&1", std::env::var("HOME").unwrap_or_default()));
    assert!(refused(&home), "the home folder stays closed: {home}");
}

fn network_is_up() -> bool {
    std::net::ToSocketAddrs::to_socket_addrs(&("example.com", 443))
        .map(|mut a| a.next().is_some())
        .unwrap_or(false)
}

fn a_compiler_runs() {
    let rig = Rig::new("cc");
    if !rig.usable() {
        return;
    }
    if !Command::new("cc").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("no C compiler here, skipping");
        return;
    }
    fs::write(rig.workspace.join("a.c"), "int main(void) { return 7; }\n").unwrap();
    let (code, out) = rig.run("cc a.c -o a && ./a; echo rc=$?");
    assert!(out.contains("rc=7"), "a compiler must work inside the sandbox ({code}): {out}");
    let (_, out) = rig.run("ls /private/var/folders 2>&1 | head -1; cat /etc/passwd | head -0");
    #[cfg(target_os = "macos")]
    assert!(!out.contains("zz") && refused(&out), "the system temp folder stays closed: {out}");
    #[cfg(not(target_os = "macos"))]
    let _ = out;
}
