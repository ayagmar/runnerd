"""Writes the CLI reference pages from the programs' own `--help`, so the reference is clap's text and nothing else.

    python3 site/scripts/reference/cli.py BIN_DIR           # rewrite the pages in PAGES under site/src/content/docs/reference
    python3 site/scripts/reference/cli.py BIN_DIR --check   # fail when a page differs from what the help says

BIN_DIR holds `runnerd` and `runnerctl`, such as target/debug. check-docs-commands.sh runs the check in CI.
"""
import re
import subprocess
import sys
from pathlib import Path

OUT = Path(__file__).resolve().parents[2] / "src/content/docs/reference"
ENTRY = re.compile(r"^  (?:(-\w), |    )?(--[\w-]+(?: <[^>]+>)?|<[^>]+>(?:\.\.\.)?|\[[^\]]+\](?:\.\.\.)?)(?:\s{2,}(.*))?$")
COMMAND = re.compile(r"^  ([a-z][\w-]*)\s{2,}(.*)$")
# Each page: its file, title, description, program and the top-level commands it shows, in --help's order; the
# commands of a program are split over pages so that none is too long to read. Every command must be on one.
PAGES = [
    ("runnerd", "runnerd", "Every `runnerd` command and option but simulate's, as `runnerd --help` prints them.", "runnerd",
     ["init", "check-config", "doctor", "serve", "stop"]),
    ("runnerd-simulate", "runnerd simulate", "Every option of `runnerd simulate`, as its `--help` prints them.", "runnerd",
     ["simulate"]),
    ("runnerctl", "runnerctl", "The `runnerctl` commands that show something, and their options, as `--help` prints them.",
     "runnerctl", ["status", "jobs", "job", "logs", "watch", "tui", "insights", "caches", "config", "doctor", "web"]),
    ("runnerctl-actions", "runnerctl actions",
     "The `runnerctl` commands that change something, and their options, as `--help` prints them.", "runnerctl",
     ["pause", "resume", "cancel", "rerun", "reseed", "shell", "records", "sizing", "notify"]),
]


def help_text(bin_dir, words):
    run = subprocess.run([f"{bin_dir}/{words[0]}", *words[1:], "--help"], capture_output=True, text=True, check=True)
    return run.stdout


def parse(text):
    """The about paragraphs, the usage line, and each section's entries as (name, short flag, paragraphs)."""
    lines = text.rstrip("\n").split("\n")
    usage = next(i for i, line in enumerate(lines) if line.startswith("Usage: "))
    about = [p.strip().replace("\n", " ") for p in "\n".join(lines[:usage]).strip().split("\n\n") if p.strip()]
    sections, section, entry = {}, None, None
    for line in lines[usage + 1 :]:
        if re.match(r"^[A-Z][\w ]*:$", line):
            section = sections.setdefault(line[:-1], [])
            entry = None
            continue
        if section is None:
            continue
        found = (COMMAND if section is sections.get("Commands") else ENTRY).match(line)
        if found and section is sections.get("Commands"):
            section.append((found[1], None, [found[2]]))
            entry = section[-1]
        elif found:
            section.append((found[2], found[1], [found[3]] if found[3] else []))
            entry = section[-1]
        elif entry and line.strip():
            text = line.strip()
            paragraphs = entry[2]
            # A list of possible values is a paragraph a line.
            own = text == "Possible values:" or text.startswith("- ")
            if paragraphs and paragraphs[-1] is not None and not own and not paragraphs[-1].startswith("- ") and paragraphs[-1] != "Possible values:":
                paragraphs[-1] += " " + text
            else:
                if paragraphs and paragraphs[-1] is None:
                    paragraphs.pop()
                paragraphs.append(text)
        elif entry and entry[2] and entry[2][-1] is not None:
            entry[2].append(None)
    for entries in sections.values():
        for _, _, paragraphs in entries:
            while paragraphs and paragraphs[-1] is None:
                paragraphs.pop()
    return about, lines[usage][len("Usage: ") :], sections


def text(paragraph):
    """Help text as Markdown: angle brackets outside code are shown, not read as HTML."""
    parts = re.split(r"(`[^`]*`)", paragraph)
    return "".join(p if p.startswith("`") else p.replace("<", "&lt;").replace(">", "&gt;") for p in parts)


def describe(paragraphs):
    out, listing = [], False
    for p in paragraphs:
        if p is None:
            continue
        if p == "Possible values:":
            listing = True
            continue
        if listing and p.startswith("- "):
            name, _, what = p[2:].partition(":")
            out.append(f"\n  - `{name.strip()}`: {text(what.strip())}")
            continue
        out.append(("\n\n  " if listing else " " if out else "") + text(p))
        listing = False
    return "".join(out)


def command(bin_dir, words, global_options, out, only=None):
    about, usage, sections = parse(help_text(bin_dir, words))
    # The heading is the command and its arguments; options are listed below it, subcommands get their own.
    args = [w for w in usage.split()[len(words) :] if w not in ("[OPTIONS]", "<COMMAND>", "[COMMAND]")]
    if len(words) > 1:
        out.append(f"\n{'#' * min(len(words), 4)} `{' '.join(words + args)}`\n")
    out.append("\n" + "\n\n".join(text(p) for p in about) + "\n")
    entries = [
        (name, short, paragraphs)
        for section in ("Arguments", "Options")
        for name, short, paragraphs in sections.get(section, [])
        if name not in global_options and name not in ("--help", "--version")
    ]
    if entries:
        out.append("\n")
        for name, short, paragraphs in entries:
            flag = f"`{short}`, " if short else ""
            what = describe(paragraphs)
            out.append(f"- {flag}`{name}`" + (f": {what}" if what else "") + "\n")
    for name, _, _ in sections.get("Commands", []):
        if name != "help" and (only is None or name in only):
            command(bin_dir, words + [name], global_options, out)


def page(bin_dir, slug, title, description, program, commands):
    _, _, sections = parse(help_text(bin_dir, [program]))
    listed = {name for name, _, _ in sections.get("Commands", [])} - {"help"}
    missing = listed - {c for p in PAGES if p[3] == program for c in p[4]}
    if missing:
        sys.exit(f"cli.py: `{program}` has commands no page shows: {sorted(missing)}; add them to PAGES")
    # Options every command takes are said once, at the end, not under each command.
    options = [o for o in sections.get("Options", []) if o[0] not in ("--help", "--version")]
    others = [f"[{p[1]}](../{p[0]}/)" for p in PAGES if p[3] == program and p[0] != slug]
    out = [
        "---\n",
        f"title: {title}\n",
        f"description: {description}\n",
        "---\n\n",
        f"<!-- Written by site/scripts/reference/cli.py from `{program} --help`. Change the help, then run it. -->\n",
    ]
    command(bin_dir, [program], {o[0] for o in options}, out, commands)
    if others:
        out.insert(6, f"\nThe other commands are on {', '.join(others)}.\n")
    if options:
        out.append("\n## Every command\n\n")
        for name, short, paragraphs in options:
            flag = f"`{short}`, " if short else ""
            out.append(f"- {flag}`{name}`: {describe(paragraphs)}\n")
    return "".join(out)


def main():
    bin_dir, check = sys.argv[1], "--check" in sys.argv[2:]
    status = 0
    for spec in PAGES:
        path = OUT / f"{spec[0]}.md"
        program = spec[3]
        body = page(bin_dir, *spec)
        if check:
            if not path.exists() or path.read_text() != body:
                print(f"{path} differs from `{program} --help`; run python3 site/scripts/reference/cli.py {bin_dir}", file=sys.stderr)
                status = 1
        else:
            OUT.mkdir(parents=True, exist_ok=True)
            path.write_text(body)
    sys.exit(status)


main()
