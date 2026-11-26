//! A GitHub App's key, JWT and installation token against generated test keys (never a real one) and a fake GitHub
//! on a loopback port: which key files are taken, that the JWT verifies, what an exchange GitHub refuses or answers
//! badly gives, that nothing secret shows in an error, and that the exchange is kept apart from the installation's
//! own requests.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};

use super::{App, AppKey, Coverage, Granted, KeyError, Level, Missing};
use crate::api::Visibility;
use crate::github::{Class, Github};
use crate::testdir::TestDir;

const TOKEN: &str = "ghs_sentinelInstallationToken0123";
const APP_ID: u64 = 4242;
const INSTALLATION: u64 = 77;
const MINT_PATH: &str = "/app/installations/77/access_tokens";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/github/app/testdata")
        .join(name)
}

/// The fixture `name` copied into `dir` with `mode`, as a user would save it.
fn key_file(dir: &TestDir, name: &str, mode: u32) -> PathBuf {
    let path = dir.0.join(name);
    std::fs::copy(fixture(name), &path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

/// `bytes` written to `dir/name` with mode 0600.
fn written(dir: &TestDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.0.join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn test_key() -> AppKey {
    let dir = TestDir::new("app-key");
    AppKey::load(&key_file(&dir, "test-pkcs1.pem", 0o600)).unwrap()
}

/// Whether `jwt` is App `APP_ID`'s, RS256-signed by `key`, issued a minute before `now` and lasting nine minutes.
fn assert_jwt(jwt: &str, key: &AppKey, now: u64) {
    let parts: Vec<&str> = jwt.split('.').collect();
    let [header, claims, signature] = parts.as_slice() else {
        panic!("a JWT has three parts");
    };
    let decode = |part: &str| -> serde_json::Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    };
    assert_eq!(
        decode(header),
        serde_json::json!({"alg": "RS256", "typ": "JWT"})
    );
    let claims_json = decode(claims);
    assert_eq!(claims_json["iss"], serde_json::json!(APP_ID.to_string()));
    let iat = claims_json["iat"].as_u64().unwrap();
    let exp = claims_json["exp"].as_u64().unwrap();
    assert!(
        iat.abs_diff(now - 60) <= 2,
        "issued a minute back: {iat} for {now}"
    );
    assert_eq!(exp - iat, 600, "lasts 9 minutes from now");
    UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, key.0.public().as_ref())
        .verify(
            format!("{header}.{claims}").as_bytes(),
            &URL_SAFE_NO_PAD.decode(signature).unwrap(),
        )
        .unwrap();
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn github_issued_pkcs1_and_pkcs8_keys_sign_a_jwt_that_verifies() {
    let dir = TestDir::new("app-keys");
    for name in ["test-pkcs1.pem", "test-pkcs8.pem"] {
        let key = AppKey::load(&key_file(&dir, name, 0o600)).unwrap();
        let jwt = key.jwt(APP_ID, SystemTime::now()).unwrap();
        assert_jwt(&jwt.0, &key, now_secs());
        assert_eq!(
            format!("{key:?} {jwt:?}"),
            "AppKey(<redacted>) Jwt(<redacted>)"
        );
    }
    // Whitespace around the block is not text outside it.
    let padded = written(
        &dir,
        "padded.pem",
        format!(
            "\n  \n{}\n\n",
            std::fs::read_to_string(fixture("test-pkcs8.pem")).unwrap()
        )
        .as_bytes(),
    );
    AppKey::load(&padded).unwrap();
    // A read-only key is as good.
    AppKey::load(&key_file(&dir, "test-pkcs8.pem", 0o400)).unwrap();
}

#[test]
fn an_encrypted_a_non_rsa_or_a_malformed_key_is_refused_without_repeating_it() {
    let dir = TestDir::new("bad-keys");
    let pkcs1 = std::fs::read_to_string(fixture("test-pkcs1.pem")).unwrap();
    let body = pkcs1.lines().nth(1).unwrap().to_owned();
    let cases: Vec<(PathBuf, &str)> = vec![
        (key_file(&dir, "test-encrypted.pem", 0o600), "encrypted"),
        (
            key_file(&dir, "test-pkcs1-encrypted.pem", 0o600),
            "encrypted",
        ),
        (key_file(&dir, "test-ec.pem", 0o600), "not an RSA private key"),
        (
            written(
                &dir,
                "openssh",
                b"-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjE=\n-----END OPENSSH PRIVATE KEY-----\n",
            ),
            "not an RSA private key",
        ),
        (
            written(
                &dir,
                "not-base64",
                b"-----BEGIN RSA PRIVATE KEY-----\nsentinel!!not*base64\n-----END RSA PRIVATE KEY-----\n",
            ),
            "not base64",
        ),
        (
            written(
                &dir,
                "not-der",
                format!(
                    "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
                    base64::engine::general_purpose::STANDARD.encode("sentinel bytes, not DER")
                )
                .as_bytes(),
            ),
            "not an RSA private key",
        ),
        (
            written(
                &dir,
                "cut",
                format!("-----BEGIN RSA PRIVATE KEY-----\n{body}\n").as_bytes(),
            ),
            "does not end",
        ),
        (written(&dir, "token", TOKEN.as_bytes()), "not a PEM file"),
        (
            written(&dir, "trailing", format!("{pkcs1}sentinel trailing text\n").as_bytes()),
            "text after the PEM block",
        ),
        (
            written(&dir, "two-blocks", format!("{pkcs1}\n{pkcs1}").as_bytes()),
            "text after the PEM block",
        ),
        (
            written(&dir, "leading", format!("sentinel leading text\n{pkcs1}").as_bytes()),
            "text before the PEM block",
        ),
        (written(&dir, "binary", &[0xff, 0xfe, 0x00, 0x80]), "not a PEM file"),
    ];
    for (path, says) in cases {
        let error = AppKey::load(&path).unwrap_err();
        assert!(
            matches!(error, KeyError::Unusable { .. }),
            "{}: {error:?}",
            path.display()
        );
        let text = format!("{error} {error:?}");
        assert!(text.contains(says), "{}: {text}", path.display());
        assert!(
            !text.contains("sentinel") && !text.contains(&body[..16]) && !text.contains("MII"),
            "the error repeats nothing of the file: {text}"
        );
    }
}

#[test]
fn a_key_file_others_may_read_a_link_a_directory_or_a_large_file_is_refused() {
    let dir = TestDir::new("key-files");
    for mode in [0o644, 0o640, 0o604, 0o660] {
        let path = key_file(&dir, "test-pkcs1.pem", mode);
        let error = AppKey::load(&path).unwrap_err();
        assert!(
            matches!(error, KeyError::Shared { mode: found, .. } if found == mode),
            "mode {mode:o}: {error:?}"
        );
        assert!(error.to_string().contains("chmod 600"), "{error}");
        // Nothing here changes its mode.
        let after = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(after, mode, "the key's mode is left as it was");
    }
    let target = key_file(&dir, "test-pkcs8.pem", 0o600);
    let link = dir.0.join("link.pem");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(
        matches!(AppKey::load(&link), Err(KeyError::Symlink { .. })),
        "a link at the path's end is refused"
    );
    let directory = dir.0.join("keys");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        matches!(AppKey::load(&directory), Err(KeyError::NotFile { .. })),
        "a directory is no key"
    );
    let large = written(&dir, "large.pem", &vec![b'A'; 70 * 1024]);
    assert!(
        matches!(AppKey::load(&large), Err(KeyError::TooLarge { .. })),
        "a file past 64 KiB is not read"
    );
    assert!(
        matches!(
            AppKey::load(&dir.0.join("missing.pem")),
            Err(KeyError::Open { .. })
        ),
        "a missing file cannot be opened"
    );
}

/// A request the fake GitHub received.
#[derive(Debug, Clone)]
struct Seen {
    method: Method,
    uri: String,
    headers: HeaderMap,
}

type Handler = dyn Fn(&Seen) -> Response + Send + Sync;

/// Serves `handler` on a loopback port in place of api.github.com; returns its URL and what it received.
async fn fake_github(
    handler: impl Fn(&Seen) -> Response + Send + Sync + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler: Arc<Handler> = Arc::new(handler);
    let log = seen.clone();
    let app = Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap| {
        let request = Seen {
            method,
            uri: uri.to_string(),
            headers,
        };
        log.lock().unwrap().push(request.clone());
        let response = handler(&request);
        async move { response }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// GitHub's answer to a token request: `TOKEN`, expiring `lasts` after now, with `permissions`.
fn minted(lasts: Duration, permissions: &str) -> Response {
    let expires =
        crate::time::rfc3339(crate::time::now_ms() + u64::try_from(lasts.as_millis()).unwrap());
    (
        StatusCode::CREATED,
        [("content-type", "application/json")],
        format!(
            r#"{{"token":"{TOKEN}","expires_at":"{expires}","permissions":{permissions},"repository_selection":"selected"}}"#
        ),
    )
        .into_response()
}

const ALL: &str = r#"{"administration":"write","actions":"write","contents":"read","pull_requests":"read","metadata":"read"}"#;

fn app(url: &str) -> App {
    App::with_key(
        APP_ID,
        INSTALLATION,
        test_key(),
        url,
        Duration::from_secs(10),
    )
    .unwrap()
}

#[tokio::test]
async fn a_token_is_minted_with_a_signed_jwt_and_lasts_as_github_says() {
    let (url, seen) = fake_github(|request| match request.uri.as_str() {
        MINT_PATH => minted(Duration::from_hours(1), ALL),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let app = app(&url);
    let minted = app.mint().await.unwrap();
    assert_eq!(minted.token.token.0, TOKEN);
    let lasts = minted
        .token
        .expires_at
        .duration_since(SystemTime::now())
        .unwrap();
    assert!(
        lasts > Duration::from_mins(58) && lasts <= Duration::from_hours(1),
        "lasts an hour: {lasts:?}"
    );
    assert_eq!(minted.permissions["actions"], Granted::Level(Level::Write));
    let seen = seen.lock().unwrap().clone();
    let [request] = seen.as_slice() else {
        panic!("one request: {seen:?}");
    };
    assert_eq!(request.method, Method::POST);
    let jwt = request.headers["authorization"]
        .to_str()
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap();
    assert_jwt(jwt, &app.key, now_secs());
}

#[tokio::test]
async fn a_token_lasts_as_long_as_githubs_clock_says_whatever_this_hosts_says() {
    // GitHub's clock is two hours ahead of this host's: its token lasts an hour from GitHub's now.
    let (url, _) = fake_github(|_| {
        let ahead = crate::time::now_ms() + 2 * 3_600_000;
        let expires = crate::time::rfc3339(ahead + 3_600_000);
        let date = httpdate(ahead);
        (
            StatusCode::CREATED,
            [
                ("content-type", "application/json"),
                ("date", date.as_str()),
            ],
            format!(r#"{{"token":"{TOKEN}","expires_at":"{expires}"}}"#),
        )
            .into_response()
    })
    .await;
    let minted = app(&url).mint().await.unwrap();
    let lasts = minted
        .token
        .expires_at
        .duration_since(SystemTime::now())
        .unwrap();
    assert!(
        lasts > Duration::from_mins(58) && lasts <= Duration::from_hours(1),
        "an hour, not three: {lasts:?}"
    );
}

/// `ms` since the epoch as an HTTP `Date`.
fn httpdate(ms: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let rfc = crate::time::rfc3339(ms);
    let (date, time) = rfc.trim_end_matches('Z').split_once('T').unwrap();
    let mut parts = date.split('-');
    let (year, month, day) = (
        parts.next().unwrap(),
        parts.next().unwrap().parse::<usize>().unwrap(),
        parts.next().unwrap(),
    );
    let weekday = DAYS[usize::try_from(ms / 86_400_000 % 7).unwrap()];
    format!("{weekday}, {day} {} {year} {time} GMT", MONTHS[month - 1])
}

#[tokio::test]
async fn a_token_github_refuses_or_answers_badly_is_an_error_that_repeats_nothing_it_sent() {
    let body = format!(r#"{{"message":"sentinel {TOKEN}","token":"{TOKEN}"}}"#);
    let past = crate::time::rfc3339(crate::time::now_ms() - 60_000);
    let cases: Vec<(Response, &str)> = vec![
        (
            (StatusCode::UNAUTHORIZED, body.clone()).into_response(),
            "401",
        ),
        ((StatusCode::FORBIDDEN, body.clone()).into_response(), "403"),
        (
            (
                StatusCode::TOO_MANY_REQUESTS,
                [("retry-after", "60")],
                body.clone(),
            )
                .into_response(),
            "429",
        ),
        (
            (StatusCode::INTERNAL_SERVER_ERROR, body.clone()).into_response(),
            "500",
        ),
        (
            (StatusCode::CREATED, format!("sentinel {TOKEN} not json")).into_response(),
            "does not parse",
        ),
        (
            (
                StatusCode::CREATED,
                format!(r#"{{"token":"{TOKEN}","expires_at":12}}"#),
            )
                .into_response(),
            "does not parse",
        ),
        (
            (
                StatusCode::CREATED,
                format!(r#"{{"token":"{TOKEN}","expires_at":"sentinel"}}"#),
            )
                .into_response(),
            "expiry that does not parse",
        ),
        (
            (
                StatusCode::CREATED,
                format!(r#"{{"token":"{TOKEN}","expires_at":"{past}"}}"#),
            )
                .into_response(),
            "already expired",
        ),
    ];
    for (response, says) in cases {
        let response = Arc::new(Mutex::new(Some(response)));
        let (url, _) = fake_github(move |_| {
            response
                .lock()
                .unwrap()
                .take()
                .unwrap_or_else(|| StatusCode::GONE.into_response())
        })
        .await;
        let error = app(&url).mint().await.err().unwrap();
        let text = format!("{error} {error:?}");
        assert!(text.contains(says), "{says}: {text}");
        assert!(
            !text.contains("sentinel") && !text.contains("ghs_"),
            "nothing GitHub sent shows: {text}"
        );
    }
}

#[tokio::test]
async fn a_token_request_follows_no_redirect_and_is_given_up_in_time() {
    let (url, seen) = fake_github(|request| match request.uri.as_str() {
        MINT_PATH => (StatusCode::TEMPORARY_REDIRECT, [("location", "/elsewhere")]).into_response(),
        _ => minted(Duration::from_hours(1), ALL),
    })
    .await;
    let error = app(&url).mint().await.err().unwrap();
    assert!(error.to_string().contains("307"), "{error}");
    let uris: Vec<String> = seen.lock().unwrap().iter().map(|s| s.uri.clone()).collect();
    assert_eq!(uris, [MINT_PATH], "the redirect is not followed");

    let app = Router::new().fallback(|| async {
        tokio::time::sleep(Duration::from_secs(30)).await;
        StatusCode::CREATED
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let slow = App::with_key(
        APP_ID,
        INSTALLATION,
        test_key(),
        &url,
        Duration::from_millis(200),
    )
    .unwrap();
    let error = slow.mint().await.err().unwrap();
    assert!(error.to_string().contains("no answer in time"), "{error}");
}

#[tokio::test]
async fn the_token_exchange_never_moves_the_installations_rate_limit_counts_or_backoff() {
    let reset = (now_secs() + 3600).to_string();
    let reset_for_mint = reset.clone();
    let (url, _) = fake_github(move |request| match request.uri.as_str() {
        MINT_PATH => {
            let mut response = minted(Duration::from_hours(1), ALL);
            let headers = response.headers_mut();
            for (name, value) in [
                ("x-ratelimit-limit", "5000"),
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", reset_for_mint.as_str()),
                ("x-ratelimit-resource", "core"),
                ("retry-after", "3600"),
            ] {
                headers.insert(name, value.parse().unwrap());
            }
            response
        }
        "/repos/o/r" => (
            [
                ("content-type", "application/json"),
                ("x-ratelimit-limit", "5000"),
                ("x-ratelimit-remaining", "4999"),
                ("x-ratelimit-reset", reset.as_str()),
                ("x-ratelimit-resource", "core"),
            ],
            r#"{"default_branch":"main","visibility":"private"}"#,
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let github = Github::new_app(&url, Arc::new(app(&url))).unwrap();
    github.authenticate().await.unwrap();
    assert_eq!(
        github.blocked_until(),
        None,
        "the mint's headers hold nothing"
    );
    assert!(
        github.requests().is_empty(),
        "the mint is not counted as the installation's"
    );
    assert_eq!(
        crate::github::GithubApi::spend(&github).rate_limit,
        None,
        "the mint's limit is not the installation's"
    );
    let _: serde_json::Value = github.get("/repos/o/r").await.unwrap();
    let spend = crate::github::GithubApi::spend(&github);
    assert_eq!(
        spend.rate_limit.map(|limit| limit.remaining),
        Some(4999),
        "the installation's own answers are read as before"
    );
    assert_eq!(github.requests().tally(Class::Repository).total(), 1);
    assert_eq!(github.blocked_until(), None);
}

#[tokio::test]
async fn requests_that_find_no_token_at_once_share_one_mint_and_send_the_token_it_gave() {
    let mints = Arc::new(AtomicUsize::new(0));
    let counted = mints.clone();
    let (url, seen) = fake_github(move |request| match request.uri.as_str() {
        MINT_PATH => {
            counted.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(100));
            minted(Duration::from_hours(1), ALL)
        }
        _ => ([("content-type", "application/json")], "{}").into_response(),
    })
    .await;
    let github = Arc::new(Github::new_app(&url, Arc::new(app(&url))).unwrap());
    let reads = (0..8).map(|n| {
        let github = github.clone();
        tokio::spawn(async move {
            github
                .get::<serde_json::Value>(&format!("/repos/o/r{n}"))
                .await
        })
    });
    for read in futures_util::future::join_all(reads).await {
        read.unwrap().unwrap();
    }
    assert_eq!(mints.load(Ordering::SeqCst), 1, "one mint for all eight");
    let seen = seen.lock().unwrap();
    let bearers: Vec<&str> = seen
        .iter()
        .filter(|request| request.uri != MINT_PATH)
        .map(|request| request.headers["authorization"].to_str().unwrap())
        .collect();
    assert_eq!(bearers.len(), 8);
    assert!(
        bearers
            .iter()
            .all(|bearer| *bearer == format!("Bearer {TOKEN}")),
        "each sends the minted token"
    );
}

#[tokio::test]
async fn the_probe_reads_permissions_and_every_page_of_the_installations_repositories_and_writes_nothing()
 {
    let (url, seen) = fake_github(move |request| match request.uri.as_str() {
        MINT_PATH => minted(
            Duration::from_hours(1),
            r#"{"administration":"write","actions":"read","contents":"read","metadata":"read"}"#,
        ),
        "/installation/repositories?per_page=100&page=1" => (
            [
                ("content-type", "application/json"),
                (
                    "link",
                    "<https://api.github.com/installation/repositories?per_page=100&page=2>; rel=\"next\"",
                ),
            ],
            r#"{"total_count":2,"repositories":[{"full_name":"Halyard/Ledger","visibility":"private"}]}"#,
        )
            .into_response(),
        "/installation/repositories?per_page=100&page=2" => (
            [("content-type", "application/json")],
            r#"{"total_count":2,"repositories":[{"full_name":"halyard/site","visibility":"public"}]}"#,
        )
            .into_response(),
        // Public, readable with any token, and not the installation's.
        "/repos/halyard/public-tools" => ([("content-type", "application/json")], "{}").into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    })
    .await;
    let names = [
        "halyard/ledger",
        "halyard/site",
        "halyard/public-tools",
        "halyard/ledger",
    ]
    .map(str::to_owned);
    let installation = app(&url).probe(&names).await.unwrap();
    assert_eq!(
        installation.repositories,
        BTreeMap::from([
            (
                "halyard/ledger".to_owned(),
                Coverage::Covered(Some(Visibility::Private))
            ),
            ("halyard/public-tools".to_owned(), Coverage::Uncovered),
            (
                "halyard/site".to_owned(),
                Coverage::Covered(Some(Visibility::Public))
            ),
        ])
    );
    assert_eq!(installation.uncovered(), ["halyard/public-tools"]);
    assert_eq!(
        installation.missing(),
        [
            Missing {
                name: "actions",
                needed: Level::Write,
                has: Granted::Level(Level::Read),
            },
            Missing {
                name: "pull_requests",
                needed: Level::Read,
                has: Granted::Nothing,
            },
        ]
    );
    let seen = seen.lock().unwrap();
    let writes: Vec<&Seen> = seen
        .iter()
        .filter(|request| request.method != Method::GET && request.uri != MINT_PATH)
        .collect();
    assert!(writes.is_empty(), "only the mint is not a read: {writes:?}");
    for request in seen.iter().filter(|request| request.uri != MINT_PATH) {
        assert_eq!(
            request.headers["authorization"].to_str().unwrap(),
            format!("Bearer {TOKEN}"),
            "read with the installation's token"
        );
    }
    assert!(
        !format!("{installation:?}").contains(TOKEN),
        "the probe keeps no token"
    );
}

#[tokio::test]
async fn an_installation_with_every_permission_lacks_none_and_admin_counts_as_write() {
    let (url, _) = fake_github(|request| match request.uri.as_str() {
        MINT_PATH => minted(
            Duration::from_hours(1),
            r#"{"administration":"admin","actions":"write","contents":"write","pull_requests":"read"}"#,
        ),
        _ => ([("content-type", "application/json")], r#"{"repositories":[]}"#).into_response(),
    })
    .await;
    let installation = app(&url).probe(&[]).await.unwrap();
    assert_eq!(installation.missing(), []);
}

#[tokio::test]
async fn words_github_sends_that_runnerd_does_not_know_are_never_kept() {
    let (url, _) = fake_github(|request| match request.uri.as_str() {
        MINT_PATH => minted(
            Duration::from_hours(1),
            r#"{"administration":"write","actions":"sentinel-ghs_leak","contents":{"sentinel":1},"pull_requests":"read","sentinel_extra":"ghs_leak"}"#,
        ),
        _ => (
            [("content-type", "application/json")],
            r#"{"repositories":[{"full_name":"o/r","visibility":"sentinel-ghs_leak"}]}"#,
        )
            .into_response(),
    })
    .await;
    let installation = app(&url).probe(&["o/r".to_owned()]).await.unwrap();
    assert_eq!(installation.permissions["actions"], Granted::Unrecognized);
    assert_eq!(installation.permissions["contents"], Granted::Unrecognized);
    assert_eq!(
        installation.repositories["o/r"],
        Coverage::Covered(None),
        "a visibility runnerd does not know is none"
    );
    let shown = format!("{installation:?} {:?}", installation.missing());
    assert!(
        !shown.contains("sentinel") && !shown.contains("ghs_"),
        "nothing GitHub sent is kept: {shown}"
    );
}
