use clap::Parser;

/// Command-line client for the runnerd daemon.
#[derive(Parser)]
#[command(version)]
struct Cli {}

fn main() {
    Cli::parse();
}
