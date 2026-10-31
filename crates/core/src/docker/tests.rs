//! That the JIT config stays out of `Debug` output, the create request and the log, that only `unix://` Docker
//! hosts are accepted, and which container a failed start says it left. No Docker needed: a fake Engine answers on a
//! Unix socket.

use std::collections::BTreeMap;

use super::{DockerApi, FILES_LIMIT, JitConfig, RunnerSpec, connect_to, create_body, files_in};
use crate::isolation::Sysbox;

const SECRET: &str = "sentinel-jit-config-c2VjcmV0";

fn spec() -> RunnerSpec {
    RunnerSpec {
        name: "runnerd-jvm-1".to_owned(),
        image: "runnerd-runner:jvm".to_owned(),
        profile: "jvm".to_owned(),
        slice: "ci.slice".to_owned(),
        cpus: 8,
        memory: "9G".parse().unwrap(),
        env: Vec::new(),
        mounts: Vec::new(),
        network: None,
        labels: BTreeMap::new(),
        entrypoint: None,
        jit_config: Some(JitConfig(SECRET.to_owned())),
    }
}

#[test]
fn the_jit_config_stays_out_of_debug_output_and_the_create_request() {
    let spec = spec();
    assert!(!format!("{spec:?}").contains(SECRET));
    let body = create_body(&spec, &Sysbox);
    assert!(!format!("{body:?}").contains(SECRET));
    assert!(!serde_json::to_string(&body).unwrap().contains(SECRET));
}

#[test]
fn only_unix_docker_hosts_are_accepted() {
    for host in [
        "tcp://127.0.0.1:2375",
        "ssh://user@host",
        "npipe:////./pipe/docker_engine",
    ] {
        assert!(connect_to(Some(host)).is_err(), "{host}");
    }
    for host in [None, Some(""), Some("unix:///run/docker.sock")] {
        assert!(connect_to(host).is_ok(), "{host:?}");
    }
}

/// Collects every log record, at every level, as bollard emits them.
struct Capture(std::sync::Mutex<String>);

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let mut text = self.0.lock().unwrap();
        text.push_str(&record.args().to_string());
        text.push('\n');
    }

    fn flush(&self) {}
}

static CAPTURE: Capture = Capture(std::sync::Mutex::new(String::new()));

#[tokio::test]
async fn starting_a_runner_never_logs_the_jit_config() {
    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let socket = std::env::temp_dir().join(format!("runnerd-test-{}.sock", std::process::id()));
    // A Docker Engine that accepts every request: create, upload, start.
    let server = serve(&socket, 3, |request| {
        if request.starts_with("POST /containers/create") {
            answer("201 Created", r#"{"Id":"fake","Warnings":[]}"#)
        } else {
            answer("204 No Content", "")
        }
    });

    let docker = connect_to(Some(&format!("unix://{}", socket.display()))).unwrap();
    docker.start(&spec(), &Sysbox).await.unwrap();
    let received = server.await.unwrap();

    assert!(
        String::from_utf8_lossy(&received).contains(SECRET),
        "the upload did not carry the config"
    );
    let logged = CAPTURE.0.lock().unwrap();
    assert!(
        logged.contains("request:"),
        "bollard logged no request:\n{logged}"
    );
    assert!(!logged.contains(SECRET), "{logged}");
}

/// An HTTP answer with a JSON `body`.
fn answer(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Docker's answer while it is busy, or after it timed out.
fn busy() -> Vec<u8> {
    answer("500 Internal Server Error", r#"{"message":"busy"}"#)
}

/// A Docker Engine at `socket` that takes `requests` requests, one per connection, answers each as `respond` says
/// for its request line, and returns what it received, then removes its socket.
fn serve(
    socket: &std::path::Path,
    requests: usize,
    respond: impl Fn(&str) -> Vec<u8> + Send + 'static,
) -> tokio::task::JoinHandle<Vec<u8>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let _ = std::fs::remove_file(socket);
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let socket = socket.to_owned();
    tokio::spawn(async move {
        let mut received = Vec::new();
        for _ in 0..requests {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                let Some(end) = text.find("\r\n\r\n") else {
                    continue;
                };
                let head = text[..end].to_ascii_lowercase();
                let body = request.len() - end - 4;
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .map(|n| n.trim().parse::<usize>().unwrap());
                let done = match length {
                    Some(length) => body >= length,
                    None if head.contains("transfer-encoding: chunked") => {
                        request.ends_with(b"0\r\n\r\n")
                    }
                    None => true,
                };
                if done || n == 0 {
                    break;
                }
            }
            let line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            stream.write_all(&respond(&line)).await.unwrap();
            received.extend_from_slice(&request);
        }
        std::fs::remove_file(&socket).unwrap();
        received
    })
}

#[tokio::test]
async fn a_failed_start_names_the_container_it_left_only_while_docker_keeps_it() {
    // Each case: the requests the start makes, what Docker answers them, and the container the error names.
    type Respond = fn(&str) -> Vec<u8>;
    let cases: [(&str, usize, Respond, Option<&str>); 5] = [
        (
            "a create whose answer is lost, with a failed removal",
            3,
            |request| {
                if request.starts_with("GET /containers/runnerd-jvm-1/json") {
                    answer("200 OK", r#"{"Id":"made"}"#)
                } else {
                    busy()
                }
            },
            Some("made"),
        ),
        (
            "a create that made nothing",
            2,
            |request| {
                if request.starts_with("POST /containers/create") {
                    busy()
                } else {
                    answer("404 Not Found", r#"{"message":"no such container"}"#)
                }
            },
            None,
        ),
        (
            "a create Docker refused",
            1,
            |_| answer("409 Conflict", r#"{"message":"name in use"}"#),
            None,
        ),
        (
            "a start that failed, with a removal that succeeded",
            4,
            |request| {
                if request.starts_with("POST /containers/create") {
                    answer("201 Created", r#"{"Id":"fake","Warnings":[]}"#)
                } else if request.starts_with("POST /containers/fake/start") {
                    busy()
                } else {
                    answer("204 No Content", "")
                }
            },
            None,
        ),
        (
            "a start that failed, with a failed removal",
            4,
            |request| {
                if request.starts_with("POST /containers/create") {
                    answer("201 Created", r#"{"Id":"fake","Warnings":[]}"#)
                } else if request.starts_with("PUT ") {
                    answer("200 OK", "")
                } else {
                    busy()
                }
            },
            Some("fake"),
        ),
    ];
    for (index, (case, requests, respond, left)) in cases.into_iter().enumerate() {
        let socket = std::env::temp_dir().join(format!(
            "runnerd-test-{}-start-{index}.sock",
            std::process::id()
        ));
        let server = serve(&socket, requests, respond);
        let docker = connect_to(Some(&format!("unix://{}", socket.display()))).unwrap();
        let failed = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            docker.start(&spec(), &Sysbox),
        )
        .await
        .unwrap_or_else(|_| panic!("{case}: the start made more requests than {requests}"))
        .expect_err(case);
        server.await.unwrap();
        assert_eq!(failed.container.as_deref(), left, "{case}");
    }
}

/// A tar as Docker archives a stopped runner's `/home/runner/_diag`, rooted at the directory's own name, whose worker
/// log names `job`. Before it, the pages the log capture kept hold more than `FILES_LIMIT`.
pub(crate) fn diag_archive(job: u64) -> Vec<u8> {
    use std::io::Read;

    let page = (FILES_LIMIT + 1) as u64;
    let worker = format!(
        "[2026-10-31 12:00:01Z INFO Worker] Job message:\n {{\n  \"k\": \"check_run_id\",\n  \"v\": {job}.0\n}}\n"
    );
    let mut tar = tar::Builder::new(Vec::new());
    let mut append = |path: &str, size: Option<u64>, contents: &mut dyn Read| {
        let mut header = tar::Header::new_gnu();
        header.set_mode(0o644);
        if let Some(size) = size {
            header.set_size(size);
        } else {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
        }
        tar.append_data(&mut header, path, contents).unwrap();
    };
    append("_diag/", None, &mut std::io::empty());
    append("_diag/.runnerd-pages/", None, &mut std::io::empty());
    append(
        "_diag/.runnerd-pages/1_5a1c0d9e-1b7e-4b52-9c8f-0e4f3a2b1c00_1.log",
        Some(page),
        &mut std::io::repeat(b'x').take(page),
    );
    append(
        "_diag/Runner_20261031-115959-utc.log",
        Some(9),
        &mut "listening".as_bytes(),
    );
    append(
        "_diag/Worker_20261031-120000-utc.log",
        Some(worker.len() as u64),
        &mut worker.as_bytes(),
    );
    append("_diag/pages/", None, &mut std::io::empty());
    tar.into_inner().unwrap()
}

#[tokio::test]
async fn files_reads_the_worker_log_past_the_pages_the_capture_kept_and_no_deeper() {
    let socket =
        std::env::temp_dir().join(format!("runnerd-test-{}-files.sock", std::process::id()));
    let server = serve(&socket, 1, |request| {
        assert!(
            request.starts_with("GET /containers/stopped/archive?path=%2Fhome%2Frunner%2F_diag"),
            "{request}"
        );
        let tar = diag_archive(5);
        let mut answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-tar\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            tar.len()
        )
        .into_bytes();
        answer.extend_from_slice(&tar);
        answer
    });
    let docker = connect_to(Some(&format!("unix://{}", socket.display()))).unwrap();
    let read = docker
        .files("stopped", "/home/runner/_diag", "Worker_")
        .await
        .unwrap();
    server.await.unwrap();
    assert!(read.complete, "the pages kept count for nothing");
    let names: Vec<&str> = read.files.iter().map(|file| file.name.as_str()).collect();
    assert_eq!(names, ["Worker_20261031-120000-utc.log"]);
    assert_eq!(
        crate::observe::capture::worker_job(&read.files[0].contents),
        Some(5)
    );
}

#[test]
fn a_directory_read_that_breaks_off_is_not_complete() {
    let tar = diag_archive(5);
    // Right after `_diag/`'s header, between entries, and within the pages the capture kept.
    for cut in [512, tar.len() / 2] {
        let read = files_in(&tar[..cut], "Worker_");
        assert_eq!(read.files, [], "cut at {cut}");
        assert!(
            !read.complete,
            "cut at {cut}: the worker log may be in the rest"
        );
    }
    assert!(
        files_in(&tar[..], "Worker_").complete,
        "the whole archive is"
    );
}
