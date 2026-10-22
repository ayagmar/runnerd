use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use runnerd_core::config::{ByteSize, Config, slice_memory_max};

mod doctor;

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
    /// Check that the host can run runners, and print the fix for what it is missing.
    Doctor {
        /// Also check this config against the slice it names.
        config: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Some(Command::CheckConfig { path, memory_max }) => check_config(&path, memory_max),
        Some(Command::Doctor { config }) => doctor(config.as_deref()),
        None => ExitCode::SUCCESS,
    }
}

fn check_config(path: &Path, memory_max: Option<ByteSize>) -> ExitCode {
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
    let checked = config
        .validate(memory_max)
        .and_then(|warnings| Ok((warnings, config.memory_caps()?)));
    match checked {
        Ok((warnings, total)) => {
            for warning in warnings {
                eprintln!("warning: {warning}");
            }
            println!("{}: valid, memory caps sum to {total}", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error),
    }
}

fn doctor(path: Option<&Path>) -> ExitCode {
    let config = match path.map(Config::load).transpose() {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };
    let host = doctor::Host::read(config.as_ref());
    let checks = doctor::checks(&host, config.as_ref());
    for check in &checks {
        println!("{check}");
    }
    if checks
        .iter()
        .any(|check| check.status == doctor::Status::Fail)
    {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn fail(error: &dyn std::fmt::Display) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::FAILURE
}
