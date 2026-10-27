#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn main() -> std::process::ExitCode {
    use agentbench_server::cli::{Cli, exit, logging, run};
    use clap::Parser;
    let cli = Cli::parse();
    if cli.command.is_none() {
        return match agentbench::run_native(cli.home) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agentbench: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    if !cli.interactive() {
        logging();
    }
    exit(agentbench::runtime().block_on(run(cli)))
}

#[cfg(any(target_arch = "wasm32", target_os = "android"))]
fn main() {}
