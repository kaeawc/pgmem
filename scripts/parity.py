#!/usr/bin/env python3
"""Parity tracking between the Go test suite and its Rust port.

  scripts/parity.py generate   # (re)write parity/go_tests.tsv from the Go sources, keeping statuses
  scripts/parity.py check      # verify every Go test is ported (or explained); print the tally

Each Go test maps to a Rust test function named snake_case(<name without "Test">). The
manifest records where the Rust test lives and its status:
  ported     the Rust test exists and asserts the same behaviour
  deviation  the Rust test asserts real Postgres behaviour where pgmem-go deviated (see notes)
  pending    not ported yet
"""
import os
import re
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MANIFEST = os.path.join(ROOT, "parity", "go_tests.tsv")
HEADER = "go_package\tgo_file\tgo_test\trust_test\tstatus\tnotes"


def snake(name):
    name = name[4:] if name.startswith("Test") else name
    out = []
    for part in name.split("_"):
        part = re.sub(r"([A-Z]{2,})s(?=[A-Z0-9]|$)", lambda m: m.group(1).capitalize() + "s", part)
        s = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", part)
        s = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s)
        out.append(s.lower())
    return "_".join(p for p in out if p)


def go_tests():
    found = []
    for dirpath, _, files in os.walk(ROOT):
        if "/.git" in dirpath or "/target" in dirpath:
            continue
        for f in sorted(files):
            if not f.endswith("_test.go"):
                continue
            path = os.path.join(dirpath, f)
            pkg = os.path.relpath(dirpath, ROOT)
            with open(path) as fh:
                for line in fh:
                    m = re.match(r"func (Test\w+)\(t \*testing\.T\)", line)
                    if m:
                        found.append((pkg, f, m.group(1)))
    return sorted(found)


def default_location(pkg, f, test):
    base = f[: -len("_test.go")]
    fn = snake(test)
    if pkg == "postgres" or pkg.startswith("examples/"):
        mod = base if pkg == "postgres" else "example_" + pkg.split("/")[1]
        return f"crates/pgmem/tests/it/{mod}.rs::{fn}"
    crate = {"storage": "pgmem-storage", "types": "pgmem-types", "exec": "pgmem-exec", "postgres/parse": "pgmem-parse"}[pkg]
    return f"crates/{crate}::{fn}"


def load():
    rows = {}
    if os.path.exists(MANIFEST):
        with open(MANIFEST) as fh:
            for line in fh.read().splitlines()[1:]:
                if not line.strip():
                    continue
                cols = line.split("\t")
                cols += [""] * (6 - len(cols))
                rows[(cols[0], cols[1], cols[2])] = cols
    return rows


def generate():
    old = load()
    lines = [HEADER]
    for pkg, f, test in go_tests():
        prev = old.get((pkg, f, test))
        if prev:
            lines.append("\t".join(prev))
        else:
            lines.append("\t".join([pkg, f, test, default_location(pkg, f, test), "pending", ""]))
    os.makedirs(os.path.dirname(MANIFEST), exist_ok=True)
    with open(MANIFEST, "w") as fh:
        fh.write("\n".join(lines) + "\n")
    print(f"wrote {len(lines) - 1} tests to {os.path.relpath(MANIFEST, ROOT)}")


def rust_test_exists(location):
    path, fn = location.split("::")
    full = os.path.join(ROOT, path)
    pattern = re.compile(r"fn " + re.escape(fn) + r"\s*\(")
    targets = []
    if os.path.isdir(full):
        for dirpath, _, files in os.walk(full):
            targets += [os.path.join(dirpath, x) for x in files if x.endswith(".rs")]
    elif os.path.exists(full):
        targets = [full]
    for t in targets:
        with open(t) as fh:
            if pattern.search(fh.read()):
                return True
    return False


def check():
    rows = load()
    current = set(go_tests())
    if current and set(rows) != current:
        missing = current - set(rows)
        print(f"manifest out of date ({len(missing)} Go tests not listed); run: scripts/parity.py generate")
        return 1
    tally = {}
    errors = []
    for key, cols in sorted(rows.items()):
        status = cols[4]
        tally[status] = tally.get(status, 0) + 1
        if status in ("ported", "deviation") and not rust_test_exists(cols[3]):
            errors.append(f"{cols[3]} (for {key[0]}/{key[1]} {key[2]}) is marked {status} but does not exist")
        if status == "deviation" and not cols[5]:
            errors.append(f"{key[2]} is a deviation without notes")
    total = len(rows)
    done = tally.get("ported", 0) + tally.get("deviation", 0)
    print(f"parity: {done}/{total} Go tests ported ({tally.get('ported', 0)} ported, {tally.get('deviation', 0)} deviations, {tally.get('pending', 0)} pending)")
    for e in errors:
        print("ERROR:", e)
    return 1 if errors else 0


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "check"
    if cmd == "generate":
        generate()
    else:
        sys.exit(check())
