//! The machine a daemon runs on, as its status and messages name it (ADR 0158), and how a title says it. Owns the
//! wording only: the daemon decides the name once at start (`HostIdentity`).

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
