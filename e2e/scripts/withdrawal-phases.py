#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""Split each scenario-c withdrawal into phases (F-032 T-032-12, NFR-15).

Usage: withdrawal-phases.py <artifact>/aledgent

Reads, from an `e2e-zombienet.yml` run with `stress_phases` and the aledgent driver: the driver's
`stress/withdrawals.json` (wall-clock marks per withdrawal), the adapter's logs (signing, the light
client's transaction status, observations and checks), `stress/heads.jsonl` (the chain clock:
every best and finalized head of the collator and the relay chain) and the collator's log
(`stress/kreivo01.log.gz`). Prints one row per withdrawal, in milliseconds:

  total          request to Settled, as the harness saw it (100 ms poll)
  core           request to submitted: ingress, the core's decision, submit_outbound
  pickup         submitted to the batch signed and broadcast by the adapter
  to_pool        signed to the transaction in the collator's pool (when its hash is logged)
  to_block       broadcast to the block that includes it, imported at the collator
  node_finality  that block imported to finalized, at the collator
  lc_lag         finalized at the collator to finalized in the adapter's light client
  lc_best/final  signed to in-best-block / in-finalized-block in the light client
  observe        finalized in the light client to the check reported to the core
  sign_to_obs    signed to the check reported
  to_settled     the check reported to Settled seen by the harness

then p50, min and max of each, and the chain clock's block time and finality lag (best import to
finalized) over the whole run.
"""
import glob, gzip, json, re, statistics, sys
from datetime import datetime, timezone

d = sys.argv[1]
W = json.load(open(f"{d}/stress/withdrawals.json"))["withdrawals"]
W = [w for w in W if "settled_at" in w]

def ts(s):
    s = s.rstrip("Z")
    if "." in s:
        a, b = s.split(".")
        s = a + "." + b[:6]
        f = "%Y-%m-%dT%H:%M:%S.%f"
    else:
        f = "%Y-%m-%dT%H:%M:%S"
    return int(datetime.strptime(s, f).replace(tzinfo=timezone.utc).timestamp() * 1000)

ansi = re.compile(r"\x1b\[[0-9;]*m")
signed, status, checked, reported, steps = [], [], [], [], []
for f in sorted(glob.glob(f"{d}/adapter-*.log")):
    for line in open(f, errors="replace"):
        line = ansi.sub("", line).replace('"', '')
        m = re.match(r"(\S+Z)\s", line)
        if not m:
            continue
        t = ts(m.group(1))
        if "batch signed and stored" in line:
            h = re.search(r" hash=([0-9a-f]+)", line).group(1)
            signed.append((t, h))
        elif "a submitted transaction's status" in line:
            h = re.search(r"hash=([0-9a-f]+)", line).group(1)
            st = re.search(r"status=(\S+)", line).group(1)
            ms = int(re.search(r"since_submit_ms=(\d+)", line).group(1))
            status.append((t, h, st, ms))
        elif "provisional observation checked" in line:
            op = re.search(r"operation=(\S+)", line).group(1)
            at = int(re.search(r" at=(\d+)", line).group(1))
            fin = int(re.search(r"finalized=(\d+)", line).group(1))
            checked.append((t, op, at, fin))
        elif "observation reported" in line:
            op = re.search(r"operation=(\S+)", line).group(1)
            at = int(re.search(r" at=(\d+)", line).group(1))
            fy = re.search(r"finality=(\S+)", line).group(1)
            reported.append((t, op, at, fy))
        elif " step serving=" in line and "finalized=" in line:
            fin = int(re.search(r"finalized=(\d+)", line).group(1))
            best = int(re.search(r" best=(\d+)", line).group(1))
            steps.append((t, fin, best))

heads = []
try:
    for line in open(f"{d}/stress/heads.jsonl"):
        try:
            o = json.loads(line)
        except Exception:
            continue
        if "number" in o:
            heads.append(o)
except FileNotFoundError:
    pass

def first(chain, kind, n, after=0):
    for o in heads:
        if o["chain"] == chain and o["kind"] == kind and o["number"] >= n and o["t"] >= after:
            return o["t"]
    return None

pool = {}
try:
    for line in gzip.open(f"{d}/stress/kreivo01.log.gz", "rt", errors="replace"):
        line = ansi.sub("", line)
        for h in re.findall(r"0x([0-9a-f]{64})", line):
            if h in pool:
                continue
            m = re.match(r"(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d+)", line)
            if m:
                pool[h] = ts(m.group(1).replace(" ", "T"))
except FileNotFoundError:
    pass

rows = []
for w in W:
    op = w["operation"]
    r0, sub, done = w["requested_at"], w["submitted_at"], w["settled_at"]
    sg = [(t, h) for t, h in signed if sub - 50 <= t <= done]
    if not sg:
        continue
    t_sign, h = sg[0]
    st = {s: (t, ms) for t, hh, s, ms in status if hh == h}
    rep = [x for x in reported if x[1] == op]
    at = rep[0][2] if rep else None
    chk = [x for x in checked if x[1] == op]
    fin_rep = [x for x in rep if x[3] == "final"]
    t_final_seen = chk[0][0] if chk else (fin_rep[0][0] if fin_rep else None)
    row = {"op": op[-6:], "total": done - r0, "core": sub - r0, "pickup": t_sign - sub}
    tp = pool.get(h)
    if tp:
        row["to_pool"] = tp - t_sign
    if at is not None and heads:
        tb = first("kreivo", "best", at, t_sign - 10000)
        tf = first("kreivo", "finalized", at, t_sign - 10000)
        if tb:
            row["to_block"] = tb - (tp or t_sign)
        if tb and tf:
            row["node_finality"] = tf - tb
        if tf and "in-finalized-block" in st:
            row["lc_lag"] = st["in-finalized-block"][0] - tf
    if "in-best-block" in st:
        row["lc_best"] = st["in-best-block"][0] - t_sign
    if "in-finalized-block" in st:
        row["lc_final"] = st["in-finalized-block"][0] - t_sign
        if t_final_seen:
            row["observe"] = t_final_seen - st["in-finalized-block"][0]
    if t_final_seen:
        row["sign_to_obs"] = t_final_seen - t_sign
        row["to_settled"] = done - t_final_seen
    row["at"] = at
    rows.append(row)

keys = ["total", "core", "pickup", "to_pool", "to_block", "node_finality", "lc_lag", "lc_best",
        "lc_final", "observe", "sign_to_obs", "to_settled"]
print("op     " + " ".join(f"{k:>13}" for k in keys) + "    at")
for r in rows:
    print(f"{r['op']} " + " ".join(f"{r.get(k, ''):>13}" for k in keys) + f"  {r['at']}")
print()
for k in keys:
    v = [r[k] for r in rows if k in r]
    if v:
        print(f"{k:>13}: n={len(v)} p50={statistics.median(v):.0f} min={min(v)} max={max(v)}")

# Chain clock: block time and finality lag at the collator, and on the relay chain.
for chain in ("kreivo", "relay"):
    best = {}
    fins = []
    for o in heads:
        if o["chain"] != chain:
            continue
        if o["kind"] == "best":
            best.setdefault(o["number"], o["t"])
        elif o["kind"] == "finalized":
            fins.append((o["t"], o["number"]))
    if not best:
        continue
    ns = sorted(best)
    gaps = [best[b] - best[a] for a, b in zip(ns, ns[1:]) if b == a + 1]
    lags = []
    for n in ns[5:-40]:
        tf = next((t for t, f in fins if f >= n), None)
        if tf:
            lags.append(tf - best[n])
    if gaps:
        print(f"{chain}: block time p50={statistics.median(gaps):.0f} ms mean={statistics.mean(gaps):.0f}; "
              f"blocks {ns[0]}..{ns[-1]} over {(best[ns[-1]] - best[ns[0]]) / 1000:.0f} s")
    if lags:
        lags.sort()
        q = lambda p: lags[min(len(lags) - 1, int(p * len(lags)))]
        print(f"{chain}: finality lag (best import -> finalized) p50={q(0.5)} p90={q(0.9)} p99={q(0.99)} max={lags[-1]} ms (n={len(lags)})")
