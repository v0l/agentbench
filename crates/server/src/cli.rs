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
            let out = github(&store, &crate::github::gh_program(), action)?;
            println!("{}", serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?);
            Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::outbox::{Ask, propose};
    use crate::github::testing::Rig;
    use agentbench_core::{PostMode, Trust};
    use serde_json::json;

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
