#!/usr/bin/env python3
"""Reads a repository's mise.toml and mise.lock for the mise-tools image (docs/adr/0149).

check DIR               refuses inputs outside the exact shapes the recipe has verified; prints why, one line each,
                        and exits 1
build-config FILE       prints the config the build installs from: mise.toml's [tools] only, after `check` passed
tools DIR               prints one line per binding of a [tools] request to its lock entry, tab-separated: tool, the
                        request, the locked version, the version it prints, and the command its name suggests
                        (`github:sbdchd/squawk` is `squawk`); exits 1 if `check` refuses DIR
mise VERSION SHA256     exits 1 unless that mise version and binary digest are in tested-mise.txt beside this file
                        or at /opt/mise/share/tested-mise.txt

The recipe accepts only what it has checked mise installs from the lock with the lock's digest compared against the
downloaded bytes before they are installed, each shape with a wrong-digest build (test.sh), and only with a mise
version and binary that passed that whole matrix (tested-mise.txt):

- `aqua:<owner>/<repo>` and `github:<owner>/<repo>`: one release asset of that repository
  (src/backend/aqua.rs, src/backend/github.rs);
- `core:node`: the official linux-x64 build from nodejs.org at exactly its canonical URL, with no `install` kind, so
  the lock's digest is the one checked: node.rs drops it for any URL whose parsed form differs from the lock's string
  (src/plugins/core/node.rs);
- `core:java`: a Temurin JDK from Adoptium's GitHub releases (src/plugins/core/java.rs);
- `core:python`: a CPython build from python-build-standalone; PyPy and source builds are refused, and the build
  sets python.compile false (src/plugins/core/python.rs).

Every value that reaches mise is a string matched whole against a narrow grammar, so no template or other dynamic
syntax does. Python's standard library only: it runs in the runner image, which has python3.
"""

import json
import re
import sys
import tomllib
from pathlib import Path

PLATFORM_KEY = "platforms.linux-x64"
NAME = r"[A-Za-z0-9][A-Za-z0-9._-]*"
TOOL_KEY = re.compile(rf"(?:[a-z0-9][a-z0-9-]*|(?:aqua|github):{NAME}/{NAME})")
REPO_BACKEND = re.compile(rf"(?:aqua|github):({NAME}/{NAME})")
# A literal version: no template braces, no spaces, no `prefix:`, `path:`, `ref:` or other selector.
VERSION = re.compile(r"[0-9A-Za-z][0-9A-Za-z.+_-]*")
DYNAMIC = {"latest", "lts", "system", "stable", "nightly"}
CHECKSUM = re.compile(r"(?:sha256|blake3):[0-9a-f]{64}")
CPYTHON = re.compile(r"3\.\d+\.\d+")
TEMURIN = re.compile(r"temurin-(\d+)[0-9.+]*(?:\.LTS)?")
MISE_VERSION = re.compile(r"\d+\.\d+\.\d+")
SHA256 = re.compile(r"[0-9a-f]{64}")
ENTRY_FIELDS = {"version", "backend", "specifiers"}
PLATFORM_FIELDS = {"checksum", "url", "url_api", "provenance", "checksum_source"}
OTHER_PLATFORM = re.compile(r"platforms\.(?:linux|macos|windows)-(?:x64|arm64)")
# What mise.toml may hold besides [tools]. None of it reaches the build or the proof (build-config); a job runs its
# hooks and tasks as on any runner.
CONFIG_TABLES = {"tools", "settings", "env", "hooks", "tasks", "task_config", "vars", "watch_files"}
TESTED_MISE = [Path(__file__).resolve().parent / "tested-mise.txt", Path("/opt/mise/share/tested-mise.txt")]


def whole(pattern: re.Pattern, value) -> re.Match | None:
    """The match of all of `value` against `pattern`, when `value` is a string."""
    return pattern.fullmatch(value) if isinstance(value, str) else None


def load(path: Path) -> dict:
    with path.open("rb") as file:
        return tomllib.load(file)


def entries(lock: dict) -> list[tuple[str, dict]]:
    """Every locked tool version, as (tool, entry); a tool with several versions has several entries."""
    found = []
    tools = lock.get("tools", {})
    for tool, versions in (tools.items() if isinstance(tools, dict) else []):
        for entry in versions if isinstance(versions, list) else [versions]:
            found.append((tool, entry if isinstance(entry, dict) else {}))
    return found


def url_refusal(backend: str, version: str, url) -> str | None:
    """Why `url` is not the one download the recipe accepts for this backend and version, or None."""
    if not isinstance(url, str):
        return "is not a string"
    if backend == "core:node":
        canonical = f"https://nodejs.org/dist/v{version}/node-v{version}-linux-x64.tar.gz"
        return None if url == canonical else f"is not exactly {canonical}"
    repo = whole(REPO_BACKEND, backend)
    if repo:
        pattern = rf"https://github\.com/{re.escape(repo.group(1))}/releases/download/[^/?#\s]+/[^/?#\s]+"
    elif backend == "core:java" and whole(TEMURIN, version):
        n = whole(TEMURIN, version).group(1)
        pattern = (
            rf"https://github\.com/adoptium/temurin{n}-binaries/releases/download/jdk-[0-9.%A-Za-z]+/"
            rf"OpenJDK{n}U-jdk_x64_linux_hotspot_[0-9._]+\.tar\.gz"
        )
    elif backend == "core:python":
        pattern = (
            r"https://github\.com/astral-sh/python-build-standalone/releases/download/(\d{8})/"
            rf"cpython-{re.escape(version)}\+\1-x86_64-unknown-linux-gnu-install_only(?:_stripped)?\.tar\.gz"
        )
    else:
        return f"is not a {backend} download this recipe accepts"
    return None if re.fullmatch(pattern, url) else f"is not a {backend} download this recipe accepts"


def entry_refusals(tool: str, entry: dict) -> list[str]:
    version = entry.get("version")
    name = f"{tool}@{version if isinstance(version, str) else '?'}"
    backend = entry.get("backend")
    refusals = []
    shapes = {"core:node": "node", "core:java": "java", "core:python": "python"}
    if whole(REPO_BACKEND, backend):
        if ":" in tool and tool != backend:
            refusals.append(f"{name}: the key names {tool} but the lock installs {backend}")
    elif isinstance(backend, str) and backend in shapes:
        if tool != shapes[backend]:
            refusals.append(f"{name}: the key {tool} is not {shapes[backend]} for {backend}")
    else:
        refusals.append(f"{name}: backend `{backend}` is not one whose lock digest this recipe has tested")
        backend = ""
    if not whole(VERSION, version):
        refusals.append(f"{name}: version `{version}` is not a literal version")
        version = ""
    if backend == "core:python" and not whole(CPYTHON, version):
        refusals.append(f"{name}: only CPython 3.x.y is accepted; PyPy and other builds compile from source")
    if backend == "core:java" and not whole(TEMURIN, version):
        refusals.append(f"{name}: only a Temurin JDK is accepted")
    specifiers = entry.get("specifiers")
    if not (isinstance(specifiers, list) and specifiers and all(whole(VERSION, s) for s in specifiers)):
        refusals.append(f"{name}: no literal specifiers, so no request can be bound to it")
    for field in sorted(set(entry) - ENTRY_FIELDS - {PLATFORM_KEY}):
        if not whole(OTHER_PLATFORM, field):
            refusals.append(f"{name}: field `{field}` is not one this recipe has tested (an install kind or option)")
    platform = entry.get(PLATFORM_KEY)
    if not isinstance(platform, dict):
        refusals.append(f"{name}: mise.lock has no linux-x64 entry")
        return refusals
    for field in sorted(set(platform) - PLATFORM_FIELDS):
        refusals.append(f"{name}: linux-x64 field `{field}` is not one this recipe has tested")
    if not whole(CHECKSUM, platform.get("checksum")):
        refusals.append(f"{name}: no full sha256 or blake3 linux-x64 digest in mise.lock")
    why = url_refusal(backend, version, platform.get("url"))
    if why:
        refusals.append(f"{name}: linux-x64 url {json.dumps(platform.get('url'))} {why}")
    if "url_api" in platform:
        repo = whole(REPO_BACKEND, backend)
        api = rf"https://api\.github\.com/repos/{re.escape(repo.group(1))}/releases/assets/\d+" if repo else None
        if not (api and isinstance(platform["url_api"], str) and re.fullmatch(api, platform["url_api"])):
            refusals.append(f"{name}: url_api is not an asset of {backend}")
    # With a lock digest, mise only checks that the recorded provenance's setting is on (src/backend/aqua.rs, verify).
    if platform.get("provenance", "github-attestations") not in ("github-attestations", "cosign"):
        refusals.append(f"{name}: provenance `{platform['provenance']}` is not one this recipe has tested")
    if platform.get("checksum_source", "mise-versions") != "mise-versions":
        refusals.append(f"{name}: checksum_source `{platform['checksum_source']}` is not one this recipe knows")
    return refusals


def bindings(config: dict, lock: dict) -> tuple[list[tuple[str, str, dict]], list[str]]:
    """Each [tools] request bound to its one lock entry, as (tool, request, entry), and why any request is not."""
    bound, refusals = [], []
    locked = entries(lock)
    tools = config.get("tools", {})
    if not isinstance(tools, dict) or not tools:
        return bound, ["[tools]: empty; there is nothing to install"]
    for tool, value in tools.items():
        if not whole(TOOL_KEY, tool):
            refusals.append(f"{json.dumps(tool)}: not a literal tool name or <aqua|github>:<owner>/<repo>")
            continue
        for request in value if isinstance(value, list) else [value]:
            if not isinstance(request, str):
                refusals.append(f"{tool}: {request!r} is not a literal version; tables and options are refused")
                continue
            if not whole(VERSION, request) or request in DYNAMIC:
                refusals.append(f"{tool}: {json.dumps(request)} is not a literal version")
                continue
            matches = [e for t, e in locked if t == tool and request in (e.get("specifiers") or [])]
            if len(matches) != 1:
                refusals.append(f"{tool}@{request}: matches {len(matches)} lock entries, not exactly one")
                continue
            bound.append((tool, request, matches[0]))
    return bound, refusals


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
    bound, why = bindings(config, lock)
    refusals += why
    for tool, entry in entries(lock):
        refusals += entry_refusals(tool, entry)
        count = sum(1 for _, _, e in bound if e is entry)
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


def tools(directory: Path) -> str:
    bound, _ = bindings(load(directory / "mise.toml"), load(directory / "mise.lock"))
    lines = []
    for tool, request, entry in bound:
        version = entry["version"]
        command = re.split(r"[:/]", tool)[-1]
        lines.append(f"{tool}\t{request}\t{version}\t{numeric(version)}\t{command}")
    return "\n".join(lines) + "\n"


def tested_mise(version: str, sha256: str) -> bool:
    if not (whole(MISE_VERSION, version) and whole(SHA256, sha256)):
        return False
    for path in TESTED_MISE:
        if path.is_file():
            rows = [line.split() for line in path.read_text().splitlines() if line and not line.startswith("#")]
            return [version, sha256] in rows
    return False


def main(argv: list[str]) -> int:
    if len(argv) == 4 and argv[1] == "mise":
        if tested_mise(argv[2], argv[3]):
            return 0
        print(f"refused: mise {argv[2]} with SHA-256 {argv[3]} has not passed the recipe's wrong-digest builds; "
              "tested-mise.txt lists those that have", file=sys.stderr)
        return 1
    if len(argv) != 3 or argv[1] not in ("check", "build-config", "tools"):
        print(__doc__, file=sys.stderr)
        return 2
    command, path = argv[1], Path(argv[2])
    if command in ("check", "tools"):
        refusals = check(path)
        for refusal in refusals:
            print(f"refused: {refusal}", file=sys.stderr)
        if refusals:
            return 1
        if command == "check":
            return 0
    sys.stdout.write(build_config(path) if command == "build-config" else tools(path))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
