//! How a message reaches a `[notify]` target: the `Sender` each kind implements, one module per kind, and the
//! post they share. A sender turns an `api::Notification` into the URL and JSON body its kind takes, and reads
//! whether the answer means delivered; the post adds the target's headers and the daemon's HTTP client. Neither
//! logs, nor returns an error holding, a target's URL or a header's value.

use std::time::Duration;

use runnerd_core::api::Notification;
use runnerd_core::config::{NotifyKind, NotifyTarget};

use super::{ntfy, webhook};

/// What a sender posts: a JSON body to a URL. Not `Debug`: the URL is a secret.
pub(super) struct Post {
    /// Where it goes.
    pub url: String,
    /// The JSON it carries.
    pub body: Vec<u8>,
}

/// One kind of target: what it is sent for a notification, and what answer means it took it.
pub(super) trait Sender: Send + Sync {
    /// The post that sends `notification`; an error never holds the URL.
    fn post(&self, notification: &Notification) -> Result<Post, String>;

    /// Whether the target took the message, from the status it answered.
    fn took(&self, status: reqwest::StatusCode) -> Result<(), String> {
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("the target answered HTTP {status}"))
        }
    }
}

/// The sender for `target`'s kind.
pub(super) fn sender(target: &NotifyTarget) -> Box<dyn Sender> {
    match target.kind {
        NotifyKind::Webhook => Box::new(webhook::Webhook::new(target.url.clone())),
        NotifyKind::Ntfy => Box::new(ntfy::Ntfy::new(target.url.clone())),
    }
}

/// An HTTP client for targets, with the daemon's TLS provider and `timeout` per request. It follows no redirect:
/// a POST followed on a 301 or 302 arrives as a GET without its body, which a server can answer with 200, and its
/// headers would go on to the other host.
pub(super) fn client(timeout: Duration) -> Result<reqwest::Client, String> {
    runnerd_core::github::install_crypto_provider();
    reqwest::Client::builder()
        .user_agent(concat!("runnerd/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        // Built before any URL is set, so its error names none.
        .map_err(|build_error| build_error.to_string())
}

/// Posts `notification` to `target` through `sender`; the error says what failed in words that never hold the URL
/// or a header.
pub(super) async fn deliver(
    http: &reqwest::Client,
    sender: &dyn Sender,
    target: &NotifyTarget,
    notification: &Notification,
) -> Result<(), String> {
    let Post { url, body } = sender.post(notification)?;
    let mut request = http
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body);
    for (name, value) in &target.headers {
        request = request.header(name.as_str(), value.expose());
    }
    let response = request.send().await.map_err(|send| describe(&send))?;
    sender.took(response.status())
}

/// What failed, in words that hold neither the URL nor anything a TLS or DNS error might quote from it.
fn describe(send: &reqwest::Error) -> String {
    if send.is_timeout() {
        "the request timed out".to_owned()
    } else if send.is_connect() {
        "cannot connect to the target".to_owned()
    } else if send.is_builder() {
        "the url or a header cannot be sent".to_owned()
    } else {
        "the request failed".to_owned()
    }
}
