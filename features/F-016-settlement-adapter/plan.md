<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-016 — Settlement adapter

| | |
|---|---|
| Milestone | M-2 — On VOS, end to end |
| Track | B |
| Home | `services/settle` |
| Depends on | F-003, F-005, F-009, F-010, F-019 (the pallet's calls and unit tests) |
| Unblocks | F-017, F-018 |
| Realises | DEC-8, DEC-7 (instruction half) |

## Goal
`aledgent-settle` is the only component that touches Kreivo. It holds the signing keys, outside the core.
It takes instructions from the core's outbox and stores each one before it acts. It signs and submits
through a light client, and after a restart it replays the same signed bytes. It reads the pallet's inbox
with a cursor and reports what it finds as observations. It keeps executed, provably not executed and
unknown apart. It never writes the book and never states an outcome.

## Spec traces
- `REQ-SA-1`–`SA-8` — declared support, network check, observations only, three dispositions, durable
  record, persist before submit, complete observation with visible gaps, no account authority.
- `REQ-SO-5`–`SO-8` — instruction content, durable acknowledgement, replay not re-creation, no settlement
  ordering on other surfaces. `REQ-SO-4` — unknown is never failed.
- `REQ-SI-1`–`SI-5` — what an observation carries, unassociated arrivals, the ledger's verdict returned.
- `REQ-SL-3`, `REQ-SL-4` — gate on finality, submit with expiry.
- `REQ-AR-2`, `REQ-AR-3`, `NFR-8` — no book writes; keys outside the core and outside the environment.
- `REQ-SN-2` — shortfall parks, one alert per position, resumes by itself. `REQ-AS-2` — start-up asset check.
- `REQ-IN-5`, `REQ-IN-6` — completeness re-scan; one authoritative source.
- `AC-D2.*`, `AC-E1.4`, `AC-E1.5`, `AC-E3.*`, `AC-E4.*`; `INV-11`, `INV-12`, `INV-13`.

## Scope
**In:** the process and its store; the signer boundary; the light-client connection; start-up assertions;
the capability manifest; instruction intake and acknowledgement; the nonce and era manager; submission and
replay; the inbox reader and observation reports; dispositions; liquidity parking; the completeness
re-scan; submission of anchor instructions.
**Out:** deriving any status (`F-010`); reservations (`F-004`); the pallet (`F-019`); what to anchor and
when (`F-017`); paging and dashboards (`F-018`); notifications (`F-009`).

## Design

**Process.** One active instance per set of signer keys. A lease in its store fences a second instance.
Two adapters on one key are one nonce lane with two owners. The adapter reads the
core's outbox with a query. It writes to the core only through ingress (`F-009`). So every core input is in
the ingress log. Its core principal holds only three capabilities: `ack_instruction`, `record_observation`
and `declare_adapter`. It holds no authority over any ledger account (`REQ-SA-8`).

**Proposal — signer.** Keys live in a cloud KMS or HSM as secp256k1 keys. Kreivo accepts ECDSA through
`MultiSignature`. The KMS signs the 32-byte blake2 digest, so the adapter holds only a key handle.
*Alternative:* a separate signer process on its own host with an encrypted sr25519 keystore. Both keep key
material out of the core, the environment, logs and URLs (`REQ-AR-3`, `NFR-8`). There is one key per nonce
lane. Keys are registered on the pallet instance (`F-019`).

**Proposal — chain access.** Link Sube and smoldot directly, and port the VOS substrate extension's
prepare, submit and nonce model. *Alternative:* embed the extension crate as a library,
which `DEC-8` allows. Either way, encoding is driven by live metadata only. The deployed runtime differs
from the source snapshot, including the asset-id enum. An asset id that does not
parse is an error. The old decoder silently fell back to the native asset.

**Start-up assertions** (`REQ-SA-2`, `REQ-AS-2`). The adapter refuses to serve unless:
- the genesis hash is the declared one;
- the runtime `spec_version` is on an allow-list;
- the pallet is in the metadata;
- our instance is registered and our keys are on it;
- each supported asset's on-chain decimals equal the registry's precision. On Kreivo decimals are metadata
  only, and off-chain copies of them have been seen to drift.
On any failure it declares `refusing` to the core. **Proposal:** a status declaration with a heartbeat, sent
through ingress. The core refuses `submit_outbound` with `ERR-SettlementLayerUnavailable` while the last
status is `refusing` or too old (`AC-E4.2`).

**Capability manifest** (`REQ-SA-1`). `{genesis, instance, [(asset, deposit | withdraw)]}`. An operator
registers it through `CTR-OPS` (`REQ-OS-2`). The type sits in `ledger-core`. The core checks it at
submission and refuses with `ERR-UnsupportedLeg` (`AC-E4.1`). The check lives in `F-010`'s submit path.
This feature supplies the manifest and the test.

**Intake** (`REQ-SO-5`, `REQ-SO-6`). For each outbox instruction `{op_ref, asset, amount, destination,
expiry, reserved}`:
1. Validate: destination is an address on this network; asset is supported; `reserved` is true. An
   instruction without a reservation is never acknowledged (`INV-13`).
2. Insert into `instructions` keyed by `op_ref`, with a digest. Same `op_ref` with a different digest
   raises an alarm and nothing else happens.
3. Send `ack_instruction(op_ref, digest)` through ingress. The core then moves `Reserved → Submitted`.
Anchor instructions from `F-017` use the same path with `{number, commitment}`.

**Submission.** Per instruction: `Acknowledged → Parked | Signed → Broadcast → Final | NotExecuted |
Unknown`.
- *Liquidity.* Before signing, read at the finalized head: the instance position for the asset, less
  pending withdrawals, must cover the amount. The signer's native balance must be above a fee floor
  (`REQ-SN-2`). If not, park. Raise one alert per (instance, asset) position and one per fee position,
  not one per operation (`AC-E3.1`). Parked instructions resume by themselves when the balance recovers
  (`AC-E3.2`). A parked instruction is never signed.
- *Sign.* Pick a lane and an explicit nonce `N`. Pick a mortal era whose end falls at or before the
  instruction's expiry at the fastest known block time. Build `withdraw(instance, op_ref, to, asset,
  amount)`. Sign it. Store the signed bytes, `N` and the era end **before** broadcast (`REQ-SA-6`). After a
  crash the same bytes are broadcast again (`REQ-SO-7`).
- *Nonce rule,* from the extension's model. A nonce stays reserved while its extrinsic is
  uncertain. It is released only when the finalized account nonce passes it, or when its era end is
  finalized. **Proposal:** one extrinsic in flight per lane; several lanes for throughput; pipelining only
  after measurement.
- *Time.* Expiry is wall-clock time. Mortality is in blocks and bounded by the runtime's `BlockHashCount`.
  Block time is uncertain: 0.5–0.6 s when blocks flow, 30–90 s to inclusion when the chain is idle.
  So a disposition is always decided by finalized block height, never by the
  clock or by a count of blocks (`REQ-SL-3`, `REQ-SL-4`). Nonce, era and lane never leave the adapter
  (`REQ-SO-8`).

**Observation** (`REQ-SA-7`, `REQ-SI-1`–`SI-5`). One cursor per instance: the next inbox sequence. The
reader pages `Inbox(instance, seq..)` at the finalized head, at most 32 rows per page. It
never moves past a sequence it has not read. Each entry becomes an observation `{coordinate, kind, payload
with the chain's own error text, finality as observed, block}`. It goes through ingress as
`record_observation`. The core's verdict is stored: applied, already applied, ignored or not associated.
Deposits carry only an `account_ref`, with no correlator (`REQ-SI-2`). Our own extrinsics seen at best
block are reported as provisional. The finalized read confirms or contradicts them, and the core decides
(`REQ-IN-3`, `REQ-IN-4`).
**Proposal — coordinate:** `(instance, inbox seq, hash of the entry)`, where the entry includes its block
number. *Alternative:* `(instance, seq)` with finalized reports only. That is simpler, but then `Orphaned`
cannot be reached from the inbox. The choice must match `F-010` and `F-019`.
If a range cannot be read, because it was pruned before the read, the adapter records an explicit gap
`{instance, from, to, reason}` that operators can see (`AC-D2.2`). The reader is the only source that
writes. Any other observer may corroborate and never writes (`REQ-IN-6`).

**Dispositions** (`REQ-SA-4`). Never collapsed:
- *Executed:* a finalized `Withdrawn{op_ref}` entry.
- *Provably not executed:* a finalized `WithdrawRefused{op_ref, error}` entry; or our extrinsic failed in
  a finalized block, so its nonce is spent; or its era end is finalized while the signer's finalized nonce
  is still at or below `N`; or it was never signed and its expiry passed. In the last case the adapter
  first writes a tombstone, so it can never sign it later. **Proposal:** each non-inclusion proof is an
  observation of kind `NotIncluded` at coordinate `(instance, signer, N, finalized block hash)`.
- *Unknown:* anything else. The adapter reports nothing new. The core's deadline leads to `Needs-Review`
  (`REQ-OP-6`, `AC-E1.5`).
A chain amount that differs from the reservation is reported as seen. The core detects `ERR-AmountMismatch`.
A multi-leg operation with only some legs executed is never parked. Each leg is reported, and the core
moves it to `Needs-Review` (`AC-E3.3`).

**Durable state** (`REQ-SA-5`). Tables: `instructions` (what it was asked), `extrinsics` (signed bytes,
lane, nonce, era, status), `observations` (what the network did, raw payload), `reports` (what it told the
core, and the verdict), `cursors`, `gaps`, `alerts`. **Proposal:** PostgreSQL, a database and role separate
from ingress. *Alternative:* SQLite in WAL mode with `synchronous=FULL`.

**Re-scan** (`REQ-IN-5`, `AC-D2.1`). A periodic job reads inbox ranges again. It compares them with the
coordinates the core has recorded, and reports any difference as an incident.

## Test plan
- **Unit and property.** The nonce manager never reuses an uncertain nonce. Fault injection between
  persist and broadcast shows that recovery broadcasts the same bytes, and no second extrinsic exists for
  one `op_ref` (`REQ-SA-6`, `REQ-SO-7`). A table-driven test covers each disposition (`AC-E1.4`,
  `AC-E1.5`). Report payloads carry no status field (`INV-11`). An instruction with `reserved = false` is
  never acknowledged (`INV-13`). A second report of one coordinate gets "already applied" (`INV-12`).
- **Against a local Kreivo with the pallet:** `AC-E1.4` (position too small → `WithdrawRefused` →
  release); `AC-E1.5` (block production stopped after broadcast → unknown → non-terminal); `AC-E3.1` (20
  withdrawals on a short position → parked, one alert); `AC-E3.2` (top up → all resume); `AC-E3.3`;
  `AC-E4.1`; `AC-E4.2` (wrong genesis; wrong decimals); `AC-D2.1`; `AC-D2.2` (entries pruned before the
  read → a visible gap).
- **Restart matrix.** Kill the adapter in every submission state. From storage alone it states what it
  was asked, what the chain did and what it told the core (`REQ-SA-5`, `AC-K3.1`).
- **Key hygiene.** No key material in the environment, logs or error text (`NFR-8`).

## Exit criterion
`AC-D2.*`, `AC-E1.4`, `AC-E1.5`, `AC-E3.*` and `AC-E4.*` pass against a local Kreivo with the pallet. The
restart matrix loses no instruction. It never produces a second extrinsic for one operation reference.

This feature also closes the end-to-end run from `F-019`: on zombienet, with the pallet, the adapter and
the core, `AC-D1.*` and `AC-E1.*` complete end to end.

## Open points
1. **Proposals:** KMS signer; Sube and smoldot directly; coordinate format (agree with `F-010`, `F-019`);
   `NotIncluded` observations; status heartbeat; PostgreSQL store.
2. **Parking versus deadline.** A parked instruction past its operation deadline goes to `Needs-Review`
   (`REQ-OP-6`), while `AC-E3.2` expects an automatic resume. Past its expiry it was never signed, so it is
   provably not executed and is released (`REQ-OP-9`). The owner should confirm this reading.
3. **Throughput.** One lane carries about one extrinsic per finality interval. An earlier Kreivo integration
   measured ~0.105 ops/s per nonce domain. No target is stated. Lanes need
   several keys per instance (`F-019`).
4. **`NFR-8`.** An adapter key can withdraw a whole position. A per-window limit in the pallet is
   proposed in `F-019`.
5. `BlockHashCount` for the deployed runtime is not yet known. It caps instruction expiry.
6. Kreivo produces blocks on demand. Liveness needs a crank that buys coretime. This is
   an operational dependency, surfaced by `F-018`.
7. The `zombienet` build drops two transaction extensions. Local tests do not encode
   exactly as production does.
8. Filling a gap from an archive (`REQ-IN-5`) depends on one archive node.
9. `OQ-25` (partial multi-leg) and `OQ-30` (several assets on day one) are open. The adapter is generic
   over assets meanwhile. `RISK-4`: governance delays the pallet, so `M-2` runs on a local Kreivo.
