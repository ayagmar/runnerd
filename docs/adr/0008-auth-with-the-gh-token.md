# 0008. Authenticate with the `gh` CLI's token

## Context

Individuals running runnerd already have `gh` logged in. A GitHub App needs setup per user.

## Decision

The daemon reads its token from `gh auth token` (`github::gh_token`) and reads it again when GitHub answers 401. A GitHub App comes later.

## Consequences

Zero setup for individuals. The token has the user's full scopes, so it must never leave `github::Github` ([invariants](../../ARCHITECTURE.md#invariants)).
