//! `kind = "discord"`: posts Discord's execute-webhook JSON, one embed with the title, the message and the link,
//! coloured by event, that pings no one. Knows nothing of other kinds.

use runnerd_core::api::{Notification, NotifyEvent};
use runnerd_core::config::Secret;
use serde_json::json;

use super::send::{Post, Sender};

/// The most characters Discord takes in an embed's title and description. Together they stay under the 6000 an
/// embed may hold in all.
const TITLE: usize = 256;
const DESCRIPTION: usize = 4096;

/// A Discord channel's webhook at a secret URL. Discord answers 204 when it took the message, and 429 when it
/// limits the rate, which is retried like any failed send.
pub(super) struct Discord {
    url: Secret,
}

impl Discord {
    pub(super) fn new(url: Secret) -> Discord {
        Discord { url }
    }
}

impl Sender for Discord {
    fn post(&self, notification: &Notification) -> Result<Post, String> {
        let color = match notification.event {
            NotifyEvent::Failed => 0x00ED_4245,
            NotifyEvent::Lost | NotifyEvent::Runner => 0x00FE_A73C,
            NotifyEvent::Insights | NotifyEvent::Digest => 0x0099_AAB5,
            NotifyEvent::Test => 0x0057_F287,
        };
        let mut embed = json!({
            "title": shorten(&notification.title, TITLE),
            "color": color,
        });
        if !notification.message.is_empty() {
            embed["description"] = json!(shorten(&notification.message, DESCRIPTION));
        }
        if let Some(url) = &notification.url {
            embed["url"] = json!(url);
        }
        // Mentions off: a job or branch named `@everyone` pings no one.
        let body = json!({
            "embeds": [embed],
            "allowed_mentions": { "parse": [] },
        });
        Ok(Post {
            url: self.url.expose().to_owned(),
            body: serde_json::to_vec(&body).map_err(|encode| encode.to_string())?,
        })
    }
}

/// `text` cut to at most `most` characters, ending in `…` when it was cut.
fn shorten(text: &str, most: usize) -> String {
    if text.chars().count() <= most {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(most - 1).collect();
    short.push('…');
    short
}
