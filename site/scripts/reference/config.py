"""Checks the configuration reference against the config types, through the `runnerd` binary that reads them.

    python3 site/scripts/reference/config.py BIN_DIR

For every table of runnerd.toml, `runnerd check-config` is given the table with a key it does not know; serde's error
lists the keys the type accepts, and the reference's table for it (configuration.md and configuration-more.md) must list exactly those. Then every default the
reference gives as a TOML value is written into a small config, which must pass the check: a misspelt key or a value
of the wrong kind fails. check-docs-commands.sh runs it in CI. That each default is the one the code uses is
runnerd-core's test `every_default_the_configuration_reference_gives_is_the_one_the_config_uses`.
"""
import re
import subprocess
import sys
import tempfile
from pathlib import Path

DOCS = Path(__file__).resolve().parents[2] / "src/content/docs/reference"
PAGES = [DOCS / "configuration.md", DOCS / "configuration-more.md"]

# The smallest config that passes, by table, with each table's keys. Written in this order.
BASE = {
    "": {},
    "host": {"slice": '"runnerd.slice"', "isolation": '"sysbox"'},
    "caches": {},
    "caches.mirror": {"enabled": "true", "memory": '"512M"', "cpus": "1", "ttl": '"168h"'},
    "caches.pnpm": {"max_size": '"5G"'},
    "caches.maven": {"max_size": '"5G"'},
    "caches.build": {},
    "profiles.jvm": {"image": '"ghcr.io/ayagmar/runnerd-runner:jvm-0.1.1"', "cpus": "8", "memory": '"9G"', "slots": "2"},
    "[repositories]": {"name": '"halyard/ledger"', "profile": '"jvm"', "labels": '["self-hosted"]'},
    "history": {},
    "records": {},
    "sizing": {},
    "web": {},
    "github": {},
    "notify": {},
    "[notify.targets]": {"name": '"phone"', "kind": '"ntfy"', "url": '"https://ntfy.sh/halyard-ci"'},
}
# The reference's headings, as the tables they document.
SECTIONS = {
    "[host]": "host",
    "[caches]": "caches",
    "[profiles.<name>]": "profiles.jvm",
    "[[repositories]]": "[repositories]",
    "[history]": "history",
    "[records]": "records",
    "[sizing]": "sizing",
    "[github]": "github",
    "[web]": "web",
    "[notify]": "notify",
    "[[notify.targets]]": "[notify.targets]",
}


def toml(tables):
    out = []
    for table, keys in tables.items():
        if table:
            out.append(f"[{table}]")
        out.extend(f"{key} = {value}" for key, value in keys.items())
        out.append("")
    return "\n".join(out)


def check(bin_dir, tables):
    with tempfile.NamedTemporaryFile("w", suffix=".toml") as file:
        file.write(toml(tables))
        file.flush()
        run = subprocess.run(
            [f"{bin_dir}/runnerd", "check-config", file.name, "--memory-max", "100G"], capture_output=True, text=True
        )
    return run.returncode == 0, run.stderr


def accepted(bin_dir, table):
    """The keys the type behind a table accepts, from serde's error on one it does not."""
    tables = {t: dict(k) for t, k in BASE.items()}
    tables[table]["zz_unknown"] = "1"
    ok, err = check(bin_dir, tables)
    found = re.search(r"unknown field `zz_unknown`, expected (.*)", err)
    if ok or not found:
        sys.exit(f"runnerd check-config did not name the keys of [{table}]:\n{err}")
    return set(re.findall(r"`([^`]+)`", found[1]))


def documented():
    """Each documented key as (table, key, default as written)."""
    rows, table = [], None
    for line in (line for page in PAGES for line in page.read_text().splitlines()):
        heading = re.match(r"^#{2,4} `(.+)`$", line)
        if heading:
            table = SECTIONS.get(heading[1])
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")] if line.startswith("| `") else None
        if table is None or not cells or len(cells) < 3:
            continue
        for key in re.findall(r"`([^`]+)`", cells[0]):
            sub, _, leaf = key.rpartition(".")
            rows.append((f"{table}.{sub}" if sub else table, leaf, cells[1]))
    return rows


def main():
    bin_dir = sys.argv[1]
    rows = documented()
    problems = []
    top = {table.strip("[]").split(".")[0] for table in SECTIONS.values()}
    if top != accepted(bin_dir, ""):
        problems.append(f"the reference's tables are {sorted(top)}, the config's {sorted(accepted(bin_dir, ''))}")
    for table in BASE:
        if not table:
            continue
        keys = accepted(bin_dir, table)
        listed = {key for t, key, _ in rows if t == table}
        # A table of tables is documented by its keys' prefix (`mirror.enabled` for [caches.mirror]).
        listed |= {t.split(".")[-1] for t, _, _ in rows if t.startswith(f"{table}.")}
        if table == "notify":
            listed.add("targets")
        if listed != keys:
            problems.append(
                f"[{table}]: the reference lists {sorted(listed - keys)} that the config lacks, "
                f"and lacks {sorted(keys - listed)}"
            )
    for table, key, default in rows:
        value = re.fullmatch(r"`([^`]+)`", default)
        # A key that is a table of its own in the base config (`targets`) is checked as that table.
        if not value or value[1].startswith("$") or {f"{table}.{key}", f"[{table}.{key}]"} & BASE.keys():
            continue
        tables = {t: dict(k) for t, k in BASE.items()}
        tables.setdefault(table, {})[key] = value[1]
        ok, err = check(bin_dir, tables)
        if not ok:
            problems.append(f"[{table}] {key} = {value[1]}, its documented default, does not pass: {err.strip()}")
    for problem in problems:
        print(f"the configuration reference: {problem}", file=sys.stderr)
    if problems:
        sys.exit(1)
    print(f"the configuration reference: {len(rows)} keys match the config")


main()
