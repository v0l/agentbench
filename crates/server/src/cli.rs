use crate::{Config, DEFAULT_LISTEN, home, serve_with, store::Store, token};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about)]
pub struct Cli {
    #[arg(long, env = "FOREMAN_HOME")]
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
    Add {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
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
            eprintln!("foreman: {e}");
            ExitCode::FAILURE
        }
    }
}

pub async fn run(cli: Cli) -> Result<(), String> {
    let home = cli.home.unwrap_or_else(home);
    let config = Config::load(&home.join("foreman.toml"))?;
    let store = Store::open(&home.join("foreman.db")).map_err(|e| e.0)?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
            tracing::info!("foreman {} in {}", env!("CARGO_PKG_VERSION"), home.display());
            let listen = cli
                .listen
                .or(config.listen)
                .unwrap_or_else(|| DEFAULT_LISTEN.parse().expect("default listen address"));
            let web = cli.web.or(config.web.clone());
            serve_with(store, config.agents(), listen, web, async {
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
            let p = crate::projects::add(&store, &path, name.as_deref()).map_err(|e| e.0)?;
            println!("{} {}", p.name, p.path);
            Ok(())
        }
        Command::Agents => {
            for a in config.agents() {
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
