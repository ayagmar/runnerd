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
  or a token, without repeating it. A `[github]` area that does not parse is refused in runnerd's own words with its
  line, never the parser's, which quote the text: a token or a key pasted there would show. The config holds the
  path, never the key.
- **The key is read once, at start, and checked.** `github::app::AppKey::load` opens the file without following a
  symbolic link at its end and checks the opened file: a regular file, owned by the daemon's effective user, no group
  or other permission bits (0600 or 0400), at most 64 KiB. It never changes the mode. It takes GitHub's PEM, PKCS#1
  (`RSA PRIVATE KEY`) or PKCS#8 (`PRIVATE KEY`), one block and only whitespace around it, and refuses an encrypted
  key, another key type, text before or after the block and anything that does not parse, naming the file and never
  its content. ring signs (its `alloc` feature, named in `runnerd-core`'s
  manifest); `base64` decodes PEM and encodes the JWT. No new crypto crate.
- **A token is minted on a path of its own.** An RS256 JWT (`iat` a minute back, `exp` nine minutes later, `iss` the
  App's ID) is exchanged by `POST /app/installations/{id}/access_tokens` on `App`'s own client: no redirect followed,
  10 s at most, under the 15 s the token's read is given. Its answer never reaches `Github`'s rate-limit reading, its
  counts, its backoff or its kept answers. The token's life is `expires_at` less GitHub's `Date`, so a host clock that
  is off does not move it, then ADR 0181's rules: kept until 5 minutes before it ends, an hour at most, a failed mint
  backed off. An error says what failed and GitHub's status, never GitHub's words, the key, the JWT or the token.
- **The daemon starts only with a first token and an installation that covers what it serves.** `runnerd serve` reads
  the key, mints, and lists the installation's repositories before the engine starts. It stops with the reason when
  any of that fails, when the installation does not cover a configured repository, and when it does not cover a
  repository the history says still drains, whose runners the daemon must remove: the operator adds it to the
  installation or starts with `gh` until the drain ends. A daemon that cannot register or remove its runners does not
  run.
- **A reload adds only a repository the running installation covers.** A save or reload that adds repositories asks
  the running installation, with a token minted for it, and is refused when it does not cover one or cannot be asked.
  So every repository served, and every one that drains from a later reload, is covered. Archive lookups of
  pinned actions are anonymous (`PublicReads::Anonymous`).
- **Auth changes at a restart only.** The key and the client are the start's. A save or a reload whose `[github.app]`
  differs from what the daemon started with, rather than from the text saved before, warns that it takes effect at
  the next start, on every save until then. The doctor's cached checks read the saved config, so when its auth is not
  the running one they carry a `GitHub auth` warning that says so. A save never reads the key file.
- **Nothing GitHub sends is repeated as it came.** The doctor's facts keep only what runnerd recognizes: a permission
  level is `read`, `write` (or `admin`), none or unrecognized, a visibility one of the three, and nothing else of the
  answers. Every error that keeps GitHub's message or a parser's words has what looks like a credential (GitHub's
  token prefixes, a JWT, a bearer's value, a PEM block) replaced by `<redacted>` and is cut at 300 characters.
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
- A daemon whose installation lost a repository it serves, or that GitHub cannot be asked about, does not start; a
  reload that adds a repository fails while GitHub cannot be asked. Coverage is read from at most 10 pages (1000
  repositories) within 20 s.
- An older runnerd refuses a config with `[github.app]`; remove the table to go back.
- The key's bytes stay in memory for the daemon's life, inside ring's key type; runnerd does not claim to erase them.
