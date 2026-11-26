//! How the daemon authenticates to GitHub (ADR 0182): with `gh auth token`, or, when the config has `[github.app]`,
//! as that GitHub App's installation and never with `gh`. Chosen once at start: a reload or a save changes what the
//! next start uses, not the running daemon, so a save is compared with the start's choice, never with the text saved
//! before it. Under an App the installation must cover what the daemon serves: every configured and draining
//! repository at start, and each repository a reload adds before the reload is taken.
//!
//! It must not hold a token: `github::Github` does. The App's key is read at start, and by the doctor's own check.

use std::sync::Arc;
use std::time::Duration;

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

/// What the running daemon authenticates with, as it started: with the App's installation when it is one, which a
/// reload asks whether it covers a repository the reload adds.
#[derive(Debug, Clone, Default)]
pub(crate) enum Active {
    /// `gh auth token`: it sees every repository its user may.
    #[default]
    Gh,
    /// A GitHub App's installation.
    App { config: GithubApp, app: Arc<App> },
}

impl Active {
    /// What it is, to compare with a saved config's.
    pub(crate) fn auth(&self) -> Auth {
        match self {
            Active::Gh => Auth::Gh,
            Active::App { config, .. } => Auth::App(config.clone()),
        }
    }

    /// Why the daemon may not serve `added`, repositories a reload would add, if it may not: under an App, those the
    /// installation does not cover, or that it could not be asked about. Nothing under `gh`.
    pub(crate) async fn refuses(&self, added: &[String]) -> Option<String> {
        let Active::App { app, .. } = self else {
            return None;
        };
        if added.is_empty() {
            return None;
        }
        match covered(app, added).await {
            Err(error) => Some(format!(
                "cannot check that the GitHub App's installation covers {}: {error}; not saved",
                added.join(", ")
            )),
            Ok(uncovered) if uncovered.is_empty() => None,
            Ok(uncovered) => Some(format!(
                "the GitHub App's installation does not cover {}, so runnerd cannot register runners there; add \
                 them to its repository access, then save again",
                uncovered.join(", ")
            )),
        }
    }
}

/// How long checking an installation's coverage may take: as long as the doctor's reads.
const COVERAGE_DEADLINE: Duration = crate::doctor::READ_DEADLINE;

/// Which of `repositories` `app`'s installation does not cover, read within [`COVERAGE_DEADLINE`].
async fn covered(app: &App, repositories: &[String]) -> Result<Vec<String>, String> {
    match tokio::time::timeout(COVERAGE_DEADLINE, app.probe(repositories)).await {
        Ok(Ok(installation)) => Ok(installation
            .uncovered()
            .into_iter()
            .map(str::to_owned)
            .collect()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!(
            "GitHub did not answer within {}s",
            COVERAGE_DEADLINE.as_secs()
        )),
    }
}

/// The GitHub client `config` authenticates with, on `base` (`github::API_URL`, or a test server), with a token read
/// now, and what it is: a daemon that could not register runners does not start. `gh` as before. A GitHub App's key
/// is read from its file and checked, a first token minted, and the installation must cover every configured
/// repository and every one of `draining`, whose runners the daemon must still remove: one it does not is the
/// operator's to resolve before the switch.
pub(crate) async fn connect(
    config: &Config,
    base: &str,
    draining: &[String],
) -> Result<(Github, Active), String> {
    let Some(app_config) = &config.github.app else {
        // Read once now, so a missing login fails here rather than at the first poll.
        github::gh_token().map_err(|error| error.to_string())?;
        let github =
            Github::new(base, Box::new(github::gh_token)).map_err(|error| error.to_string())?;
        return Ok((github, Active::Gh));
    };
    let app = Arc::new(App::new(app_config, base).map_err(|error| error.to_string())?);
    let github = Github::new_app(base, app.clone()).map_err(|error| error.to_string())?;
    github
        .authenticate()
        .await
        .map_err(|error| format!("the GitHub App's first token: {error}"))?;
    let configured = config.repository_names();
    let mut asked = configured.clone();
    asked.extend(draining.iter().cloned());
    let uncovered = covered(&app, &asked).await.map_err(|error| {
        format!("cannot check which repositories the GitHub App's installation covers: {error}")
    })?;
    let lower = |names: &[String]| -> Vec<String> {
        names.iter().map(|name| name.to_ascii_lowercase()).collect()
    };
    let configured = lower(&configured);
    let (served, drains): (Vec<String>, Vec<String>) = uncovered
        .into_iter()
        .partition(|name| configured.contains(&name.to_ascii_lowercase()));
    if !served.is_empty() {
        return Err(format!(
            "the GitHub App's installation does not cover {}, so runnerd cannot register runners there: add them to \
             its repository access, or remove them from the config",
            served.join(", ")
        ));
    }
    if !drains.is_empty() {
        return Err(format!(
            "{} still drain runners the GitHub App's installation could not remove, as it does not cover them: add \
             them to its repository access, or start with gh (no [github.app]) until their drains end",
            drains.join(", ")
        ));
    }
    Ok((
        github,
        Active::App {
            config: app_config.clone(),
            app,
        },
    ))
}

#[cfg(test)]
mod tests;
