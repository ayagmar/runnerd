//! A fake GitHub on a loopback port and a GitHub App's test key, for the tests of the daemon's GitHub App auth. The
//! key is generated for tests (`runnerd-core`'s `src/github/app/testdata`), never a real one.

#![allow(
    clippy::unwrap_used,
    reason = "a test helper: a server or a file that cannot be made fails the test"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

/// The token the fake GitHub mints.
pub(crate) const TOKEN: &str = "ghs_sentinelDaemonInstallationToken";

/// A request the fake GitHub received: its method and its path with the query.
pub(crate) type Seen = Arc<Mutex<Vec<(Method, String)>>>;

/// Serves `handler`, given each request's method and path, on a loopback port in place of api.github.com; returns
/// its URL and what it received.
pub(crate) async fn fake_github(
    handler: impl Fn(&Method, &str) -> Response + Send + Sync + 'static,
) -> (String, Seen) {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    let handler = Arc::new(handler);
    let app = Router::new().fallback(move |method: Method, uri: Uri| {
        let path = uri.to_string();
        log.lock().unwrap().push((method.clone(), path.clone()));
        let response = handler(&method, &path);
        async move { response }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// GitHub's answer to an installation token request: [`TOKEN`], with `permissions`, a JSON object. It says it lasts
/// for decades; runnerd keeps a token an hour at most.
pub(crate) fn minted(permissions: &str) -> Response {
    (
        StatusCode::CREATED,
        [("content-type", "application/json")],
        format!(
            r#"{{"token":"{TOKEN}","expires_at":"2099-01-01T00:00:00Z","permissions":{permissions},"repository_selection":"selected"}}"#
        ),
    )
        .into_response()
}

/// A JSON answer.
pub(crate) fn json(body: &str) -> Response {
    ([("content-type", "application/json")], body.to_owned()).into_response()
}

/// The test key, PKCS#1, copied into `dir` with `mode`.
pub(crate) fn key_file(dir: &Path, mode: u32) -> PathBuf {
    let path = dir.join("github-app.pem");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../core/src/github/app/testdata/test-pkcs1.pem"
        ),
        &path,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// `[github.app]` for installation 77 of App 4242, with its key at `key`.
pub(crate) fn app_table(key: &Path) -> String {
    format!(
        "\n[github.app]\napp_id = 4242\ninstallation_id = 77\nprivate_key = \"{}\"\n",
        key.display()
    )
}
