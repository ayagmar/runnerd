//! The actions a workflow or an action's metadata pins by commit: each step's `uses: owner/repo[/path]@<sha>`, read
//! from the parsed YAML as the runner reads it, never from a script's text. Pure functions over text; it fetches
//! nothing. A file it cannot parse is `Unreadable`, never read as pinning nothing. Never a tag or a branch, which can
//! move: an archive is kept under the commit it is, and only a pin names that commit.

/// The deepest an action may nest composite actions, as the runner allows it (`Constants.CompositeActionsMaxDepth`):
/// the pins of an archive are followed this many levels from a workflow.
pub(crate) const MAX_DEPTH: u32 = 9;

/// An action pinned by commit: what a runner asks an archive for.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub(crate) struct Pin {
    /// `owner/repo` as the pin writes it; GitHub's API names the archive (`ActionRepository::full_name`).
    pub(crate) repository: String,
    /// The directory of the action inside the repository, `""` for its root.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(crate) path: String,
    /// The commit, 40 lowercase hex digits.
    pub(crate) sha: String,
}

/// Whether `path` in a repository's tree is a file pins are read from: a workflow (`.github/workflows/*.yml` or
/// `.yaml`) or an action's metadata anywhere (`action.yml`, `action.yaml`).
pub(crate) fn read_for_pins(path: &str) -> bool {
    is_workflow(path) || is_action_file(path)
}

/// Whether `path` is a workflow: a `.yml` or `.yaml` file right in `.github/workflows/`.
fn is_workflow(path: &str) -> bool {
    path.strip_prefix(".github/workflows/").is_some_and(|name| {
        !name.contains('/')
            && std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "yml" || extension == "yaml")
    })
}

/// Whether `path` is an action's metadata file: `action.yml` or `action.yaml`, at the root or in a directory.
pub(crate) fn is_action_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "action.yml" || name == "action.yaml"
}

/// The directory an action file is in, `""` at the root: the path a pin names its action by.
pub(crate) fn action_dir(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Why a file's pins could not be read: what it says is not known, so what it pins is not either.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Unreadable {
    /// It is not YAML, or not within the parser's limits.
    #[error("it is not YAML a runner reads: {0}")]
    Yaml(String),
    /// It holds more than one YAML document.
    #[error("it holds more than one YAML document")]
    Documents,
    /// It is not UTF-8.
    #[error("it is not UTF-8")]
    Text,
}

/// What the file at `path` pins by commit: a workflow's steps' `uses`, or an action's composite steps'.
pub(crate) fn file_pins(path: &str, bytes: &[u8]) -> Result<Vec<Pin>, Unreadable> {
    if is_workflow(path) {
        workflow_pins(std::str::from_utf8(bytes).map_err(|_| Unreadable::Text)?)
    } else {
        action_file_pins(bytes)
    }
}

/// What an action's metadata file pins by commit.
pub(crate) fn action_file_pins(bytes: &[u8]) -> Result<Vec<Pin>, Unreadable> {
    action_pins(std::str::from_utf8(bytes).map_err(|_| Unreadable::Text)?)
}

/// Every action a workflow pins by commit: each `jobs.<job>.steps[].uses`, in order, each once. A job's own `uses`,
/// a reusable workflow, is no action archive.
pub(crate) fn workflow_pins(text: &str) -> Result<Vec<Pin>, Unreadable> {
    let Some(document) = document(text)? else {
        return Ok(Vec::new());
    };
    let jobs = document
        .get("jobs")
        .and_then(Yaml::as_mapping)
        .into_iter()
        .flat_map(|jobs| jobs.values());
    Ok(unique(jobs.flat_map(|job| steps_uses(job.get("steps")))))
}

/// Every action an action's metadata pins by commit: each `runs.steps[].uses` of a composite action, in order, each
/// once.
pub(crate) fn action_pins(text: &str) -> Result<Vec<Pin>, Unreadable> {
    let Some(document) = document(text)? else {
        return Ok(Vec::new());
    };
    let runs = document.get("runs");
    Ok(unique(steps_uses(runs.and_then(|runs| runs.get("steps")))))
}

use saphyr::{LoadableYamlNode as _, SafelyIndex as _, Yaml};

/// The one YAML document `text` holds, `None` for an empty file. saphyr bounds what its aliases may expand to.
fn document(text: &str) -> Result<Option<Yaml<'_>>, Unreadable> {
    let mut documents =
        Yaml::load_from_str(text).map_err(|error| Unreadable::Yaml(error.to_string()))?;
    match documents.len() {
        0 => Ok(None),
        1 => Ok(documents.pop()),
        _ => Err(Unreadable::Documents),
    }
}

/// The `uses` of each step of `steps`, a sequence of mappings, that is a string.
fn steps_uses<'yaml>(steps: Option<&'yaml Yaml<'_>>) -> impl Iterator<Item = &'yaml str> {
    steps
        .and_then(Yaml::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|step| step.get("uses").and_then(Yaml::as_str))
}

/// The pins of `values` that name an action by commit, in order, each once.
fn unique<'value>(values: impl Iterator<Item = &'value str>) -> Vec<Pin> {
    let mut found: Vec<Pin> = Vec::new();
    for value in values {
        if let Some(pin) = pin(value.trim())
            && !found.contains(&pin)
        {
            found.push(pin);
        }
    }
    found
}

/// A pin from a `uses:` value, if it names a repository's action by commit.
fn pin(value: &str) -> Option<Pin> {
    let (name, reference) = value.rsplit_once('@')?;
    if !commit(reference) {
        return None;
    }
    let mut parts = name.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let path: Vec<&str> = parts.collect();
    if !segment(owner) || !segment(repo) || !path.iter().all(|part| segment(part)) {
        return None;
    }
    let path = path.join("/");
    let workflow = path == ".github/workflows" || path.starts_with(".github/workflows/");
    if workflow {
        return None;
    }
    Some(Pin {
        repository: format!("{owner}/{repo}"),
        path,
        sha: reference.to_owned(),
    })
}

/// Whether `part` is a name GitHub gives an owner, a repository or a directory: letters, digits, `-`, `_` and `.`,
/// never `.` or `..` alone.
fn segment(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Whether `name` is a repository's `owner/repo`, safe to make a file name of.
pub(crate) fn repository(name: &str) -> bool {
    name.split_once('/')
        .is_some_and(|(owner, repo)| segment(owner) && segment(repo))
}

/// Whether `reference` is a full commit ID as a pin writes it: 40 lowercase hex digits.
pub(crate) fn commit(reference: &str) -> bool {
    reference.len() == 40
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::{Pin, Unreadable, action_pins, file_pins, read_for_pins, workflow_pins};

    const SHA: &str = "08eba0b27e820071cde6df949e0beb9ba4906955";

    fn pin(repository: &str, path: &str) -> Pin {
        Pin {
            repository: repository.to_owned(),
            path: path.to_owned(),
            sha: SHA.to_owned(),
        }
    }

    #[test]
    fn only_actions_pinned_by_commit_are_read_never_a_tag_a_local_action_an_image_or_a_reusable_workflow()
     {
        let workflow = format!(
            "jobs:\n  call:\n    uses: o/shared/.github/workflows/ci.yml@{SHA}\n  test:\n    steps:\n      \
             - uses: actions/checkout@{SHA} # v6\n      - uses: \"actions/setup-node@{SHA}\"\n      \
             - name: tailscale\n        uses: 'tailscale/github-action/oauth@{SHA}'\n      \
             - uses: actions/cache@v4\n      - uses: ./.github/actions/mise\n      \
             - uses: docker://alpine@sha256:{SHA}{SHA}\n      - uses: actions/checkout@{}\n      \
             - uses: o/../evil@{SHA}\n      - uses: actions/checkout@{SHA}\n      \
             - run: 'echo uses: actions/fake@{SHA}'\n",
            SHA.to_uppercase()
        );
        assert_eq!(
            workflow_pins(&workflow),
            Ok(vec![
                pin("actions/checkout", ""),
                pin("actions/setup-node", ""),
                pin("tailscale/github-action", "oauth"),
            ])
        );
    }

    #[test]
    fn a_step_s_uses_is_read_however_yaml_writes_it_and_never_from_a_script() {
        let workflow = format!(
            "jobs:\n  test:\n    steps:\n      - {{uses: actions/checkout@{SHA}}}\n      \
             - \"uses\": actions/setup-node@{SHA}\n      - name: script\n        run: |\n          \
             cat > ci.yml <<EOF\n          - uses: actions/fake@{SHA}\n          EOF\n      \
             - &mise\n        uses: jdx/mise-action@{SHA}\n  lint:\n    steps:\n      - *mise\n"
        );
        assert_eq!(
            workflow_pins(&workflow),
            Ok(vec![
                pin("actions/checkout", ""),
                pin("actions/setup-node", ""),
                pin("jdx/mise-action", ""),
            ])
        );
        let action = format!(
            "runs:\n  using: composite\n  steps:\n    - uses: actions/checkout@{SHA}\n    \
             - run: 'echo uses: actions/fake@{SHA}'\n      shell: bash\n"
        );
        assert_eq!(action_pins(&action), Ok(vec![pin("actions/checkout", "")]));
        assert_eq!(
            action_pins("runs:\n  using: node24\n  main: index.js\n"),
            Ok(vec![])
        );
    }

    #[test]
    fn a_file_that_is_not_yaml_a_runner_reads_is_unreadable_not_empty() {
        for broken in ["jobs: [\n", "a: 1\n---\nb: 2\n", "jobs:\n\ttest: 1\n"] {
            assert!(
                workflow_pins(broken).is_err(),
                "{broken:?} is not read as pinning nothing"
            );
        }
        let laughs = (1..12).fold(
            String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n"),
            |text, level| {
                let refs = vec![format!("*a{}", level - 1); 10].join(", ");
                format!("{text}a{level}: &a{level} [{refs}]\n")
            },
        );
        assert!(
            workflow_pins(&laughs).is_err(),
            "aliases that expand past the parser's bound"
        );
        assert_eq!(
            file_pins(".github/workflows/ci.yml", b"\xff\xfe"),
            Err(Unreadable::Text)
        );
    }

    #[test]
    fn workflows_and_every_action_file_are_read_and_nothing_else() {
        for read in [
            ".github/workflows/ci.yml",
            ".github/workflows/release.yaml",
            "action.yml",
            ".github/actions/mise/action.yaml",
            "tools/deploy/action.yml",
        ] {
            assert!(read_for_pins(read), "{read} is read");
        }
        for skipped in [
            ".github/workflows/nested/ci.yml",
            ".github/workflows/README.md",
            ".github/dependabot.yml",
            "docs/action.yml.md",
        ] {
            assert!(!read_for_pins(skipped), "{skipped} is not read");
        }
    }
}
