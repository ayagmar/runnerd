//! Test support: a fake daemon on a temporary Unix socket, serving whatever routes a test gives it, and a client
//! with short timeouts for it. Never the user's socket.

#![allow(
    clippy::unwrap_used,
    reason = "a test helper: a fake daemon that cannot start fails the test"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use crate::client::{Client, Timeouts};

/// A fake daemon serving until the test ends; its directory is removed on drop.
#[derive(Debug)]
pub(crate) struct Fake {
    dir: PathBuf,
    socket: PathBuf,
    server: tokio::task::JoinHandle<()>,
}

impl Fake {
    /// Call inside a Tokio runtime: it binds and serves there.
    pub(crate) fn serve(router: axum::Router) -> Fake {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "runnerctl-unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("runnerd.sock");
        let server = serve(&socket, router);
        Fake {
            dir,
            socket,
            server,
        }
    }

    /// Stops listening, as a daemon that died: new connections are refused, the socket file stays. Connections
    /// already open are left to finish.
    pub(crate) async fn stop(&mut self) {
        self.server.abort();
        let _ = (&mut self.server).await;
    }

    /// Listens again on the same socket, with `router`.
    pub(crate) fn serve_again(&mut self, router: axum::Router) {
        std::fs::remove_file(&self.socket).unwrap();
        self.server = serve(&self.socket, router);
    }

    /// A client of it whose requests time out after `request` and whose streams after `idle` of silence.
    pub(crate) fn client(&self, request: Duration, idle: Duration) -> Client {
        Client::new(self.socket.clone()).with_timeouts(Timeouts { request, idle })
    }
}

fn serve(socket: &std::path::Path, router: axum::Router) -> tokio::task::JoinHandle<()> {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    })
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
