#!/usr/bin/env python3
"""Reads a repository's mise.toml and mise.lock for the mise-tools image (docs/adr/0148).

check DIR          refuses inputs the build cannot verify; prints why, one line per tool, and exits 1
build-config FILE  prints the config the build installs from: mise.toml's [tools] and nothing else, so none of the
                   repository's hooks, tasks, env or settings runs as root while the image is built
tools LOCK         prints one line per locked tool version, tab-separated: tool, version request, the version it
                   prints, and the command its name suggests (`github:sbdchd/squawk` is `squawk`)
match LOCK         compares a job's mise.lock with the one the image was built from: exits 1, naming each, when a
                   tool version both lock has differs between them, so the job must not use the image's install

Python's standard library only: it runs in the runner image, which has python3 and nothing else of the kind.
"""

import json
import re
import sys
import tomllib
from pathlib import Path

PLATFORM = "linux-x64"
PLATFORM_KEY = f"platforms.{PLATFORM}"
# What the image was built from; `match` compares against it.
IMAGE_LOCK = Path("/opt/mise/share/mise.lock")

# Backends that, at mise v2026.10.18, check a download against the lock's checksum before they unpack it, and install
# from the lock's URL under --locked: aqua (src/backend/aqua.rs), github (src/backend/github.rs), http
# (src/backend/http.rs), and the core java, node and python plugins (src/plugins/core/). Each calls
# Backend::verify_checksum, which compares the lock's checksum when there is one and otherwise makes one up from the
# download (src/backend/mod.rs), hence the checksum this script requires of every tool.
BACKENDS = ("aqua:", "github:", "http:")
CORE_BACKENDS = ("core:java", "core:node", "core:python")
CHECKSUM = re.compile(r"^(sha256|sha512|blake3):[0-9a-f]{64,128}$")
# Tool options that select what is installed and run nothing: anything else, `postinstall` and `install_env` among
# them, is refused.
TOOL_OPTIONS = {"version", "os"}
# What mise.toml may hold besides [tools]. The build installs from [tools] only (build-config), so these never run
# while it does; a job runs its hooks and tasks as it would on any runner.
CONFIG_TABLES = {"tools", "settings", "env", "hooks", "tasks", "task_config", "vars", "watch_files", "min_version"}


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


def requests(value) -> list:
    """A [tools] value as its list of requests: a version string, a table, or a list of either."""
    return value if isinstance(value, list) else [value]


def check(directory: Path) -> list[str]:
    config = load(directory / "mise.toml")
    lock = load(directory / "mise.lock")
    refusals = []
    for table in sorted(set(config) - CONFIG_TABLES):
        if table == "plugins":
            refusals.append("[plugins]: a plugin is code from a repository, run as root at build time")
        elif table in ("alias", "tool_alias"):
            refusals.append(f"[{table}]: an alias can point a tool at another backend than its lock entry's")
        else:
            refusals.append(f"[{table}]: not a table this recipe knows; it cannot tell what it changes")
    tools = config.get("tools", {})
    if not tools:
        refusals.append("[tools]: empty; there is nothing to install")
    for tool, value in tools.items():
        for request in requests(value):
            if isinstance(request, dict):
                for option in sorted(set(request) - TOOL_OPTIONS):
                    refusals.append(f"{tool}: option `{option}` may run a command or change what is installed")
            elif not isinstance(request, str):
                refusals.append(f"{tool}: {request!r} is not a version")
            elif request.startswith(("path:", "ref:")):
                refusals.append(f"{tool}: `{request}` is a directory or a source build, not a locked download")
        if tool not in lock.get("tools", {}):
            refusals.append(f"{tool}: mise.lock has no entry for it")
    for key in sorted(set(lock) - {"lockfile_version", "tools"}):
        refusals.append(f"mise.lock [{key}]: not a table this recipe knows; it cannot tell what it changes")
    for tool, entry in entries(lock):
        name = f"{tool}@{entry.get('version', '?')}"
        backend = entry.get("backend", "")
        if not (backend.startswith(BACKENDS) or backend in CORE_BACKENDS):
            refusals.append(f"{name}: backend `{backend}` is not one this recipe knows checks the lock's checksum")
        platform = entry.get(PLATFORM_KEY)
        if not isinstance(platform, dict):
            refusals.append(f"{name}: mise.lock has no {PLATFORM} entry")
            continue
        if not CHECKSUM.match(str(platform.get("checksum", ""))):
            refusals.append(f"{name}: no {PLATFORM} checksum in mise.lock, so mise would trust what it downloads")
        if not platform.get("url"):
            refusals.append(f"{name}: no {PLATFORM} url in mise.lock, so mise would look one up")
    return refusals


def toml_value(value) -> str:
    """A [tools] value as TOML: strings, lists and inline tables of them, which is all `check` lets through."""
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(f"{json.dumps(k)} = {toml_value(v)}" for k, v in value.items()) + " }"
    raise ValueError(f"cannot write {value!r}")


def build_config(path: Path) -> str:
    tools = load(path).get("tools", {})
    lines = ["# Made by runnerd's mise-tools recipe from the repository's mise.toml: its [tools] only.", "[tools]"]
    lines += [f"{json.dumps(tool)} = {toml_value(value)}" for tool, value in tools.items()]
    return "\n".join(lines) + "\n"


def numeric(version: str) -> str:
    """The version number a tool prints of itself: `temurin-25.0.5+8.0.LTS` prints 25.0.5."""
    found = re.search(r"\d+(\.\d+)+", version)
    return found.group(0) if found else version


def tools(path: Path) -> str:
    lines = []
    for tool, entry in entries(load(path)):
        version = entry["version"]
        request = (entry.get("specifiers") or [version])[0]
        command = re.split(r"[:/]", tool)[-1]
        lines.append(f"{tool}\t{request}\t{numeric(version)}\t{command}")
    return "\n".join(lines) + "\n"


def identity(entry: dict) -> dict:
    """What decides the bytes installed for this platform: the backend, its options and the platform's entry."""
    return {
        "backend": entry.get("backend"),
        "options": entry.get("options"),
        "platform": entry.get(PLATFORM_KEY),
    }


def match(path: Path) -> list[str]:
    image = {(tool, entry.get("version")): identity(entry) for tool, entry in entries(load(IMAGE_LOCK))}
    differs = []
    for tool, entry in entries(load(path)):
        key = (tool, entry.get("version"))
        if key in image and image[key] != identity(entry):
            differs.append(f"{tool}@{key[1]}: the image installed it from another lock entry")
    return differs


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    command, path = argv[1], Path(argv[2])
    if command == "check":
        refusals = check(path)
        for refusal in refusals:
            print(f"refused: {refusal}", file=sys.stderr)
        return 1 if refusals else 0
    if command == "build-config":
        sys.stdout.write(build_config(path))
        return 0
    if command == "tools":
        sys.stdout.write(tools(path))
        return 0
    if command == "match":
        differs = match(path)
        for line in differs:
            print(line, file=sys.stderr)
        return 1 if differs else 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
