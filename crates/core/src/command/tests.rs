//! The commands runnerd runs, each in a process group of its own: what they wrote, their deadline, and a stop.

#![allow(clippy::unwrap_used, reason = "tests")]

use std::process::Command;

use super::{Running, output_within};
use crate::testdir::TestDir;

/// Whether the process `pid` runs: not gone, and not a zombie its new parent has yet to reap.
fn runs(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit(") ")
            .next()
            .is_some_and(|rest| !rest.starts_with('Z'))
    })
}

/// Waits up to 2 s for `pid` to end.
fn ends(pid: &str) -> bool {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while runs(pid) {
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    true
}

fn sh(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    command
}

#[test]
fn a_commands_output_is_read_as_it_wrote_it() {
    let running = Running::new();
    let output = output_within(
        &mut sh("echo out; echo err >&2; exit 3"),
        std::time::Duration::from_secs(10),
        &running,
    )
    .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stdout, b"out\n");
    assert_eq!(output.stderr, b"err\n");
}

#[test]
fn a_command_past_its_deadline_is_killed_with_what_it_started() {
    let dir = TestDir::new("command");
    let pid_file = dir.0.join("pid");
    let running = Running::new();
    let started = std::time::Instant::now();
    let read = output_within(
        &mut sh(&format!(
            "sleep 30 & echo $! > {}; sleep 30",
            pid_file.display()
        )),
        std::time::Duration::from_millis(300),
        &running,
    );
    assert_eq!(
        read.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut,
        "past its deadline"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    assert!(ends(pid.trim()), "what it started is killed too");
}

#[test]
fn a_command_that_ends_leaving_its_pipes_open_is_bounded_too() {
    let dir = TestDir::new("command");
    let pid_file = dir.0.join("pid");
    let running = Running::new();
    let started = std::time::Instant::now();
    // The shell ends at once; the sleep it leaves holds its stdout.
    let read = output_within(
        &mut sh(&format!(
            "sleep 30 & echo $! > {}; exit 0",
            pid_file.display()
        )),
        std::time::Duration::from_millis(300),
        &running,
    );
    assert_eq!(read.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "not until the pipe closes: {:?}",
        started.elapsed()
    );
    let pid = std::fs::read_to_string(&pid_file).unwrap();
    assert!(ends(pid.trim()), "the process holding the pipe is killed");
}

#[test]
fn a_stop_kills_the_commands_running_and_starts_none() {
    let running = std::sync::Arc::new(Running::new());
    let started = std::time::Instant::now();
    let reading = {
        let running = running.clone();
        std::thread::spawn(move || {
            output_within(
                &mut sh("sleep 30"),
                std::time::Duration::from_secs(60),
                &running,
            )
        })
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    running.stop();
    let stopped = reading.join().unwrap().unwrap();
    assert!(!stopped.status.success(), "killed: {stopped:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(
        output_within(
            &mut sh("true"),
            std::time::Duration::from_secs(10),
            &running
        )
        .is_err(),
        "no command starts once stopped"
    );
}
