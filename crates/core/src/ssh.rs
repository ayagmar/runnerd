//! The SSH forward a client reaches another machine's daemon through (ADR 0164): one `ssh -N -L` per client, from a
//! socket in a private directory to the daemon's socket there, started again when it exits and stopped when the
//! client is dropped. Owns the `ssh` arguments, the private directory and the wording of what `ssh` reported; it
//! knows nothing of HTTP. Running `ssh` is [`SshProgram`], the boundary tests replace: no test runs the real one.

use std::ffi::OsString;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::client::ClientError;
use crate::config::machines::{Machine, SshTarget, check_socket};

/// How long `ssh` gets to sign in and set up the forward. Its own `ConnectTimeout` is shorter, so a host that does
/// not answer is reported by `ssh` itself.
const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// How often the forward's socket is looked for while `ssh` sets it up.
const POLL: Duration = Duration::from_millis(25);

/// Options every `ssh` here runs with: no terminal, no prompt (a password, a passphrase, a host key or a browser
/// check fails instead of hanging), and none of the owner's connection sharing, so their config cannot change how
/// this child behaves or outlives it.
const OPTIONS: [&str; 9] = [
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ControlPath=none",
    "-o",
    "ControlMaster=no",
    "-o",
    "ConnectTimeout=10",
];

/// The remote command `machine add` runs to find the daemon's socket: its user's runtime directory, as `runnerd
/// serve` takes it, or where systemd puts it.
const RUNTIME_DIR: &str = r#"printf '%s\n' "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}""#;

/// Runs `ssh`. [`OpenSsh`] runs the real one; tests give a fake.
pub trait SshProgram: fmt::Debug + Send + Sync {
    /// Starts `ssh args…` in the background with its standard error into `log` and nothing on its standard input or
    /// output.
    fn start(&self, args: &[OsString], log: File) -> io::Result<Box<dyn Forwarding>>;

    /// Runs `ssh args…` to its end and returns what it printed.
    fn run(&self, args: &[OsString]) -> io::Result<SshOutput>;
}

/// An `ssh` started by [`SshProgram::start`].
pub trait Forwarding: fmt::Debug + Send {
    /// Whether it has exited, without waiting.
    fn exited(&mut self) -> io::Result<bool>;

    /// Kills it and waits for it, if it still runs.
    fn stop(&mut self);
}

/// What an `ssh` that ran to its end printed.
#[derive(Debug, Clone, Default)]
pub struct SshOutput {
    /// Whether it exited with 0.
    pub success: bool,
    /// Its standard output.
    pub stdout: String,
    /// Its standard error.
    pub stderr: String,
}

/// The `ssh` on `PATH`.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenSsh;

impl SshProgram for OpenSsh {
    fn start(&self, args: &[OsString], log: File) -> io::Result<Box<dyn Forwarding>> {
        // Same process group as runnerctl, so a Ctrl-C that ends it ends the forward too.
        let child = Command::new("ssh")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(not_found)?;
        Ok(Box::new(SshChild(child)))
    }

    fn run(&self, args: &[OsString]) -> io::Result<SshOutput> {
        let output = Command::new("ssh")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(not_found)?;
        Ok(SshOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn not_found(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound {
        io::Error::new(
            io::ErrorKind::NotFound,
            "ssh is not on PATH; install OpenSSH's client",
        )
    } else {
        error
    }
}

#[derive(Debug)]
struct SshChild(Child);

impl Forwarding for SshChild {
    fn exited(&mut self) -> io::Result<bool> {
        Ok(self.0.try_wait()?.is_some())
    }

    fn stop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

/// `ssh`'s arguments to forward `local` to `remote` on `target` and do nothing else. The forward fails rather than
/// half-starts when it cannot be set up, and replaces a socket a killed `ssh` left at `local`.
pub(crate) fn forward_args(local: &Path, remote: &Path, target: &SshTarget) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-N".into()];
    args.extend(OPTIONS.iter().map(OsString::from));
    for option in [
        "StreamLocalBindUnlink=yes",
        "ExitOnForwardFailure=yes",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
    ] {
        args.extend(["-o".into(), option.into()]);
    }
    let mut forward = local.as_os_str().to_owned();
    forward.push(":");
    forward.push(remote.as_os_str());
    args.extend(["-L".into(), forward, "--".into(), target.as_str().into()]);
    args
}

/// `ssh`'s arguments to run `command` on `target`.
fn command_args(target: &SshTarget, command: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = OPTIONS.iter().map(OsString::from).collect();
    args.extend(["--".into(), target.as_str().into(), command.into()]);
    args
}

/// Where the daemon on `target` listens, from its user's runtime directory there.
pub fn remote_socket(program: &dyn SshProgram, target: &SshTarget) -> Result<PathBuf, ClientError> {
    let ran = program
        .run(&command_args(target, RUNTIME_DIR))
        .map_err(|error| unreachable(target.as_str(), error.to_string()))?;
    if !ran.success {
        return Err(failed(
            target.as_str(),
            target,
            &ran.stderr,
            Failure::Exited,
        ));
    }
    let dir = ran.stdout.trim();
    let socket = Path::new(dir).join("runnerd.sock");
    if dir.is_empty() || dir.contains('\n') {
        return Err(unreachable(
            target.as_str(),
            format!(
                "its shell did not say where its runtime directory is (it printed {dir:?}); name the daemon's \
                 socket with --remote-socket"
            ),
        ));
    }
    check_socket(&socket).map_err(|error| unreachable(target.as_str(), error))?;
    Ok(socket)
}

fn unreachable(machine: &str, reason: String) -> ClientError {
    ClientError::Unreachable {
        machine: machine.to_owned(),
        reason,
    }
}

/// How an `ssh` failed.
#[derive(Debug, Clone, Copy)]
enum Failure {
    /// It exited.
    Exited,
    /// It ran past [`OPEN_TIMEOUT`] without setting the forward up.
    TimedOut,
}

/// What `ssh` failing for `machine` means, from what it wrote to `stderr`. One that needed a person (a password, a
/// passphrase, a host key or Tailscale's browser check) says so, since `BatchMode` stops it from asking.
fn failed(machine: &str, target: &SshTarget, stderr: &str, how: Failure) -> ClientError {
    let said = stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("");
    let lower = stderr.to_lowercase();
    let needs_you = [
        "permission denied",
        "host key verification failed",
        "identification has changed",
        "passphrase",
        "password",
        "tailscale",
        "login.",
    ]
    .iter()
    .any(|clue| lower.contains(clue));
    let what = match how {
        Failure::Exited => format!("ssh {target} failed"),
        Failure::TimedOut => format!(
            "ssh {target} did not set up the forward within {}s",
            OPEN_TIMEOUT.as_secs()
        ),
    };
    let said = if said.is_empty() {
        String::new()
    } else {
        format!(" (ssh said: {said})")
    };
    let reason = if needs_you {
        format!(
            "{what}: it needs you to sign in, and runnerctl runs it with BatchMode=yes, which cannot ask for a \
             password, a passphrase, a host key or a browser check; run `ssh {target}` once yourself, then try \
             again{said}"
        )
    } else {
        format!("{what}{said}")
    };
    unreachable(machine, reason)
}

/// A directory only this user can enter, for the forward's socket and `ssh`'s log: created, never reused, so no
/// other user can plant a socket or a link in it.
fn private_dir(base: &Path) -> io::Result<PathBuf> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = base.join(format!("runnerctl-{}-{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && n < 1000 => {}
            Err(error) => return Err(error),
        }
    }
}

/// A forward to one machine's daemon, for one client and its clones. Dropping the last stops `ssh` and removes the
/// directory. Nothing of it survives the client: a new one starts its own.
#[derive(Debug)]
pub(crate) struct Tunnel {
    machine: Machine,
    program: Arc<dyn SshProgram>,
    /// The private directory: the forward's socket and `ssh`'s log.
    dir: PathBuf,
    /// The `ssh` that forwards, or the last one, which may have exited.
    forward: Mutex<Box<dyn Forwarding>>,
}

impl Tunnel {
    /// Starts `ssh` forwarding a socket in a new private directory under `base` to `machine`'s daemon, and waits
    /// until the forward is set up.
    pub(crate) async fn open(
        machine: Machine,
        program: Arc<dyn SshProgram>,
        base: &Path,
    ) -> Result<Tunnel, ClientError> {
        let dir = private_dir(base).map_err(|error| {
            unreachable(
                &machine.name,
                format!(
                    "cannot make a private directory in {}: {error}",
                    base.display()
                ),
            )
        })?;
        let started = start(&machine, program.as_ref(), &dir).await;
        match started {
            Ok(forward) => Ok(Tunnel {
                machine,
                program,
                dir,
                forward: Mutex::new(forward),
            }),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                Err(error)
            }
        }
    }

    /// The machine it reaches.
    pub(crate) fn machine(&self) -> &Machine {
        &self.machine
    }

    /// The local end of the forward.
    pub(crate) fn socket(&self) -> PathBuf {
        socket(&self.dir)
    }

    /// Starts `ssh` again when the one forwarding has exited, as after the network went away.
    pub(crate) async fn ensure(&self) -> Result<(), ClientError> {
        let mut forward = self.forward.lock().await;
        let exited = forward.exited().map_err(|error| {
            unreachable(&self.machine.name, format!("cannot check on ssh: {error}"))
        })?;
        if exited {
            forward.stop();
            *forward = start(&self.machine, self.program.as_ref(), &self.dir).await?;
        }
        Ok(())
    }

    /// The error for a connection through the forward that broke before an answer: the daemon is not running
    /// there when `ssh` last said it could not open the remote socket, else `error` as it was.
    pub(crate) fn broken(&self, error: ClientError) -> ClientError {
        let log = std::fs::read_to_string(log(&self.dir)).unwrap_or_default();
        let last = log.lines().rfind(|line| !line.trim().is_empty());
        if last.is_some_and(|line| line.contains("open failed") || line.contains("connect failed"))
        {
            return unreachable(
                &self.machine.name,
                format!(
                    "runnerd is not running there (nothing listens on {}); start it on {} with `systemctl --user \
                     start runnerd`",
                    self.machine.socket.display(),
                    self.machine.name
                ),
            );
        }
        error
    }

    /// The error for a forward whose local socket is gone or refuses, as when `ssh` exited between two requests.
    pub(crate) fn closed(&self) -> ClientError {
        let log = std::fs::read_to_string(log(&self.dir)).unwrap_or_default();
        failed(&self.machine.name, &self.machine.ssh, &log, Failure::Exited)
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.forward.get_mut().stop();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn socket(dir: &Path) -> PathBuf {
    dir.join("runnerd.sock")
}

fn log(dir: &Path) -> PathBuf {
    dir.join("ssh.log")
}

/// Starts `ssh` forwarding `dir`'s socket to `machine`'s daemon and waits until it listens, `ssh` exits or
/// [`OPEN_TIMEOUT`] passes.
async fn start(
    machine: &Machine,
    program: &dyn SshProgram,
    dir: &Path,
) -> Result<Box<dyn Forwarding>, ClientError> {
    let local = socket(dir);
    let _ = std::fs::remove_file(&local);
    let log_path = log(dir);
    let local_error =
        |error: io::Error| unreachable(&machine.name, format!("cannot start ssh: {error}"));
    let log_file = File::create(&log_path).map_err(local_error)?;
    let mut forward = program
        .start(
            &forward_args(&local, &machine.socket, &machine.ssh),
            log_file,
        )
        .map_err(local_error)?;
    let deadline = tokio::time::Instant::now() + OPEN_TIMEOUT;
    loop {
        if local.exists() {
            return Ok(forward);
        }
        let exited = forward.exited().map_err(local_error)?;
        let timed_out = tokio::time::Instant::now() >= deadline;
        if exited || timed_out {
            forward.stop();
            let said = std::fs::read_to_string(&log_path).unwrap_or_default();
            let how = if exited {
                Failure::Exited
            } else {
                Failure::TimedOut
            };
            return Err(failed(&machine.name, &machine.ssh, &said, how));
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::{self, Write as _};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::routing::get;

    use super::{Failure, Forwarding, SshOutput, SshProgram, failed, forward_args};
    use crate::client::{Client, ClientError, Endpoint};
    use crate::config::machines::Machine;
    use crate::testdir::TestDir;

    /// What the fake `ssh` does once started.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Remote {
        /// Sets the forward up to a daemon that answers.
        Serves,
        /// Exits at once, as when the key is refused.
        RefusesKey,
        /// Sets the forward up, but nothing listens on the remote socket.
        DaemonDown,
    }

    /// A fake `ssh`: binds the forward's local socket itself and serves a fake daemon on it, so no test runs the
    /// real one or opens a network connection.
    #[derive(Debug)]
    struct FakeSsh {
        remote: Remote,
        starts: AtomicU32,
        /// `ssh`s stopped while still running.
        kills: Arc<AtomicU32>,
        /// The last one started, to make it exit as a dropped network would.
        last: Mutex<Option<Arc<AtomicBool>>>,
        args: Mutex<Vec<OsString>>,
    }

    impl FakeSsh {
        fn new(remote: Remote) -> Arc<FakeSsh> {
            Arc::new(FakeSsh {
                remote,
                starts: AtomicU32::new(0),
                kills: Arc::new(AtomicU32::new(0)),
                last: Mutex::new(None),
                args: Mutex::new(Vec::new()),
            })
        }

        /// The last `ssh` exits, and its socket is left behind as a killed one leaves it.
        fn drop_connection(&self) {
            if let Some(exited) = self.last.lock().unwrap().as_ref() {
                exited.store(true, Ordering::SeqCst);
            }
        }
    }

    #[derive(Debug)]
    struct FakeForward {
        exited: Arc<AtomicBool>,
        kills: Arc<AtomicU32>,
        server: Option<tokio::task::JoinHandle<()>>,
    }

    impl Forwarding for FakeForward {
        fn exited(&mut self) -> io::Result<bool> {
            let exited = self.exited.load(Ordering::SeqCst);
            if exited && let Some(server) = self.server.take() {
                server.abort();
            }
            Ok(exited)
        }

        fn stop(&mut self) {
            if !self.exited.swap(true, Ordering::SeqCst) {
                self.kills.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(server) = self.server.take() {
                server.abort();
            }
        }
    }

    impl SshProgram for FakeSsh {
        fn start(&self, args: &[OsString], mut log: File) -> io::Result<Box<dyn Forwarding>> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            *self.args.lock().unwrap() = args.to_vec();
            let exited = Arc::new(AtomicBool::new(false));
            *self.last.lock().unwrap() = Some(exited.clone());
            let forward = |server| FakeForward {
                exited: exited.clone(),
                kills: self.kills.clone(),
                server,
            };
            if self.remote == Remote::RefusesKey {
                writeln!(log, "me@skiff: Permission denied (publickey).")?;
                exited.store(true, Ordering::SeqCst);
                return Ok(Box::new(forward(None)));
            }
            let at = args.iter().position(|arg| arg == "-L").unwrap() + 1;
            let local = args[at].to_string_lossy();
            let local = PathBuf::from(local.split_once(':').unwrap().0);
            let listener = std::os::unix::net::UnixListener::bind(&local)?;
            listener.set_nonblocking(true)?;
            let listener = tokio::net::UnixListener::from_std(listener)?;
            let server = if self.remote == Remote::Serves {
                let router = Router::new().route("/v1/hello", get(|| async { "\"hi\"" }));
                tokio::spawn(async move {
                    let _ = axum::serve(listener, router).await;
                })
            } else {
                tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let _ = writeln!(
                            log,
                            "channel 2: open failed: connect failed: No such file or directory"
                        );
                        drop(stream);
                    }
                })
            };
            Ok(Box::new(forward(Some(server))))
        }

        fn run(&self, _: &[OsString]) -> io::Result<SshOutput> {
            Ok(SshOutput::default())
        }
    }

    fn skiff() -> Machine {
        Machine {
            name: "skiff".to_owned(),
            ssh: "me@skiff".parse().unwrap(),
            socket: PathBuf::from("/run/user/1001/runnerd.sock"),
            id: "1a2b3c4d".to_owned(),
        }
    }

    async fn open(ssh: &Arc<FakeSsh>, base: &Path) -> Result<Client, ClientError> {
        Client::open(Endpoint::Ssh(skiff()), ssh.clone(), base).await
    }

    fn entries(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    /// Catches: an `ssh` left running, or its directory left behind, once runnerctl is done; and a clone (as the
    /// TUI's tasks hold) stopping the forward under the others.
    #[tokio::test]
    async fn requests_go_through_one_forward_that_stops_with_the_last_clone() {
        let base = TestDir::new("ssh-forward");
        let ssh = FakeSsh::new(Remote::Serves);
        let client = open(&ssh, &base.0).await.unwrap();
        assert_eq!(
            client.machine().map(|machine| machine.name.as_str()),
            Some("skiff")
        );
        let dir = client.socket().parent().unwrap().to_owned();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "only this user can reach the forward's socket");
        let clone = client.clone();
        assert_eq!(client.get::<String>("/v1/hello").await.unwrap(), "hi");
        drop(client);
        assert_eq!(clone.get::<String>("/v1/hello").await.unwrap(), "hi");
        assert_eq!(
            ssh.starts.load(Ordering::SeqCst),
            1,
            "one ssh for every request"
        );
        assert_eq!(
            ssh.kills.load(Ordering::SeqCst),
            0,
            "a clone keeps the forward"
        );
        drop(clone);
        assert_eq!(
            ssh.kills.load(Ordering::SeqCst),
            1,
            "the last clone stops ssh"
        );
        assert_eq!(entries(&base.0), 0, "the private directory is removed");
    }

    /// Catches: a forward that exited after a network drop failing every later request of a long-lived TUI.
    #[tokio::test]
    async fn a_forward_whose_ssh_exited_is_started_again_on_the_next_request() {
        let base = TestDir::new("ssh-restart");
        let ssh = FakeSsh::new(Remote::Serves);
        let client = open(&ssh, &base.0).await.unwrap();
        ssh.drop_connection();
        assert_eq!(client.get::<String>("/v1/hello").await.unwrap(), "hi");
        assert_eq!(ssh.starts.load(Ordering::SeqCst), 2, "ssh started again");
    }

    /// Catches: a refused key reported as "runnerd is not running", or the private directory left behind.
    #[tokio::test]
    async fn an_ssh_that_cannot_sign_in_fails_opening_and_says_how_to_sign_in() {
        let base = TestDir::new("ssh-refused");
        let ssh = FakeSsh::new(Remote::RefusesKey);
        let error = open(&ssh, &base.0).await.unwrap_err();
        let text = error.to_string();
        assert!(
            text.starts_with("cannot reach skiff: ssh me@skiff failed"),
            "{text}"
        );
        assert!(text.contains("run `ssh me@skiff` once"), "{text}");
        assert_eq!(entries(&base.0), 0, "the private directory is removed");
    }

    /// Catches: a daemon that is down there worded as a broken HTTP exchange.
    #[tokio::test]
    async fn a_daemon_not_running_there_is_named_with_its_socket() {
        let base = TestDir::new("ssh-down");
        let ssh = FakeSsh::new(Remote::DaemonDown);
        let client = open(&ssh, &base.0).await.unwrap();
        let error = client.get::<String>("/v1/hello").await.unwrap_err();
        assert!(
            matches!(&error, ClientError::Unreachable { reason, .. }
                if reason.contains("runnerd is not running there (nothing listens on /run/user/1001/runnerd.sock)")),
            "{error}"
        );
    }

    /// Catches: an option the owner's config could override, or a target placed where ssh reads options.
    #[test]
    fn the_forward_takes_no_prompt_no_shared_connection_and_ends_its_options_before_the_target() {
        let target = "me@skiff".parse().unwrap();
        let args = forward_args(
            Path::new("/run/user/1000/runnerctl-1-0/runnerd.sock"),
            Path::new("/run/user/1001/runnerd.sock"),
            &target,
        );
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        for option in [
            "BatchMode=yes",
            "ControlPath=none",
            "StreamLocalBindUnlink=yes",
            "ExitOnForwardFailure=yes",
        ] {
            assert!(args.iter().any(|arg| arg == option), "{option} in {args:?}");
        }
        assert!(
            args.contains(&"-N".to_owned()) && args.contains(&"-T".to_owned()),
            "no command and no terminal: {args:?}"
        );
        let end = args.len();
        assert_eq!(args[end - 2..], ["--".to_owned(), "me@skiff".to_owned()]);
        assert_eq!(
            args[end - 3],
            "/run/user/1000/runnerctl-1-0/runnerd.sock:/run/user/1001/runnerd.sock"
        );
    }

    /// Catches: a sign-in `BatchMode` could not ask for reported as a bare "Permission denied" with no way forward.
    #[test]
    fn a_failure_that_needed_a_person_says_to_sign_in_once_by_hand() {
        let target = "skiff".parse().unwrap();
        let ClientError::Unreachable { reason, .. } = failed(
            "skiff",
            &target,
            "me@skiff: Permission denied (publickey).\n",
            Failure::Exited,
        ) else {
            panic!("unreachable")
        };
        assert!(reason.contains("BatchMode=yes"), "{reason}");
        assert!(reason.contains("run `ssh skiff` once"), "{reason}");
        assert!(
            reason.ends_with("(ssh said: me@skiff: Permission denied (publickey).)"),
            "{reason}"
        );
        let ClientError::Unreachable { reason, .. } = failed(
            "skiff",
            &target,
            "# Tailscale SSH requires an additional check.\n# To authenticate, visit: https://login.tailscale.com/a/x\n",
            Failure::TimedOut,
        ) else {
            panic!("unreachable")
        };
        assert!(reason.contains("browser check"), "{reason}");
        assert!(reason.contains("within 20s"), "{reason}");
        let ClientError::Unreachable { reason, .. } = failed(
            "skiff",
            &target,
            "ssh: Could not resolve hostname skiff: Name or service not known\n",
            Failure::Exited,
        ) else {
            panic!("unreachable")
        };
        assert!(
            !reason.contains("BatchMode"),
            "a DNS failure needs no sign-in: {reason}"
        );
    }
}
