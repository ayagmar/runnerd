# 0064. Each notification target kind is a sender of its own, and Discord is one

Status: **Accepted** (2026-10-25). Amends [0051](0051-insights-and-notifications.md).

## Context

ADR 0051 gave `[notify]` two kinds of target, `webhook` and `ntfy`. The daemon's notifier built each request in one
`match` on the kind, inside the module that also folds events into messages, keeps each target's cursor and
retries. Every new kind would grow that match and put a third service's JSON beside the scheduling of messages.

## Decision

- **A kind is a `Sender`.** `api/notify/send.rs` defines the trait: a sender turns an `api::Notification` into the
  post its kind takes, a URL and a JSON body; any 2xx answer means delivered, for every kind. Each kind implements it in its own module beside it (`webhook.rs`, `ntfy.rs`, `discord.rs`) and knows nothing of the
  others or of the notifier.
- **A sender is built from the target's config at each send.** The config stays the only copy of the URL, kind and
  headers, so a reload that changes them takes effect at the next send; `runnerctl notify test` sends the same way. The only `match` on the kind is the one
  that builds the sender.
- **What every kind shares stays in one place.** The HTTP client (no redirects, the daemon's TLS provider, the
  timeout), the target's headers, the content type and the wording of a failed send are `send.rs`'s, so no sender
  can log a URL or a header, and a failed send of any kind is retried with the same backoff.

- **Discord is a kind.** ADR 0051 left Discord to a relay in front of a webhook. Pointed at Discord directly, a
  webhook target gets `HTTP 400`: Discord's execute-webhook endpoint needs `content` or `embeds`. `kind =
  "discord"` posts one embed with the title, the message as its description and the link as its URL, coloured by
  event (failures red, lost jobs and runners amber, insights and the digest grey, a test green), and
  `allowed_mentions: {"parse": []}`, so a job or branch name never pings anyone. A title past Discord's 256
  characters or a message past 4096 is cut with `…` rather than refused; the two together stay under the 6000
  an embed may hold. Discord answers 204 when it took the message, a 2xx like any other; its 429 is a failed send,
  retried with the same backoff.

## Consequences

- A new kind is a module and one line where senders are built; the notifier's folding, cursors and retries do not
  change.
- Config stays a closed enum (`config::NotifyKind`): a kind is validated when the config is read, and an older
  daemon refuses a kind it does not know rather than sending it as another: 0.0.14 refuses a config with
  `kind = "discord"` as an unknown variant.
