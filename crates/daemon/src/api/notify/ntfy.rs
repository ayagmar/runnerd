//! `kind = "ntfy"`: publishes the title, message, priority and link as ntfy's JSON to the topic's server. Knows
//! nothing of other kinds.

use runnerd_core::api::{Notification, NotifyEvent, Priority};
use runnerd_core::config::Secret;
use serde_json::json;

use super::send::{Post, Sender};

/// An ntfy topic at a secret URL, `scheme://host/topic`.
pub(super) struct Ntfy {
    url: Secret,
}

impl Ntfy {
    pub(super) fn new(url: Secret) -> Ntfy {
        Ntfy { url }
    }
}

impl Sender for Ntfy {
    fn post(&self, notification: &Notification) -> Result<Post, String> {
        // Checked by `Config::validate`: `scheme://host/topic`.
        let (server, topic) = self
            .url
            .expose()
            .rsplit_once('/')
            .ok_or_else(|| "the url is not an ntfy topic's".to_owned())?;
        let priority = match notification.priority {
            Priority::Low => 2,
            Priority::Default => 3,
            Priority::High => 4,
        };
        let tag = match notification.event {
            NotifyEvent::Failed => "x",
            NotifyEvent::Lost | NotifyEvent::Runner => "warning",
            NotifyEvent::Insights => "chart_with_upwards_trend",
            NotifyEvent::Digest => "memo",
            NotifyEvent::Test => "white_check_mark",
        };
        let mut body = json!({
            "topic": topic,
            "title": notification.title,
            "message": notification.message,
            "priority": priority,
            "tags": [tag],
        });
        if let Some(click) = &notification.url {
            body["click"] = json!(click);
        }
        Ok(Post {
            url: format!("{server}/"),
            body: serde_json::to_vec(&body).map_err(|encode| encode.to_string())?,
        })
    }
}
