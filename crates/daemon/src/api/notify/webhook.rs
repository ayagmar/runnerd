//! `kind = "webhook"`: posts `api::Notification` as JSON to the target's URL. Knows nothing of other kinds.

use runnerd_core::api::Notification;
use runnerd_core::config::Secret;

use super::send::{Post, Sender};

/// A webhook at a secret URL.
pub(super) struct Webhook {
    url: Secret,
}

impl Webhook {
    pub(super) fn new(url: Secret) -> Webhook {
        Webhook { url }
    }
}

impl Sender for Webhook {
    fn post(&self, notification: &Notification) -> Result<Post, String> {
        Ok(Post {
            url: self.url.expose().to_owned(),
            body: serde_json::to_vec(notification).map_err(|encode| encode.to_string())?,
        })
    }
}
