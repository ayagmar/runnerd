//! The actions a workflow or an action's metadata pins by commit: each `uses: owner/repo[/path]@<sha>`. Pure
//! functions over text; it fetches nothing. Never a tag or a branch, which can move: an archive is kept under the
//! commit it is, and only a pin names that commit.

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
    let yaml = |name: &str| {
        std::path::Path::new(name)
            .extension()
            .is_some_and(|extension| extension == "yml" || extension == "yaml")
    };
    let workflow = path
        .strip_prefix(".github/workflows/")
        .is_some_and(|name| !name.contains('/') && yaml(name));
    workflow || is_action_file(path)
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

/// Every action `text`, a workflow or an action's metadata, pins by commit, in order, each once. A local action
/// (`./…`), a Docker image (`docker://…`), a tag or a branch, and a reusable workflow (`…/.github/workflows/…@sha`),
/// which is no action archive, are left out.
pub(crate) fn pins(text: &str) -> Vec<Pin> {
    let mut found: Vec<Pin> = Vec::new();
    for line in text.lines() {
        let Some(value) = uses_value(line) else {
            continue;
        };
        if let Some(pin) = pin(value)
            && !found.contains(&pin)
        {
            found.push(pin);
        }
    }
    found
}

/// The value of a `uses:` key on `line`, as a step (`- uses: …`) or a job writes it, without its quotes and comment.
fn uses_value(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line.strip_prefix("- ").map_or(line, str::trim_start);
    let value = line.strip_prefix("uses:")?;
    let value = match value.find(" #") {
        Some(comment) => &value[..comment],
        None => value,
    };
    let value = value.trim();
    let unquoted = value
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
        });
    Some(unquoted.unwrap_or(value))
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
    let segment = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    };
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

/// Whether `reference` is a full commit ID as a pin writes it: 40 lowercase hex digits.
pub(crate) fn commit(reference: &str) -> bool {
    reference.len() == 40
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::{Pin, pins, read_for_pins};

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
             - run: echo uses: actions/fake@{SHA}\n",
            SHA.to_uppercase()
        );
        assert_eq!(
            pins(&workflow),
            vec![
                pin("actions/checkout", ""),
                pin("actions/setup-node", ""),
                pin("tailscale/github-action", "oauth"),
            ]
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
