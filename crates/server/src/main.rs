use clap::{Parser, Subcommand};
use foreman_server::{Config, DEFAULT_LISTEN, home, serve_with, store::Store, token};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long, env = "FOREMAN_HOME")]
    home: Option<PathBuf>,
    #[arg(long)]
    listen: Option<SocketAddr>,
    #[arg(long)]
    web: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
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

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("foreman: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let home = cli.home.unwrap_or_else(home);
    let config = Config::load(&home.join("foreman.toml"))?;
    let store = Store::open(&home.join("foreman.db")).map_err(|e| e.0)?;
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => {
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
            let p =
                foreman_server::projects::add(&store, &path, name.as_deref()).map_err(|e| e.0)?;
            println!("{} {}", p.name, p.path);
            Ok(())
        }
        Command::Agents => {
            for a in config.agents() {
                let info = a.info();
                println!(
                    "{:<10} {:<14} {}",
                    info.name,
                    if info.available { "installed" } else { "not installed" },
                    a.program().display()
                );
            }
            Ok(())
        }
    }
}
