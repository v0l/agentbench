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
    Pause,
    Resume,
    Outbox {
        #[arg(long)]
        project: Option<String>,
    },
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
            serve_config(store, &config, listen, web, async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
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
        Command::Github { action } => {
            use crate::github::outbox;
            match action {
                GithubAction::Pause => outbox::set_paused(&store, true),
                GithubAction::Resume => outbox::set_paused(&store, false),
                GithubAction::Outbox { project } => {
                    let id = match project {
                        Some(name) => Some(store.projects().map_err(|e| e.0)?.into_iter().find(|p| p.name == name).ok_or_else(|| format!("no project named {name}"))?.id),
                        None => None,
                    };
                    for o in store.outbox(id).map_err(|e| e.0)? {
                        println!("{:<5} {:<9} {:<14} #{:<5} by {:<14} {}", o.id, o.status, o.kind, o.number, o.asked_by, o.text.replace('\n', " "));
                    }
                    Ok(())
                }
                GithubAction::Approve { id, text, all } => match (id, all) {
                    (Some(id), _) => outbox::approve(&store, id, text.as_deref(), "user"),
                    (None, Some(name)) => {
                        let p = store.projects().map_err(|e| e.0)?.into_iter().find(|p| p.name == name).ok_or_else(|| format!("no project named {name}"))?;
                        outbox::approve_all(&store, p.id, "user").map(|ids| println!("approved {}", ids.len()))
                    }
                    _ => Err("give a row id or --all <project>".into()),
                },
                GithubAction::Reject { id } => outbox::reject(&store, id),
            }
        }
        Command::Chat { .. } => unreachable!("handled before the store opens"),
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
