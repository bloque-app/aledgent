<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: the audit trail does not reach the audit store

`REQ-AT-1`, `REQ-AT-2`, `REQ-AT-3`, `NFR-6`, `NFR-11` · `T-013-13` · **Pages:** `AledgentAuditNotShipped`,
`AledgentAuditShipLag`, `AledgentAuditStoreDown`, `AledgentAuditStoreRefused`, `AledgentAuditBackpressure`
· **Owner:** security (platform for the store's volume) · **Routed:** at once, every 15 minutes.

**The behaviour, as built** (`T-013-13`).

1. The core emits every audit record inside its commit, into its audit outbox.
2. A shipper thread in the core's process (`aledgent-core-host serve`, or ingress's in-process core)
   copies the outbox to the audit store, in batches, outside the core's locks. It uses the store's own
   writer credential, `ALEDGENT_AUDIT_WRITER_CREDENTIAL`.
3. The store takes each record **once, in order**: only the next sequence number, linked to the one
   before. A record it already holds is accepted again and appended nothing; any other is refused.
4. Only once the store confirmed the append do the records leave the outbox. A restart drops, as the
   archive replays, the records the store already holds. Nothing is lost: every record can be derived
   again from the archive.
5. **Back-pressure.** At `max_unshipped` records waiting (65 536 by default, about 23 MiB), the core
   refuses new inputs. They stay `Received` in ingress's log, in order, and decide once the store
   takes records again. Nothing decides without its record, and nothing is dropped.

| Alert | Means |
|---|---|
| `AledgentAuditNotShipped` | No `audit_store` in the configuration: nothing ships, the outbox grows with the book |
| `AledgentAuditStoreDown` | The last rounds did not reach the store: the volume, its mount, its permissions, the credential |
| `AledgentAuditShipLag` | A record has waited over five minutes: the store is down, or slower than the core |
| `AledgentAuditBackpressure` | The outbox is at its bound: **the core decides nothing** until the store takes records |
| `AledgentAuditStoreRefused` | The store refused the core's record, or holds another at its head. **Shipping has stopped.** An integrity incident |

## Preconditions

- [ ] Read the signals: `aledgent_audit_unshipped_records`, `aledgent_audit_ship_lag_ms`,
  `aledgent_audit_store_up`, `aledgent_audit_store_refused`, `aledgent_audit_backpressure`.
- [ ] Read the core's standard error (the core container's log stream). Look for
  `the audit store does not take records` or an `aledgent-core: ALERT` line naming the record.
- [ ] Tell the cases apart: **not configured**, **unreachable** (store up 0, not refused), or **refused**.

## Steps

1. **Not configured.** Add `audit_store` to the deployment's configuration (`ingress.json`, which
   `aledgent-core-host serve` reads too): `{"dir": "<the audit volume's mount>"}`. Provision
   `ALEDGENT_AUDIT_WRITER_CREDENTIAL` from Vault (16 bytes or more). The directory must not be inside
   the core's `--state`; the store refuses it. Restart the core. An empty directory is initialised on
   the first start. The first rounds then ship the whole trail.
2. **Unreachable.** Restore the store's volume: its mount, free space and permissions for the core's
   user. Do not move or edit the store's files. The shipper retries every 500 ms by itself, so no
   restart is needed. If back-pressure has started, decisions resume as soon as the outbox drains
   under its bound.
3. **Refused.** Do not restart in a loop, and do not "repair" the store: it is evidence (`REQ-AT-3`).
   - Capture the `ALERT` line: it names the record number and the store's refusal (`Rewrite`, `Gap`,
     `ChainBreak`, or the head that differs).
   - Usual causes:
     - the store belongs to another book (a reused volume);
     - the core was restored from an older backup while the store kept going, so the store holds
       records of inputs the restored core no longer has;
     - someone wrote to the store.
   - After a restore of the core ([total-loss-recovery](total-loss-recovery.md)): the store is ahead
     of the restored book. Keep it aside, read-only, as evidence of the decisions the restore lost.
     Point `audit_store.dir` at a store restored from a backup at or before the core's restore point,
     and restart.
   - Treat it as a security incident, under the operator's incident process, at its highest severity.
     The security owner decides with the evidence. The core's trail and the store's are compared with
     `opsctl` reads, never by editing either.
   - While refused, the outbox grows to its bound, then the core holds every input. That is by
     design: no decision without its record kept apart.
4. **Back-pressure that will last.** The outbox's bound is configuration (`audit_store.max_unshipped`).
   It can be raised, with a restart, only if the store will return. Each record holds about 350 bytes of
   the core's memory, so check the core's memory headroom first.

## Verification

- [ ] `aledgent_audit_store_up == 1`, `aledgent_audit_store_refused == 0`, `aledgent_audit_backpressure == 0`.
- [ ] `aledgent_audit_unshipped_records` falls to a few, and `aledgent_audit_ship_lag_ms` to under a second.
- [ ] `aledgent_audit_shipped_records` rises with the book.
- [ ] `opsctl backup audit` takes a verified copy of the store. It re-reads every segment and walks the chain.
- [ ] If inputs were held: the oldest undecided row falls under the decision bound, and nothing is
  still `Received` after the drain.

## Rollback

Nothing here changes the book. A configuration added in step 1 can be removed again with a restart.
The records already shipped stay in the store, which never removes a record.

## Exercised

On every change: `ops/tests/alerts.rs` checks that each audit alert above pages the test route, and the
shipper's own test in `services/ingress/src/audit_ship.rs` covers the store's once-in-order refusal. The
refused case end to end is not exercised automatically yet; it is exercised by hand in a staging
deployment.
