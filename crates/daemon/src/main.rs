use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use runnerd_core::config::{ByteSize, Config, slice_memory_max};

/// Runs ephemeral GitHub Actions runners in containers within a host budget.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Validate a config file against the slice's memory budget.
    CheckConfig {
        path: PathBuf,
        /// Check against this MemoryMax (such as 33G) instead of reading it from systemd.
        #[arg(long)]
        memory_max: Option<ByteSize>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Some(Command::CheckConfig { path, memory_max }) => check_config(&path, memory_max),
        None => ExitCode::SUCCESS,
    }
}

fn check_config(path: &std::path::Path, memory_max: Option<ByteSize>) -> ExitCode {
    let config = match Config::load(path) {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let memory_max = match memory_max {
        Some(memory_max) => Some(memory_max),
        None => match slice_memory_max(&config.host.slice) {
            Ok(memory_max) => memory_max,
            Err(error) => return fail(&error),
        },
    };
    match config.validate(memory_max) {
        Ok(warnings) => {
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            println!(
                "{}: valid, memory caps sum to {}",
                path.display(),
                config.memory_caps()
            );
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error),
    }
}

fn fail(error: &dyn std::fmt::Display) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::FAILURE
}
