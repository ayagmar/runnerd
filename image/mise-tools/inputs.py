#!/usr/bin/env python3
"""Reads a repository's mise.toml and mise.lock for the mise-tools image (docs/adr/0149).

check DIR          refuses inputs outside the exact shapes the recipe has verified; prints why, one line each, exit 1
build-config FILE  prints the config the build installs from: mise.toml's [tools] only, after `check` passed
tools LOCK         prints one line per locked tool version, tab-separated: tool, version request, the version it
                   prints, and the command its name suggests (`github:sbdchd/squawk` is `squawk`); exits 1 on a lock
                   it cannot read

The recipe accepts only what it has checked mise v2026.10.18 installs from the lock with the lock's digest compared
against the downloaded bytes before they are installed, each shape with a wrong-digest test (test.sh):

- `aqua:<owner>/<repo>` and `github:<owner>/<repo>`: one release asset of that repository
  (src/backend/aqua.rs, src/backend/github.rs);
- `core:node`: the official linux-x64 build from nodejs.org, with no `install` kind, so the lock's URL and digest
  are the ones checked (src/plugins/core/node.rs);
- `core:java`: a Temurin JDK from Adoptium's GitHub releases (src/plugins/core/java.rs);
- `core:python`: a CPython build from python-build-standalone; PyPy and source builds are refused, and the build
  sets python.compile false (src/plugins/core/python.rs).

Every value that reaches mise is a literal of a narrow grammar, so no template or other dynamic syntax does.
Python's standard library only: it runs in the runner image, which has python3.
"""

import json
import re
import sys
import tomllib
from pathlib import Path

PLATFORM_KEY = "platforms.linux-x64"
NAME = r"[A-Za-z0-9][A-Za-z0-9._-]*"
TOOL_KEY = re.compile(rf"^(?:[a-z0-9][a-z0-9-]*|(?:aqua|github):{NAME}/{NAME})$")
# A literal version: no template braces, no spaces, no `prefix:`, `path:`, `ref:` or other selector.
VERSION = re.compile(r"^[0-9A-Za-z][0-9A-Za-z.+_-]*$")
DYNAMIC = {"latest", "lts", "system", "stable", "nightly"}
CHECKSUM = re.compile(r"^(?:sha256|blake3):[0-9a-f]{64}$")
ENTRY_FIELDS = {"version", "backend", "specifiers"}
PLATFORM_FIELDS = {"checksum", "url", "url_api", "provenance", "checksum_source"}
OTHER_PLATFORM = re.compile(r"^platforms\.(?:linux|macos|windows)-(?:x64|arm64)$")
# What mise.toml may hold besides [tools]. None of it reaches the build or the proof (build-config); a job runs its
# hooks and tasks as on any runner.
CONFIG_TABLES = {"tools", "settings", "env", "hooks", "tasks", "task_config", "vars", "watch_files"}


def load(path: Path) -> dict:
    with path.open("rb") as file:
        return tomllib.load(file)


def entries(lock: dict) -> list[tuple[str, dict]]:
    """Every locked tool version, as (tool, entry); a tool with several versions has several entries."""
    found = []
    for tool, versions in lock.get("tools", {}).items():
        for entry in versions if isinstance(versions, list) else [versions]:
            found.append((tool, entry))
    return found


def expected_url(backend: str, version: str) -> re.Pattern | None:
    """The only download URLs the recipe accepts for a backend and version."""
    v = re.escape(version)
    if backend.startswith(("aqua:", "github:")):
        repo = re.escape(backend.split(":", 1)[1])
        return re.compile(rf"^https://github\.com/{repo}/releases/download/[^/?#\s]+/[^/?#\s]+$")
    if backend == "core:node":
        return re.compile(rf"^https://nodejs\.org/dist/v{v}/node-v{v}-linux-x64\.tar\.gz$")
    if backend == "core:java":
        major = re.match(r"^temurin-(\d+)", version)
        if not major:
            return None
        n = major.group(1)
        return re.compile(
            rf"^https://github\.com/adoptium/temurin{n}-binaries/releases/download/jdk-[0-9.%A-Za-z]+/"
            rf"OpenJDK{n}U-jdk_x64_linux_hotspot_[0-9._]+\.tar\.gz$"
        )
    if backend == "core:python":
        return re.compile(
            r"^https://github\.com/astral-sh/python-build-standalone/releases/download/(\d{8})/"
            rf"cpython-{v}\+\1-x86_64-unknown-linux-gnu-install_only(?:_stripped)?\.tar\.gz$"
        )
    return None


def entry_refusals(tool: str, entry: dict) -> list[str]:
    version = str(entry.get("version", ""))
    name = f"{tool}@{version or '?'}"
    backend = entry.get("backend", "")
    refusals = []
    shapes = {"core:node": "node", "core:java": "java", "core:python": "python"}
    if backend.startswith(("aqua:", "github:")):
        if not TOOL_KEY.match(backend) or ":" not in backend:
            refusals.append(f"{name}: backend `{backend}` is not <aqua|github>:<owner>/<repo>")
        if tool.startswith(("aqua:", "github:")) and tool != backend:
            refusals.append(f"{name}: the key names {tool} but the lock installs {backend}")
    elif backend in shapes:
        if tool != shapes[backend]:
            refusals.append(f"{name}: the key {tool} is not {shapes[backend]} for {backend}")
    else:
        refusals.append(f"{name}: backend `{backend}` is not one whose lock digest this recipe has tested")
    if not VERSION.match(version):
        refusals.append(f"{name}: version `{version}` is not a literal version")
    if backend == "core:python" and not re.match(r"^3\.\d+\.\d+$", version):
        refusals.append(f"{name}: only CPython 3.x.y is accepted; PyPy and other builds compile from source")
    if backend == "core:java" and not re.match(r"^temurin-\d+[0-9.+]*(?:\.LTS)?$", version):
        refusals.append(f"{name}: only a Temurin JDK is accepted")
    specifiers = entry.get("specifiers")
    if not (isinstance(specifiers, list) and specifiers and all(isinstance(s, str) for s in specifiers)):
        refusals.append(f"{name}: no specifiers, so no request can be bound to it")
    for field in sorted(set(entry) - ENTRY_FIELDS - {PLATFORM_KEY}):
        if not OTHER_PLATFORM.match(field):
            refusals.append(f"{name}: field `{field}` is not one this recipe has tested (an install kind or option)")
    platform = entry.get(PLATFORM_KEY)
    if not isinstance(platform, dict):
        refusals.append(f"{name}: mise.lock has no linux-x64 entry")
        return refusals
    for field in sorted(set(platform) - PLATFORM_FIELDS):
        refusals.append(f"{name}: linux-x64 field `{field}` is not one this recipe has tested")
    if not CHECKSUM.match(str(platform.get("checksum", ""))):
        refusals.append(f"{name}: no full sha256 or blake3 linux-x64 digest in mise.lock")
    pattern = expected_url(backend, version)
    url = str(platform.get("url", ""))
    if pattern is None or not pattern.match(url):
        refusals.append(f"{name}: linux-x64 url `{url}` is not the {backend} download this recipe accepts")
    if "url_api" in platform:
        repo = backend.split(":", 1)[1] if ":" in backend else ""
        api = rf"^https://api\.github\.com/repos/{re.escape(repo)}/releases/assets/\d+$"
        if not backend.startswith(("aqua:", "github:")) or not re.match(api, str(platform["url_api"])):
            refusals.append(f"{name}: url_api is not an asset of {repo or backend}")
    # With a lock digest, mise only checks that the recorded provenance's setting is on (src/backend/aqua.rs, verify).
    if platform.get("provenance", "github-attestations") not in ("github-attestations", "cosign"):
        refusals.append(f"{name}: provenance `{platform['provenance']}` is not one this recipe has tested")
    if platform.get("checksum_source", "mise-versions") != "mise-versions":
        refusals.append(f"{name}: checksum_source `{platform['checksum_source']}` is not one this recipe knows")
    return refusals


def check(directory: Path) -> list[str]:
    config = load(directory / "mise.toml")
    lock = load(directory / "mise.lock")
    refusals = []
    for table in sorted(set(config) - CONFIG_TABLES):
        reason = {
            "plugins": "a plugin is code from a repository, run as root at build time",
            "alias": "an alias can point a tool at another backend than its lock entry's",
            "tool_alias": "an alias can point a tool at another backend than its lock entry's",
        }.get(table, "not a table this recipe knows; it cannot tell what it changes")
        refusals.append(f"[{table}]: {reason}")
    for key in sorted(set(lock) - {"lockfile_version", "tools"}):
        refusals.append(f"mise.lock [{key}]: not a table this recipe knows")
    if lock.get("lockfile_version") != 3:
        refusals.append("mise.lock: lockfile_version is not 3, the format this recipe reads")
    tools = config.get("tools", {})
    if not tools:
        refusals.append("[tools]: empty; there is nothing to install")
    locked = entries(lock)
    selected = {id(entry): 0 for _, entry in locked}
    for tool, value in tools.items():
        if not TOOL_KEY.match(tool):
            refusals.append(f"{json.dumps(tool)}: not a literal tool name or <aqua|github>:<owner>/<repo>")
            continue
        requests = value if isinstance(value, list) else [value]
        for request in requests:
            if not isinstance(request, str):
                refusals.append(f"{tool}: {request!r} is not a literal version; tables and options are refused")
                continue
            if not VERSION.match(request) or request in DYNAMIC:
                refusals.append(f"{tool}: {json.dumps(request)} is not a literal version")
                continue
            matches = [e for t, e in locked if t == tool and request in (e.get("specifiers") or [])]
            if len(matches) != 1:
                refusals.append(f"{tool}@{request}: matches {len(matches)} lock entries, not exactly one")
                continue
            selected[id(matches[0])] += 1
    for tool, entry in locked:
        refusals += entry_refusals(tool, entry)
        count = selected[id(entry)]
        if count != 1:
            refusals.append(f"{tool}@{entry.get('version', '?')}: selected by {count} requests, not exactly one")
    return refusals


def build_config(path: Path) -> str:
    """[tools] as TOML. Only after `check`, so each key and version is a literal of the grammar above."""
    lines = ["# Made by runnerd's mise-tools recipe from the repository's mise.toml: its [tools] only.", "[tools]"]
    for tool, value in load(path).get("tools", {}).items():
        versions = value if isinstance(value, list) else [value]
        lines.append(f"{json.dumps(tool)} = [{', '.join(json.dumps(v) for v in versions)}]")
    return "\n".join(lines) + "\n"


def numeric(version: str) -> str:
    """The version number a tool prints of itself: `temurin-25.0.5+8.0.LTS` prints 25.0.5."""
    found = re.search(r"\d+(?:\.\d+)+", version)
    return found.group(0) if found else version


def tools(path: Path) -> str:
    lines = []
    for tool, entry in entries(load(path)):
        version = entry["version"]
        request = entry["specifiers"][0]
        command = re.split(r"[:/]", tool)[-1]
        lines.append(f"{tool}\t{request}\t{numeric(version)}\t{command}")
    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    if len(argv) != 3 or argv[1] not in ("check", "build-config", "tools"):
        print(__doc__, file=sys.stderr)
        return 2
    command, path = argv[1], Path(argv[2])
    if command == "check":
        refusals = check(path)
        for refusal in refusals:
            print(f"refused: {refusal}", file=sys.stderr)
        return 1 if refusals else 0
    sys.stdout.write(build_config(path) if command == "build-config" else tools(path))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
