# 0182. A GitHub App installation can authenticate runnerd

Status: **Accepted** (2026-11-26). Amends [0008](0008-auth-with-the-gh-token.md) and
[0147](0147-runners-read-verified-action-archives-from-runnerds-cache.md). Builds on
[0181](0181-credentials-and-public-reads-are-separate-domains.md). A config change: `[github.app]`. No wire or schema
change.

## Context

`gh`'s token is a person's: it carries every scope they gave `gh`, and its rate limit is theirs, shared with
everything else they run. A GitHub App installation's token is scoped to the repositories and permissions the App is
given, and its limit is the installation's (at least 5000 an hour). ADR 0181 split the token's lifecycle and the
archives' public reads so an expiring, scoped token can be used.

## Decision

- **`[github.app]` is the switch.** `app_id`, `installation_id` and `private_key`, all required, no other key. Without
  the table runnerd uses `gh auth token` as before; with it, the App only: no `gh` command runs, and nothing falls back
  to `gh`. `Config::validate` refuses a zero ID and a `private_key` that is not an absolute path, or that holds a key
  or a token, without repeating it. The config holds the path, never the key.
- **The key is read once, at start, and checked.** `github::app::AppKey::load` opens the file without following a
  symbolic link at its end and checks the opened file: a regular file, owned by the daemon's effective user, no group
  or other permission bits (0600 or 0400), at most 64 KiB. It never changes the mode. It takes GitHub's PEM, PKCS#1
  (`RSA PRIVATE KEY`) or PKCS#8 (`PRIVATE KEY`), and refuses an encrypted key, another key type and anything that does
  not parse, naming the file and never its content. ring signs (its `alloc` feature, named in `runnerd-core`'s
  manifest); `base64` decodes PEM and encodes the JWT. No new crypto crate.
- **A token is minted on a path of its own.** An RS256 JWT (`iat` a minute back, `exp` nine minutes later, `iss` the
  App's ID) is exchanged by `POST /app/installations/{id}/access_tokens` on `App`'s own client: no redirect followed,
  10 s at most, under the 15 s the token's read is given. Its answer never reaches `Github`'s rate-limit reading, its
  counts, its backoff or its kept answers. The token's life is `expires_at` less GitHub's `Date`, so a host clock that
  is off does not move it, then ADR 0181's rules: kept until 5 minutes before it ends, an hour at most, a failed mint
  backed off. An error says what failed and GitHub's status, never GitHub's words, the key, the JWT or the token.
- **The daemon starts only with a first token.** `runnerd serve` reads the key and mints before the engine starts, and
  stops with the reason when either fails: a daemon that cannot register runners does not run. Archive lookups of
  pinned actions are anonymous (`PublicReads::Anonymous`).
- **Auth changes at a restart only.** The key and the client are the start's. A save or a reload whose `[github.app]`
  differs from what the daemon started with, rather than from the text saved before, warns that it takes effect at
  the next start, on every save until then. The doctor's cached checks read the saved config, so when its auth is not
  the running one they carry a `GitHub auth` warning that says so. A save never reads the key file.
- **The doctor checks an App without `gh` and without writing.** It reads the key as the start does, mints a token,
  checks its permissions, lists every page of `GET /installation/repositories` (at most 1000 repositories), and reads
  each configured repository's coverage and visibility from that listing. Membership never comes from
  `GET /repos/{repo}`: an installation token may read a public repository the installation does not cover. It fails
  a missing permission and an uncovered repository. Nothing it sends writes: no trial registration, cancel or re-run.
- **The permissions are repository permissions.** Administration: write (runner registration and deletion), Actions:
  write (cancel and re-run, and a forced pause cancels runs even with `job_actions = false`), Contents: read (branch
  heads, compares, the archives' refs, trees and blobs), Pull requests: read (the build cache's trust checks), and
  Metadata: read, which every App has. No organization permission.

## Consequences

- One installation's limit is shared by every daemon that uses it: two hosts with one installation share it.
- Pinned actions' lookups share GitHub's 60 anonymous requests an hour per address (ADR 0181): the action archives
  serve less until lookups are spaced out.
- Rotating the key, or switching between `gh` and an App, needs a restart. A repository or permission added to the
  installation reaches the daemon with its next token, within the hour: GitHub scopes a token when it mints it.
- A repository still draining from an older config is not checked against the installation: switching to an App that
  does not cover it may leave its runners for GitHub to remove.
- An older runnerd refuses a config with `[github.app]`; remove the table to go back.
- The key's bytes stay in memory for the daemon's life, inside ring's key type; runnerd does not claim to erase them.
