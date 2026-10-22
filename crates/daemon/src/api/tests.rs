use std::sync::Arc;

use reqwest::StatusCode;
use runnerd_core::api::{Budget, ProfileStatus, Status};
use runnerd_core::scheduler::engine::{Control, Handle, Shutdown};
use tokio::sync::watch;

use super::{bind, serve};

fn status() -> Status {
    Status {
        version: "0.1.0".to_owned(),
        shutting_down: false,
        budget: Budget {
            slice: "ci.slice".to_owned(),
            memory_max: Some(33 << 30),
            memory_in_use: 0,
        },
        profiles: vec![ProfileStatus {
            name: "sandbox".to_owned(),
            image: "runnerd-runner:base".to_owned(),
            cpus: 2,
            memory: 4 << 30,
            reseeding: false,
            slots: Vec::new(),
        }],
        repositories: Vec::new(),
    }
}

/// Serves the API on a socket in a fresh directory; returns a client bound to it and the controls behind it.
async fn daemon() -> (reqwest::Client, Arc<Control>, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let socket = dir.0.join("runnerd.sock");
    let control = Arc::new(Control::new(["o/app".to_owned()]));
    let (_sender, receiver) = watch::channel(status());
    let handle = Handle {
        status: receiver,
        control: control.clone(),
    };
    let listener = bind(&socket).await.unwrap();
    let mode = std::fs::metadata(&socket).unwrap();
    assert_eq!(
        std::os::unix::fs::PermissionsExt::mode(&mode.permissions()) & 0o777,
        0o600
    );
    tokio::spawn(async move {
        let _sender = _sender;
        serve(listener, handle, std::future::pending())
            .await
            .unwrap();
    });
    runnerd_core::github::install_crypto_provider();
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .unwrap();
    (client, control, dir)
}

#[tokio::test]
async fn status_lists_the_configured_profiles() {
    let (client, _, _dir) = daemon().await;
    let response = client.get("http://runnerd/v1/status").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Status = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(body, status());
}

#[tokio::test]
async fn repositories_pause_and_resume_by_owner_and_name() {
    let (client, control, _dir) = daemon().await;
    let post = async |path: &str| {
        client
            .post(format!("http://runnerd{path}"))
            .send()
            .await
            .unwrap()
            .status()
    };
    assert_eq!(
        post("/v1/repositories/o/app/pause").await,
        StatusCode::NO_CONTENT
    );
    assert!(control.is_paused("o/app"));
    assert_eq!(
        post("/v1/repositories/o/app/resume").await,
        StatusCode::NO_CONTENT
    );
    assert!(!control.is_paused("o/app"));
    assert_eq!(
        post("/v1/repositories/o/other/pause").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn shutdown_is_graceful_unless_forced() {
    let (client, control, _dir) = daemon().await;
    let response = client
        .post("http://runnerd/v1/shutdown")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(control.mode(), Shutdown::Graceful);
    client
        .post("http://runnerd/v1/shutdown")
        .header("content-type", "application/json")
        .body(r#"{"force":true}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(control.mode(), Shutdown::Force);
}

#[tokio::test]
async fn a_second_daemon_does_not_take_over_a_live_socket() {
    let (_client, _, dir) = daemon().await;
    assert!(bind(&dir.0.join("runnerd.sock")).await.is_err());
}

mod tempdir {
    use std::path::PathBuf;

    /// A directory under the system temp dir, removed on drop.
    pub struct Dir(pub PathBuf);

    impl Dir {
        pub fn new() -> Dir {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("runnerd-test-api-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
