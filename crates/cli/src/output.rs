//! The terminal: results on stdout, errors on stderr, and whether to colour. The only module allowed to print.
//! Colour only when stdout is a terminal and `NO_COLOR` is unset or empty (<https://no-color.org>).

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    reason = "this module is the binary's terminal output"
)]

use std::fmt::Display;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use crate::human::Paint;

/// Whether stdout is a terminal.
pub(crate) fn is_terminal() -> bool {
    std::io::stdout().is_terminal()
}

/// How to paint stdout.
pub(crate) fn paint() -> Paint {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    Paint {
        colour: is_terminal() && !no_color,
    }
}

/// Prints text as it is, adding no newline. Returns false once stdout is closed, as when piped into `head`:
/// streaming commands then stop, and a closed stdout is not an error.
pub(crate) fn text(text: &str) -> bool {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .is_ok()
}

/// Prints a line on stdout; false once stdout is closed.
pub(crate) fn line(line: impl Display) -> bool {
    text(&format!("{line}\n"))
}

/// Replaces the screen with `frame`: the cursor goes home, the frame is drawn, the rest is cleared.
pub(crate) fn redraw(frame: &str) -> bool {
    text(&format!("\x1b[H{}\x1b[J", frame.replace('\n', "\x1b[K\n")))
}

/// Writes bytes to stdout as they come, such as a shell's output; false once stdout is closed.
pub(crate) fn bytes(bytes: &[u8]) -> bool {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(bytes)
        .and_then(|()| stdout.flush())
        .is_ok()
}

/// The terminal's size as columns and rows, when stdout is one.
pub(crate) fn terminal_size() -> Option<(u16, u16)> {
    let size = rustix::termios::tcgetwinsize(std::io::stdout()).ok()?;
    (size.ws_col > 0 && size.ws_row > 0).then_some((size.ws_col, size.ws_row))
}

/// The keys typed on stdin, as they are typed, from a thread of their own: a blocked read there does not hold up
/// the program's exit.
pub(crate) fn stdin_chunks() -> tokio::sync::mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buffer = [0; 4096];
        loop {
            match std::io::Read::read(&mut stdin, &mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if sender.blocking_send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    receiver
}

/// Stdin in raw mode, while it is a terminal: every key goes to a shell as it is typed, Ctrl-C included. Dropping
/// it puts the terminal back as it was.
pub(crate) struct RawMode(Option<rustix::termios::Termios>);

/// Puts stdin in raw mode, when it is a terminal.
pub(crate) fn raw_mode() -> RawMode {
    let stdin = std::io::stdin();
    let Ok(before) = rustix::termios::tcgetattr(&stdin) else {
        return RawMode(None);
    };
    let mut raw = before.clone();
    raw.make_raw();
    match rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw) {
        Ok(()) => RawMode(Some(before)),
        Err(_) => RawMode(None),
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(before) = &self.0 {
            let _ = rustix::termios::tcsetattr(
                std::io::stdin(),
                rustix::termios::OptionalActions::Now,
                before,
            );
        }
    }
}

/// Prints an error on stderr and returns the exit code for it.
pub(crate) fn fail(error: &dyn Display) -> ExitCode {
    eprintln!("error: {error}");
    ExitCode::FAILURE
}

/// Takes over the terminal for `runnerctl tui`: raw mode and the alternate screen. [`restore`] gives it back; a
/// panic gives it back too.
#[cfg(feature = "tui")]
pub(crate) fn full_screen() -> std::io::Result<ratatui::DefaultTerminal> {
    ratatui::try_init()
}

/// Leaves raw mode and the alternate screen.
#[cfg(feature = "tui")]
pub(crate) fn restore() -> std::io::Result<()> {
    ratatui::try_restore()
}

/// Takes the terminal again after [`restore`], as when continued after Ctrl-Z. The new terminal has drawn nothing
/// yet, so its first frame draws every cell; unlike `Terminal::clear`, it does not ask the terminal where its
/// cursor is, an answer some terminals are slow to give.
#[cfg(feature = "tui")]
pub(crate) fn resume() -> std::io::Result<ratatui::DefaultTerminal> {
    use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
    enable_raw_mode()?;
    ratatui::crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;
    ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
}
