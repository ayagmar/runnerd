//! `runnerctl tui` in a pseudo-terminal against a fake daemon on a temporary socket: signals and Ctrl-Z give the
//! terminal back (cooked mode, main screen) before the process quits or stops, continuing takes it again and draws
//! the whole screen, and a terminal that goes away, as when an SSH connection drops, ends it.

#![cfg(feature = "tui")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking"
)]

use std::convert::Infallible;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::routing::get;
use futures_util::stream;
use runnerd_core::api::{Budget, HostSamples, JobPage, Status};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::process::{Pid, Signal, kill_process};
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, Winsize, tcgetattr, tcsetwinsize};

/// A fresh directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> TempDir {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "runnerctl-tui-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A daemon with nothing configured and nothing happening; its event stream stays open and quiet.
fn serve(socket: &Path) {
    let status = Status {
        version: "0.0.1".to_owned(),
        shutting_down: false,
        stopping: None,
        github_limited_until: None,
        budget: Budget {
            admission: runnerd_core::api::AdmissionPolicy::Caps,
            held: false,
            slice: "runnerd.slice".to_owned(),
            memory_max: None,
            memory_in_use: 0,
            pooled: None,
            counted: None,
            mirror_memory: 0,
            seed_memory: Vec::new(),
            shell_memory: Vec::new(),
            slice_cpus: None,
            host: None,
            cpu: None,
        },
        profiles: Vec::new(),
        repositories: Vec::new(),
        queued: Vec::new(),
    };
    let app = Router::new()
        .route(
            "/v1/status",
            get(move || {
                let status = status.clone();
                async move { axum::Json(status) }
            }),
        )
        .route(
            "/v1/jobs",
            get(|| async {
                axum::Json(JobPage {
                    jobs: Vec::new(),
                    next: None,
                })
            }),
        )
        .route(
            "/v1/host/samples",
            get(|| async {
                axum::Json(HostSamples {
                    interval: 2,
                    samples: Vec::new(),
                })
            }),
        )
        .route(
            "/v1/events",
            get(|| async { Body::from_stream(stream::pending::<Result<Bytes, Infallible>>()) }),
        );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let listener = runtime
        .block_on(async { tokio::net::UnixListener::bind(socket) })
        .unwrap();
    std::thread::spawn(move || {
        runtime.block_on(async { axum::serve(listener, app).await.unwrap() });
    });
}

/// How `runnerctl tui` holds its terminal.
#[derive(Clone, Copy)]
enum Session {
    /// In a process group of its own, as a shell runs a job, with the terminal as plain stdio: closing it sends no
    /// signal.
    Job,
    /// The leader of a session of its own whose controlling terminal it is, as a login shell over SSH: closing it
    /// hangs it up, and the kernel sends SIGHUP and SIGCONT.
    Leader,
}

/// `runnerctl tui` on a pseudo-terminal of 100×30.
struct Tui {
    child: Child,
    /// The terminal's other end; `None` once closed.
    master: Option<Arc<File>>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Tui {
    fn start(socket: &Path, session: Session) -> Tui {
        let master =
            openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        tcsetwinsize(
            &master,
            Winsize {
                ws_row: 30,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            },
        )
        .unwrap();
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .open(name.to_str().unwrap())
            .unwrap();
        let mut command = match session {
            Session::Job => {
                let mut command = Command::new(env!("CARGO_BIN_EXE_runnerctl"));
                command.process_group(0);
                command
            }
            // util-linux's setsid execs in place: the child it starts is runnerctl.
            Session::Leader => {
                let mut command = Command::new("setsid");
                command.arg("--ctty").arg(env!("CARGO_BIN_EXE_runnerctl"));
                command
            }
        };
        let child = command
            .arg("--socket")
            .arg(socket)
            .arg("tui")
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave))
            .spawn()
            .unwrap();
        let master = Arc::new(File::from(master));
        let output = Arc::new(Mutex::new(Vec::new()));
        // A weak hold, so that dropping `Tui::master` closes the terminal: the reader lets go within a poll.
        let (reader, read) = (Arc::downgrade(&master), output.clone());
        std::thread::spawn(move || read_all(&reader, &read));
        Tui {
            child,
            master: Some(master),
            output,
        }
    }

    fn master(&self) -> &File {
        self.master.as_deref().expect("the terminal is open")
    }

    /// Closes the terminal's other end, as sshd does when the connection drops.
    fn close(&mut self) {
        self.master = None;
    }

    /// Whether the terminal is in raw mode: no line editing, no echo.
    fn raw(&self) -> bool {
        let modes = tcgetattr(self.master()).unwrap().local_modes;
        !modes.intersects(LocalModes::ICANON | LocalModes::ECHO)
    }

    /// Whether the terminal is as a shell wants it: line editing and echo.
    fn cooked(&self) -> bool {
        let modes = tcgetattr(self.master()).unwrap().local_modes;
        modes.contains(LocalModes::ICANON | LocalModes::ECHO)
    }

    /// Whether the process is stopped, as after Ctrl-Z in a shell.
    fn stopped(&self) -> bool {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", self.child.id())).unwrap();
        let state = stat.rsplit_once(") ").unwrap().1;
        state.starts_with('T')
    }

    /// Everything written since `from`, and the new length.
    fn written_since(&self, from: usize) -> (String, usize) {
        let output = self.output.lock().unwrap();
        (
            String::from_utf8_lossy(&output[from.min(output.len())..]).into_owned(),
            output.len(),
        )
    }

    fn written(&self) -> usize {
        self.output.lock().unwrap().len()
    }

    fn signal(&self, signal: Signal) {
        kill_process(Pid::from_child(&self.child), signal).unwrap();
    }

    fn key(&self, bytes: &[u8]) {
        self.master().write_all(bytes).unwrap();
    }

    fn exited(&mut self) -> bool {
        self.child.try_wait().unwrap().is_some()
    }

    /// Whether it exited as `q` ends it; call once it exited.
    fn succeeded(&mut self) -> bool {
        self.child.try_wait().unwrap().unwrap().success()
    }
}

/// Copies what the terminal shows into `output` until the terminal is closed or its last reader is gone.
fn read_all(master: &Weak<File>, output: &Mutex<Vec<u8>>) {
    let mut buffer = [0; 4096];
    let timeout = Timespec::try_from(Duration::from_millis(20)).unwrap();
    while let Some(master) = master.upgrade() {
        let mut fds = [PollFd::new(&*master, PollFlags::IN)];
        if poll(&mut fds, Some(&timeout)).is_err() || fds[0].revents().is_empty() {
            continue;
        }
        match (&*master).read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => output.lock().unwrap().extend_from_slice(&buffer[..n]),
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Waits up to 10 s for `condition`.
fn wait(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Starts the TUI as a shell's job and waits until it drew its screen in raw mode.
fn started(dir: &TempDir) -> Tui {
    started_in(dir, Session::Job)
}

fn started_in(dir: &TempDir, session: Session) -> Tui {
    let socket = dir.0.join("runnerd.sock");
    serve(&socket);
    let tui = Tui::start(&socket, session);
    wait("the first screen", || {
        tui.raw() && tui.written_since(0).0.contains("Overview")
    });
    tui
}

/// Leaving the alternate screen, which a restored terminal must have done last.
const LEAVE: &str = "\x1b[?1049l";
const ENTER: &str = "\x1b[?1049h";

#[test]
fn sigterm_quits_and_gives_the_terminal_back() {
    let dir = TempDir::new();
    let mut tui = started(&dir);
    tui.signal(Signal::TERM);
    wait("runnerctl to exit", || tui.exited());
    assert!(tui.cooked(), "the terminal is left in raw mode");
    assert!(tui.written_since(0).0.contains(LEAVE));
}

#[test]
fn sigint_quits_and_gives_the_terminal_back() {
    let dir = TempDir::new();
    let mut tui = started(&dir);
    tui.signal(Signal::INT);
    wait("runnerctl to exit", || tui.exited());
    assert!(tui.cooked(), "the terminal is left in raw mode");
    assert!(tui.written_since(0).0.contains(LEAVE));
}

#[test]
fn ctrl_z_gives_the_terminal_back_and_continuing_draws_it_again() {
    let dir = TempDir::new();
    let mut tui = started(&dir);
    let before = tui.written();
    tui.key(b"\x1a");
    wait("runnerctl to stop", || tui.stopped());
    assert!(tui.cooked(), "stopped with the terminal in raw mode");
    assert!(tui.written_since(before).0.contains(LEAVE));

    let stopped_at = tui.written();
    tui.signal(Signal::CONT);
    wait("the screen drawn again", || {
        let (since, _) = tui.written_since(stopped_at);
        tui.raw() && since.contains(ENTER) && since.contains("Overview")
    });
    tui.key(b"q");
    wait("runnerctl to exit", || tui.exited());
    assert!(tui.cooked());
}

#[test]
fn sigtstp_gives_the_terminal_back_before_stopping() {
    let dir = TempDir::new();
    let mut tui = started(&dir);
    tui.signal(Signal::TSTP);
    wait("runnerctl to stop", || tui.stopped());
    assert!(tui.cooked(), "stopped with the terminal in raw mode");

    let stopped_at = tui.written();
    tui.signal(Signal::CONT);
    wait("the screen drawn again", || {
        let (since, _) = tui.written_since(stopped_at);
        tui.raw() && since.contains("Overview")
    });
    tui.signal(Signal::TERM);
    wait("runnerctl to exit", || tui.exited());
    assert!(tui.cooked());
}

#[test]
fn closing_the_terminal_ends_the_tui() {
    let dir = TempDir::new();
    let mut tui = started(&dir);
    tui.close();
    wait("runnerctl to exit", || tui.exited());
    assert!(tui.succeeded(), "a terminal gone ends the TUI as q does");
}

#[test]
fn a_hangup_ends_the_tui() {
    let dir = TempDir::new();
    let mut tui = started_in(&dir, Session::Leader);
    tui.close();
    wait("runnerctl to exit", || tui.exited());
    assert!(tui.succeeded(), "a terminal gone ends the TUI as q does");
}

#[test]
fn a_hangup_while_suspended_ends_the_tui_once_continued() {
    let dir = TempDir::new();
    let mut tui = started_in(&dir, Session::Leader);
    tui.key(b"\x1a");
    wait("runnerctl to stop", || tui.stopped());
    // The kernel sends the session's leader SIGHUP and then SIGCONT.
    tui.close();
    wait("runnerctl to exit", || tui.exited());
    assert!(tui.succeeded(), "a terminal gone ends the TUI as q does");
}
