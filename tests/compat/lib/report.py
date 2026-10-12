#!/usr/bin/env python3
"""Aggregate RESULT|suite|case|STATUS|detail lines into a matrix; exit 1 on any FAIL."""
import sys, collections, re
rows = []
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    p = line.rstrip("\n").split("|", 4)
    if len(p) == 5:
        rows.append(p[1:])
suites = collections.OrderedDict()
for s, c, st, d in rows:
    suites.setdefault(s, collections.Counter())[st] += 1
cols = ["PASS", "FAIL", "KNOWN-FAIL", "SKIP"]
print("\n=== compatibility matrix ===")
w = max([len(s) for s in suites] + [5])
print(f"{'suite':<{w}}  " + "  ".join(f"{c:>10}" for c in cols))
tot = collections.Counter()
for s, cnt in suites.items():
    print(f"{s:<{w}}  " + "  ".join(f"{cnt[c]:>10}" for c in cols))
    tot.update(cnt)
print(f"{'TOTAL':<{w}}  " + "  ".join(f"{tot[c]:>10}" for c in cols))
fails = [r for r in rows if r[2] == "FAIL"]
if fails:
    print("\n--- FAIL (unexpected; these fail the run) ---")
    for s_, c, _, d in fails:
        print(f"[{s_}] {c}: {d[:300]}")
known = collections.OrderedDict()
for s_, c, st, d in rows:
    if st == "KNOWN-FAIL":
        reason = d.split(" // ")[0]
        m = re.match(r"((?:PG|MY)-\d+(?:/(?:PG|MY)-\d+)*)", reason)
        key = m.group(1) if m else reason[:60]
        known.setdefault(key, {"reason": reason, "items": []})["items"].append((s_, c))
if known:
    print("\n--- KNOWN-FAIL grouped by issue id (details: tests/compat/KNOWN_ISSUES.md) ---")
    for key in sorted(known, key=lambda k: (k[:2], int(re.sub(r"\D.*", "", k[3:]) or 0) if k[:2] in ("PG", "MY") else 0)):
        v = known[key]
        suites_ = sorted({i[0] for i in v["items"]})
        print(f"* {key}: {len(v['items'])} case(s); e.g. {v['reason'][len(key)+1:][:150].strip(': ')}\n    in: {', '.join(suites_)}")
skips = [r for r in rows if r[2] == "SKIP"]
if skips:
    print("\n--- SKIP ---")
    for s_, c, _, d in skips:
        print(f"[{s_}] {c}: {d[:300]}")
sys.exit(1 if tot["FAIL"] else 0)
