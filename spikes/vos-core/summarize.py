#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""W2 (gate G1): the run's samples against the gate's bounds, as Markdown.

    summarize.py OUT

Reads OUT/fit-*.txt and every OUT/*.csv the host wrote
(conn,seq,kind,classes,message,blobs,args,gas,outcome,us,peak).
"""
import csv
import glob
import os
import sys
from collections import defaultdict

GAS_GATE = 80_000_000
GAS_CEILING = 100_000_000
CODE_BOUND = 2 << 20
REQUEST = 64 << 10
HEAP = 512 << 10

out = sys.argv[1]
print(f"# W2 / G1 run `{os.path.basename(out)}`\n")

for path in sorted(glob.glob(os.path.join(out, "fit-*.txt"))):
    kv = {}
    for line in open(path):
        if "=" in line:
            for part in line.split():
                if "=" in part:
                    k, v = part.split("=", 1)
                    kv[k] = v
    name = os.path.basename(path)[4:-4]
    blob = int(kv.get("code_blob_bytes", "0") or 0)
    verdict = "pass" if blob and blob <= CODE_BOUND else "FAIL"
    print(
        f"- **{name}**: backend {kv.get('backend','?')}; code blob {blob:,} B against "
        f"{CODE_BOUND:,} ({blob - CODE_BOUND:+,} B, {verdict}); program {int(kv.get('actor_program_bytes','0') or 0):,} B; "
        f"stack {kv.get('stack_bytes','?')}; derive {kv.get('derive_us','?')} us; "
        f"install step {kv.get('install_step_us','?')} us; first query {kv.get('first_query_us','?')} us"
    )
print()

sizes = os.path.join(out, "archive.sizes")
if os.path.exists(sizes):
    with open(sizes) as f:
        recs = list(csv.DictReader(f))
    if recs:
        big = max(recs, key=lambda r: int(r["request"]))
        print(
            f"- **Recorded book** (code-history fixture, {len(recs)} invocations, each input alone): "
            f"largest request {int(big['request']):,} B ({big['classes']}) against {REQUEST:,}\n"
        )

rows = []
for path in sorted(glob.glob(os.path.join(out, "*.csv"))):
    run = os.path.basename(path)[:-4]
    with open(path) as f:
        for r in csv.DictReader(f):
            r["run"] = run
            rows.append(r)

if not rows:
    print("No samples.")
    sys.exit(0)

# Row 3d's fix (z, core path only) is reported apart: it is another program.
zrows = [r for r in rows if r["run"].startswith("zcore-")]
rows = [r for r in rows if not r["run"].startswith("zcore-")]

by_class = defaultdict(list)
for r in rows:
    key = r["kind"] if r["kind"] != "input" else r["classes"] or "input"
    by_class[key].append(r)

print("| Class | Inputs | Gas p50 | Gas max | Max vs 80 M | Args max (B) | Peak heap max (B) | Outcomes |")
print("|---|---:|---:|---:|---|---:|---:|---|")
worst_gas = worst_args = worst_heap = 0
for key in sorted(by_class, key=lambda k: -max(int(r["gas"]) for r in by_class[k])):
    rs = by_class[key]
    gas = sorted(int(r["gas"]) for r in rs)
    args = max(int(r["args"]) for r in rs)
    peaks = [int(r["peak"]) for r in rs if r.get("peak")]
    outcomes = defaultdict(int)
    for r in rs:
        outcomes[r["outcome"]] += 1
    p50 = gas[(len(gas) - 1) // 2]
    mx = gas[-1]
    worst_gas, worst_args = max(worst_gas, mx), max(worst_args, args)
    if peaks:
        worst_heap = max(worst_heap, max(peaks))
    verdict = "pass" if mx <= GAS_GATE else ("over 80 M" if mx <= GAS_CEILING else "OVER 100 M")
    peak = f"{max(peaks):,}" if peaks else "—"
    print(f"| {key} | {len(rs)} | {p50:,} | {mx:,} | {verdict} | {args:,} | {peak} |", end="")
    print(" " + ", ".join(f"{k} {v}" for k, v in sorted(outcomes.items())) + " |")

print()
print(f"- Worst gas: {worst_gas:,} against the 80 M gate ({worst_gas - GAS_GATE:+,}).")
print(f"- Largest arguments: {worst_args:,} B against the 64 KiB request ({worst_args - REQUEST:+,}).")
if worst_heap:
    print(f"- Peak heap: {worst_heap:,} B against 512 KiB ({worst_heap - HEAP:+,}).")
failed = [r for r in rows if r["outcome"] != "done"]
if failed:
    print(f"- Inputs that did not run to a reply: {len(failed)}, by run: "
          + ", ".join(sorted({r['run'] + ':' + r['outcome'] for r in failed})))

if zrows:
    print("\n## Row 3d's fix: opt-level z, linked without `--unique`, the core path only\n")
    print("| Class | Backend | Inputs | Gas p50 | Gas max | Max vs 80 M | Time p50 (us) |")
    print("|---|---|---:|---:|---:|---|---:|")
    groups = defaultdict(list)
    for r in zrows:
        backend = r["run"].split("-")[1]
        key = r["kind"] if r["kind"] != "input" else r["classes"]
        groups[(key, backend)].append(r)
    for (key, backend), rs in sorted(groups.items(), key=lambda kv: (-max(int(r["gas"]) for r in kv[1]), kv[0][1])):
        gas = sorted(int(r["gas"]) for r in rs)
        us = sorted(int(r["us"]) for r in rs)
        mx = gas[-1]
        verdict = "pass" if mx <= GAS_GATE else ("over 80 M" if mx <= GAS_CEILING else "OVER 100 M")
        print(f"| {key} | {backend} | {len(rs)} | {gas[(len(gas)-1)//2]:,} | {mx:,} | {verdict} | {us[(len(us)-1)//2]:,} |")
