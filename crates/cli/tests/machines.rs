//! `runnerctl machine` and `--machine` as a person runs them, with a fake `ssh` first on `PATH`: a script that
//! forwards the local socket it is given to a fake daemon's socket on this machine, as `ssh -L` would to another.
//! No test runs the real `ssh`, and every file is under a temporary directory with its own config and runtime dirs.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests fail by panicking"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use runnerd_core::api::Status;

/// The fake `ssh`: prints the remote runtime directory for a command, or forwards `-L local:remote` until killed,
/// writing its pid where the test can find it.
const FAKE_SSH: &str = r#"import os, socket, sys, threading
args = sys.argv[1:]
with open(os.environ["FAKE_SSH_LOG"], "a") as log:
    log.write(" ".join(args) + "\n")
if "-N" not in args:
    print(os.environ["FAKE_REMOTE_RUNTIME"])
    sys.exit(0)
with open(os.environ["FAKE_SSH_PID"], "w") as pid:
    pid.write(str(os.getpid()))
local, remote = args[args.index("-L") + 1].split(":", 1)
server = socket.socket(socket.AF_UNIX)
server.bind(local)
server.listen()
def pipe(a, b):
    try:
        while True:
            data = a.recv(65536)
            if not data:
                break
            b.sendall(data)
    except OSError:
        pass
    try:
        b.shutdown(socket.SHUT_WR)
    except OSError:
        pass
def serve(conn):
    up = socket.socket(socket.AF_UNIX)
    try:
        up.connect(remote)
    except OSError:
        sys.stderr.write("channel 2: open failed: connect failed: No such file or directory\n")
        sys.stderr.flush()
        conn.close()
        return
    threading.Thread(target=pipe, args=(conn, up), daemon=True).start()
    pipe(up, conn)
while True:
    conn, _ = server.accept()
    threading.Thread(target=serve, args=(conn,), daemon=True).start()
"#;

/// A temporary home for one test: `bin` with the fake `ssh`, `config`, `runtime` and `remote`, where the fake
/// daemon listens. Removed on drop.
struct Home(PathBuf);

impl Home {
    fn new() -> Home {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "runnerctl-machines-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        for dir in ["bin", "config", "runtime", "remote"] {
            std::fs::create_dir_all(path.join(dir)).unwrap();
        }
        let ssh = path.join("bin/ssh");
        // The system's Python, not a version manager's shim, which may need the real home.
        let python = ["/usr/bin/python3", "/bin/python3"]
            .into_iter()
            .find(|python| Path::new(python).exists())
            .unwrap_or("/usr/bin/env python3");
        std::fs::write(&ssh, format!("#!{python}\n{FAKE_SSH}")).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Home(path)
    }

    fn runnerctl(&self, args: &[&str]) -> Output {
        let path = format!(
            "{}:{}",
            self.0.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new(env!("CARGO_BIN_EXE_runnerctl"))
            .args(args)
            .env("PATH", path)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .env("FAKE_REMOTE_RUNTIME", self.0.join("remote"))
            .env("FAKE_SSH_LOG", self.0.join("ssh.log"))
            .env("FAKE_SSH_PID", self.0.join("ssh.pid"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                child.kill().unwrap();
                panic!("runnerctl {args:?} did not finish within 20 s");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        child.wait_with_output().unwrap()
    }

    /// Whether the last forwarding `ssh` still runs, or its private directory is still there.
    fn forward_left(&self) -> Option<String> {
        if let Ok(pid) = std::fs::read_to_string(self.0.join("ssh.pid"))
            && Path::new("/proc").join(pid.trim()).exists()
        {
            return Some(format!("ssh {pid} still runs"));
        }
        let left: Vec<_> = std::fs::read_dir(self.0.join("runtime"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        (!left.is_empty()).then(|| format!("left in the runtime dir: {left:?}"))
    }

    /// Serves a fake daemon whose status names `machine` on the remote socket.
    fn serve(&self, status: Status) {
        let app = Router::new().route(
            "/v1/status",
            get(move || {
                let status = status.clone();
                async move { axum::Json(status) }
            }),
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(async { tokio::net::UnixListener::bind(self.0.join("remote/runnerd.sock")) })
            .unwrap();
        std::thread::spawn(move || {
            runtime.block_on(async { axum::serve(listener, app).await.unwrap() });
        });
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(self.0.join("ssh.pid")) {
            let _ = Command::new("kill")
                .arg(pid.trim())
                .stderr(Stdio::null())
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Catches: `--machine` reading this machine's daemon, an `ssh` or its socket left behind after runnerctl exits,
/// and the arguments a real `ssh` would refuse or read differently.
#[test]
fn an_added_machine_is_asked_over_ssh_and_nothing_is_left_running() {
    let home = Home::new();
    let status = runnerd_fixtures::daemon().status;
    let id = status.machine.clone().unwrap().id;
    home.serve(status.clone());

    let added = home.runnerctl(&["machine", "add", "skiff", "--ssh", "me@skiff"]);
    assert!(added.status.success(), "{}", text(&added.stderr));
    assert!(
        text(&added.stdout).contains(&format!("machine id {id}")),
        "{}",
        text(&added.stdout)
    );
    assert_eq!(home.forward_left(), None);
    let file = std::fs::read_to_string(home.0.join("config/runnerd/machines.toml")).unwrap();
    assert!(
        file.contains(&format!("{}", home.0.join("remote/runnerd.sock").display())),
        "{file}"
    );

    let read = home.runnerctl(&["--machine", "skiff", "status", "--json"]);
    assert!(read.status.success(), "{}", text(&read.stderr));
    let printed: Status = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(printed, status, "the remote daemon's status, unchanged");
    assert_eq!(home.forward_left(), None);

    let log = std::fs::read_to_string(home.0.join("ssh.log")).unwrap();
    let forward = log.lines().find(|line| line.starts_with("-N")).unwrap();
    assert!(forward.ends_with("-- me@skiff"), "{forward}");
    assert!(
        forward.contains("BatchMode=yes") && forward.contains("ControlPath=none"),
        "{forward}"
    );

    let listed = home.runnerctl(&["machine", "list"]);
    assert!(
        text(&listed.stdout).starts_with(&format!("skiff  ssh me@skiff  id {id}")),
        "{}",
        text(&listed.stdout)
    );
    let removed = home.runnerctl(&["machine", "rm", "skiff"]);
    assert!(removed.status.success(), "{}", text(&removed.stderr));
    let gone = home.runnerctl(&["--machine", "skiff", "status"]);
    assert!(
        text(&gone.stderr).contains("no machine named skiff"),
        "{}",
        text(&gone.stderr)
    );
}

/// Catches: `web` opening a login link from a file that is on the other machine, and `--socket` silently winning
/// over `--machine`.
#[test]
fn what_cannot_work_over_ssh_is_refused_with_what_does() {
    let home = Home::new();
    home.serve(runnerd_fixtures::daemon().status);
    let added = home.runnerctl(&["machine", "add", "skiff", "--ssh", "skiff"]);
    assert!(added.status.success(), "{}", text(&added.stderr));

    let web = home.runnerctl(&["--machine", "skiff", "web"]);
    assert!(!web.status.success(), "plain web is refused");
    assert!(
        text(&web.stderr).contains("web --qr"),
        "{}",
        text(&web.stderr)
    );
    assert_eq!(home.forward_left(), None);

    let both = home.runnerctl(&["--machine", "skiff", "--socket", "/x.sock", "status"]);
    assert!(!both.status.success(), "--machine and --socket conflict");
    assert!(
        text(&both.stderr).contains("cannot be used with"),
        "{}",
        text(&both.stderr)
    );

    let option = home.runnerctl(&["machine", "add", "lab", "--ssh=-oProxyCommand=x"]);
    assert!(
        !option.status.success(),
        "a target ssh would read as an option is refused"
    );
    assert!(
        text(&option.stderr).contains("cannot start with `-`"),
        "{}",
        text(&option.stderr)
    );
}

/// Catches: a remote daemon that is down reported as a broken exchange, or doctor falling back to this machine's
/// `runnerd doctor` and checking the wrong host.
#[test]
fn a_remote_daemon_that_is_down_is_named_and_doctor_runs_nothing_here() {
    let home = Home::new();
    home.serve(runnerd_fixtures::daemon().status);
    let added = home.runnerctl(&["machine", "add", "skiff", "--ssh", "skiff"]);
    assert!(added.status.success(), "{}", text(&added.stderr));
    std::fs::remove_file(home.0.join("remote/runnerd.sock")).unwrap();

    let doctor = home.runnerctl(&["--machine", "skiff", "doctor"]);
    assert!(!doctor.status.success(), "a daemon that is down fails");
    let said = text(&doctor.stderr);
    assert!(
        said.contains("cannot reach skiff: runnerd is not running there"),
        "{said}"
    );
    assert!(
        !text(&doctor.stdout).contains("running `runnerd doctor` here"),
        "{}",
        text(&doctor.stdout)
    );
    assert_eq!(home.forward_left(), None);
}
