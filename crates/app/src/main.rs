#[cfg(not(any(target_arch = "wasm32", target_os = "android")))]
fn main() -> std::process::ExitCode {
    use clap::Parser;
    use foreman_server::cli::{Cli, exit, logging, run};
    let cli = Cli::parse();
    if cli.command.is_none() {
        return match foreman::run_native(cli.home) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("foreman: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    logging();
    exit(foreman::runtime().block_on(run(cli)))
}

#[cfg(any(target_arch = "wasm32", target_os = "android"))]
fn main() {}
