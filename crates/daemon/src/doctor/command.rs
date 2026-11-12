//! The commands the doctor runs (`systemctl`, `gh`), each in a process group of its own, so none outlives its
//! deadline or the daemon: past its deadline, its whole group is killed, whatever its processes did with the pipes,
//! and once the daemon stops, every group still running is killed and no command starts (ADR 0132).
//!
//! It must not run anything the doctor did not ask for, or signal a process it did not start.

use std::io::{self, Read as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use runnerd_core::api;
use rustix::process::{Pid, Signal, kill_process_group};

/// The commands of this process: those running, and whether it stopped. Only its methods here write it.
pub(crate) struct Running(Mutex<Groups>);

struct Groups {
    /// The process groups of the commands running: each command's own, led by it.
    running: Vec<Pid>,
    /// The daemon stopped: no command starts.
    stopped: bool,
}

/// The daemon's commands.
pub(crate) static RUNNING: Running = Running::new();

impl Running {
    pub(crate) const fn new() -> Running {
        Running(Mutex::new(Groups {
            running: Vec::new(),
            stopped: false,
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Groups> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Kills every command running and what it started, and starts none after: their reads end at once.
    pub(crate) fn stop(&self) {
        let mut groups = self.lock();
        groups.stopped = true;
        for group in &groups.running {
            let _ = kill_process_group(*group, Signal::KILL);
        }
    }

    /// Starts `command` in a group of its own, unless the daemon stopped.
    fn spawn(&self, command: &mut Command) -> io::Result<(Child, Pid)> {
        let mut groups = self.lock();
        if groups.stopped {
            return Err(io::Error::other("runnerd is stopping"));
        }
        let child = command.process_group(0).spawn()?;
        let group = Pid::from_child(&child);
        groups.running.push(group);
        Ok((child, group))
    }

    fn ended(&self, group: Pid) {
        self.lock().running.retain(|running| *running != group);
    }
}

/// `command`'s output, as `Command::output` gives it, with no input; with its group killed past `deadline`, which
/// covers reading what it wrote too: a process it left behind holding its pipes does not hold the read.
pub(crate) fn output_within(
    command: &mut Command,
    deadline: Duration,
    running: &Running,
) -> io::Result<Output> {
    let until = Instant::now() + deadline;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, group) = running.spawn(command)?;
    let read = collect(&mut child, until);
    if !matches!(read, Some(Ok(_))) {
        // Before the leader is reaped, so the ID is still the group's.
        let _ = kill_process_group(group, Signal::KILL);
    }
    // Reaps the leader, if it was not; once killed, at once.
    let _ = child.wait();
    running.ended(group);
    read.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "it did not finish within {}",
                api::duration(deadline.as_secs())
            ),
        )
    })?
}

/// What `child` wrote and how it ended, or `None` when that did not all come by `until`.
fn collect(child: &mut Child, until: Instant) -> Option<io::Result<Output>> {
    // Read as it runs, so a full pipe never stops it.
    let (sender, read) = mpsc::channel();
    let pipes: [Option<Box<dyn io::Read + Send>>; 2] = [
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn io::Read + Send>),
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn io::Read + Send>),
    ];
    for (index, pipe) in pipes.into_iter().enumerate() {
        let sender = sender.clone();
        // Not joined: one whose writer outlives the deadline ends when the group is killed.
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            let _ = sender.send((index, bytes));
        });
    }
    let status: ExitStatus = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Some(Err(error)),
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut output = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    for _ in 0..2 {
        let (index, bytes) = read
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .ok()?;
        if index == 0 {
            output.stdout = bytes;
        } else {
            output.stderr = bytes;
        }
    }
    Some(Ok(output))
}
