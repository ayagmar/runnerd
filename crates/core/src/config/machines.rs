//! `machines.toml`: the other machines a client reaches over SSH, each by a name, its SSH target, the path of its
//! daemon's socket there and its machine id (ADR 0164). A client's own file, beside no daemon: it parses, checks and
//! edits the list and words what is wrong with it; it reads and writes no file and runs no `ssh`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{HOST_NAME_MAX, host_segment};

/// What `ssh` connects to: a host, `user@host`, a `Host` of the SSH config or an `ssh://` URI. Never starts with `-`,
/// which `ssh` would read as an option, and holds no space or control character.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SshTarget(String);

impl SshTarget {
    /// The target as `ssh` takes it.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SshTarget {
    type Error = String;

    fn try_from(text: String) -> Result<SshTarget, String> {
        if text.is_empty() {
            return Err("an SSH target cannot be empty".to_owned());
        }
        if text.starts_with('-') {
            return Err(format!(
                "{text}: an SSH target cannot start with `-`, which ssh would read as an option"
            ));
        }
        if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!(
                "{text:?}: an SSH target cannot hold a space or a control character"
            ));
        }
        Ok(SshTarget(text))
    }
}

impl std::str::FromStr for SshTarget {
    type Err = String;

    fn from_str(text: &str) -> Result<SshTarget, String> {
        SshTarget::try_from(text.to_owned())
    }
}

impl From<SshTarget> for String {
    fn from(target: SshTarget) -> String {
        target.0
    }
}

impl fmt::Display for SshTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One machine of `machines.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    /// What `--machine` names it by: lower-case letters and digits, as `[host] name` is.
    pub name: String,
    /// What `ssh` connects to.
    pub ssh: SshTarget,
    /// The daemon's socket on that machine, as `machine add` found it.
    pub socket: PathBuf,
    /// The machine's id, as its daemon's status said at `machine add` (`api::Machine::id`).
    pub id: String,
}

/// The machines of `machines.toml`, in the order they were added. Every name and every id is unique.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machines {
    #[serde(default, rename = "machine", skip_serializing_if = "Vec::is_empty")]
    machines: Vec<Machine>,
}

/// Whether `name` can name a machine: lower-case letters and digits, 1 to 20 of them, as `[host] name`.
pub fn check_name(name: &str) -> Result<(), String> {
    if !name.is_empty() && host_segment(name) == name {
        return Ok(());
    }
    let suggestion = host_segment(name);
    let hint = if suggestion.is_empty() {
        String::new()
    } else {
        format!(", such as {suggestion}")
    };
    Err(format!(
        "{name:?}: a machine's name is lower-case letters and digits, 1 to {HOST_NAME_MAX} of them{hint}"
    ))
}

/// Whether `socket` can be forwarded to: an absolute path without `:`, which `ssh -L` reads as a separator.
pub fn check_socket(socket: &Path) -> Result<(), String> {
    let text = socket.to_string_lossy();
    if !socket.is_absolute() {
        return Err(format!("{text}: a remote socket's path must be absolute"));
    }
    if text.contains(':') || text.chars().any(char::is_control) {
        return Err(format!(
            "{text}: a remote socket's path cannot hold `:` or a control character, which ssh -L cannot forward"
        ));
    }
    Ok(())
}

impl Machines {
    /// Reads `machines.toml`'s text and checks every machine, its name and socket, and that no two share a name or
    /// an id.
    pub fn parse(text: &str) -> Result<Machines, String> {
        let parsed: Machines = toml::from_str(text).map_err(|error| error.to_string())?;
        let mut machines = Machines::default();
        for machine in parsed.machines {
            check_name(&machine.name)?;
            check_socket(&machine.socket)?;
            machines.refuse_clash(&machine)?;
            machines.machines.push(machine);
        }
        Ok(machines)
    }

    /// The file's text, which [`Machines::parse`] reads back.
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string(self).map_err(|error| error.to_string())
    }

    /// Every machine, in the order they were added.
    pub fn all(&self) -> &[Machine] {
        &self.machines
    }

    /// The machine named `name`, or why there is none.
    pub fn get(&self, name: &str) -> Result<&Machine, String> {
        self.machines
            .iter()
            .find(|machine| machine.name == name)
            .ok_or_else(|| self.unknown(name))
    }

    /// Adds `machine`, unless its name or id is another's, or its id is `local`'s, the machine this client runs on.
    pub fn add(&mut self, machine: Machine, local: Option<&str>) -> Result<(), String> {
        check_name(&machine.name)?;
        check_socket(&machine.socket)?;
        if local == Some(machine.id.as_str()) {
            return Err(format!(
                "{} has machine id {}, the same as this machine's: it is this machine, or a clone of it whose \
                 /etc/machine-id was copied, and the two daemons would take each other's runners for their own",
                machine.ssh, machine.id
            ));
        }
        self.refuse_clash(&machine)?;
        self.machines.push(machine);
        Ok(())
    }

    /// Removes the machine named `name` and returns it, or says there is none.
    pub fn remove(&mut self, name: &str) -> Result<Machine, String> {
        let index = self
            .machines
            .iter()
            .position(|machine| machine.name == name)
            .ok_or_else(|| self.unknown(name))?;
        Ok(self.machines.remove(index))
    }

    fn refuse_clash(&self, machine: &Machine) -> Result<(), String> {
        for other in &self.machines {
            if other.name == machine.name {
                return Err(format!(
                    "a machine named {} is already added, reached with ssh {}; `runnerctl machine rm {}` first",
                    other.name, other.ssh, other.name
                ));
            }
            if other.id == machine.id {
                return Err(format!(
                    "{} has machine id {}, the same as {}: it is the same machine, or a clone whose \
                     /etc/machine-id was copied, and the two daemons would take each other's runners for their own",
                    machine.ssh, machine.id, other.name
                ));
            }
        }
        Ok(())
    }

    fn unknown(&self, name: &str) -> String {
        if self.machines.is_empty() {
            return format!(
                "no machine named {name}: none is added; `runnerctl machine add {name} --ssh <target>` adds one"
            );
        }
        let names: Vec<&str> = self
            .machines
            .iter()
            .map(|machine| machine.name.as_str())
            .collect();
        format!(
            "no machine named {name}; the machines are {}",
            names.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Machine, Machines, SshTarget, check_name};

    fn machine(name: &str, id: &str) -> Machine {
        Machine {
            name: name.to_owned(),
            ssh: format!("me@{name}").parse().unwrap(),
            socket: PathBuf::from("/run/user/1000/runnerd.sock"),
            id: id.to_owned(),
        }
    }

    /// Catches: a target such as `-oProxyCommand=…` reaching ssh's argument list as an option.
    #[test]
    fn a_target_that_ssh_would_read_as_an_option_is_refused() {
        for bad in ["-oProxyCommand=touch /tmp/x", "", "host name", "host\nname"] {
            assert!(
                bad.parse::<SshTarget>().is_err(),
                "{bad:?} is refused as a target"
            );
        }
        for good in ["skiff", "me@skiff.tail.ts.net", "ssh://me@skiff:2222"] {
            assert!(good.parse::<SshTarget>().is_ok(), "{good:?} is a target");
        }
        let file = "[[machine]]\nname = \"skiff\"\nssh = \"-oProxyCommand=x\"\nsocket = \"/s\"\nid = \"a\"\n";
        assert!(
            Machines::parse(file).is_err(),
            "a hand-edited file with such a target is refused too"
        );
    }

    /// Catches: two entries for one machine, or a clone sharing its machine id, being added side by side.
    #[test]
    fn a_name_or_an_id_already_added_and_this_machines_own_id_are_refused() {
        let mut machines = Machines::default();
        machines.add(machine("skiff", "1a2b3c4d"), None).unwrap();
        let same_name = machines
            .add(machine("skiff", "99999999"), None)
            .unwrap_err();
        assert!(same_name.contains("already added"), "{same_name}");
        let same_id = machines.add(machine("lab", "1a2b3c4d"), None).unwrap_err();
        assert!(same_id.contains("the same as skiff"), "{same_id}");
        let local = machines
            .add(machine("lab", "0f0f0f0f"), Some("0f0f0f0f"))
            .unwrap_err();
        assert!(local.contains("this machine's"), "{local}");
        assert_eq!(machines.all().len(), 1, "nothing refused was added");
        machines.add(machine("lab", "0f0f0f0f"), None).unwrap();
        assert_eq!(machines.all().len(), 2);
    }

    /// Catches: a file the client wrote that it cannot read back, or a hand edit that duplicates an id slipping in.
    #[test]
    fn the_file_reads_back_what_was_written_and_refuses_a_duplicate_by_hand() {
        let mut machines = Machines::default();
        machines.add(machine("skiff", "1a2b3c4d"), None).unwrap();
        machines.add(machine("lab", "0f0f0f0f"), None).unwrap();
        let text = machines.to_toml().unwrap();
        assert_eq!(Machines::parse(&text).unwrap(), machines);
        assert_eq!(Machines::parse("").unwrap(), Machines::default());
        let skiff = text.split("[[machine]]").nth(1).unwrap();
        let doubled = format!(
            "{text}[[machine]]{}",
            skiff.replace("\"skiff\"", "\"skiff2\"")
        );
        let error = Machines::parse(&doubled).unwrap_err();
        assert!(
            error.contains("the same as skiff"),
            "a copied entry with a new name keeps its id and is refused: {error}"
        );
        assert!(
            Machines::parse("[[machine]]\nname = \"skiff\"\nssh = \"skiff\"\nsocket = \"relative.sock\"\nid = \"a\"\n")
                .is_err(),
            "a relative socket path is refused"
        );
    }

    #[test]
    fn removing_or_reading_an_unknown_machine_names_the_known_ones() {
        let mut machines = Machines::default();
        let none = machines.get("skiff").unwrap_err();
        assert!(none.contains("machine add skiff"), "{none}");
        machines.add(machine("skiff", "1a2b3c4d"), None).unwrap();
        let unknown = machines.remove("lab").unwrap_err();
        assert!(unknown.contains("the machines are skiff"), "{unknown}");
        assert_eq!(machines.remove("skiff").unwrap().name, "skiff");
        assert!(machines.all().is_empty(), "skiff is gone");
    }

    #[test]
    fn a_name_is_letters_and_digits_and_a_bad_one_gets_a_suggestion() {
        assert!(check_name("skiff2").is_ok(), "skiff2 is a name");
        let error = check_name("Work-Laptop").unwrap_err();
        assert!(error.contains("such as worklaptop"), "{error}");
        assert!(check_name("").is_err(), "an empty name is refused");
    }
}
