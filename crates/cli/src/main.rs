//! `runnerctl`: the command-line client. It talks to the daemon's API only, never to Docker or GitHub.

use clap::Parser;

/// Command-line client for the runnerd daemon.
#[derive(Parser)]
#[command(version)]
struct Cli {}

fn main() {
    Cli::parse();
}
