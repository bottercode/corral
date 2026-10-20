use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = corral::cli::Cli::parse();
    match corral::cli::run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("corral: {e:#}");
            ExitCode::FAILURE
        }
    }
}
