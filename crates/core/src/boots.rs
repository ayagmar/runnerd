//! What the host says about its boots: whether a past boot ended in an orderly shutdown whose final sync finished,
//! read from the systemd journal, and whether this host's journal keeps past boots for runnerd to read (ADR 0141).
//! It runs `journalctl`, within a deadline (`command`), and reads nothing else; it never writes, and never decides
//! what to empty.

use std::process::Command;
use std::time::Duration;

use crate::command;
use crate::config::PastBoots;

/// What `systemd-shutdown` logs before it syncs every filesystem, once every unit and so every container is stopped.
const SYNCING: &str = "Syncing filesystems and block devices.";

/// What `systemd-shutdown` logs once that sync returned, before it signals what is left and unmounts.
const SYNCED: &str = "Sending SIGTERM to remaining processes";

/// How long `journalctl` may take before its process group is killed and the journal reads as silent.
const JOURNAL_DEADLINE: Duration = Duration::from_secs(10);

/// What `journalctl` with `args` prints, one message a line; `None` when it fails, past `JOURNAL_DEADLINE`, or once
/// the daemon stops (`command::RUNNING`).
fn journal(args: &[&str]) -> Option<String> {
    let output = command::output_within(
        Command::new("journalctl")
            .args(["--no-pager", "--quiet", "--output=cat"])
            .args(args),
        JOURNAL_DEADLINE,
        &command::RUNNING,
    )
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether the boot `boot` (the kernel's boot ID, with or without dashes) ended in an orderly shutdown or reboot whose
/// final sync finished: the journal kept `systemd-shutdown`'s lines from both sides of it. `false` when the journal
/// does not say: a crash, a journal that keeps no past boot, one runnerd's user cannot read, or no `journalctl`.
pub fn ended_synced(boot: &str) -> bool {
    let Some(id) = journal_id(boot) else {
        return false;
    };
    journal(&[
        &format!("_BOOT_ID={id}"),
        "SYSLOG_IDENTIFIER=systemd-shutdown",
    ])
    .is_some_and(|log| shutdown_synced(&log))
}

/// A boot ID as the journal matches it: 32 hex digits, without the dashes `/proc` prints.
fn journal_id(boot: &str) -> Option<String> {
    let id: String = boot.chars().filter(|c| *c != '-').collect();
    (id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())).then_some(id)
}

/// Whether `systemd-shutdown`'s lines of one boot, oldest first, show its sync of every filesystem finished: the line
/// before it, then the one after, with no timeout between. A sync that timed out logs a line of its own, and goes on.
pub fn shutdown_synced(log: &str) -> bool {
    let mut lines = log.lines().map(str::trim);
    if !lines.any(|line| line == SYNCING) {
        return false;
    }
    for line in lines {
        if line.contains("timed out") {
            return false;
        }
        if line.starts_with(SYNCED) {
            return true;
        }
    }
    false
}

/// Whether this host's journal keeps past boots where runnerd can read them: a line of `systemd`'s from the boot
/// before this one. A user outside the `systemd-journal`, `adm` and `wheel` groups reads only their own entries, so
/// none of this boot's either; a journal kept in memory (`Storage=volatile`, or no `/var/log/journal`), or vacuumed
/// below one boot, has this boot's and not the last one's.
pub fn past_boots() -> PastBoots {
    let systemd_line =
        |boot: &str| journal(&[boot, "_PID=1", "--lines=1"]).is_some_and(|line| !line.is_empty());
    if systemd_line("--boot=-1") {
        PastBoots::Read
    } else if systemd_line("--boot=0") {
        PastBoots::NotKept
    } else {
        PastBoots::Unreadable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As this host's journal kept an orderly poweroff.
    const POWEROFF: &str = "Syncing filesystems and block devices.\nSending SIGTERM to remaining processes...\n\
                            Sending SIGKILL to remaining processes...\nUnmounting file systems.\n";

    #[test]
    fn an_orderly_shutdown_shows_its_sync_finished_and_nothing_else_does() {
        assert!(shutdown_synced(POWEROFF), "an orderly poweroff synced");
        for (log, why) in [
            ("", "a crash leaves no line of systemd-shutdown"),
            (
                "Syncing filesystems and block devices.\n",
                "a crash during the final sync leaves only the line before it",
            ),
            (
                "Syncing filesystems and block devices.\n\
                 Syncing filesystems and block devices - timed out, issuing SIGKILL to PID 4321.\n\
                 Sending SIGTERM to remaining processes...\n",
                "a sync that timed out did not finish",
            ),
            (
                "Sending SIGTERM to remaining processes...\n",
                "a signal with no sync before it says nothing of the sync",
            ),
        ] {
            assert!(!shutdown_synced(log), "{why}");
        }
    }

    #[test]
    fn a_boot_id_is_matched_as_the_journal_keeps_it_and_nothing_else_is() {
        assert_eq!(
            journal_id("36603faa-3828-48f0-adaf-04233699f072").as_deref(),
            Some("36603faa382848f0adaf04233699f072")
        );
        for bad in [
            "",
            "torn",
            "36603faa-3828",
            "_BOOT_ID=1 MESSAGE=x 36603faa3828",
        ] {
            assert_eq!(journal_id(bad), None, "{bad:?} is no boot ID");
        }
    }
}
