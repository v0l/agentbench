use super::grants::{self, Decision};
use super::skillset;
use super::spawn::{self, Confine};
use super::tools::{self, number as num, text as pick};
use super::{Backend, Cx};
use serde_json::Value;
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT: u64 = 120;
const MAX_TIMEOUT: u64 = 900;
const OUTPUT_WAIT_MAX: u64 = 60;

pub const NAMES: [&str; 18] = [
    "bash",
    "bash_output",
    "bash_kill",
    "request_access",
    "load_skill",
    "list_files",
    "read_file",
    "write_file",
    "edit_file",
    "search_files",
    "workspace",
    "todo_write",
    "web_search",
    "web_fetch",
    "find_skills",
    "code_outline",
    "code_find",
    "code_diagnostics",
];

fn text(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(String::from)
}

fn number(args: &Value, key: &str) -> Option<usize> {
    args.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

fn need(args: &Value, key: &str) -> Result<String, String> {
    text(args, key).ok_or_else(|| format!("missing `{key}`"))
}

pub async fn run(
    backend: &dyn Backend,
    cx: &Cx,
    name: &str,
    args: &Value,
) -> Option<Result<String, String>> {
    if !NAMES.contains(&name) || backend.leave_out().contains(&name) {
        return backend.call(name, args, cx).await;
    }
    Some(Toolbox { backend, cx }.run(name, args).await)
}

pub fn job_owner(cx: &Cx) -> String {
    cx.workspace.root().display().to_string()
}

struct Toolbox<'a> {
    backend: &'a dyn Backend,
    cx: &'a Cx,
}

impl Toolbox<'_> {
    async fn run(&self, name: &str, args: &Value) -> Result<String, String> {
        let cx = self.cx;
        let services = &cx.services;
        match name {
            "workspace" => Ok(workspace_note(&cx.workspace)),
            "request_access" => self.request_access(args).await,
            "bash" if tools::flag(args, &["background"]).unwrap_or(false) => {
                let command = need(args, "command")?;
                let confine = self.confine().await;
                let handle =
                    services.jobs.start(&job_owner(cx), cx.workspace.root(), &command, &confine)?;
                Ok(format!(
                    "started in the background as {handle}. bash_output {handle} reads what it prints; bash_kill {handle} stops it."
                ))
            }
            "bash" => self.bash(args).await,
            "bash_output" => match pick(args, &["handle", "id"]) {
                None => Ok(services.jobs.list(&job_owner(cx))),
                Some(h) => {
                    let wait = num(args, &["wait_s", "wait"]).unwrap_or(0) as u64;
                    let wait = Duration::from_secs(wait.min(OUTPUT_WAIT_MAX));
                    services.jobs.output(&job_owner(cx), h, wait, &cx.stop).await
                }
            },
            "bash_kill" => {
                let h = pick(args, &["handle", "id"]).ok_or("missing `handle`")?;
                services.jobs.kill(&job_owner(cx), h).await
            }
            "todo_write" => super::todo::write(args),
            #[cfg(feature = "web")]
            "web_search" => super::web::search(args, &cx.stop).await,
            #[cfg(feature = "web")]
            "web_fetch" => {
                super::web::fetch(args, &cx.workspace, services.reader.as_deref(), &cx.stop).await
            }
            "find_skills" => super::skills::find(args, &cx.stop).await,
            "code_outline" | "code_find" | "code_diagnostics" => {
                services.code.run(name, args, &cx.workspace, &cx.stop).await
            }
            _ => self.file_tool(name, args).await,
        }
    }

    async fn file_tool(&self, name: &str, args: &Value) -> Result<String, String> {
        let (name, args, ws) = (name.to_string(), args.clone(), self.cx.workspace.clone());
        let preferred = self.backend.preferred_skills();
        tokio::task::spawn_blocking(move || match name.as_str() {
            "load_skill" => {
                let skills = skillset::discover(&skillset::roots(ws.root()), &preferred);
                skillset::load(
                    &skills,
                    text(&args, "name").as_deref(),
                    text(&args, "file").as_deref(),
                )
            }
            "list_files" => ws.list(text(&args, "path").as_deref(), number(&args, "depth")),
            "read_file" => need(&args, "path")
                .and_then(|p| ws.read(&p, number(&args, "offset"), number(&args, "limit"))),
            "write_file" => need(&args, "path")
                .and_then(|p| ws.write(&p, &text(&args, "content").unwrap_or_default())),
            "edit_file" => (|| {
                let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
                ws.edit(
                    &need(&args, "path")?,
                    &need(&args, "old")?,
                    &text(&args, "new").unwrap_or_default(),
                    all,
                )
            })(),
            "search_files" => {
                need(&args, "pattern").and_then(|p| ws.search(&p, text(&args, "path").as_deref()))
            }
            other => Err(format!("{other} is not a built-in tool")),
        })
        .await
        .map_err(|e| e.to_string())?
    }

    async fn request_access(&self, args: &Value) -> Result<String, String> {
        let given = need(args, "path")?;
        let write = args.get("write").and_then(Value::as_bool).unwrap_or(false);
        let reason = text(args, "reason").unwrap_or_default();
        if reason.trim().is_empty() {
            return Err("give a reason: the user reads it when deciding".into());
        }
        let ws = &self.cx.workspace;
        let real = grants::vet(&given, write, self.backend.config_dir().as_deref(), ws.root())?;
        if real.starts_with(ws.root()) {
            return Ok(format!("{} is inside the workspace already", real.display()));
        }
        if ws.grants().covers(&real, write) {
            return Ok(format!(
                "{} is already shared{}",
                real.display(),
                if write { " for writing" } else { "" }
            ));
        }
        let desk = self.cx.desk.clone();
        let ring = move || desk.ring();
        let decision = ws.grants().ask(real.clone(), write, &reason, &self.cx.stop, &ring).await?;
        match decision {
            Decision::Deny => Err(format!("the user said no to {}", real.display())),
            Decision::Read if write => Ok(format!(
                "the user shared {} read only: you can read it but not change it",
                real.display()
            )),
            Decision::Read => Ok(format!("the user shared {} read only", real.display())),
            Decision::Write => {
                Ok(format!("the user shared {} for reading and writing", real.display()))
            }
        }
    }

    async fn confine(&self) -> Confine {
        let tool_dirs = self.backend.tool_dirs(self.cx).await;
        let shared = self
            .cx
            .workspace
            .grants()
            .list()
            .into_iter()
            .map(|g| super::sandbox::Shared { path: g.path, write: g.write })
            .collect();
        Confine::Sandbox { tool_dirs, shared, env: Vec::new() }
    }

    async fn bash(&self, args: &Value) -> Result<String, String> {
        let command = need(args, "command")?;
        let timeout = args
            .get("timeout_s")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT)
            .clamp(1, MAX_TIMEOUT);
        let confine = self.confine().await;
        let prepared = spawn::prepare(&command, self.cx.workspace.root(), &confine)
            .map_err(|e| format!("bash is {e}"))?;
        let started = Instant::now();
        let out = spawn::run(prepared, None, Duration::from_secs(timeout), &self.cx.stop).await?;
        spawn::report(format!("$ {command}\n"), out, started.elapsed(), timeout)
    }
}

pub fn workspace_note(ws: &super::workspace::Workspace) -> String {
    let mut out = format!("The workspace is {}", ws.root().display());
    let shared = ws.grants().list();
    if shared.is_empty() {
        out.push_str(".\nNo other folder is shared. request_access asks the user to share one.");
        return out;
    }
    out.push_str(".\nThe user has also shared:");
    for g in shared {
        let how = if g.write { "read and write" } else { "read only" };
        out.push_str(&format!("\n- {} ({how})", g.path.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::{Path, PathBuf};

    struct Stub;

    #[crate::async_trait]
    impl crate::Backend for Stub {
        fn app(&self) -> &'static str {
            "egui-bench-test"
        }

        fn brief(&self, _: &Path, _: &[skillset::Skill]) -> String {
            String::new()
        }

        fn welcome(&self) -> crate::Welcome {
            crate::Welcome {
                kicker: String::new(),
                title: String::new(),
                text: String::new(),
                placeholder: String::new(),
            }
        }
    }

    struct Rig {
        base: PathBuf,
        ws: crate::workspace::Workspace,
        elsewhere: PathBuf,
        cx: Cx,
    }

    fn rig(name: &str) -> Rig {
        let base = std::env::temp_dir().join(format!("local-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("work")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        std::fs::write(base.join("elsewhere").join("lib.txt"), "library\n").unwrap();
        let (desk, _asks) = crate::Desk::new(|| {});
        let ws = crate::workspace::Workspace::open(&base.join("work")).unwrap();
        Rig {
            cx: Cx::new(ws.clone(), desk),
            ws,
            elsewhere: base.join("elsewhere").canonicalize().unwrap(),
            base,
        }
    }

    fn call(rig: &Rig, name: &str, args: Value) -> Result<String, String> {
        crate::runtime().block_on(run(&Stub, &rig.cx, name, &args)).unwrap()
    }

    fn answer(rig: &Rig, decision: Decision) -> std::thread::JoinHandle<()> {
        let grants = rig.ws.grants().clone();
        std::thread::spawn(move || {
            let id = loop {
                if let Some(r) = grants.pending().pop() {
                    break r.id;
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            grants.decide(id, decision);
        })
    }

    #[test]
    fn access_is_asked_for_and_then_the_file_tools_reach_the_folder() {
        let rig = rig("access");
        let lib = rig.elsewhere.join("lib.txt");
        let lib = lib.to_str().unwrap();
        assert!(
            call(&rig, "read_file", json!({ "path": lib })).unwrap_err().contains("request_access")
        );

        let user = answer(&rig, Decision::Read);
        let got = call(
            &rig,
            "request_access",
            json!({ "path": rig.elsewhere, "reason": "the parts library" }),
        );
        user.join().unwrap();
        assert!(got.unwrap().contains("read only"));
        assert_eq!(call(&rig, "read_file", json!({ "path": lib })).unwrap(), "library\n");
        assert!(call(&rig, "write_file", json!({ "path": lib, "content": "x" })).is_err());
        assert!(call(&rig, "workspace", json!({})).unwrap().contains("(read only)"));
        let _ = std::fs::remove_dir_all(&rig.base);
    }

    #[test]
    fn a_no_is_an_error_and_shares_nothing() {
        let rig = rig("deny");
        let user = answer(&rig, Decision::Deny);
        let got = call(
            &rig,
            "request_access",
            json!({ "path": rig.elsewhere, "reason": "the parts library" }),
        );
        user.join().unwrap();
        assert!(got.unwrap_err().contains("said no"));
        assert!(call(&rig, "workspace", json!({})).unwrap().contains("No other folder"));
        let _ = std::fs::remove_dir_all(&rig.base);
    }

    #[test]
    fn a_request_needs_a_reason_and_refuses_credentials_without_asking() {
        let rig = rig("refuse");
        let got = call(&rig, "request_access", json!({ "path": rig.elsewhere }));
        assert!(got.unwrap_err().contains("reason"));
        assert!(rig.ws.grants().pending().is_empty());
        let got = call(&rig, "request_access", json!({ "path": "/", "reason": "everything" }));
        assert!(got.unwrap_err().contains("too broad"));
        assert!(rig.ws.grants().pending().is_empty());
        let got = call(&rig, "request_access", json!({ "path": "nowhere-at-all", "reason": "x" }));
        assert!(got.is_err());
        let inside = call(&rig, "request_access", json!({ "path": "sub", "reason": "x" }));
        assert!(inside.is_err(), "a folder that does not exist cannot be shared");
        std::fs::create_dir_all(rig.base.join("work").join("sub")).unwrap();
        let inside = call(&rig, "request_access", json!({ "path": "sub", "reason": "x" }));
        assert!(inside.unwrap().contains("inside the workspace already"));
        let _ = std::fs::remove_dir_all(&rig.base);
    }

    #[test]
    fn asking_again_for_what_is_shared_does_not_ask_the_user() {
        let rig = rig("again");
        rig.ws.grants().grant(rig.elsewhere.clone(), true);
        let got = call(
            &rig,
            "request_access",
            json!({ "path": rig.elsewhere, "reason": "again", "write": true }),
        );
        assert!(got.unwrap().contains("already shared"));
        assert!(rig.ws.grants().pending().is_empty());
        let _ = std::fs::remove_dir_all(&rig.base);
    }
}

#[cfg(all(test, unix))]
mod jobs_and_more {
    use super::*;
    use serde_json::json;

    struct Open;

    #[crate::async_trait]
    impl Backend for Open {
        fn app(&self) -> &'static str {
            "egui-bench-test"
        }

        fn brief(&self, _: &std::path::Path, _: &[skillset::Skill]) -> String {
            String::new()
        }

        fn welcome(&self) -> crate::Welcome {
            crate::Welcome {
                kicker: String::new(),
                title: String::new(),
                text: String::new(),
                placeholder: String::new(),
            }
        }

        fn leave_out(&self) -> Vec<&'static str> {
            vec!["web_search"]
        }
    }

    fn cx() -> (Cx, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let ws = crate::workspace::Workspace::open(dir.path()).unwrap();
        let (desk, _asks) = crate::Desk::new(|| {});
        (Cx::new(ws, desk), dir)
    }

    #[tokio::test]
    async fn todo_write_answers_with_the_list() {
        let (cx, _d) = cx();
        let out = run(
            &Open,
            &cx,
            "todo_write",
            &json!({"items": [{"text": "a", "status": "in_progress"}]}),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(out, "[~] a");
    }

    #[tokio::test]
    async fn a_tool_the_app_leaves_out_goes_to_the_app_instead() {
        let (cx, _d) = cx();
        assert_eq!(run(&Open, &cx, "web_search", &json!({"query": "x"})).await, None);
    }

    #[tokio::test]
    async fn without_a_sandbox_bash_says_why() {
        if crate::sandbox::supported() {
            return;
        }
        let (cx, _d) = cx();
        let err = run(&Open, &cx, "bash", &json!({"command": "true"})).await.unwrap().unwrap_err();
        assert!(err.contains("no sandbox"), "{err}");
        let err = run(&Open, &cx, "bash", &json!({"command": "true", "background": true}))
            .await
            .unwrap()
            .unwrap_err();
        assert!(err.contains("no sandbox"), "{err}");
    }

    #[tokio::test]
    async fn bash_output_with_no_handle_lists_nothing_at_first() {
        let (cx, _d) = cx();
        let out = run(&Open, &cx, "bash_output", &json!({})).await.unwrap().unwrap();
        assert_eq!(out, "no background commands");
        assert!(run(&Open, &cx, "bash_kill", &json!({})).await.unwrap().is_err());
    }
}
