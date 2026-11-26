---
title: How do I run runnerd as a GitHub App?
description: Create an App with four repository permissions, install it on the repositories you serve, save its key with chmod 600 and set [github.app]; runnerd then never runs gh.
---

By default runnerd uses `gh auth token`: your own token, with every scope you gave `gh`, and your own rate limit. A GitHub App's installation token reaches only the repositories you install the App on, with only the permissions you give it, and its rate limit is the installation's, at least 5000 requests an hour.

## Create the App

On GitHub, open **Settings > Developer settings > GitHub Apps > New GitHub App** for your account, or the organization's settings for an organization's repositories. Give it a name and a homepage URL, and turn **Webhook** off: runnerd polls.

Under **Repository permissions**, set:

| Permission | Access | runnerd uses it to |
|---|---|---|
| Administration | Read and write | register and remove runners |
| Actions | Read and write | read runs and jobs, cancel and re-run them; a [forced pause](../../reference/api/#routes) cancels runs even with `job_actions = false` |
| Contents | Read-only | compare a job's commit with its branch, and read the files the [action archives](../caches/) need |
| Pull requests | Read-only | check who opened a pull request before its job may write to the build cache |
| Metadata | Read-only | given to every App |

No organization or account permission is needed. Create the App and note its **App ID**.

## Install it and save its key

Under **Install App**, install it on the account that owns the repositories and choose **Only select repositories**: the ones in your `[[repositories]]`. The installation's settings URL ends with its ID, such as `.../settings/installations/789012`.

On the App's page, **Generate a private key**. Move the downloaded `.pem` where the user runnerd runs as owns it, and let no one else read it:

```sh
mv ~/Downloads/runnerd-halyard.*.private-key.pem ~/.config/runnerd/github-app.pem
chmod 600 ~/.config/runnerd/github-app.pem
```

runnerd refuses a key that others may read, a symbolic link, an encrypted key, or a file another user owns. It never changes the file's mode itself.

## Set it

```toml
[github.app]
app_id = 123456
installation_id = 789012
private_key = "/home/dana/.config/runnerd/github-app.pem"
```

`private_key` is the file's absolute path; runnerd refuses a key or a token written in its place. Check it, then restart:

```sh
runnerd doctor
systemctl --user restart runnerd
```

At start, runnerd checks that the installation covers every repository it serves and every one it is still draining runners from, and does not start otherwise. A save that adds a repository the installation does not cover is refused.

With `[github.app]` set, `runnerd doctor` runs no `gh` command. It mints a token, checks the four permissions, and lists the installation's repositories to check that it covers each one you serve; it sends nothing that writes. At start the daemon mints its first token and does not start if it cannot.

## What changes

- The daemon reads the key and chooses between `gh` and the App only when it starts. Saving `[github.app]` from the dashboard or a reload warns that it takes effect at the next restart, and the doctor's checks say when the saved config is not what the daemon runs with. Restart after rotating the key, too.
- A repository you add to the installation, or a permission you accept, reaches runnerd with its next token, within the hour.
- Every host that uses one installation shares its rate limit.
- The [action archives](../caches/) look up each pinned action's repository without a token, within GitHub's 60 anonymous requests an hour per address. With many pinned repositories, runners get no archives until every lookup is answered; the archives already kept stay.
- An older runnerd refuses a config with `[github.app]`: remove the table to go back to `gh`.
