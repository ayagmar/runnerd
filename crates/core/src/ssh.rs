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

/// Options every `ssh` here runs with. The owner's `~/.ssh/config` still says how to reach the host (its `Host`
/// aliases, user, keys, port, jump host); these override what would change how this child behaves (ADR 0164): no
/// terminal, no prompt (a password, a passphrase, a host key or a browser check fails instead of hanging), none of
/// the owner's connection sharing, no fork into the background, no remote or local command of theirs, a log level
/// that writes the lines [`failed`] and [`Tunnel::broken`] read and no debug lines after them, and keepalives so a
/// stalled connection ends. A first value wins in ssh, so these beat the config's. `ForkAfterAuthentication` and
/// `SessionType` need OpenSSH 8.7 or later.
const OPTIONS: [&str; 19] = [
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ControlPath=none",
    "-o",
    "ControlMaster=no",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ForkAfterAuthentication=no",
    "-o",
    "RemoteCommand=none",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "LogLevel=INFO",
    "-o",
    "ServerAliveInterval=15",
];

/// With [`OPTIONS`]' `ServerAliveInterval`: a connection that answered no keepalive for 45 s ends.
const ALIVE_COUNT: &str = "ServerAliveCountMax=3";

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
        output_within(Command::new("ssh").args(args), OPEN_TIMEOUT).map_err(not_found)
    }
}

/// Runs `command` with nothing on its standard input to its end and returns what it printed, or kills it once
/// `limit` passed. What it printed is read on threads of their own, so a full pipe cannot stall it.
fn output_within(command: &mut Command, limit: Duration) -> io::Result<SshOutput> {
    use std::io::Read as _;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let read = |pipe: Option<Box<dyn io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut text = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut text);
            }
            String::from_utf8_lossy(&text).into_owned()
        })
    };
    let stdout = read(child.stdout.take().map(|pipe| Box::new(pipe) as _));
    let stderr = read(child.stderr.take().map(|pipe| Box::new(pipe) as _));
    let deadline = std::time::Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // The readers end when the pipes close; a grandchild holding one would keep them, so they are left.
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("ssh did not finish within {}s", limit.as_secs()),
            ));
        }
        std::thread::sleep(POLL);
    };
    Ok(SshOutput {
        success: status.success(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
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

/// A real `ssh`. Dropped, it is killed and reaped: `std::process::Child` would leave it running, still forwarding,
/// when an open is given up on or fails on the way.
#[derive(Debug)]
struct SshChild(Child);

impl Drop for SshChild {
    fn drop(&mut self) {
        self.stop();
    }
}

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
        ALIVE_COUNT,
    ] {
        args.extend(["-o".into(), option.into()]);
    }
    let mut forward = local.as_os_str().to_owned();
    forward.push(":");
    forward.push(remote.as_os_str());
    args.extend(["-L".into(), forward, "--".into(), target.as_str().into()]);
    args
}

/// `ssh`'s arguments to run `command` on `target`, whatever session type the owner's config sets for it.
fn command_args(target: &SshTarget, command: &str) -> Vec<OsString> {
    let mut args: Vec<OsString> = OPTIONS.iter().map(OsString::from).collect();
    for option in [ALIVE_COUNT, "SessionType=default"] {
        args.extend(["-o".into(), option.into()]);
    }
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
/// passphrase, a new host key or Tailscale's browser check) says so, since `BatchMode` stops it from asking; a
/// host key that changed does not, since signing in by hand does not fix it, and ssh's own words stand.
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
        "passphrase",
        "password",
        "tailscale ssh",
        "login.tailscale.com",
        "to authenticate, visit",
    ]
    .iter()
    .any(|clue| lower.contains(clue))
        && !lower.contains("identification has changed");
    let too_old = lower.contains("bad configuration option");
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
    let reason = if too_old {
        format!("{what}: runnerctl needs OpenSSH 8.7 or later{said}")
    } else if needs_you {
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

/// A private directory removed when dropped, unless kept: an open given up on or failed leaves nothing behind.
struct Made(Option<PathBuf>);

impl Made {
    fn keep(mut self) -> PathBuf {
        self.0.take().unwrap_or_default()
    }
}

impl Drop for Made {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Where a connection through the forward began in `ssh`'s log, so only what `ssh` said since is read for it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mark(u64);

/// A connection through the forward, and where it began in `ssh`'s log.
#[derive(Debug)]
pub(crate) struct Opened {
    /// The connection.
    pub(crate) stream: tokio::net::UnixStream,
    /// Where it began in `ssh`'s log.
    pub(crate) mark: Mark,
}

/// How long a connection refused by a forward whose `ssh` still runs is tried again: OpenSSH creates the socket a
/// moment before it listens on it.
const LISTEN_GRACE: Duration = Duration::from_millis(250);

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
        let made = Made(Some(private_dir(base).map_err(|error| {
            unreachable(
                &machine.name,
                format!(
                    "cannot make a private directory in {}: {error}",
                    base.display()
                ),
            )
        })?));
        let dir = made.0.clone().unwrap_or_default();
        check_socket(&socket(&dir)).map_err(|why| {
            unreachable(
                &machine.name,
                format!(
                    "cannot forward from {}: {why}; set XDG_RUNTIME_DIR to a directory without one",
                    socket(&dir).display()
                ),
            )
        })?;
        let forward = start(&machine, program.as_ref(), &dir).await?;
        Ok(Tunnel {
            machine,
            program,
            dir: made.keep(),
            forward: Mutex::new(forward),
        })
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

    /// Connects through the forward, starting `ssh` again first when it exited. A socket that refuses while
    /// `ssh` runs is tried again for [`LISTEN_GRACE`].
    pub(crate) async fn connect(&self) -> Result<Opened, ClientError> {
        self.ensure().await?;
        let mark = Mark(std::fs::metadata(log(&self.dir)).map_or(0, |meta| meta.len()));
        let deadline = tokio::time::Instant::now() + LISTEN_GRACE;
        loop {
            match tokio::net::UnixStream::connect(self.socket()).await {
                Ok(stream) => return Ok(Opened { stream, mark }),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    if tokio::time::Instant::now() >= deadline || self.exited().await {
                        return Err(self.closed());
                    }
                    tokio::time::sleep(POLL).await;
                }
                Err(error) => {
                    return Err(ClientError::Transport(format!(
                        "{}: {error}",
                        self.socket().display()
                    )));
                }
            }
        }
    }

    /// Whether the `ssh` forwarding has exited; one that cannot be checked on counts as exited.
    async fn exited(&self) -> bool {
        self.forward.lock().await.exited().unwrap_or(true)
    }

    /// The error for a connection through the forward begun at `mark` that broke before an answer: the daemon is
    /// not running there when `ssh` said since that it could not open the remote socket; `ssh` failed when it
    /// exited; else `error` as it was.
    pub(crate) async fn broken(&self, error: ClientError, mark: Mark) -> ClientError {
        let log = std::fs::read(log(&self.dir)).unwrap_or_default();
        let since = usize::try_from(mark.0)
            .ok()
            .and_then(|at| log.get(at..))
            .unwrap_or_default();
        let since = String::from_utf8_lossy(since);
        if since
            .lines()
            .any(|line| line.contains("open failed") || line.contains("connect failed"))
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
        if self.exited().await {
            return self.closed();
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
        // Exited first: an ssh that bound the socket and then failed another forward is not set up.
        let exited = forward.exited().map_err(local_error)?;
        if !exited && local.exists() {
            return Ok(forward);
        }
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
    use std::time::Duration;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::{
        Failure, Forwarding, SshChild, SshOutput, SshProgram, command_args, failed, forward_args,
        output_within,
    };
    use crate::client::{Client, ClientError, Endpoint};
    use crate::config::machines::Machine;
    use crate::testdir::TestDir;

    /// What the fake `ssh` does: once started, and with each connection through its forward.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Remote {
        /// Sets the forward up to a daemon that answers.
        Serves,
        /// Exits at once, as when the key is refused.
        RefusesKey,
        /// Sets the forward up, but nothing listens on the remote socket: ssh logs that the channel failed.
        DaemonDown,
        /// The daemon there closes the connection before answering; ssh logs nothing.
        HangsUp,
        /// ssh itself exits on the next connection, as when keepalives found the network gone.
        ExitsMidRequest,
        /// The socket's path appears before ssh listens on it, as between OpenSSH's `bind` and `listen`.
        ListensLate,
        /// ssh binds the socket, then exits because another forward failed.
        BindsThenExits,
        /// ssh never sets the forward up and never exits.
        NeverListens,
    }

    /// A fake `ssh`: binds the forward's local socket itself and answers on it as `remote` says, so no test runs
    /// the real one or opens a network connection.
    #[derive(Debug)]
    struct FakeSsh {
        remote: Arc<Mutex<Remote>>,
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
                remote: Arc::new(Mutex::new(remote)),
                starts: AtomicU32::new(0),
                kills: Arc::new(AtomicU32::new(0)),
                last: Mutex::new(None),
                args: Mutex::new(Vec::new()),
            })
        }

        /// From now on, ssh and the daemon behind it do as `remote` says.
        fn set(&self, remote: Remote) {
            *self.remote.lock().unwrap() = remote;
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

    /// As [`SshChild`]: dropping one stops it.
    impl Drop for FakeForward {
        fn drop(&mut self) {
            self.stop();
        }
    }

    /// Answers each connection through the forward on `listener` as `remote` says at that moment.
    async fn answer(
        listener: tokio::net::UnixListener,
        remote: Arc<Mutex<Remote>>,
        exited: Arc<AtomicBool>,
        mut log: File,
    ) {
        while let Ok((mut stream, _)) = listener.accept().await {
            let now = *remote.lock().unwrap();
            match now {
                Remote::DaemonDown => {
                    let _ = writeln!(
                        log,
                        "channel 2: open failed: connect failed: No such file or directory"
                    );
                }
                Remote::ExitsMidRequest => {
                    let _ = writeln!(log, "client_loop: send disconnect: Broken pipe");
                    exited.store(true, Ordering::SeqCst);
                    return;
                }
                Remote::HangsUp => {}
                _ => {
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|end| end == b"\r\n\r\n") {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => request.extend_from_slice(&buffer[..n]),
                        }
                    }
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 4\r\n\r\n\"hi\"",
                        )
                        .await;
                }
            }
        }
    }

    fn bind(local: &Path) -> io::Result<tokio::net::UnixListener> {
        let listener = std::os::unix::net::UnixListener::bind(local)?;
        listener.set_nonblocking(true)?;
        tokio::net::UnixListener::from_std(listener)
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
            let at = args.iter().position(|arg| arg == "-L").unwrap() + 1;
            let local = args[at].to_string_lossy();
            let local = PathBuf::from(local.split_once(':').unwrap().0);
            let remote = *self.remote.lock().unwrap();
            match remote {
                Remote::RefusesKey => {
                    writeln!(log, "me@skiff: Permission denied (publickey).")?;
                    exited.store(true, Ordering::SeqCst);
                    Ok(Box::new(forward(None)))
                }
                Remote::NeverListens => Ok(Box::new(forward(None))),
                Remote::BindsThenExits => {
                    drop(std::os::unix::net::UnixListener::bind(&local)?);
                    writeln!(log, "Error: forwarding failed")?;
                    exited.store(true, Ordering::SeqCst);
                    Ok(Box::new(forward(None)))
                }
                Remote::ListensLate => {
                    // A path that refuses connections until ssh listens.
                    File::create(&local)?;
                    let shared = self.remote.clone();
                    let up = exited.clone();
                    let server = tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        let _ = std::fs::remove_file(&local);
                        if let Ok(listener) = bind(&local) {
                            answer(listener, shared, up, log).await;
                        }
                    });
                    Ok(Box::new(forward(Some(server))))
                }
                _ => {
                    let listener = bind(&local)?;
                    let server =
                        tokio::spawn(answer(listener, self.remote.clone(), exited.clone(), log));
                    Ok(Box::new(forward(Some(server))))
                }
            }
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

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
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

    /// Catches: an open given up on (a timeout, `q` in the TUI) leaving the `ssh` it started running and its
    /// private directory behind.
    #[tokio::test]
    async fn an_open_given_up_on_stops_its_ssh_and_removes_its_directory() {
        let base = TestDir::new("ssh-cancel");
        let ssh = FakeSsh::new(Remote::NeverListens);
        let given_up = tokio::time::timeout(Duration::from_millis(200), open(&ssh, &base.0)).await;
        assert!(given_up.is_err(), "still opening after 200ms");
        assert_eq!(ssh.kills.load(Ordering::SeqCst), 1, "its ssh is stopped");
        assert_eq!(entries(&base.0), 0, "the private directory is removed");
    }

    /// Catches: a real `ssh` child dropped without being stopped (a cancelled start, an error on the way), which
    /// `std::process::Child` leaves running and keeps forwarding.
    #[test]
    fn a_dropped_ssh_child_is_killed_and_reaped() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        drop(SshChild(child));
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "sleep {pid} is gone"
        );
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

    /// Catches: an `ssh` that bound the socket and then exited (another forward of the owner's config failed)
    /// taken as a forward that is set up.
    #[tokio::test]
    async fn an_ssh_that_bound_its_socket_then_exited_fails_opening() {
        let base = TestDir::new("ssh-bound-exited");
        let ssh = FakeSsh::new(Remote::BindsThenExits);
        let error = open(&ssh, &base.0).await.unwrap_err().to_string();
        assert!(error.contains("forwarding failed"), "{error}");
    }

    /// Catches: the moment between OpenSSH creating the socket and listening on it reported as ssh having failed.
    #[tokio::test]
    async fn a_forward_that_listens_a_moment_after_its_socket_appears_is_used() {
        let base = TestDir::new("ssh-late");
        let ssh = FakeSsh::new(Remote::ListensLate);
        let client = open(&ssh, &base.0).await.unwrap();
        ssh.set(Remote::Serves);
        assert_eq!(client.get::<String>("/v1/hello").await.unwrap(), "hi");
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

    /// Catches: a daemon restart hours ago, still the last line of ssh's log, blamed for a later break while the
    /// daemon runs.
    #[tokio::test]
    async fn a_daemon_that_was_down_once_is_not_blamed_for_a_later_break() {
        let base = TestDir::new("ssh-stale");
        let ssh = FakeSsh::new(Remote::DaemonDown);
        let client = open(&ssh, &base.0).await.unwrap();
        client.get::<String>("/v1/hello").await.unwrap_err();
        ssh.set(Remote::Serves);
        assert_eq!(client.get::<String>("/v1/hello").await.unwrap(), "hi");
        ssh.set(Remote::HangsUp);
        let error = client.get::<String>("/v1/hello").await.unwrap_err();
        assert!(
            matches!(error, ClientError::Transport(_)),
            "the break as it was: {error}"
        );
    }

    /// Catches: ssh exiting mid-request (keepalives found the network gone) shown as a bare transport error
    /// without the machine or what ssh said.
    #[tokio::test]
    async fn ssh_exiting_mid_request_names_the_machine_and_what_ssh_said() {
        let base = TestDir::new("ssh-exits");
        let ssh = FakeSsh::new(Remote::Serves);
        let client = open(&ssh, &base.0).await.unwrap();
        ssh.set(Remote::ExitsMidRequest);
        let error = client.get::<String>("/v1/hello").await.unwrap_err();
        assert!(
            matches!(&error, ClientError::Unreachable { machine, reason }
                if machine == "skiff" && reason.contains("Broken pipe")),
            "{error}"
        );
    }

    /// Catches: a private directory whose path has a `:`, which `-L` would split in the wrong place, handed to ssh.
    #[tokio::test]
    async fn a_private_directory_with_a_colon_is_refused_before_ssh_runs() {
        let base = TestDir::new("ssh-colon");
        let odd = base.0.join("ci:1");
        std::fs::create_dir(&odd).unwrap();
        let ssh = FakeSsh::new(Remote::Serves);
        let error = open(&ssh, &odd).await.unwrap_err().to_string();
        assert!(error.contains(':'), "{error}");
        assert_eq!(ssh.starts.load(Ordering::SeqCst), 0, "ssh never ran");
        assert_eq!(entries(&odd), 0, "the private directory is removed");
    }

    /// Catches: an option the owner's `~/.ssh/config` could set that changes how this child behaves: forking
    /// away from runnerctl, running a remote command, logging too little or too much to read; and a target placed
    /// where ssh reads options.
    #[test]
    fn the_forward_overrides_what_the_owners_config_could_change_and_ends_its_options_before_the_target()
     {
        let target = "me@skiff".parse().unwrap();
        let args = strings(&forward_args(
            Path::new("/run/user/1000/runnerctl-1-0/runnerd.sock"),
            Path::new("/run/user/1001/runnerd.sock"),
            &target,
        ));
        for option in [
            "BatchMode=yes",
            "ControlPath=none",
            "ControlMaster=no",
            "ForkAfterAuthentication=no",
            "RemoteCommand=none",
            "PermitLocalCommand=no",
            "LogLevel=INFO",
            "ServerAliveInterval=15",
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

        let args = strings(&command_args(&target, "true"));
        for option in [
            "SessionType=default",
            "RemoteCommand=none",
            "ForkAfterAuthentication=no",
            "ServerAliveInterval=15",
        ] {
            assert!(args.iter().any(|arg| arg == option), "{option} in {args:?}");
        }
        assert_eq!(args[args.len() - 3..], ["--", "me@skiff", "true"]);
    }

    /// Catches: `machine add` hanging forever on a connection that stalled after it was set up.
    #[test]
    fn a_command_that_runs_past_its_time_is_killed_and_reported() {
        let mut sleeper = std::process::Command::new("sleep");
        sleeper.arg("30");
        let started = std::time::Instant::now();
        let error = output_within(&mut sleeper, Duration::from_millis(200)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "gave up in time"
        );
        let mut echo = std::process::Command::new("sh");
        echo.args(["-c", "echo out; echo err >&2"]);
        let output = output_within(&mut echo, Duration::from_secs(10)).unwrap();
        assert!(output.success, "sh ran to its end");
        assert_eq!(
            (output.stdout.as_str(), output.stderr.as_str()),
            ("out\n", "err\n")
        );
    }

    fn reason(error: ClientError) -> String {
        match error {
            ClientError::Unreachable { reason, .. } => reason,
            other => panic!("unreachable, not {other}"),
        }
    }

    /// Catches: a sign-in `BatchMode` could not ask for reported as a bare "Permission denied" with no way forward;
    /// and that advice given where signing in by hand does not help.
    #[test]
    fn a_failure_that_needed_a_person_says_to_sign_in_once_by_hand() {
        let target = "skiff".parse().unwrap();
        let said = |stderr: &str, how| reason(failed("skiff", &target, stderr, how));
        let refused = said(
            "me@skiff: Permission denied (publickey).\n",
            Failure::Exited,
        );
        assert!(refused.contains("BatchMode=yes"), "{refused}");
        assert!(refused.contains("run `ssh skiff` once"), "{refused}");
        assert!(
            refused.ends_with("(ssh said: me@skiff: Permission denied (publickey).)"),
            "{refused}"
        );
        let browser = said(
            "# Tailscale SSH requires an additional check.\n# To authenticate, visit: https://login.tailscale.com/a/x\n",
            Failure::TimedOut,
        );
        assert!(browser.contains("browser check"), "{browser}");
        assert!(browser.contains("within 20s"), "{browser}");
        let old = said(
            "command-line line 0: Bad configuration option: forkafterauthentication\n",
            Failure::Exited,
        );
        assert!(old.contains("needs OpenSSH 8.7 or later"), "{old}");
        for stderr in [
            "ssh: Could not resolve hostname skiff: Name or service not known\n",
            "ssh: Could not resolve hostname login.lab.internal: Name or service not known\n",
            "ssh: connect to host skiff.tailscale.example port 22: Connection refused\n",
            "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@\nHost key for skiff has changed and you have requested strict checking.\n",
        ] {
            let text = said(stderr, Failure::Exited);
            assert!(
                !text.contains("BatchMode"),
                "signing in by hand does not help: {text}"
            );
        }
    }
}
