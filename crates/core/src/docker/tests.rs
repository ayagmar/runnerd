use super::{DockerApi, JitConfig, RunnerSpec, connect_to, create_body};
use crate::isolation::Sysbox;

const SECRET: &str = "sentinel-jit-config-c2VjcmV0";

fn spec() -> RunnerSpec {
    RunnerSpec {
        name: "runnerd-trellis-1".to_owned(),
        image: "runnerd-runner:trellis".to_owned(),
        profile: "trellis".to_owned(),
        slice: "ci.slice".to_owned(),
        cpus: 8,
        memory: "9G".parse().unwrap(),
        env: Vec::new(),
        mounts: Vec::new(),
        network: None,
        labels: Default::default(),
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    log::set_logger(&CAPTURE).unwrap();
    log::set_max_level(log::LevelFilter::Trace);
    let socket = std::env::temp_dir().join(format!("runnerd-test-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    // A Docker Engine that accepts every request, one per connection, and keeps what it received.
    let server = tokio::spawn(async move {
        let mut received = Vec::new();
        // create, upload, start
        for _ in 0..3 {
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
            let response: &[u8] = if request.starts_with(b"POST /containers/create") {
                b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: 27\r\nConnection: close\r\n\r\n{\"Id\":\"fake\",\"Warnings\":[]}"
            } else {
                b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n"
            };
            stream.write_all(response).await.unwrap();
            received.extend_from_slice(&request);
        }
        received
    });

    let docker = connect_to(Some(&format!("unix://{}", socket.display()))).unwrap();
    docker.start(&spec(), &Sysbox).await.unwrap();
    let received = server.await.unwrap();
    std::fs::remove_file(&socket).unwrap();

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
