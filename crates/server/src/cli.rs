use crate::{Config, DEFAULT_LISTEN, home, saved_model, serve_config, store::Store, token};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

pub fn version_text() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| format!("{} (api {})", env!("CARGO_PKG_VERSION"), agentbench_core::API_VERSION))
}

#[derive(Parser)]
#[command(version = version_text(), about)]
pub struct Cli {
    #[arg(long)]
    pub home: Option<PathBuf>,
    #[arg(long)]
    pub listen: Option<SocketAddr>,
    #[arg(long)]
    pub web: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum GithubAction {
    /// The GitHub state of a project: gh, repo, sync, webhook and rate limit.
    Status {
        project: String,
    },
    /// Sync a project's issues and pull requests from GitHub now.
    Sync {
        project: String,
    },
    /// Stop everything in the outbox from being posted.
    Pause,
    /// Let the outbox post again.
    Resume,
    /// List the outbox rows, newest last.
    Outbox {
        #[arg(long)]
        project: Option<String>,
        /// Only rows with this status: proposed, approved, done, failed, rejected, local.
        #[arg(long)]
        status: Option<String>,
    },
    /// Approve one row (optionally with edited text) or every proposed row of a project.
    Approve {
        id: Option<i64>,
        #[arg(long)]
        text: Option<String>,
        #[arg(long)]
        all: Option<String>,
    },
    Reject {
        id: i64,
    },
    Pulls {
        #[arg(long)]
        project: String,
    },
    Review {
        pr: i64,
        #[arg(long)]
        project: String,
    },
    Merge {
        pr: i64,
        #[arg(long)]
        project: String,
    },
    /// Show or set up the webhook of a project. --secret makes a new secret and prints it once.
    Webhook {
        project: String,
        /// The public address of this server, such as https://bench.example.com.
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        secret: bool,
        /// Create or update the webhook on GitHub through gh (needs admin rights on the repo).
        #[arg(long)]
        create: bool,
    },
}

#[derive(Subcommand)]
pub enum IssueAction {
    New {
        title: String,
        #[arg(long)]
        project: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    Set {
        id: i64,
        #[arg(long)]
        model: String,
    },
    Levels {
        project: String,
        #[arg(long)]
        low: Option<String>,
        #[arg(long)]
        med: Option<String>,
        #[arg(long)]
        high: Option<String>,
    },
}

fn level_flag(text: &str) -> Result<Option<agentbench_core::ModelLevel>, String> {
    if text.trim().eq_ignore_ascii_case("default") || text.trim().is_empty() {
        return Ok(None);
    }
    agentbench_core::ModelLevel::parse(text).map(Some).ok_or_else(|| "--model is low, med, high or default".to_string())
}

#[derive(Subcommand)]
pub enum Command {
    Serve,
    #[command(about = "Show the autopilot loop state, with host pressure and the scaled worker cap.")]
    Autopilot,
    /// Create issues, set their model level, or show and set a project's level table.
    Issue {
        #[command(subcommand)]
        action: IssueAction,
    },
    Token {
        #[arg(long)]
        rotate: bool,
    },
    Agents,
    /// Restart the server once no turn is running, or once the drain limit passes.
    Restart {
        /// Seconds to wait for running turns; default the `restart_drain_secs` setting (120).
        #[arg(long)]
        drain_secs: Option<u64>,
    },
    /// Sync the open issues and pull requests of a GitHub project now.
    GithubSync {
        /// The project, by name.
        project: String,
    },
    /// Review and post what agents queued for GitHub.
    Github {
        #[command(subcommand)]
        action: GithubAction,
    },
    /// Compact the notes of one project (or the global notes) with the saved model.
    CompactMemory {
        /// The project, by name; leave out for the global notes.
        #[arg(long)]
        project: Option<String>,
        /// A model name or level to use instead of the saved one.
        #[arg(long)]
        model: Option<String>,
    },
    /// Show which languages are found in a folder, and whether a language server is installed.
    Code {
        /// The project folder, default the current one.
        path: Option<PathBuf>,
    },
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    /// Chat with an agent in the terminal. By default the chat runs in this process, in the
    /// current folder, and is kept nowhere; --server uses the tracked chats of the server.
    Chat {
        /// The first message; starts a chat at once.
        prompt: Vec<String>,
        /// Carry on with the most recent chat.
        #[arg(short = 'c', long = "continue")]
        resume: bool,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        model: Option<String>,
        /// The project to chat in, by name or path; default the home folder.
        #[arg(long)]
        project: Option<String>,
        /// With --server, use a server on another machine.
        #[arg(long, env = "AGENTBENCH_URL")]
        url: Option<String>,
        #[arg(long, env = "AGENTBENCH_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Chat through the agentbench server as a tracked session, starting it if needed.
        #[arg(long, alias = "tracked")]
        server: bool,
        /// Leave out the memory notes and the user's instruction files (standalone chats).
        #[arg(long)]
        no_memory: bool,
        /// Leave out the skills list and the skill tools (standalone chats).
        #[arg(long)]
        no_skills: bool,
        /// Leave out MCP servers (standalone chats).
        #[arg(long)]
        no_mcp: bool,
        /// Same as --no-memory --no-skills --no-mcp.
        #[arg(long)]
        bare: bool,
        /// Write a plain transcript under the data folder when the chat ends (standalone chats).
        #[arg(long)]
        save: bool,
    },
}

impl Cli {
    pub fn interactive(&self) -> bool {
        matches!(self.command, Some(Command::Chat { .. }))
    }
}

fn project_named(store: &Store, name: &str) -> Result<agentbench_core::Project, String> {
    let all = store.projects().map_err(|e| e.0)?;
    all.iter()
        .find(|p| p.name == name)
        .or_else(|| name.parse::<i64>().ok().and_then(|id| all.iter().find(|p| p.id == id)))
        .cloned()
        .ok_or_else(|| format!("no project named {name}"))
}

pub fn github(store: &Store, gh: &str, action: GithubAction) -> Result<serde_json::Value, String> {
    use crate::github::{outbox, webhook};
    use serde_json::json;
    let status = |p: &agentbench_core::Project| {
        let last = store.setting(&format!("github_last_sync_{}", p.id)).ok().flatten().and_then(|v| v.parse().ok());
        crate::github::status(store, gh, p, last)
    };
    Ok(match action {
        GithubAction::Status { project } => json!(status(&project_named(store, &project)?)),
        GithubAction::Sync { project } => {
            let p = project_named(store, &project)?;
            if !p.tracker.is_github() {
                return Err(format!("{} does not use GitHub", p.name));
            }
            let out = crate::github::sync_project(store, gh, p.id, crate::store::now(), true);
            if let crate::github::Outcome::Failed(e) = &out {
                return Err(e.clone());
            }
            json!({"synced": p.name, "outcome": format!("{out:?}").to_lowercase(), "status": status(&p)})
        }
        GithubAction::Pause => {
            outbox::set_paused(store, true)?;
            json!({"paused": true})
        }
        GithubAction::Resume => {
            outbox::set_paused(store, false)?;
            json!({"paused": false})
        }
        GithubAction::Outbox { project, status } => {
            let id = project.map(|n| project_named(store, &n).map(|p| p.id)).transpose()?;
            let items: Vec<_> = outbox::linked(store, id)
                .map_err(|e| e.0)?
                .into_iter()
                .filter(|o| status.as_deref().is_none_or(|s| o.status == s))
                .map(|o| {
                    let mut v = json!(o);
                    v["what"] = json!(o.describe());
                    v
                })
                .collect();
            json!({"paused": outbox::is_paused(store), "items": items})
        }
        GithubAction::Approve { id, text, all } => match (id, all) {
            (Some(id), _) => {
                outbox::approve(store, id, text.as_deref(), "user")?;
                json!({"approved": [id]})
            }
            (None, Some(name)) => json!({"approved": outbox::approve_all(store, project_named(store, &name)?.id, "user")?}),
            _ => return Err("give a row id or --all <project>".into()),
        },
        GithubAction::Reject { id } => {
            outbox::reject(store, id)?;
            json!({"rejected": id})
        }
        GithubAction::Pulls { project } => json!(store.pull_requests(project_named(store, &project)?.id).map_err(|e| e.0)?),
        GithubAction::Review { pr, project } => {
            let p = project_named(store, &project)?;
            store.pull_request(p.id, pr).map_err(|e| e.0)?.ok_or_else(|| format!("no mirrored pull request #{pr}"))?;
            store.ask_pr_review(p.id, pr).map_err(|e| e.0)?;
            json!({"review_asked": pr})
        }
        GithubAction::Merge { pr, project } => {
            let p = project_named(store, &project)?;
            let done = crate::github::merge::merge(store, gh, p.id, pr, crate::github::merge::OWNER)?;
            json!({"merged": pr, "result": done})
        }
        GithubAction::Webhook { project, base, secret, create } => {
            let p = project_named(store, &project)?;
            if let Some(b) = base {
                store.set_setting(webhook::BASE_SETTING, b.trim()).map_err(|e| e.0)?;
            }
            let mut out = json!({"url": webhook::url(store, p.id), "secret_set": webhook::secret(store, p.id).is_some(), "last_delivery": webhook::last_delivery(store, p.id)});
            if secret {
                let made = webhook::rotate(store, p.id)?;
                out["secret"] = json!(made.secret);
                out["secret_set"] = json!(true);
                out["note"] = json!("the secret is shown once; paste it into the webhook on GitHub or run with --create");
            }
            if create {
                out["created"] = webhook::create(store, gh, p.id)?;
            }
            out
        }
    })
}

pub enum Target {
    Remote(crate::tui::Server),
    Local(PathBuf),
}

pub fn resolve_target(home: &std::path::Path, url: Option<String>, token: Option<String>) -> Result<Target, String> {
    match (url.filter(|u| !u.trim().is_empty()), token.filter(|t| !t.trim().is_empty())) {
        (Some(url), Some(token)) => Ok(Target::Remote(crate::tui::Server { url, token })),
        (Some(url), None) => Err(format!("AGENTBENCH_URL is {url} but AGENTBENCH_TOKEN is not set")),
        (None, _) => {
            let local = crate::local::Local::open(Some(home.to_path_buf()))?;
            if local.running() {
                Ok(Target::Remote(crate::tui::Server { url: local.url(), token: local.token }))
            } else {
                Ok(Target::Local(home.to_path_buf()))
            }
        }
    }
}

async fn remote_call(
    server: &crate::tui::Server,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let mut req = crate::http::client()
        .request(method, format!("{}{path}", server.base()))
        .bearer_auth(&server.token)
        .timeout(std::time::Duration::from_secs(120));
    if let Some(b) = body {
        req = req.json(&b);
    }
    let r = req.send().await.map_err(|e| format!("{}: {e}", server.base()))?;
    let status = r.status();
    let text = r.text().await.map_err(|e| e.to_string())?;
    if status.is_success() {
        return Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::Null));
    }
    Err(serde_json::from_str::<agentbench_core::ApiError>(&text).map(|e| e.error).unwrap_or_else(|_| status.to_string()))
}

pub async fn github_remote(server: &crate::tui::Server, action: GithubAction) -> Result<serde_json::Value, String> {
    use reqwest::Method;
    const GET: Method = Method::GET;
    const POST: Method = Method::POST;
    const PUT: Method = Method::PUT;
    use serde_json::json;
    let project_id = |name: String| async move {
        let all = remote_call(server, GET, "/api/projects", None).await?;
        let all = all.as_array().cloned().unwrap_or_default();
        all.iter()
            .find(|p| p["name"].as_str() == Some(name.as_str()))
            .or_else(|| name.parse::<i64>().ok().and_then(|id| all.iter().find(|p| p["id"].as_i64() == Some(id))))
            .and_then(|p| p["id"].as_i64())
            .ok_or_else(|| format!("no project named {name}"))
    };
    let on = || eprintln!("on {}", server.base());
    Ok(match action {
        GithubAction::Status { project } => {
            let id = project_id(project).await?;
            remote_call(server, GET, &format!("/api/projects/{id}/github"), None).await?
        }
        GithubAction::Sync { project } => {
            let id = project_id(project.clone()).await?;
            let status = remote_call(server, POST, &format!("/api/projects/{id}/github/sync"), None).await?;
            json!({"synced": project, "outcome": "synced", "status": status})
        }
        GithubAction::Pause => {
            remote_call(server, POST, "/api/github/pause", Some(json!({"paused": true}))).await?;
            json!({"paused": true})
        }
        GithubAction::Resume => {
            remote_call(server, POST, "/api/github/pause", Some(json!({"paused": false}))).await?;
            json!({"paused": false})
        }
        GithubAction::Outbox { project, status } => {
            let path = match project {
                Some(n) => format!("/api/outbox?project={}", project_id(n).await?),
                None => "/api/outbox".to_string(),
            };
            let list = remote_call(server, GET, &path, None).await?;
            let items: Vec<_> = list["items"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|o| status.as_deref().is_none_or(|s| o["status"].as_str() == Some(s)))
                .map(|mut o| {
                    if let Ok(item) = serde_json::from_value::<agentbench_core::OutboxItem>(o.clone()) {
                        o["what"] = json!(item.describe());
                    }
                    o
                })
                .collect();
            json!({"paused": list["paused"].as_bool().unwrap_or(false), "items": items})
        }
        GithubAction::Approve { id, text, all } => match (id, all) {
            (Some(id), _) => {
                on();
                remote_call(server, POST, &format!("/api/outbox/{id}/approve"), Some(json!({"text": text}))).await?;
                json!({"approved": [id]})
            }
            (None, Some(name)) => {
                let id = project_id(name).await?;
                on();
                remote_call(server, POST, &format!("/api/projects/{id}/outbox/approve-all"), None).await?
            }
            _ => return Err("give a row id or --all <project>".into()),
        },
        GithubAction::Reject { id } => {
            on();
            remote_call(server, POST, &format!("/api/outbox/{id}/reject"), None).await?;
            json!({"rejected": id})
        }
        GithubAction::Pulls { project } => {
            let id = project_id(project).await?;
            remote_call(server, GET, &format!("/api/projects/{id}/pulls"), None).await?
        }
        GithubAction::Review { pr, project } => {
            let id = project_id(project).await?;
            remote_call(server, POST, &format!("/api/projects/{id}/pulls/{pr}/review"), None).await?
        }
        GithubAction::Merge { pr, project } => {
            let id = project_id(project).await?;
            on();
            remote_call(server, POST, &format!("/api/projects/{id}/pulls/{pr}/merge"), None).await?
        }
        GithubAction::Webhook { project, base, secret, create } => {
            let id = project_id(project).await?;
            if let Some(b) = base {
                remote_call(server, PUT, "/api/settings/github", Some(json!({"webhook_base": b.trim()}))).await?;
            }
            let status = remote_call(server, GET, &format!("/api/projects/{id}/github"), None).await?;
            let mut out = json!({"url": status["webhook_url"], "secret_set": status["webhook_secret_set"], "last_delivery": status["webhook_last"]});
            if secret {
                let made = remote_call(server, POST, &format!("/api/projects/{id}/github/webhook"), None).await?;
                out["url"] = made["url"].clone();
                out["secret"] = made["secret"].clone();
                out["secret_set"] = json!(true);
                out["note"] = json!("the secret is shown once; paste it into the webhook on GitHub or run with --create");
            }
            if create {
                out["created"] = remote_call(server, POST, &format!("/api/projects/{id}/github/webhook/create"), None).await?;
            }
            out
        }
    })
}

pub async fn github_command(
    home: &std::path::Path,
    url: Option<String>,
    token: Option<String>,
    action: GithubAction,
) -> Result<serde_json::Value, String> {
    match resolve_target(home, url, token)? {
        Target::Remote(server) => github_remote(&server, action).await,
        Target::Local(home) => {
            eprintln!("using the local store at {}", home.join("agentbench.db").display());
            let store = Store::open_with_memory(&home.join("agentbench.db"), crate::memory::default_root()).map_err(|e| e.0)?;
            github(&store, &crate::github::gh_program(), action)
        }
    }
}

pub async fn ask_restart(server: &crate::tui::Server, drain_secs: Option<u64>) -> Result<agentbench_core::Restarted, String> {
    let session = std::env::var("AGENTBENCH_SESSION").ok().and_then(|s| s.parse().ok());
    let ask = agentbench_core::RestartRequest { drain_secs, session };
    let patience = drain_secs.unwrap_or(crate::restart::DRAIN_MAX) + 60;
    let r = crate::http::client()
        .post(format!("{}/api/server/restart", server.base()))
        .bearer_auth(&server.token)
        .json(&ask)
        .timeout(std::time::Duration::from_secs(patience))
        .send()
        .await
        .map_err(|e| format!("{}: {e}", server.base()))?;
    let status = r.status();
    let text = r.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(serde_json::from_str::<agentbench_core::ApiError>(&text).map(|e| e.error).unwrap_or_else(|_| status.to_string()));
    }
    serde_json::from_str(&text).map_err(|e| format!("{e}: {text}"))
}

pub async fn restart_command(
    home: &std::path::Path,
    url: Option<String>,
    token: Option<String>,
    drain_secs: Option<u64>,
) -> Result<String, String> {
    let Target::Remote(server) = resolve_target(home, url, token)? else {
        return Err("the agentbench server is not running, so there is nothing to restart".into());
    };
    eprintln!("waiting for running turns to finish before restarting {}", server.base());
    let done = ask_restart(&server, drain_secs).await?;
    println!("{}", done.summary());
    let health = format!("{}/health", server.base());
    let gone = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < gone && crate::http::client().get(&health).send().await.is_ok() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let back = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while std::time::Instant::now() < back {
        if crate::http::client().get(&health).send().await.is_ok_and(|r| r.status().is_success()) {
            return Ok(format!("the server is back at {}", server.base()));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Err(format!("the server did not come back at {} within a minute", server.base()))
}

pub fn autopilot_text(a: &agentbench_core::AutopilotState) -> String {
    let mut out = format!("autopilot: {}", a.status_line());
    if let Some(p) = &a.pressure
        && p.enabled
    {
        out.push_str(&format!("\nhost pressure: {}", p.state.as_str()));
        if !p.reason.is_empty() {
            out.push_str(&format!(" ({})", p.reason));
        }
        out.push_str(&format!(", workers {} of {}", p.effective_cap, p.configured_cap));
        let r = &p.readings;
        if r.known {
            let pct = |v: Option<f32>| v.map_or("?".to_string(), |v| format!("{v:.0}%"));
            let gb = |v: Option<u64>| v.map_or("?".to_string(), |v| format!("{:.1} GB", v as f64 / 1073741824.0));
            out.push_str(&format!(
                "\nio full {}, memory some {}, cpu some {}, available {}, swap {}{}, load {} per core",
                pct(r.io_full10),
                pct(r.memory_some10),
                pct(r.cpu_some60),
                gb(r.mem_available),
                gb(r.swap_used),
                r.swap_device.as_ref().map(|d| format!(" on {d}")).unwrap_or_default(),
                r.load_per_core.map_or("?".to_string(), |v| format!("{v:.2}")),
            ));
            for d in &r.disks {
                out.push_str(&format!("\n{} ({}): writes {:.1} ms, queue {:.1} ms", d.device, d.path, d.write_ms, d.queue_ms));
            }
        } else {
            out.push_str("\nreadings unknown on this host");
        }
    }
    out
}

pub async fn autopilot_command(home: &std::path::Path, url: Option<String>, token: Option<String>) -> Result<Option<String>, String> {
    match resolve_target(home, url, token)? {
        Target::Remote(server) => {
            let v = remote_call(&server, reqwest::Method::GET, "/api/autopilot", None).await?;
            let a: agentbench_core::AutopilotState = serde_json::from_value(v).map_err(|e| e.to_string())?;
            Ok(Some(autopilot_text(&a)))
        }
        Target::Local(_) => Ok(None),
    }
}

pub fn logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
}

pub fn exit(result: Result<(), String>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentbench: {e}");
            ExitCode::FAILURE
        }
    }
}

pub async fn run(cli: Cli) -> Result<(), String> {
    let home = cli.home.unwrap_or_else(home);
    if let Some(Command::Chat {
        prompt,
        resume,
        agent,
        model,
        project,
        url,
        token,
        server,
        no_memory,
        no_skills,
        no_mcp,
        bare,
        save,
    }) = cli.command
    {
        let prompt = Some(prompt.join(" ")).filter(|p| !p.trim().is_empty());
        let tracked = server || resume || project.is_some() || agent.is_some();
        let opts = crate::tui::Options { agent, model, project, resume, prompt };
        if !tracked {
            let flags = crate::tui::Flags {
                memory: !(no_memory || bare),
                skills: !(no_skills || bare),
                mcp: !(no_mcp || bare),
                save,
            };
            return crate::tui::run_standalone(home, flags, opts).await;
        }
        let server = match (url, token) {
            (Some(url), Some(token)) => crate::tui::Server { url, token },
            (Some(_), None) => return Err("--url needs --token".into()),
            _ => crate::tui::local(Some(home))?,
        };
        return crate::tui::run(server, opts).await;
    }
    if let Some(Command::Github { .. }) = &cli.command {
        let Some(Command::Github { action }) = cli.command else { unreachable!() };
        let out = github_command(&home, std::env::var("AGENTBENCH_URL").ok(), std::env::var("AGENTBENCH_TOKEN").ok(), action).await?;
        println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
        return Ok(());
    }
    if let Some(Command::Autopilot) = &cli.command
        && let Some(out) = autopilot_command(&home, std::env::var("AGENTBENCH_URL").ok(), std::env::var("AGENTBENCH_TOKEN").ok()).await?
    {
        println!("{out}");
        return Ok(());
    }
    if let Some(Command::Restart { drain_secs }) = &cli.command {
        let out = restart_command(&home, std::env::var("AGENTBENCH_URL").ok(), std::env::var("AGENTBENCH_TOKEN").ok(), *drain_secs).await?;
        println!("{out}");
        return Ok(());
    }
    let config = Config::load(&home.join("agentbench.toml"))?;
    let store = Store::open_with_memory(&home.join("agentbench.db"), crate::memory::default_root()).map_err(|e| e.0)?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            tracing::info!("agentbench {} in {}", env!("CARGO_PKG_VERSION"), home.display());
            let listen = cli
                .listen
                .or(config.listen)
                .unwrap_or_else(|| DEFAULT_LISTEN.parse().expect("default listen address"));
            let web = cli.web.or(config.web.clone());
            let again = serve_config(store, &config, listen, web, crate::restart::terminated()).await?;
            if again {
                return Err(crate::restart::relaunch());
            }
            Ok(())
        }
        Command::Token { rotate } => {
            if rotate {
                store.delete_setting("token").map_err(|e| e.0)?;
            }
            println!("{}", token(&store).map_err(|e| e.0)?);
            Ok(())
        }
        Command::Add { path, name } => {
            let p = crate::projects::add(&store, &path, name.as_deref(), false).map_err(|e| e.0)?;
            println!("{} {}", p.name, p.path);
            Ok(())
        }
        Command::Code { path } => {
            let dir = path.unwrap_or_else(|| PathBuf::from("."));
            let dir = std::fs::canonicalize(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let code =
                crate::agent::code::Code::new(crate::agent::code::unconfined(config.code.clone()));
            let found = code.report(&dir);
            if found.is_empty() {
                println!("no language recognised in {}", dir.display());
            }
            for (spec, at, ok) in found {
                let at = at.strip_prefix(&dir).unwrap_or(&at);
                let at =
                    if at.as_os_str().is_empty() { ".".into() } else { at.display().to_string() };
                println!(
                    "{:<14} {:<28} {}",
                    spec.name,
                    at,
                    if ok {
                        format!("ready ({})", spec.command)
                    } else {
                        format!("missing: {}", spec.install)
                    }
                );
            }
            Ok(())
        }
        Command::CompactMemory { project, model: choice } => {
            let model = saved_model(&store, &config.model)
                .choose(match (&choice, crate::compact::settings(&store).model) {
                    (Some(c), _) => c.clone(),
                    (None, m) if !m.trim().is_empty() => m,
                    _ => "high".to_string(),
                }.as_str());
            let target = crate::titles::Target { url: model.url, model: model.model, key: model.key };
            let scopes = match project {
                Some(name) => {
                    let found = store
                        .projects()
                        .map_err(|e| e.0)?
                        .into_iter()
                        .find(|p| p.name == name)
                        .ok_or_else(|| format!("no project named {name}"))?;
                    crate::compact::scopes(&store, Some(found.id), false)
                }
                None => vec![(crate::memory::Scope::Global, crate::memory::GLOBAL.to_string())],
            };
            let done = crate::compact::run(&store, &target, scopes, true).await;
            println!("{}", serde_json::to_string_pretty(&done).map_err(|e| e.to_string())?);
            Ok(())
        }
        Command::GithubSync { project } => {
            let found = store
                .projects()
                .map_err(|e| e.0)?
                .into_iter()
                .find(|p| p.name == project)
                .ok_or_else(|| format!("no project named {project}"))?;
            let out = crate::github::sync_project(&store, &crate::github::gh_program(), found.id, crate::store::now(), true);
            println!("{out:?}");
            match out {
                crate::github::Outcome::Failed(e) => Err(e),
                _ => Ok(()),
            }
        }
        Command::Github { .. } => unreachable!("handled before the store opens"),
        Command::Chat { .. } => unreachable!("handled before the store opens"),
        Command::Restart { .. } => unreachable!("handled before the store opens"),
        Command::Autopilot => {
            println!("{}", autopilot_text(&crate::autopilot::state(&store)));
            Ok(())
        }
        Command::Issue { action } => {
            let find = |name: &str| -> Result<agentbench_core::Project, String> {
                store
                    .projects()
                    .map_err(|e| e.0)?
                    .into_iter()
                    .find(|p| p.name == name)
                    .ok_or_else(|| format!("no project named {name}"))
            };
            match action {
                IssueAction::New { title, project, body, model } => {
                    let level = model.as_deref().map(level_flag).transpose()?.flatten();
                    let i = store
                        .add_issue(&agentbench_core::NewIssue {
                            project_id: find(&project)?.id,
                            title,
                            body: body.unwrap_or_default(),
                            model_level: level,
                            ..Default::default()
                        })
                        .map_err(|e| e.0)?;
                    println!("#{} {} model {}", i.id, i.title, i.model_level.map_or("default", |l| l.as_str()));
                    Ok(())
                }
                IssueAction::Set { id, model } => {
                    let patch = agentbench_core::IssuePatch { model_level: Some(level_flag(&model)?), ..Default::default() };
                    let i = store.update_issue(id, &patch).map_err(|e| e.0)?;
                    println!("#{} model {}", i.id, i.model_level.map_or("default", |l| l.as_str()));
                    Ok(())
                }
                IssueAction::Levels { project, low, med, high } => {
                    let p = find(&project)?;
                    let mut table = p.model_levels.clone();
                    if let Some(v) = low {
                        table.low = v;
                    }
                    if let Some(v) = med {
                        table.med = v;
                    }
                    if let Some(v) = high {
                        table.high = v;
                    }
                    let p = store
                        .update_project(p.id, &agentbench_core::ProjectPatch { model_levels: Some(table), ..Default::default() })
                        .map_err(|e| e.0)?;
                    let shown = |m: &str, fallback: &str| if m.is_empty() { fallback.to_string() } else { m.to_string() };
                    println!("low  {}", shown(&p.model_levels.low, "(the server's low model)"));
                    println!("med  {}", shown(&p.model_levels.med, "(the default model)"));
                    println!("high {}", shown(&p.model_levels.high, "(the server's high model)"));
                    Ok(())
                }
            }
        }
        Command::Agents => {
            let model = crate::agent::native::Shared::new(saved_model(&store, &config.model));
            for a in config.agents_with(model) {
                let info = a.info();
                println!(
                    "{:<10} {:<14} {}",
                    info.name,
                    if info.available { "ready" } else { "not ready" },
                    a.about()
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::outbox::{Ask, propose};
    use crate::github::testing::Rig;
    use agentbench_core::{PostMode, Trust};
    use serde_json::json;

    #[test]
    fn the_autopilot_command_prints_the_scaled_cap_and_the_reason() {
        assert!(matches!(Cli::try_parse_from(["agentbench", "autopilot"]).unwrap().command, Some(Command::Autopilot)));
        let a = agentbench_core::AutopilotState {
            enabled: true,
            cap: 8,
            workers: 2,
            pressure: Some(agentbench_core::HostPressure {
                enabled: true,
                state: agentbench_core::PressureState::Pressure,
                reason: "io full 22%, swap 19 GB (swap on nvme1n1p1)".into(),
                effective_cap: 2,
                configured_cap: 8,
                readings: agentbench_core::PressureReadings { known: true, io_full10: Some(22.0), ..Default::default() },
                ..Default::default()
            }),
            ..Default::default()
        };
        let text = autopilot_text(&a);
        assert!(text.starts_with("autopilot: running, 2 of 8 workers; scaled to 2 of 8: io full 22%, swap 19 GB"), "{text}");
        assert!(text.contains("host pressure: pressure"), "{text}");
    }

    fn parse(args: &[&str]) -> GithubAction {
        let mut all = vec!["agentbench", "github"];
        all.extend_from_slice(args);
        match Cli::try_parse_from(all).unwrap().command {
            Some(Command::Github { action }) => action,
            _ => panic!("not a github command"),
        }
    }

    fn run(r: &Rig, args: &[&str]) -> Result<serde_json::Value, String> {
        github(&r.store, &r.gh, parse(args))
    }

    fn comment(r: &Rig, text: &str) -> i64 {
        let ask = Ask { project: r.project, kind: "comment".into(), number: 4, text: text.into(), params: json!({}), asked_by: "supervisor".into(), trusted: true };
        propose(&r.store, ask).unwrap().0
    }

    #[tokio::test]
    async fn reject_goes_to_the_server_named_by_the_url_not_the_local_store() {
        use axum::{Router, extract::Path, routing::post};
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::<i64>::new()));
        let seen = hits.clone();
        let app = Router::new().route(
            "/api/outbox/{id}/reject",
            post(move |Path(id): Path<i64>| async move {
                seen.lock().unwrap().push(id);
                axum::Json(json!({"rejected": id}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let home = tempfile::tempdir().unwrap();
        let local = Store::open_with_memory(&home.path().join("agentbench.db"), crate::memory::default_root()).unwrap();
        assert!(local.outbox_item(2).is_err());
        let out = github_command(home.path(), Some(url), Some("t".into()), GithubAction::Reject { id: 2 }).await.unwrap();
        assert_eq!(out, json!({"rejected": 2}));
        assert_eq!(*hits.lock().unwrap(), vec![2]);
    }

    #[tokio::test]
    async fn a_url_without_a_token_never_falls_back_to_the_local_store() {
        let home = tempfile::tempdir().unwrap();
        let e = github_command(home.path(), Some("http://127.0.0.1:1".into()), None, GithubAction::Reject { id: 2 }).await.unwrap_err();
        assert!(e.contains("AGENTBENCH_TOKEN"), "{e}");
        assert!(!home.path().join("agentbench.db").exists());
    }

    #[tokio::test]
    async fn approve_without_a_url_or_a_running_server_uses_the_local_store() {
        let home = tempfile::tempdir().unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        std::fs::write(home.path().join("agentbench.toml"), format!("listen = \"127.0.0.1:{port}\"\n")).unwrap();
        let store = Store::open_with_memory(&home.path().join("agentbench.db"), crate::memory::default_root()).unwrap();
        let p = store
            .add_project("demo", home.path().to_str().unwrap(), None, None)
            .unwrap();
        store
            .update_project(p.id, &agentbench_core::ProjectPatch { tracker: Some(agentbench_core::Tracker::Github { repo: "o/r".into() }), post_mode: Some(PostMode::Draft), ..Default::default() })
            .unwrap();
        let ask = Ask { project: p.id, kind: "comment".into(), number: 4, text: "hi".into(), params: json!({}), asked_by: "supervisor".into(), trusted: true };
        let id = propose(&store, ask).unwrap().0;
        drop(store);
        assert!(matches!(resolve_target(home.path(), None, None).unwrap(), Target::Local(_)));
        let out = github_command(home.path(), None, None, GithubAction::Approve { id: Some(id), text: None, all: None }).await.unwrap();
        assert_eq!(out, json!({"approved": [id]}));
    }

    #[test]
    fn every_github_subcommand_parses() {
        for args in [
            &["status", "demo"][..],
            &["sync", "demo"],
            &["pause"],
            &["resume"],
            &["outbox", "--project", "demo", "--status", "proposed"],
            &["approve", "3", "--text", "edited"],
            &["approve", "--all", "demo"],
            &["reject", "3"],
            &["pulls", "--project", "demo"],
            &["review", "9", "--project", "demo"],
            &["merge", "9", "--project", "demo"],
            &["webhook", "demo", "--base", "https://b.example", "--secret", "--create"],
        ] {
            parse(args);
        }
    }

    #[test]
    fn the_outbox_commands_answer_in_json_and_change_the_rows() {
        let r = Rig::new(PostMode::Draft, "");
        let a = comment(&r, "first");
        let b = comment(&r, "second");
        let c = comment(&r, "third");
        let listed = run(&r, &["outbox", "--project", "demo"]).unwrap();
        assert_eq!(listed["paused"], false);
        assert_eq!(listed["items"].as_array().unwrap().len(), 3);
        assert_eq!(listed["items"][0]["what"], "comment on #4");
        assert_eq!(run(&r, &["approve", &a.to_string(), "--text", "edited first"]).unwrap(), json!({"approved": [a]}));
        assert_eq!(r.store.outbox_item(a).unwrap().text, "edited first");
        assert_eq!(run(&r, &["reject", &b.to_string()]).unwrap(), json!({"rejected": b}));
        assert_eq!(run(&r, &["approve", "--all", "demo"]).unwrap(), json!({"approved": [c]}));
        let proposed = run(&r, &["outbox", "--status", "proposed"]).unwrap();
        assert!(proposed["items"].as_array().unwrap().is_empty());
        assert_eq!(run(&r, &["pause"]).unwrap(), json!({"paused": true}));
        assert!(crate::github::outbox::is_paused(&r.store));
        assert_eq!(crate::github::outbox::drain(&r.store, &r.gh, crate::store::now()), 0, "paused posts nothing");
        assert_eq!(run(&r, &["resume"]).unwrap(), json!({"paused": false}));
        assert_eq!(crate::github::outbox::drain(&r.store, &r.gh, crate::store::now()), 2);
        let sent = run(&r, &["outbox", "--status", "done"]).unwrap();
        let urls: Vec<&str> = sent["items"].as_array().unwrap().iter().filter_map(|i| i["url"].as_str()).collect();
        assert_eq!(urls, vec!["https://github.com/o/r/issues/4#issuecomment-1"; 2]);
        assert!(run(&r, &["approve"]).unwrap_err().contains("row id"));
        assert!(run(&r, &["outbox", "--project", "nope"]).unwrap_err().contains("no project named nope"));
    }

    #[test]
    fn status_reports_gh_missing_and_sync_refuses_an_internal_project() {
        let r = Rig::new(PostMode::Draft, "");
        let missing = github(&r.store, "/nonexistent/gh", parse(&["status", "demo"])).unwrap();
        assert_eq!(missing["problem"], "gh missing");
        assert_eq!(missing["repo"], "o/r");
        assert_eq!(missing["poll_secs"], 60);
        r.store.update_project(r.project, &agentbench_core::ProjectPatch { tracker: Some(agentbench_core::Tracker::Internal), ..Default::default() }).unwrap();
        assert!(run(&r, &["sync", "demo"]).unwrap_err().contains("does not use GitHub"));
    }

    #[test]
    fn pulls_review_and_merge_go_through_the_owner_rules() {
        let r = Rig::new(PostMode::Draft, "");
        r.pr(9, "kieran", Trust::Trusted, &r.head);
        let pulls = run(&r, &["pulls", "--project", "demo"]).unwrap();
        assert_eq!(pulls[0]["number"], 9);
        assert_eq!(run(&r, &["review", "9", "--project", "demo"]).unwrap(), json!({"review_asked": 9}));
        assert!(r.store.pull_request(r.project, 9).unwrap().unwrap().review_asked > 0);
        assert!(run(&r, &["review", "10", "--project", "demo"]).unwrap_err().contains("#10"));
        r.checks(9, &r.head, &["success"]);
        let e = run(&r, &["merge", "9", "--project", "demo"]).unwrap_err();
        assert!(e.contains("no review"), "{e}");
        assert!(!r.calls().iter().any(|c| c.starts_with("pr merge")));
    }

    #[test]
    fn webhook_shows_the_secret_once() {
        let r = Rig::new(PostMode::Draft, "");
        let made = run(&r, &["webhook", "demo", "--base", "https://bench.example.com", "--secret"]).unwrap();
        let secret = made["secret"].as_str().unwrap().to_string();
        assert_eq!(secret.len(), 64);
        assert_eq!(made["url"], format!("https://bench.example.com/api/github/webhook/{}", r.project));
        let later = run(&r, &["webhook", "demo"]).unwrap();
        assert_eq!(later["secret_set"], true);
        assert!(later.get("secret").is_none(), "the secret is not shown again");
        assert!(!later.to_string().contains(&secret));
    }
}
