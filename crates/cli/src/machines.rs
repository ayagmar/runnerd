//! `runnerctl machine add|list|rm` and `--machine`: reads and writes `$XDG_CONFIG_HOME/runnerd/machines.toml`, and
//! opens a client of a machine it lists (ADR 0164). Core's `config::machines` checks the list and words what is
//! wrong; `client` runs the SSH forward. This module only finds the file, asks the daemons and prints.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use runnerd_core::api::Status;
use runnerd_core::client::{Client, Endpoint, OpenSsh, SshProgram, remote_socket};
use runnerd_core::config::machines::{Machine, Machines, SshTarget, check_name, check_socket};

use crate::{Error, output, print_json};

/// `runnerctl machine`'s subcommands.
#[derive(clap::Subcommand)]
#[allow(
    clippy::doc_markdown,
    reason = "clap prints these comments as --help, where backticks would show"
)]
pub(crate) enum MachineCommand {
    /// Add a machine to reach over SSH: reads its daemon's status once, through ssh with BatchMode, and keeps its
    /// SSH target, the daemon's socket there and its machine id. Refused when its machine id is this machine's or
    /// another added machine's.
    Add {
        /// What --machine calls it: lower-case letters and digits.
        name: String,
        /// What ssh connects to, such as skiff, me@skiff.example.ts.net or a Host of ~/.ssh/config.
        #[arg(long, value_name = "TARGET")]
        ssh: SshTarget,
        /// The daemon's socket on that machine [default: its user's $XDG_RUNTIME_DIR/runnerd.sock, asked over ssh].
        #[arg(long, value_name = "PATH")]
        remote_socket: Option<PathBuf>,
    },
    /// List the added machines.
    List,
    /// Forget a machine.
    Rm { name: String },
}

/// `$XDG_CONFIG_HOME/runnerd/machines.toml`, or under `~/.config`.
pub(crate) fn file() -> Result<PathBuf, Error> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))
        .map(|dir| dir.join("runnerd").join("machines.toml"))
        .ok_or_else(|| Error::Local("neither XDG_CONFIG_HOME nor HOME is set".to_owned()))
}

/// Where the forward's private directory goes: `$XDG_RUNTIME_DIR` when it is an absolute path without `:`, which
/// `ssh -L` would split, else the system's temporary directory.
pub(crate) fn private_base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && !dir.to_string_lossy().contains(':'))
        .unwrap_or_else(std::env::temp_dir)
}

/// The machines in `path`; none when it does not exist.
pub(crate) fn read(path: &Path) -> Result<Machines, Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Machines::parse(&text)
            .map_err(|error| Error::Local(format!("{}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Machines::default()),
        Err(error) => Err(Error::Local(format!(
            "cannot read {}: {error}",
            path.display()
        ))),
    }
}

/// Writes `machines` to `path` whole: a new file of this process renamed over the old, so a crash leaves one or the
/// other and two runnerctls never write one new file.
fn write(path: &Path, machines: &Machines) -> Result<(), Error> {
    let local =
        |error: std::io::Error| Error::Local(format!("cannot write {}: {error}", path.display()));
    let text = machines.to_toml().map_err(Error::Local)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(local)?;
    }
    let new = path.with_extension(format!("toml.{}.new", std::process::id()));
    std::fs::write(&new, text).map_err(local)?;
    std::fs::rename(&new, path).map_err(local)
}

/// A client of the machine named `name` in `path`.
pub(crate) async fn client(path: &Path, name: &str) -> Result<Client, Error> {
    let machines = read(path)?;
    let machine = machines.get(name).map_err(Error::Local)?.clone();
    Ok(Client::open(Endpoint::Ssh(machine), Arc::new(OpenSsh), &private_base()).await?)
}

/// `runnerctl machine …`. `local` is this machine's daemon, asked for its id so a machine that is this one is
/// refused; it need not be running.
pub(crate) async fn run(
    command: MachineCommand,
    json: bool,
    path: &Path,
    local: Option<&Client>,
    program: Arc<dyn SshProgram>,
    private: &Path,
) -> Result<ExitCode, Error> {
    match command {
        MachineCommand::Add {
            name,
            ssh,
            remote_socket,
        } => {
            let machine = add(path, name, ssh, remote_socket, local, program, private).await?;
            if json {
                return print_json(&machine);
            }
            output::line(format_args!(
                "added {} (ssh {}, machine id {}); `runnerctl --machine {} status` reads it",
                machine.name, machine.ssh, machine.id, machine.name
            ));
        }
        MachineCommand::List => {
            let machines = read(path)?;
            if json {
                return print_json(&machines.all());
            }
            if machines.all().is_empty() {
                output::line("no machines; `runnerctl machine add <name> --ssh <target>` adds one");
            }
            for machine in machines.all() {
                output::line(format_args!(
                    "{}  ssh {}  id {}  {}",
                    machine.name,
                    machine.ssh,
                    machine.id,
                    machine.socket.display()
                ));
            }
        }
        MachineCommand::Rm { name } => {
            let mut machines = read(path)?;
            let machine = machines.remove(&name).map_err(Error::Local)?;
            write(path, &machines)?;
            if !json {
                output::line(format_args!(
                    "removed {} (ssh {})",
                    machine.name, machine.ssh
                ));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn add(
    path: &Path,
    name: String,
    ssh: SshTarget,
    socket: Option<PathBuf>,
    local: Option<&Client>,
    program: Arc<dyn SshProgram>,
    private: &Path,
) -> Result<Machine, Error> {
    check_name(&name).map_err(Error::Local)?;
    // A name already taken is refused before ssh runs.
    if let Ok(other) = read(path)?.get(&name) {
        return Err(Error::Local(format!(
            "a machine named {name} is already added, reached with ssh {}; `runnerctl machine rm {name}` first",
            other.ssh
        )));
    }
    let socket = match socket {
        Some(socket) => {
            check_socket(&socket).map_err(Error::Local)?;
            socket
        }
        None => remote_socket(program.as_ref(), &ssh)?,
    };
    let mut machine = Machine {
        name,
        ssh,
        socket,
        id: String::new(),
    };
    let client = Client::open(Endpoint::Ssh(machine.clone()), program, private).await?;
    let status: Status = client.get("/v1/status").await?;
    drop(client);
    let Some(remote) = status.machine else {
        return Err(Error::Local(format!(
            "{} runs runnerd {}, which does not say its machine id; machine add needs runnerd 0.1.6 or later there",
            machine.ssh, status.version
        )));
    };
    machine.id = remote.id;
    let local_id = match local {
        Some(local) => local
            .get::<Status>("/v1/status")
            .await
            .ok()
            .and_then(|status| status.machine)
            .map(|machine| machine.id),
        None => None,
    };
    // Read again: another runnerctl may have changed the list while ssh ran.
    let mut machines = read(path)?;
    machines
        .add(machine.clone(), local_id.as_deref())
        .map_err(Error::Local)?;
    write(path, &machines)?;
    Ok(machine)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "tests: a fake that cannot start fails the test"
)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use axum::Router;
    use axum::routing::get;
    use runnerd_core::api::{Machine as Identity, Status};

    use super::{MachineCommand, read, run};
    use crate::testing::{Fake, FakeSsh};

    fn status(id: Option<&str>) -> Status {
        let mut status = runnerd_fixtures::daemon().status;
        status.machine = id.map(|id| Identity {
            id: id.to_owned(),
            name: "skiff".to_owned(),
        });
        status
    }

    fn daemon(id: Option<&str>) -> Router {
        let status = status(id);
        Router::new().route(
            "/v1/status",
            get(move || {
                let status = status.clone();
                async move { axum::Json(status) }
            }),
        )
    }

    fn add(name: &str) -> MachineCommand {
        MachineCommand::Add {
            name: name.to_owned(),
            ssh: "me@skiff".parse().unwrap(),
            remote_socket: None,
        }
    }

    async fn added(
        dir: &Path,
        remote: Option<&str>,
        local: Option<&str>,
        name: &str,
    ) -> Result<(), String> {
        let local = Fake::serve(daemon(local));
        let local = local.client(Duration::from_secs(5), Duration::from_secs(5));
        let ssh = FakeSsh::serving(daemon(remote), "/run/user/1001\n");
        run(
            add(name),
            true,
            &dir.join("machines.toml"),
            Some(&local),
            ssh,
            dir,
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    /// Catches: a second entry for one machine (or a clone sharing its machine id, or this machine itself) being
    /// kept, so two daemons that take each other's runners look like two machines.
    #[tokio::test]
    async fn add_keeps_the_remote_socket_and_id_and_refuses_one_already_known() {
        let dir = crate::testing::TempDir::new();
        added(&dir.0, Some("1a2b3c4d"), Some("0f0f0f0f"), "skiff")
            .await
            .unwrap();
        let machines = read(&dir.0.join("machines.toml")).unwrap();
        let skiff = machines.get("skiff").unwrap();
        assert_eq!(skiff.id, "1a2b3c4d");
        assert_eq!(skiff.socket, Path::new("/run/user/1001/runnerd.sock"));
        let again = added(&dir.0, Some("1a2b3c4d"), Some("0f0f0f0f"), "lab")
            .await
            .unwrap_err();
        assert!(again.contains("the same as skiff"), "{again}");
        let itself = added(&dir.0, Some("0f0f0f0f"), Some("0f0f0f0f"), "self")
            .await
            .unwrap_err();
        assert!(itself.contains("this machine's"), "{itself}");
        assert_eq!(
            read(&dir.0.join("machines.toml")).unwrap().all().len(),
            1,
            "nothing refused was kept"
        );
    }

    /// Catches: a machine added or removed by another runnerctl while this `add` waited on ssh, lost when this one
    /// writes back the list it read before.
    #[tokio::test]
    async fn add_keeps_a_machine_another_runnerctl_added_meanwhile() {
        let dir = crate::testing::TempDir::new();
        let path = dir.0.join("machines.toml");
        let meanwhile = path.clone();
        let status = status(Some("1a2b3c4d"));
        let remote = Router::new().route(
            "/v1/status",
            get(move || {
                let status = status.clone();
                let path = meanwhile.clone();
                async move {
                    std::fs::write(
                        &path,
                        "[[machine]]\nname = \"dory\"\nssh = \"dory\"\nsocket = \"/run/user/1002/runnerd.sock\"\nid = \"9f9f9f9f\"\n",
                    )
                    .unwrap();
                    axum::Json(status)
                }
            }),
        );
        let ssh = FakeSsh::serving(remote, "/run/user/1001\n");
        run(add("skiff"), true, &path, None, ssh, &dir.0)
            .await
            .unwrap();
        let machines = read(&path).unwrap();
        assert!(machines.get("dory").is_ok(), "dory is kept");
        assert!(machines.get("skiff").is_ok(), "skiff is added");
    }

    /// Catches: a 0.1.5 daemon, which names no machine id, being kept with an empty id no duplicate check can match.
    #[tokio::test]
    async fn add_refuses_a_daemon_that_names_no_machine_id() {
        let dir = crate::testing::TempDir::new();
        let error = added(&dir.0, None, None, "skiff").await.unwrap_err();
        assert!(error.contains("needs runnerd 0.1.6 or later"), "{error}");
        assert!(
            !dir.0.join("machines.toml").exists(),
            "nothing is written for a refused machine"
        );
    }
}
