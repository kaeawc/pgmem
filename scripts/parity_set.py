#!/usr/bin/env python3
"""Updates parity/go_tests.tsv rows: parity_set.py <tsv-of-updates>
Each update line: go_test<TAB>status<TAB>notes[<TAB>rust_location]  (go_test may be pkg/file:Test to disambiguate)."""
import sys, os
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
M = os.path.join(ROOT, "parity", "go_tests.tsv")
lines = open(M).read().splitlines()
header, rows = lines[0], [l.split("\t") for l in lines[1:] if l.strip()]
for r in rows:
    r += [""] * (6 - len(r))
updates = [l.split("\t") for l in open(sys.argv[1]).read().splitlines() if l.strip() and not l.startswith("#")]
for u in updates:
    key, status, notes = u[0], u[1], u[2] if len(u) > 2 else ""
    loc = u[3] if len(u) > 3 else None
    hit = [r for r in rows if r[2] == key or f"{r[0]}/{r[1]}:{r[2]}" == key]
    if len(hit) != 1:
        sys.exit(f"{key}: matched {len(hit)} rows")
    hit[0][4], hit[0][5] = status, notes
    if loc:
        hit[0][3] = loc
open(M, "w").write("\n".join([header] + ["\t".join(r) for r in rows]) + "\n")
print(f"updated {len(updates)} rows")
