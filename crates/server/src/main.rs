use agentbench_server::cli::{Cli, exit, logging, run};
use clap::Parser;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    logging();
    exit(run(Cli::parse()).await)
}
