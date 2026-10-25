# 0064. Each notification target kind is a sender of its own

Status: **Accepted** (2026-10-25). Amends [0051](0051-insights-and-notifications.md).

## Context

ADR 0051 gave `[notify]` two kinds of target, `webhook` and `ntfy`. The daemon's notifier built each request in one
`match` on the kind, inside the module that also folds events into messages, keeps each target's cursor and
retries. Every new kind would grow that match and put a third service's JSON beside the scheduling of messages.

## Decision

- **A kind is a `Sender`.** `api/notify/send.rs` defines the trait: a sender turns an `api::Notification` into the
  post its kind takes, a URL and a JSON body, and reads whether the status answered means delivered (by default,
  any 2xx). Each kind implements it in its own module beside it (`webhook.rs`, `ntfy.rs`) and knows nothing of the
  others or of the notifier.
- **The notifier holds senders, not kinds.** Each target keeps the sender built from its config, rebuilt when a
  reload changes the target; `runnerctl notify test` builds one per target. The only `match` on the kind is the one
  that builds the sender.
- **What every kind shares stays in one place.** The HTTP client (no redirects, the daemon's TLS provider, the
  timeout), the target's headers, the content type and the wording of a failed send are `send.rs`'s, so no sender
  can log a URL or a header, and a failed send of any kind is retried with the same backoff.

## Consequences

- A new kind is a module and one line where senders are built; the notifier's folding, cursors and retries do not
  change.
- Config stays a closed enum (`config::NotifyKind`): a kind is validated when the config is read, and an older
  daemon refuses a kind it does not know rather than sending it as another.
