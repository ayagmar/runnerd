//! How the daemon authenticates to GitHub (ADR 0182): with `gh auth token`, or, when the config has `[github.app]`,
//! as that GitHub App's installation and never with `gh`. Chosen once at start: a reload or a save changes what the
//! next start uses, not the running daemon, so a save is compared with the start's choice, never with the text saved
//! before it.
//!
//! It must not read the App's key anywhere but in [`connect`], at start, nor hold a token: `github::Github` does.

use runnerd_core::config::{Config, GithubApp};
use runnerd_core::github::{self, Github, app::App};

/// What the daemon authenticates with: what it started with, or what a saved config would start with.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum Auth {
    /// `gh auth token`.
    #[default]
    Gh,
    /// The GitHub App installation `[github.app]` names.
    App(GithubApp),
}

impl Auth {
    /// What `config` authenticates with.
    pub(crate) fn of(config: &Config) -> Auth {
        config.github.app.clone().map_or(Auth::Gh, Auth::App)
    }

    /// What the config of `text` authenticates with; `gh` for a text that does not parse, as no daemon starts on it.
    pub(crate) fn of_text(text: &str) -> Auth {
        Config::parse(text).map_or(Auth::Gh, |config| Auth::of(&config))
    }

    /// It in a few words, such as `GitHub App 123's installation 456`.
    pub(crate) fn words(&self) -> String {
        match self {
            Auth::Gh => "gh auth token".to_owned(),
            Auth::App(app) => format!(
                "GitHub App {}'s installation {}",
                app.app_id, app.installation_id
            ),
        }
    }

    /// Why a saved config's auth is not the running daemon's, if it is not: `self` is what runs.
    pub(crate) fn pending(&self, saved: &Auth) -> Option<String> {
        (self != saved).then(|| {
            format!(
                "GitHub authentication takes effect when runnerd restarts: it runs with {} until then, and with {} \
                 after",
                self.words(),
                saved.words()
            )
        })
    }
}

/// The GitHub client `config` authenticates with, on `base` (`github::API_URL`, or a test server), with a token read
/// now: a daemon that could not register runners does not start. `gh` as before; a GitHub App's key read from its
/// file and checked, and a first token minted.
pub(crate) async fn connect(config: &Config, base: &str) -> Result<Github, String> {
    match &config.github.app {
        None => {
            // Read once now, so a missing login fails here rather than at the first poll.
            github::gh_token().map_err(|error| error.to_string())?;
            Github::new(base, Box::new(github::gh_token)).map_err(|error| error.to_string())
        }
        Some(app) => {
            let app = App::new(app, base).map_err(|error| error.to_string())?;
            let github = Github::new_app(base, app).map_err(|error| error.to_string())?;
            github
                .authenticate()
                .await
                .map_err(|error| format!("the GitHub App's first token: {error}"))?;
            Ok(github)
        }
    }
}

#[cfg(test)]
mod tests;
