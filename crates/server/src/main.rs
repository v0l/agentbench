use clap::Parser;
use foreman_server::cli::{Cli, exit, logging, run};
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    logging();
    exit(run(Cli::parse()).await)
}
