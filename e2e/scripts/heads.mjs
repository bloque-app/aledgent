#!/usr/bin/env node
// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// The chain clock for stress scenario c (F-032, NFR-15): every best and finalized head of each
// node given, as one JSON line per head with the wall clock in milliseconds, until killed.
//
//   node heads.mjs kreivo=ws://127.0.0.1:9944 relay=ws://127.0.0.1:9945 > heads.jsonl
//
// Read against the adapter's log (UTC, same host) and the driver's withdrawals.json, it splits a
// withdrawal's time into block inclusion, the relay chain's finality, and what the adapter adds.
// Node 22's built-in WebSocket; plain JSON-RPC, no dependency. Reconnects if a node drops.

const targets = process.argv.slice(2).map((a) => {
  const i = a.indexOf("=");
  if (i < 0) throw new Error(`expected name=ws://...: ${a}`);
  return { name: a.slice(0, i), url: a.slice(i + 1) };
});
if (targets.length === 0) {
  console.error("usage: heads.mjs name=ws://host:port ...");
  process.exit(2);
}

const out = (o) => process.stdout.write(JSON.stringify(o) + "\n");

function follow({ name, url }) {
  let ws;
  try {
    ws = new WebSocket(url);
  } catch (e) {
    out({ t: Date.now(), chain: name, error: String(e) });
    setTimeout(() => follow({ name, url }), 2000);
    return;
  }
  const kinds = new Map(); // request id or subscription id -> kind
  ws.onopen = () => {
    ws.send(JSON.stringify({ jsonrpc: "2.0", id: 1, method: "chain_subscribeNewHeads", params: [] }));
    ws.send(JSON.stringify({ jsonrpc: "2.0", id: 2, method: "chain_subscribeFinalizedHeads", params: [] }));
  };
  ws.onmessage = (m) => {
    const t = Date.now();
    let msg;
    try {
      msg = JSON.parse(typeof m.data === "string" ? m.data : m.data.toString());
    } catch {
      return;
    }
    if (msg.id === 1 || msg.id === 2) {
      kinds.set(msg.result, msg.id === 1 ? "best" : "finalized");
      return;
    }
    const sub = msg.params?.subscription;
    const head = msg.params?.result;
    if (sub === undefined || !head?.number) return;
    out({ t, chain: name, kind: kinds.get(sub) ?? "?", number: parseInt(head.number, 16) });
  };
  ws.onclose = () => {
    out({ t: Date.now(), chain: name, error: "closed" });
    setTimeout(() => follow({ name, url }), 2000);
  };
  ws.onerror = () => {};
}

targets.forEach(follow);
process.on("SIGTERM", () => process.exit(0));
