//! What the commands print to the terminal: results on stdout, warnings and errors on stderr. The only module
//! allowed to print; the daemon itself logs through `tracing`.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "this module is the binary's terminal output"
)]

use std::fmt::Display;
use std::process::ExitCode;

/// Prints a result on stdout.
pub(crate) fn line(text: impl Display) {
    println!("{text}");
}

/// Prints a warning on stderr.
pub(crate) fn warning(text: impl Display) {
    eprintln!("warning: {text}");
}

/// Prints an error on stderr and returns the exit code for it.
pub(crate) fn fail(error: &dyn Display) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::FAILURE
}
