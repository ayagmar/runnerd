//! The machine a daemon runs on, as its status and messages name it (ADR 0158), and how a title says it; and the
//! machine another runnerd's runner name says ran a job (ADR 0163). Owns the wording and that parse only: the daemon
//! decides the name once at start (`HostIdentity`), and the history keeps the runner's name as GitHub gave it.

use serde::{Deserialize, Serialize};

/// How an outcome of a job that ran elsewhere starts.
const RAN_ON: &str = "ran on ";

/// The most characters an outcome's label holds (`Outcome::label`): `ran on ` and the longest host name. A client
/// that lays outcomes out in a column needs no more room than this.
pub const OUTCOME_MAX: usize = RAN_ON.len() + crate::config::HOST_NAME_MAX;

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

    /// Where a job that ran elsewhere ran, as its outcome says it, in at most [`OUTCOME_MAX`] characters: `ran on
    /// skiff` for a runner another runnerd named ([`Machine::of_runner`]), else `ran on` the runner's name, its first
    /// characters and `…` when it is longer than a host name may be.
    pub fn ran_on(runner: &str) -> String {
        let room = OUTCOME_MAX - RAN_ON.len();
        match Machine::of_runner(runner) {
            Some(host) => format!("{RAN_ON}{host}"),
            None if runner.chars().count() <= room => format!("{RAN_ON}{runner}"),
            None => {
                let kept: String = runner.chars().take(room - 1).collect();
                format!("{RAN_ON}{}…", kept.trim_end())
            }
        }
    }

    /// A runner's name as GitHub gave it, as the history keeps it: plain text without escapes or control characters,
    /// each run of white space one space, cut as a title is; `None` when nothing is left.
    pub(crate) fn runner_text(name: &str) -> Option<String> {
        let spaced: String = name
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect();
        let plain = super::title(&spaced);
        let words: Vec<&str> = plain.split_whitespace().collect();
        (!words.is_empty()).then(|| words.join(" "))
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
        for (runner, host) in [
            ("runnerd-4f2a91c0-skiff-default-1-1763000000", Some("skiff")),
            (
                "runnerd-4f2a91c0-skiff-rust-big-12-1763000000",
                Some("skiff"),
            ),
            (
                "runnerd-4f2a91c0-a1b2c3d4e5f6g7h8i9j0-app-1-1",
                Some("a1b2c3d4e5f6g7h8i9j0"),
            ),
            // Not runnerd's.
            ("GitHub Actions 12", None),
            ("runnerd-4F2A91C0-skiff-default-1-1763000000", None),
            ("runnerd-4f2a91c-skiff-default-1-1763000000", None),
            ("runnerd-4f2a91c0-skiff-1-1763000000", None),
            ("runnerd-4f2a91c0-skiff-default-x-1763000000", None),
            ("runnerd-4f2a91c0-skiff-default-1-", None),
            ("runnerd-4f2a91c0--default-1-1", None),
            ("runnerd-4f2a91c0-Skiff-default-1-1", None),
            ("runnerd-4f2a91c0-a1b2c3d4e5f6g7h8i9j0k-app-1-1", None),
            ("runnerd-", None),
        ] {
            assert_eq!(Machine::of_runner(runner), host, "{runner:?}");
        }
    }

    #[test]
    fn a_job_that_ran_elsewhere_says_so_in_no_more_than_an_outcome_column_holds() {
        // Catches a runner name of 64 characters, which GitHub allows, pushing every other column of a table out.
        let longest_host = format!("runnerd-4f2a91c0-{}-app-1-1", "h".repeat(20));
        for (runner, said) in [
            (
                "runnerd-4f2a91c0-skiff-default-1-1763000000",
                "ran on skiff",
            ),
            (longest_host.as_str(), "ran on hhhhhhhhhhhhhhhhhhhh"),
            ("GitHub Actions 12", "ran on GitHub Actions 12"),
            ("exactly-twenty-chars", "ran on exactly-twenty-chars"),
            ("twenty-one-characters", "ran on twenty-one-characte…"),
            ("GitHub Actions 1000001234", "ran on GitHub Actions 1000…"),
            (
                "arc-prod-eu-west-1-linux-x64-runners-7b8c9d-k9x2p",
                "ran on arc-prod-eu-west-1-…",
            ),
        ] {
            assert_eq!(Machine::ran_on(runner), said, "{runner:?}");
            assert!(
                said.chars().count() <= super::OUTCOME_MAX,
                "{said:?} fits an outcome's column"
            );
        }
    }

    #[test]
    fn a_runner_name_is_kept_as_one_line_of_plain_text_or_not_at_all() {
        // Catches a tab or an escape that reaches a client's table, and a name of only white space or escapes that
        // reads as run on nothing instead of as its conclusion.
        for (name, kept) in [
            ("skiff", Some("skiff")),
            ("  ci\t1  ", Some("ci 1")),
            ("peer\u{1b}[31m-red", Some("peer-red")),
            ("a\nb", Some("a b")),
            ("   ", None),
            ("\t", None),
            ("\u{1b}[31m", None),
            ("", None),
        ] {
            assert_eq!(Machine::runner_text(name).as_deref(), kept, "{name:?}");
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
