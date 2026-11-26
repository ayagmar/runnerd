//! The daemon's start against a fake GitHub with a test App key: it starts only with a key it may use and a first
//! token minted, and a save is compared with what it started with.

use axum::http::{Method, StatusCode};
use axum::response::IntoResponse as _;
use runnerd_core::config::Config;

use super::{Auth, connect};
use crate::testdir::Dir;
use crate::testing::{TOKEN, app_table, fake_github, json, key_file, minted};

const CONFIG: &str = include_str!("../../../../examples/runnerd.toml");

const ALL: &str =
    r#"{"administration":"write","actions":"write","contents":"read","pull_requests":"read"}"#;

fn with_app(key: &std::path::Path) -> Config {
    Config::parse(&format!("{CONFIG}{}", app_table(key))).unwrap()
}

/// A fake GitHub whose installation covers `covered`, each `owner/name`.
async fn installation(covered: &'static [&'static str]) -> (String, crate::testing::Seen) {
    fake_github(move |_, path| match path {
        "/app/installations/77/access_tokens" => minted(ALL),
        "/installation/repositories?per_page=100&page=1" => {
            let listed: Vec<String> = covered
                .iter()
                .map(|name| format!(r#"{{"full_name":"{name}","visibility":"private"}}"#))
                .collect();
            json(&format!(r#"{{"repositories":[{}]}}"#, listed.join(",")))
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await
}

#[tokio::test]
async fn an_app_daemon_starts_with_a_first_token_once_its_installation_covers_what_it_serves_and_drains()
 {
    let dir = Dir::new();
    let (url, seen) = installation(&["halyard/ledger", "Halyard/Storefront", "halyard/old"]).await;
    let (_, active) = connect(
        &with_app(&key_file(&dir.0, 0o600)),
        &url,
        &["halyard/old".to_owned()],
    )
    .await
    .unwrap();
    assert_eq!(
        active.auth(),
        Auth::of(&with_app(&dir.0.join("github-app.pem")))
    );
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter().all(|(method, path)| *method == Method::GET
            || path == "/app/installations/77/access_tokens"),
        "nothing but mints writes: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|(_, path)| path.starts_with("/installation/repositories")),
        "coverage is read from the installation's listing: {seen:?}"
    );
}

#[tokio::test]
async fn an_app_daemon_does_not_start_when_its_installation_misses_a_served_or_a_draining_repository()
 {
    let dir = Dir::new();
    let key = key_file(&dir.0, 0o600);
    let (url, _) = installation(&["halyard/ledger"]).await;
    let error = connect(&with_app(&key), &url, &[]).await.err().unwrap();
    assert!(
        error.contains("does not cover halyard/storefront")
            && error.contains("remove them from the config"),
        "{error}"
    );
    let (url, _) = installation(&["halyard/ledger", "halyard/storefront"]).await;
    let error = connect(&with_app(&key), &url, &["halyard/old".to_owned()])
        .await
        .err()
        .unwrap();
    assert!(
        error.starts_with("halyard/old still drain runners") && error.contains("start with gh"),
        "{error}"
    );
}

#[tokio::test]
async fn an_app_daemon_does_not_start_without_a_key_it_may_use_or_a_first_token() {
    let dir = Dir::new();
    let (url, seen) = fake_github(|_, path| match path {
        "/app/installations/77/access_tokens" => (
            StatusCode::UNAUTHORIZED,
            format!(r#"{{"message":"sentinel {TOKEN}"}}"#),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let missing = with_app(&dir.0.join("missing.pem"));
    let error = connect(&missing, &url, &[]).await.err().unwrap();
    assert!(error.contains("cannot open"), "{error}");
    let shared = with_app(&key_file(&dir.0, 0o644));
    let error = connect(&shared, &url, &[]).await.err().unwrap();
    assert!(error.contains("chmod 600"), "{error}");
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing is sent without a key it may use"
    );
    std::fs::remove_file(dir.0.join("github-app.pem")).unwrap();
    let refused = with_app(&key_file(&dir.0, 0o600));
    let error = connect(&refused, &url, &[]).await.err().unwrap();
    assert!(
        error.contains("first token") && error.contains("401"),
        "{error}"
    );
    assert!(
        !error.contains("sentinel") && !error.contains("ghs_"),
        "{error}"
    );
}

#[test]
fn a_saved_auth_is_pending_until_a_restart_however_often_it_is_saved() {
    let dir = Dir::new();
    let app = Auth::of(&with_app(&key_file(&dir.0, 0o600)));
    let gh = Auth::of_text(CONFIG);
    assert_eq!(gh, Auth::Gh);
    assert_eq!(gh.pending(&gh), None);
    assert_eq!(app.pending(&app), None);
    let pending = gh.pending(&app).unwrap();
    assert!(
        pending.contains("runs with gh auth token until then")
            && pending.contains("GitHub App 4242's installation 77 after"),
        "{pending}"
    );
    // Compared with what started, not with the last save: saving it again still says so.
    assert_eq!(gh.pending(&app), Some(pending));
}
