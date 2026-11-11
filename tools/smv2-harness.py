#!/usr/bin/env python3
"""Run the Search Metadata v2 EM/MG harness and print a per-case table.

Usage: python tools/smv2-harness.py [--out report.md] [--ignored] [-- extra cargo args]

  --ignored  also run ignored tests (MG migration cases need the audiolib-migrate
             binary built with --features test-hooks, or AUDIOLIB_SMV2_MIGRATE_CMD;
             EM-06 r824 needs AUDIOLIB_SMV2_BASELINE_DB; admin status needs
             AUDIOLIB_SMV2_ADMIN_URL + AUDIOLIB_SMV2_ADMIN_DB, one test at a time).
Case IDs come from test names: em_09_* -> EM-09, mg_01_* -> MG-01, em_07_08_* -> both.
A case is FAIL if any of its tests failed, PASS if every test passed, PARTIAL if some
passed and the rest were skipped, SKIP if all were skipped (reason = ignore reason).
A passing test that printed `SMV2-SKIP <case>: <reason>` makes its case PARTIAL.
"""
import argparse, os, re, subprocess, sys
from collections import OrderedDict

CASES = [f"EM-{i:02}" for i in range(1, 10)] + [f"MG-{i:02}" for i in range(1, 19)]
LINE = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored)(?:, (.*))?$")
SOFT = re.compile(r"SMV2-SKIP (\S+): (.*)$")


def run(extra, ignored):
    cmd = ["cargo", "test", "-p", "audiolib-db", "--test", "smv2_em", "--test", "smv2_mg", "--no-fail-fast", *extra, "--", "--show-output"]
    if ignored:
        cmd.append("--include-ignored")
    env = dict(os.environ, RUST_TEST_THREADS=os.environ.get("RUST_TEST_THREADS", "4"))
    p = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", errors="replace", env=env)
    sys.stderr.write(p.stderr[-4000:] if p.returncode else "")
    return p.stdout + p.stderr, p.returncode


def case_ids(test):
    m = re.match(r"(em|mg)((?:_\d\d)+)_", test)
    if not m:
        return []
    return [f"{m.group(1).upper()}-{n}" for n in m.group(2).strip("_").split("_")]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out")
    ap.add_argument("--ignored", action="store_true")
    ap.add_argument("extra", nargs="*")
    a = ap.parse_args()
    out, code = run(a.extra, a.ignored)
    tests = OrderedDict()
    for line in out.splitlines():
        m = LINE.match(line.strip())
        if m:
            tests[m.group(1)] = (m.group(2), m.group(3) or "")
    soft = OrderedDict()
    for line in out.splitlines():
        m = SOFT.search(line)
        if m:
            soft.setdefault(m.group(1), []).append(m.group(2).strip())
    per = OrderedDict((c, []) for c in CASES)
    for name, (status, reason) in tests.items():
        for c in case_ids(name):
            per.setdefault(c, []).append((name, status, reason))
    rows = ["| Case | Status | Tests | Detail |", "|---|---|---|---|"]
    for c, ts in per.items():
        st = [s for _, s, _ in ts]
        if not ts:
            status, detail = "MISSING", "no test"
        elif "FAILED" in st:
            status = "FAIL"
            detail = ", ".join(n for n, s, _ in ts if s == "FAILED")
        elif all(s == "ok" for s in st) and c in soft:
            status, detail = "PARTIAL", "; ".join(f"skip ({r})" for r in soft[c])
        elif all(s == "ok" for s in st):
            status, detail = "PASS", ""
        elif "ok" in st:
            status = "PARTIAL"
            detail = "; ".join([f"{n}: skip ({r})" for n, s, r in ts if s == "ignored"] + [f"skip ({r})" for r in soft.get(c, [])])
        else:
            status = "SKIP"
            detail = "; ".join(sorted({r for _, _, r in ts}))
        rows.append(f"| {c} | {status} | {len(ts)} | {detail} |")
    table = "\n".join(rows)
    print(table)
    if a.out:
        with open(a.out, "w", encoding="utf-8") as f:
            f.write(table + "\n")
    if not tests:
        print(out[-6000:], file=sys.stderr)
    return 1 if code else 0


if __name__ == "__main__":
    sys.exit(main())
