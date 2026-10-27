use crate::{
    Config, DEFAULT_LISTEN, home, home_file, saved_model, serve_config, store::Store, token,
};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about)]
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
pub enum Command {
    Serve,
    Token {
        #[arg(long)]
        rotate: bool,
    },
    Agents,
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
    /// Chat with an agent in the terminal.
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
        /// Use a server on another machine.
        #[arg(long, env = "AGENTBENCH_URL")]
        url: Option<String>,
        #[arg(long, env = "AGENTBENCH_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
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
    if let Some(Command::Chat { prompt, resume, agent, model, project, url, token }) = cli.command {
        let server = match (url, token) {
            (Some(url), Some(token)) => crate::tui::Server { url, token },
            (Some(_), None) => return Err("--url needs --token".into()),
            _ => crate::tui::local(Some(home))?,
        };
        let prompt = Some(prompt.join(" ")).filter(|p| !p.trim().is_empty());
        return crate::tui::run(
            server,
            crate::tui::Options { agent, model, project, resume, prompt },
        )
        .await;
    }
    let config = Config::load(&home_file(&home, "agentbench.toml", "foreman.toml"))?;
    let store = Store::open(&home_file(&home, "agentbench.db", "foreman.db")).map_err(|e| e.0)?;
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
            let code = crate::agent::code::Code::new(config.code.clone());
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
        Command::Chat { .. } => unreachable!("handled before the store opens"),
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
