use agentbench_server::cli::{Cli, exit, logging, run};
use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if !cli.interactive() {
        logging();
    }
    exit(run(cli).await)
}
