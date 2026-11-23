//! The machine a daemon runs on, as its status and messages name it (ADR 0158), and how a title says it; and the
//! machine another runnerd's runner name says ran a job (ADR 0163). Owns the wording and that parse only: the daemon
//! decides the name once at start (`HostIdentity`), and the history keeps the runner's name as GitHub gave it.

use serde::{Deserialize, Serialize};

/// A machine runnerd runs on, as its runner names carry it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// 8 hex digits from its machine ID, the part of a runner name that says whose runner it is: two machines whose
    /// runners share it take each other's runners for their own.
    pub id: String,
    /// `[host] name`, or its host name: lower-case letters and digits.
    pub name: String,
}

impl Machine {
    /// What a notification's title ends with to say which machine sent it, ` on tern`; empty without a machine or
    /// a name, so a client that shortens a title can keep it.
    pub fn notification_suffix(machine: Option<&Machine>) -> String {
        match machine {
            Some(machine) if !machine.name.is_empty() => format!(" on {}", machine.name),
            _ => String::new(),
        }
    }

    /// A notification's title, saying which machine sent it: `o/app: build failed on tern`.
    pub fn notification_title(machine: Option<&Machine>, title: &str) -> String {
        format!("{title}{}", Machine::notification_suffix(machine))
    }

    /// The machine a runner name of runnerd's carries: `skiff` in `runnerd-4f2a91c0-skiff-default-1-1763000000`, the
    /// segment after `runnerd-<8 hex digits>-` up to the next dash, which holds none since ADR 0158. `None` for a name of
    /// any other form, as a runner runnerd did not start names it. A runnerd older than ADR 0158 kept dashes in that
    /// segment, so a name of its from a host `my-laptop` reads as `my`.
    pub fn of_runner(runner: &str) -> Option<&str> {
        let rest = runner.strip_prefix("runnerd-")?;
        let (id, rest) = rest.split_once('-')?;
        if id.len() != 8
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        let (rest, secs) = rest.rsplit_once('-')?;
        let (rest, slot) = rest.rsplit_once('-')?;
        let (host, profile) = rest.split_once('-')?;
        let host_ok = (1..=crate::config::HOST_NAME_MAX).contains(&host.len())
            && host
                .bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase());
        (digits(secs) && digits(slot) && host_ok && !profile.is_empty()).then_some(host)
    }

    /// Where a job that ran elsewhere ran, as its outcome says it: `ran on skiff` for a runner another runnerd named
    /// ([`Machine::of_runner`]), else `ran on` the runner's name as GitHub gave it.
    pub fn ran_on(runner: &str) -> String {
        format!("ran on {}", Machine::of_runner(runner).unwrap_or(runner))
    }

    /// A dashboard page's title, saying which machine it shows: `History · runnerd on tern`.
    pub fn page_title(machine: Option<&Machine>, title: &str) -> String {
        match machine {
            Some(machine) if !machine.name.is_empty() => {
                format!("{title} · runnerd on {}", machine.name)
            }
            _ => format!("{title} · runnerd"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Machine;

    #[test]
    fn a_job_that_ran_elsewhere_names_the_machine_its_runner_name_carries() {
        // Catches a parse that takes a dashed profile's first word for the host, or reads a name that only looks like
        // runnerd's, so a job reads as run on a machine that does not exist.
        for (runner, said) in [
            (
                "runnerd-4f2a91c0-skiff-default-1-1763000000",
                "ran on skiff",
            ),
            (
                "runnerd-4f2a91c0-skiff-rust-big-12-1763000000",
                "ran on skiff",
            ),
            (
                "runnerd-4f2a91c0-a1b2c3d4e5f6g7h8i9j0-app-1-1",
                "ran on a1b2c3d4e5f6g7h8i9j0",
            ),
            // Not runnerd's: each reads as GitHub named it.
            (
                "GitHub Actions 1000001234",
                "ran on GitHub Actions 1000001234",
            ),
            (
                "runnerd-4F2A91C0-skiff-default-1-1763000000",
                "ran on runnerd-4F2A91C0-skiff-default-1-1763000000",
            ),
            (
                "runnerd-4f2a91c-skiff-default-1-1763000000",
                "ran on runnerd-4f2a91c-skiff-default-1-1763000000",
            ),
            (
                "runnerd-4f2a91c0-skiff-1-1763000000",
                "ran on runnerd-4f2a91c0-skiff-1-1763000000",
            ),
            (
                "runnerd-4f2a91c0-skiff-default-x-1763000000",
                "ran on runnerd-4f2a91c0-skiff-default-x-1763000000",
            ),
            (
                "runnerd-4f2a91c0-skiff-default-1-",
                "ran on runnerd-4f2a91c0-skiff-default-1-",
            ),
            (
                "runnerd-4f2a91c0--default-1-1",
                "ran on runnerd-4f2a91c0--default-1-1",
            ),
            (
                "runnerd-4f2a91c0-Skiff-default-1-1",
                "ran on runnerd-4f2a91c0-Skiff-default-1-1",
            ),
            (
                "runnerd-4f2a91c0-a1b2c3d4e5f6g7h8i9j0k-app-1-1",
                "ran on runnerd-4f2a91c0-a1b2c3d4e5f6g7h8i9j0k-app-1-1",
            ),
            ("runnerd-", "ran on runnerd-"),
        ] {
            assert_eq!(Machine::ran_on(runner), said, "{runner:?}");
        }
    }

    #[test]
    fn titles_name_the_machine_only_when_it_has_a_name() {
        // Catches "failed on " and "runnerd on " from a daemon, or JSON, whose machine has an empty name.
        let tern = Machine {
            id: "4f2a91c0".to_owned(),
            name: "tern".to_owned(),
        };
        let unnamed = Machine {
            name: String::new(),
            ..tern.clone()
        };
        assert_eq!(
            Machine::notification_title(Some(&tern), "o/app: build failed"),
            "o/app: build failed on tern"
        );
        assert_eq!(
            Machine::page_title(Some(&tern), "History"),
            "History · runnerd on tern"
        );
        for machine in [None, Some(&unnamed)] {
            assert_eq!(
                Machine::notification_title(machine, "o/app: build failed"),
                "o/app: build failed"
            );
            assert_eq!(Machine::page_title(machine, "History"), "History · runnerd");
        }
    }
}
