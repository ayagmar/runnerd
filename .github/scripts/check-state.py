#!/usr/bin/env python3
"""Fails on implicit state machines: a lifecycle spread over several fields, or written from more than one place.

CLAUDE.md's "State" section says what the code must do; ADR 0114 says why. Rules, on non-test Rust:

C1  A struct a long-lived owner holds (reached from ROOTS, through structs, tuple structs and enum payloads), that is
    not wire data, with two or more lifecycle fields (bool, Option, Vec, set, map), must be sealed: those fields
    private, in a file with no child modules, so rustc proves that file is the only writer; and only methods of the
    struct write them. One that is not sealed fails once two lifecycle fields are written after construction, or at
    once if it has two bools: a struct replaced whole shows no field write.
C2  One function inserts or removes the same key in two map or set fields of Engine: two maps with one key are one map
    of a struct.
C3  The same two fields of one receiver are joined by || or && at two or more sites: a condition over a state is a
    method of it.
C4  Engine has more lifecycle-typed fields than the recorded limit: new state goes in a sealed type Engine holds.
C5  SQL updates the jobs table outside the store's writer.
C6  A named machine no rule above can see is still there, found by a fixed pattern (KNOWN).

Every finding fails. Exceptions that stay are in MARKED, with the reason every combination of the fields is meant, and
ENGINE_LIFECYCLE_LIMIT is Engine's exact count. The ratchet compares them with origin/main, and OPAQUE and ROOTS too, as
what the walk does not reach is not a finding: MARKED gains no entry, the limit does not rise, and the debt file
(.github/state-debt.txt), which listed the findings still to fix until the last of them went, does not come back.

The script first runs against state-fixtures/ and fails if it stops catching a case there. It matches names, not
types: see ADR 0114 for what it cannot see.
"""
import ast
import collections
import dataclasses
import os
import pathlib
import re
import subprocess
import sys

SCRIPT = pathlib.Path(__file__).resolve()
REPO = SCRIPT.parents[2]
DEBT = ".github/state-debt.txt"
FIXTURES = SCRIPT.parent / "state-fixtures"
LIMIT_KEY = "engine-lifecycle-fields"
MARKABLE = "# may become a marker"
COMPARED = ("MARKED", "ENGINE_LIFECYCLE_LIMIT", "OPAQUE", "ROOTS")

# The long-lived owners a lifecycle lives in. Adding one is part of the change that adds it.
ROOTS = (
    "crates/core/src/scheduler/engine.rs:Engine",
    "crates/core/src/buildcache.rs:State",
    "crates/core/src/admission/replay.rs:Replayer",
    "crates/core/src/client.rs:Events",
    "crates/core/src/observe/metrics.rs:Sampler",
    "crates/daemon/src/api/notify.rs:Target",
    "crates/daemon/src/api/events.rs:Events",
    "crates/daemon/src/api/doctor_cache.rs:Last",
    "crates/daemon/src/api/attention.rs:Snapshot",
    "crates/cli/src/tui/app.rs:App",
    "crates/cli/src/watch.rs:State",
    "crates/web/src/auth.rs:Credentials",
)
ENGINE = "crates/core/src/scheduler/engine.rs:Engine"

# Held by a root, but no lifecycle of the holder's: the walk does not go through them. Wire types are not walked either.
OPAQUE = {
    "Config": "the parsed config: a value a reload replaces whole",
    "Store": "a handle on the writer thread, not walked into: what it writes is in the database, which C5 covers, and its own in-memory fields have their marker",
    "Control": "the channel commands and the shutdown mode arrive by; the sets it keeps behind mutexes are not checked",
    "Status": "the published status: built whole by each publish",
}

# Exceptions that stay: finding -> why every combination is meant. A new entry fails the ratchet: seal the type instead.
# One that no longer matches anything fails, too.
MARKED = {
    "C1 cli App": "each field is one view's own data or one read of the daemon's, filled on its own; the footer's questions are one value, Prompts, and help is drawn over them, every combination meant",
    "C1 cli History": "what the user filters by, each on its own, and the pages behind the one shown: no field says what state another is in",
    "C1 cli JobView": "the buffers of three separate reads of the daemon, each filled when its answer comes",
    "C1 cli State": "the last value of each event stream the watch view follows: every stream is independent",
    "C1 core Images": "states holds each image's own enum; the due flag, the one removal in flight and the refused tags are independent facts",
    "C1 core JobRuns": "a window of past runs that Recent::read fills from the history: a hold and the overruns are separate evidence",
    "C1 core Noted": "two independent notes for the next pass: the containers Docker named, and that it may have missed some",
    "C1 core Predictor": "statistics over past runs, by job and by profile: learning a run adds to each window on its own",
    "C1 core Ran": "one row of the history as read: its options are NULLable columns",
    "C1 core Scopes": "three refs GitHub's answer gives, each present or not on its own, replaced whole when a runner's writes are decided",
    "C1 core State": "the build cache's tables under its one mutex: entries with their index, runners, default branches",
    "C1 core Store": "memos other threads read instead of a query (the latest samples, the held samples, the runners gone) and one flag that stops a prune: each is written alone, and none says what state another is in",
    "C1 core Trace": "the input of one replay, built whole by store::trace_at and only read after",
    "C1 core crates/core/src/buildcache.rs:Runner": "two independent tables of a runner's GitHub Actions cache: the saves it has not yet made entries, and the entries its lookups found",
    "C1 core crates/core/src/cache.rs:Trimmed": "what one trim task returns, built whole",
    "C1 core crates/core/src/scheduler.rs:Left": "three independent budgets of one plan: each is bounded or not on its own",
    "C1 core crates/core/src/scheduler/warm.rs:Run": "what is known of one workflow run so far: its name when learned, and the jobs seen",
    "C1 daemon Target": "independent deadlines combined by max (since, sent, retry_at) and the set of insights told; every combination is reached",
    "C1 web Credentials": "two independent kinds of credential behind Sessions' mutex: the dashboard's links and the open pairings",
    "C1 web Links": "the published link file: codes shrink as they are redeemed, the file and bases are fixed",
    "C2 retired slots": "a profile's slots and its config snapshot: a retired profile keeps its config until its slots are empty",
    "C3 hit_cap oom_killed &&": "two facts the kernel reports separately about a finished run, not a state of it",
}

# Engine's lifecycle fields, exactly: one more fails C4, one fewer fails until this is lowered to match.
ENGINE_LIFECYCLE_LIMIT = 19

JOB_WRITERS = ("store/write.rs", "store.rs")

# Machines the rules cannot see, by where and how they are written: finding -> (file, pattern on code without comments).
KNOWN = {
    "C6 warm.learned": ("crates/core/src/scheduler/engine/warm.rs", r"=\s*SystemTime::UNIX_EPOCH\b"),
}

SKIP = re.compile(r"(^|/)(tests?|examples|fixtures|benches)(/|$)|(^|/)(tests|testing|testdir|parity)\.rs$|/build\.rs$")
LIFE = re.compile(r"^(bool$|Atomic(Bool|U\d+|I\d+|Usize|Isize)$|Option<|Vec<|VecDeque<|BTreeSet<|HashSet<|BTreeMap<|HashMap<)")
KEYED = re.compile(r"^(BTreeSet<|HashSet<|BTreeMap<|HashMap<)")
BUILDS = re.compile(r"^(new|default|from\w*|with_\w+|parse\w*|try_from)$")
# What only shares or guards a value: a field's lifecycle is the type inside.
PATHS = re.compile(r"\b(?:std|core|alloc|tokio)::(?:\w+::)*")
WRAPPERS = re.compile(r"^(?:Arc|Rc|Mutex|RwLock|RefCell|Cell)<(.*)>$")
MUTATORS = (
    r"take|replace|insert|remove|retain|clear|get_or_insert\w*|entry|extend|append|push\w*|pop\w*|drain|truncate"
    r"|get_mut|iter_mut|values_mut|as_mut|first_mut|last_mut|sort\w*|dedup\w*"
    r"|store|fetch_\w+|swap|compare_exchange\w*|set"
)
CHAIN = r"((?:\.\w+|\[[^\]]*\])+)"
ASSIGNED = re.compile(CHAIN + r"\s*(?:\|=|&=|\+=|-=|\*=|=(?![=>]))")
MUTATED = re.compile(CHAIN + r"\s*\.\s*(?:" + MUTATORS + r")\s*(?:::<[^>]*>)?\(")
BORROWED = re.compile(r"&mut\s+\*?\w+" + CHAIN)
# Through a guard taken and used in one expression. One held in a binding is not seen (ADR 0114).
WRITES_IT = r"\s*(?:\.\s*(?:" + MUTATORS + r")\s*(?:::<[^>]*>)?\(|(?:\|=|&=|\+=|-=|\*=|=(?![=>])))"
# What may follow the guard before the write: fields of the value it guards, written too.
INSIDE = r"((?:\s*\.\w+|\[[^\]]*\])*)"
GUARDED = re.compile(
    CHAIN + r"\s*\.\s*(?:lock|write|borrow_mut)\(\)"
    r"(?:\s*\.\s*(?:unwrap\(\)|await|expect\([^)]*\)|unwrap_or_else\((?:[^()]|\([^()]*\))*\)))*"
    + INSIDE + WRITES_IT
)
# Through sync::lock, the way code here takes a lock: `*lock(&self.x) = ...`, `lock(&self.x).y.push(1)`.
LOCKED = re.compile(r"\block\(&\s*\w+" + CHAIN + r"\)" + INSIDE + WRITES_IT)


def lifecycle_type(written):
    """A field's type as the rules read it: without std's paths and what only shares or guards it."""
    ty = PATHS.sub("", written)
    while m := WRAPPERS.match(ty):
        ty = m[1].strip()
    return ty


def scrub(text):
    """Blanks comments, strings and char literals, keeping every offset."""
    out, i, n = list(text), 0, len(text)
    while i < n:
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
        elif text.startswith("/*", i):
            j = text.find("*/", i + 2)
            j = n if j < 0 else j + 2
        elif (m := re.match(r'b?r(#*)"', text[i : i + 12])) and not (text[i - 1 : i].isalnum() or text[i - 1 : i] == "_"):
            end = '"' + m[1]
            j = text.find(end, i + m.end())
            j = n if j < 0 else j + len(end)
        elif text[i] == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            j += 1
        elif text[i] == "'" and (m := re.match(r"'(\\.[^']*|[^\\'])'", text[i : i + 12])):
            j = i + m.end()
        else:
            i += 1
            continue
        for k in range(i, min(j, n)):
            if out[k] != "\n":
                out[k] = " "
        i = j
    return "".join(out)


def closing(s, i, pair="{}"):
    """The offset of the bracket closing the one at `i`."""
    depth = 0
    for j in range(i, len(s)):
        depth += (s[j] == pair[0]) - (s[j] == pair[1])
        if s[j] == pair[1] and depth == 0:
            return j
    return len(s)


def item_end(s, i):
    """The offset where the item starting at `i` ends: its closing brace, or the `;` or `,` after it."""
    depth = 0
    for j in range(i, len(s)):
        if s[j] in "([<" and s[j - 1 : j + 1] != "->":
            depth += 1
        elif s[j] in ")]" or (s[j] == ">" and s[j - 1] != "-"):
            depth -= 1
        elif s[j] == "{" and depth == 0:
            return closing(s, j)
        elif s[j] in ";," and depth == 0:
            return j
    return len(s)


def imports(code):
    """What a file's `use` lines bring in: name -> the path segments before it."""
    found = collections.defaultdict(list)
    def expand(prefix, tree):
        tree = tree.strip()
        if m := re.match(r"(.*?)\{(.*)\}$", tree, re.S):
            path = [*prefix, *[p for p in m[1].split("::") if p]]
            for part in split_top(m[2]):
                if part.strip():
                    expand(path, part)
            return
        tree = re.sub(r"\s+as\s+\w+$", "", tree)
        *path, name = tree.split("::")
        found[name.strip()].append([*prefix, *[p.strip() for p in path]])
    for m in re.finditer(r"\buse\s+([^;]+);", code):
        expand([], m[1])
    return found


def blank_test_code(s, text):
    """`text` without the items under #[cfg(test)], found in `s`, its scrubbed form, keeping offsets."""
    out = list(text)
    for attribute in re.finditer(r"#\[cfg\(test\)\]", s):
        i = attribute.end()
        while m := re.match(r"\s*#\[", s[i:]):
            i = closing(s, i + m.end() - 1, "[]") + 1
        end = item_end(s, i)
        for k in range(attribute.start(), min(end + 1, len(s))):
            if out[k] != "\n":
                out[k] = " "
    return "".join(out)


def split_top(body):
    """Splits on commas outside any bracket."""
    parts, depth, cur = [], 0, ""
    for ch in body:
        if ch in "<([{":
            depth += 1
        elif ch in ")]}" or (ch == ">" and cur[-1:] != "-"):
            depth -= 1
        if ch == "," and depth == 0:
            parts.append(cur)
            cur = ""
        else:
            cur += ch
    parts.append(cur)
    return parts


@dataclasses.dataclass
class Item:
    """A struct or an enum."""

    name: str
    file: str
    line: int
    # field -> (type, whether it is visible outside its module); a tuple struct's fields are 0, 1, ...
    fields: dict
    wire: bool
    # An enum's payload text, for walking its types.
    payload: str = ""

    @property
    def key(self):
        return f"{self.file}:{self.name}"


def parse_items(rel, s, raw):
    items, lines = [], raw.split("\n")
    def wire(line):
        k, derives = line - 2, ""
        while k >= 0 and lines[k].strip().startswith(("#[", "//")):
            if "derive(" in lines[k] and lines[k].strip().startswith("#["):
                derives += lines[k]
            k -= 1
        return "Serialize" in derives or "Deserialize" in derives
    header = re.compile(r"(?m)^[ \t]*(?:pub(?:\([a-z]+\))?\s+)?(struct|enum)\s+(\w+)\s*(?:<[^{;(]*>)?\s*(?:where[^{;(]*)?([{(;])")
    for m in header.finditer(s):
        line = s[: m.start()].count("\n") + 1
        kind, name, opener = m[1], m[2], m[3]
        fields, payload = {}, ""
        at = m.end() - 1
        if opener == ";":
            continue
        body = s[at + 1 : closing(s, at, "{}" if opener == "{" else "()")]
        if kind == "enum":
            # What its variants carry; a variant's own name is not a type.
            for part in split_top(body):
                part = re.sub(r"#\[[^\]]*\]", "", part)
                if variant := re.match(r"\s*\w+\s*([({].*)$", part, re.S):
                    payload += variant[1] + " "
        elif opener == "{":
            for part in split_top(body):
                part = re.sub(r"#\[[^\]]*\]", "", part)
                f = re.match(r"\s*(pub(?:\([a-z]+\))?\s+)?(\w+)\s*:\s*(.+)$", part, re.S)
                if f:
                    fields[f[2]] = (" ".join(f[3].split()), bool(f[1]))
        else:
            for index, part in enumerate(p for p in split_top(body) if p.strip()):
                f = re.match(r"\s*(pub(?:\([a-z]+\))?\s+)?(.+)$", part, re.S)
                fields[str(index)] = (" ".join(f[2].split()), bool(f[1]))
        items.append(Item(name, rel, line, fields, wire(line), payload))
    return items


def takes_self(s, after_name):
    """Whether the function whose name ends at `after_name` has a `self` parameter."""
    depth, i = 0, after_name
    while i < len(s) and not (s[i] == "(" and depth == 0):
        if s[i] == "<":
            depth += 1
        elif s[i] == ">" and s[i - 1] != "-":
            depth -= 1
        i += 1
    return re.match(r"\(\s*(?:&\s*(?:'\w+\s+)?)?(?:mut\s+)?self\b", s[i : i + 80]) is not None


@dataclasses.dataclass
class Write:
    """A place that writes a field."""

    file: str
    function: str
    owner: str  # the type of the enclosing impl, if any
    on_self: bool


class Source:
    """The Rust of a tree, without its tests."""

    def __init__(self, root, opaque=()):
        self.root, self.opaque = root, opaque
        self.code, self.raw, self.items, self.by_name = {}, {}, {}, collections.defaultdict(list)
        for path in sorted((root / "crates").rglob("*.rs")):
            rel = path.relative_to(root).as_posix()
            if SKIP.search(rel):
                continue
            text = path.read_text()
            scrubbed = scrub(text)
            code = blank_test_code(scrubbed, scrubbed)
            self.code[rel] = code
            # Strings kept, for what is written in SQL.
            self.raw[rel] = blank_test_code(scrubbed, text)
            for item in parse_items(rel, code, text):
                self.items.setdefault(item.key, item)
                self.by_name[item.name].append(item.key)
        self.writes = collections.defaultdict(list)
        for rel, code in self.code.items():
            self.read_writes(rel, code)

    def read_writes(self, rel, s):
        impls = [
            (m.start(), closing(s, m.end() - 1), m[1].split("::")[-1])
            for m in re.finditer(r"(?m)^impl(?:<[^{]*?>)?\s+(?:[\w:<>, ]+\s+for\s+)?([\w:]+)(?:<[^{]*>)?\s*(?:where[^{]*)?\{", s)
        ]
        functions = [(m.start(), m[1], takes_self(s, m.end())) for m in re.finditer(r"\bfn\s+(\w+)", s)]
        for pattern in (ASSIGNED, MUTATED, BORROWED, GUARDED, LOCKED):
            for m in pattern.finditer(s):
                function, method = next(((n, h) for a, n, h in reversed(functions) if a <= m.start()), (None, False))
                # A constructor builds a value no one else holds yet; a method with such a name writes one that exists.
                if function is None or (BUILDS.match(function) and not method):
                    continue
                owner = next((n for a, b, n in impls if a <= m.start() < b), None)
                on_self = s[: m.start(1)].endswith("self")
                inside = m[2] if pattern in (GUARDED, LOCKED) else ""
                for field in re.findall(r"\.(\w+)", m[1] + inside):
                    self.writes[field].append(Write(rel, function, owner, on_self))

    def resolve(self, name, holder, written):
        """What `name` means in the type `written` in `holder`: the definition in that file, the path it is qualified
        with or imported by, or else every definition in the crate, so nothing is missed."""
        keys = self.by_name.get(name, [])
        crate = holder.split("/")[1]
        same_file = [k for k in keys if k.split(":")[0] == holder]
        if same_file:
            return same_file
        same_crate = [k for k in keys if k.split("/")[1] == crate]
        if len(same_crate) <= 1:
            return same_crate
        qualified = [m for m in re.findall(rf"(\w+)::{name}\b", written)]
        paths = [path for path in imports(self.code[holder]).get(name, []) if path]
        hints = qualified + [path[-1] for path in paths]
        hints = [hint for hint in hints if hint not in ("crate", "self", "super")]
        narrowed = [k for k in same_crate for hint in hints if re.search(rf"/{hint}(\.rs|/)", k.split(":")[0])]
        return narrowed or same_crate

    def reached(self, roots):
        seen, todo = set(), list(roots)
        missing = [root for root in roots if root not in self.items]
        while todo:
            key = todo.pop()
            if key in seen or key not in self.items:
                continue
            seen.add(key)
            item = self.items[key]
            if item.wire or item.name in self.opaque:
                continue
            for written in [ty for ty, _ in item.fields.values()] + [item.payload]:
                for name in set(re.findall(r"\b[A-Z]\w*", written)):
                    todo += self.resolve(name, item.file, written)
        return seen, missing


def finding_id(source, item):
    """A struct's name in the debt file and MARKED: its crate and name, and its file where two would share them."""
    crate = item.file.split("/")[1]
    twins = [k for k in source.by_name[item.name] if k.split("/")[1] == crate]
    return f"C1 {crate} {item.name}" if len(twins) == 1 else f"C1 {crate} {item.file}:{item.name}"


def analyse(root, roots, engine, marked, opaque=(), known=()):
    """Every finding as id -> message, Engine's lifecycle field count, and problems with the check itself."""
    source, findings, problems = Source(root, opaque), {}, []
    used = set()
    seen, missing = source.reached(roots)
    problems += [f"root {root} no longer exists: update ROOTS" for root in missing]
    parents = {rel for rel, s in source.code.items() if re.search(r"(?m)^\s*(?:pub(?:\([a-z]+\))?\s+)?mod\s+\w+\s*[;{]", s)}
    for key in sorted(seen):
        item = source.items[key]
        life = {f for f, (ty, _) in item.fields.items() if LIFE.match(lifecycle_type(ty))}
        if key == engine or item.wire or len(life) < 2:
            continue
        identifier = finding_id(source, item)
        if identifier in marked:
            used.add(identifier)
            continue
        sealed = item.file not in parents and not any(item.fields[f][1] for f in life)
        crate = item.file.split("/")[1]
        hits = collections.defaultdict(list)
        for field in life:
            for write in source.writes[field]:
                if write.file != item.file and (sealed or write.file.split("/")[1] != crate):
                    continue
                other = write.on_self and write.owner and write.owner != item.name and field in {
                    f for k in source.by_name.get(write.owner, []) for f in source.items[k].fields
                }
                if not other:
                    hits[field].append(write)
        where = f"{item.file.removeprefix('crates/')}:{item.line}: {item.name}"
        if sealed:
            outside = sorted({f"{w.function} ({f})" for f, writes in hits.items() for w in writes if w.owner != item.name})
            if outside:
                findings[identifier] = f"{where}: written outside its own methods: {', '.join(outside)}"
        elif len(hits) >= 2 or sum(item.fields[f][0] == "bool" for f in life) >= 2:
            findings[identifier] = f"{where}: lifecycle fields not sealed in a leaf file: {', '.join(sorted(life))}"
    engine_item = source.items.get(engine)
    count = 0
    if engine_item:
        count = sum(1 for ty, _ in engine_item.fields.values() if LIFE.match(lifecycle_type(ty)))
        keyed = {f for f, (ty, _) in engine_item.fields.items() if KEYED.match(lifecycle_type(ty))}
        tree = engine.split(":")[0].removesuffix(".rs")
        pattern = re.compile(r"self\.(\w+)\s*\.\s*(?:insert|remove|entry)\(\s*&?\*?([\w.]+?)(?:\.clone\(\)|\.to_owned\(\)|\.to_string\(\))?\s*[,)]")
        for rel, s in source.code.items():
            if not rel.startswith(tree):
                continue
            functions = list(re.finditer(r"\bfn\s+(\w+)", s))
            for i, m in enumerate(functions):
                body = s[m.start() : functions[i + 1].start() if i + 1 < len(functions) else len(s)]
                keys = collections.defaultdict(set)
                for w in pattern.finditer(body):
                    if w[1] in keyed:
                        keys[w[2]].add(w[1])
                for key, names in keys.items():
                    names = sorted(names)
                    for a, position in zip(names, range(len(names))):
                        for b in names[position + 1 :]:
                            identifier = f"C2 {a} {b}"
                            if identifier in marked:
                                used.add(identifier)
                            else:
                                findings[identifier] = f"{rel.removeprefix('crates/')}: {m[1]} writes {a} and {b} under one key, {key}"
    sites = collections.defaultdict(list)
    for rel, s in source.code.items():
        for m in re.finditer(r"\b([a-z_]\w*)\.(\w+)(?:\.\w+\(\))?\s*(\|\||&&)\s*!?\1\.(\w+)", s):
            a, b = sorted((m[2], m[4]))
            sites[f"C3 {a} {b} {m[3]}"].append(rel)
    for identifier, files in sites.items():
        if len(files) >= 2 and identifier in marked:
            used.add(identifier)
        elif len(files) >= 2:
            findings[identifier] = f"{identifier[3:]} is spelled at {len(files)} sites: {', '.join(sorted({f.split('/')[-1] for f in files}))}"
    for rel, raw in source.raw.items():
        if re.search(r"(?i)\bUPDATE\s+jobs\b", raw) and not rel.endswith(JOB_WRITERS):
            findings[f"C5 {rel}"] = f"{rel}: updates the jobs table outside the store's writer"
    for identifier, (rel, pattern) in dict(known).items():
        if re.search(pattern, source.code.get(rel, "")):
            findings[identifier] = f"{rel}: {identifier[3:]} is still a machine no other rule sees"
    problems += [f"MARKED has {mark!r}, which no longer matches anything: remove it" for mark in sorted(set(marked) - used)]
    return findings, count, problems


def self_test():
    """The fixtures: every case in expected.txt is caught, and nothing else is."""
    expected = {
        line.strip()
        for line in (FIXTURES / "expected.txt").read_text().splitlines()
        if line.strip() and not line.startswith("#")
    }
    root = "crates/fixture/src/engine.rs:Engine"
    marked = {"C1 fixture Marked": "a fixture of a marker"}
    known = {"C6 known": ("crates/fixture/src/updates.rs", r"\bKNOWN_MACHINE\b")}
    findings, count, problems = analyse(FIXTURES, (root,), root, marked, known=known)
    stale = analyse(FIXTURES, (root,), root, marked | {"C1 fixture Gone": "a marker of nothing"})[2]
    if not any("C1 fixture Gone" in problem for problem in stale):
        problems.append("a marker that matches nothing is no longer caught")
    problems += ratchet_problems()
    found = set(findings) | {f"{LIMIT_KEY} {count}"}
    missed, extra = sorted(expected - found), sorted(found - expected)
    for line in problems + [f"the check no longer catches: {m}" for m in missed] + [f"the check now flags: {e}" for e in extra]:
        print(f"check-state fixtures: {line}", file=sys.stderr)
    return not (problems or missed or extra)


def read_debt(text):
    """The findings a debt file lists, its limit on Engine's lifecycle fields, and the findings tagged MARKABLE."""
    lines, limit, markable = set(), None, set()
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        finding, tag, _ = line.partition(MARKABLE)
        finding = finding.strip()
        if finding.startswith(LIMIT_KEY + " "):
            limit = int(finding.split()[1])
        else:
            lines.add(finding)
            if tag:
                markable.add(finding)
    return lines, limit, markable


def constants(script):
    """What a version of this script sets that the ratchet compares."""
    found = {}
    for node in ast.parse(script).body:
        if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name) and node.targets[0].id in COMPARED:
            found[node.targets[0].id] = ast.literal_eval(node.value)
    return found


def git(*arguments):
    return subprocess.run(["git", "-C", str(REPO), *arguments], capture_output=True, text=True, check=False)


def at_base(path):
    """A file as origin/main has it (FETCH_HEAD), or None."""
    shown = git("show", f"FETCH_HEAD:{path}")
    return shown.stdout if shown.returncode == 0 else None


def base_is_available():
    """Whether FETCH_HEAD is origin/main. CI's job fetched it, or the ratchet cannot run; a local run fetches it."""
    if os.environ.get("CI"):
        return git("rev-parse", "--verify", "--quiet", "FETCH_HEAD").returncode == 0
    return git("fetch", "--quiet", "origin", "main").returncode == 0


def root_identity(root):
    """A root as the ratchet follows it: its crate and name, so moving it to a leaf file within its crate is no drop."""
    path, name = root.split(":")
    return path.split("/")[1], name


def compare(base, debt, limit, marked, constant, markable=frozenset(), opaque=(), roots=()):
    """What the debt file and MARKED may not do against `base`, main's versions of them; returns the failures."""
    failures = []
    base_debt = base["debt"]
    base_lines, base_limit, base_markable = read_debt(base_debt) if base_debt is not None else (set(), None, set())
    # A line main tags may become a marker: what remains of it is meant. Any other marker would empty the list unfixed.
    added_marks = sorted(set(marked) - set(base["marked"]) - base_markable)
    failures += [f"new marker {mark!r}: seal the type instead" for mark in added_marks]
    failures += [f"{DEBT}: {line!r} newly tagged {MARKABLE!r}" for line in sorted(set(markable) - base_markable)]
    # What the walk does not reach is not a finding, so either of these would retire debt with nothing fixed.
    failures += [f"new OPAQUE key {key!r}: the walk would stop finding what it holds" for key in sorted(set(opaque) - set(base.get("opaque", ())))]
    dropped = {root_identity(root) for root in base.get("roots", ())} - {root_identity(root) for root in roots}
    failures += [f"root {name} of {crate} dropped from ROOTS: the walk would stop finding what it holds" for crate, name in sorted(dropped)]
    if debt is not None and base_debt is None:
        failures.append(f"{DEBT} came back after it was removed: fix the findings instead")
    if base_debt is not None:
        failures += [f"{DEBT}: new line {line!r}" for line in sorted((debt or set()) - base_lines)]
        if limit is not None and base_limit is not None and limit > base_limit:
            failures.append(f"{LIMIT_KEY} rose from {base_limit} to {limit}")
    if constant is not None and base["constant"] is not None and constant > base["constant"]:
        failures.append(f"ENGINE_LIFECYCLE_LIMIT rose from {base['constant']} to {constant}")
    return failures


def ratchet(debt, limit, markable, read=None):
    """Compares with origin/main, as `read` shows a file of it; returns the failures."""
    read = read or at_base
    script = SCRIPT.relative_to(REPO).as_posix()
    base_script = read(script)
    if base_script is None:
        # Only the change that adds the check has no script on main: a debt file there means this one was moved.
        if read(DEBT) is not None:
            return [f"{DEBT} is on origin/main but {script} is not, so nothing can be compared: keep the script's path"]
        return []
    on_main = constants(base_script)
    base = {
        "debt": read(DEBT),
        "marked": on_main.get("MARKED", {}),
        "constant": on_main.get("ENGINE_LIFECYCLE_LIMIT"),
        "opaque": on_main.get("OPAQUE", {}),
        "roots": on_main.get("ROOTS", ()),
    }
    return compare(base, debt, limit, MARKED, ENGINE_LIFECYCLE_LIMIT, markable, OPAQUE, ROOTS)


def ratchet_problems():
    """The cases the ratchet exists for, each named by what it would let through."""
    text = f"C1 a\nC1 b {MARKABLE}\nengine-lifecycle-fields 5\n"
    base = {"debt": text, "marked": {"C1 m": "kept"}, "constant": None}
    gone = {"debt": None, "marked": {"C1 m": "kept"}, "constant": 5}
    allowed = [
        ("a line retired", base, {"C1 a"}, 5, {"C1 m": ""}, None),
        ("the limit lowered", base, {"C1 a", "C1 b"}, 4, {"C1 m": ""}, None),
        ("a tagged line made a marker", base, {"C1 a"}, 5, {"C1 m": "", "C1 b": ""}, None),
        ("the debt file gone with the limit in the script", base, None, None, {"C1 m": ""}, 5),
        ("no debt file on main or here", gone, None, None, {"C1 m": ""}, 5),
    ]
    refused = [
        ("a new line", base, {"C1 a", "C1 b", "C1 c"}, 5, {"C1 m": ""}, None),
        ("a raised limit", base, {"C1 a"}, 6, {"C1 m": ""}, None),
        ("a new marker", base, {"C1 a"}, 5, {"C1 m": "", "C1 n": ""}, None),
        ("an untagged line made a marker", base, {"C1 b"}, 5, {"C1 m": "", "C1 a": ""}, None),
        ("a tag added here", base, {"C1 a", "C1 b"}, 5, {"C1 m": ""}, None, {"C1 a"}),
        ("the debt file back", gone, {"C1 a"}, 5, {"C1 m": ""}, 5),
        ("a raised constant", gone, None, None, {"C1 m": ""}, 6),
    ]
    walked = {**base, "opaque": {"Kept": "", "Going": ""}, "roots": ("crates/a/src/x.rs:R", "crates/b/src/y.rs:R")}
    same = (walked, {"C1 a", "C1 b"}, 5, {"C1 m": ""}, None, frozenset())
    roots = walked["roots"]
    allowed += [
        ("an OPAQUE key removed", *same, {"Kept": ""}, roots),
        ("a root added", *same, walked["opaque"], (*roots, "crates/a/src/z.rs:S")),
        ("a root moved within its crate", *same, walked["opaque"], ("crates/a/src/x/leaf.rs:R", roots[1])),
    ]
    refused += [
        ("a new OPAQUE key", *same, {**walked["opaque"], "Measured": ""}, roots),
        ("a root dropped", *same, walked["opaque"], roots[:1]),
        ("a root moved to another crate", *same, walked["opaque"], ("crates/c/src/x.rs:R", roots[1])),
    ]
    problems = [f"the ratchet refuses {name}" for name, *case in allowed if compare(*case)]
    problems += [f"the ratchet lets through {name}" for name, *case in refused if not compare(*case)]
    moved = {DEBT: text}
    if not ratchet(None, None, set(), moved.get):
        problems.append("the ratchet lets through a script moved away from main's debt file")
    if ratchet(None, None, set(), {}.get):
        problems.append("the ratchet refuses the change that adds the check")
    return problems


def main():
    if not self_test():
        return 1
    findings, count, problems = analyse(REPO, ROOTS, ENGINE, MARKED, OPAQUE, KNOWN)
    failures = list(problems)
    debt_path = REPO / DEBT
    debt, limit, markable = None, ENGINE_LIFECYCLE_LIMIT, set()
    if debt_path.exists():
        # The ratchet fails on a debt file main does not have; this only says what is wrong with it.
        debt, limit, markable = read_debt(debt_path.read_text())
        if limit is None:
            failures.append(f"{DEBT} has no {LIMIT_KEY} line")
    elif limit is None:
        failures.append(f"no {DEBT}, so set ENGINE_LIFECYCLE_LIMIT")
    elif count != limit:
        # Nothing is left to list, so the script's limit is exact.
        failures.append(f"ENGINE_LIFECYCLE_LIMIT is {limit}, Engine has {count}: they must match once the debt file is gone")
    if limit is not None and count > limit:
        failures.append(f"Engine has {count} lifecycle-typed fields, past the limit of {limit}: put new state in a sealed type")
    listed = debt or set()
    failures += [findings[i] for i in sorted(set(findings) - listed)]
    stale = sorted(listed - set(findings))
    if os.environ.get("GITHUB_EVENT_NAME") == "push":
        # Two pull requests merged in turn can retire a line neither could alone: not a reason to turn main red.
        for line in stale:
            print(f"warning: {DEBT}: stale line {line!r}", file=sys.stderr)
    else:
        failures += [f"{DEBT}: stale line {line!r}: remove it" for line in stale]
    if base_is_available():
        failures += ratchet(debt, limit, markable)
    elif os.environ.get("CI"):
        failures.append("origin/main was not fetched, so the ratchet cannot run: git fetch origin main first")
    else:
        print("check-state: origin/main cannot be fetched, so the ratchet is skipped", file=sys.stderr)
    for failure in failures:
        print(failure, file=sys.stderr)
    if failures:
        print(f"check-state: {len(failures)} problem(s). See CLAUDE.md, State, and ADR 0114.", file=sys.stderr)
        return 1
    debt_lines = f"{len(listed)} debt lines, " if debt is not None else ""
    print(f"check-state: ok ({debt_lines}{count} Engine lifecycle fields)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
