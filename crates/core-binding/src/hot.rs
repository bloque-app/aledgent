// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Agent's state layout and its size guard (`T-005-02`, `DEC-4`, `REQ-RT-1`, `REQ-RT-8`).
//!
//! Everything lives in the Agent's **Linear** lane, so that one transition commits all of it or
//! none (`REQ-RT-1`; cross-lane effects are not atomic on `saga`). The lane holds storage rows,
//! each at most 64 KiB (`vos/src/actors/storage.rs`), and the whole Agent state is refused by VOS
//! above 4 MiB (`vos-agent-sdk/src/lib.rs`, `MAX_RUNTIME_STATE_BYTES`).
//!
//! | Row | Key | Holds |
//! |---|---|---|
//! | hot | `h` | [`Hot`]: the kernel's six sub-roots, the last commit, the next posting number, the service root, the audit chain's head, the input number, the log head, the ring's bounds, the driver's principal, the budget, the outbox's counters, and the writer's code: its code version, generation and construction, any scheduled upgrade, its upgrade-authority mode and any scheduled change of it, and where it was continued (`T-037-02`) |
//! | configuration | `c` | The lifecycle configuration genesis took |
//! | delta ring | `d` ‖ input number | One [`RingEntry`] per committed input, until the driver archives it |
//!
//! That is all: **hot state is roots plus bounded records** (`DEC-4`). The kernel's leaves and
//! the library's service store are both witnessed leaves, kept by the host (`F-006`): the
//! kernel's six trees under their sub-roots, and every collection of the service store, and the
//! deliveries the Agent recorded, in the service tree under its root ([`crate::service`],
//! `ledger_core::leaves`). Each input brings the leaves it touches. So the Agent's state does
//! not grow with the book: its size is the hot row, the configuration, and at most
//! [`Limits::ring`] unarchived ring entries.
//!
//! **Budget `H`.** The Agent refuses a transition that would take its state over its budget
//! (set at genesis, at most [`MAX_BUDGET`]) with nothing written, well before VOS's own refusal.
//!
//! **The writer's code** (`T-037-02`, `REQ-RT-15`, `REQ-RT-19`, `DEC-26` (a); amendment 0033). The
//! hot row holds the code version that executes the writer's next input ([`Hot::code`]), its code
//! generation, the construction of its state commitment, the scheduled upgrade, the digest of its
//! upgrade-authority mode and any scheduled change of it, and the input number it was continued at:
//! [`MAX_CODE_BYTES`] at most, so the row stays bounded (`DEC-4`). Each ring entry names the code
//! that executed it ([`RingEntry::code`]), and the log hash covers it, so every archived entry names
//! its code.
//!
//! **Host-local fields** (`SPEC.md` §2 **Writer state**). [`Hot::driver`] (the principal the
//! platform accepts the writer's inputs from) and [`Hot::observed_slot`] (a diagnostic the platform
//! reports) are the only two. They are set at founding from the invocation, and never enter a
//! root, a ring entry, a log hash, an anchor checkpoint or an anchor (`REQ-RT-19`). Every other
//! field is writer state. Today's archive checkpoint is still the Agent's runtime state as the host
//! saved it (`agent.state`, `core-archive`), which holds this row, these two fields included, and
//! its digest covers them; once `T-037-16` makes archived checkpoints and snapshots the writer
//! state as rows, they will not be carried by a snapshot or a continuation.
//!
//! **Versions.** [`LAYOUT`] stamps the hot row, and [`LOG_DOMAIN`] every ring entry's log hash: an
//! Agent's rows or an archive written in an earlier layout fail at that check, with an error naming
//! the layout ([`decode_hot`]), not at a field.

use alloc::vec::Vec;

use ledger_core::call::Call;
use ledger_core::export::AuditHead;
use serde::{Deserialize, Serialize};

use crate::envelope::Roots;

/// The layout's version: a change of layout is an upgrade through `F-014` (`REQ-RT-8`). Version 2
/// keeps the service store as leaves, not as an image in the Agent's rows. Version 4 adds the
/// writer's code to the hot row and to each ring entry (`T-037-02`).
pub const LAYOUT: u32 = 4;

/// The layout of an Agent whose book is in its committed maps (`F-031` lever 2, `crate::mapped`):
/// 3 before the writer's code, 5 with it (`T-037-02`).
pub const LAYOUT_MAPPED: u32 = 5;

/// The hot row, decoded only if it is of a layout this build reads ([`LAYOUT`],
/// [`LAYOUT_MAPPED`]). The layout is the row's first field: an Agent founded in an earlier layout is
/// refused here, by name, and not by a field that no longer decodes.
pub fn decode_hot(bytes: &[u8]) -> Result<Hot, alloc::string::String> {
    let (layout, _) = postcard::take_from_bytes::<u32>(bytes)
        .map_err(|e| alloc::format!("the hot row's layout: {e}"))?;
    if layout != LAYOUT && layout != LAYOUT_MAPPED {
        return Err(alloc::format!(
            "the hot row is layout {layout}; this build reads layouts {LAYOUT} and \
             {LAYOUT_MAPPED} (the writer's code, T-037-02): an Agent founded before is driven by \
             the host it was founded with until the book is re-founded"
        ));
    }
    crate::envelope::decode(bytes)
}

/// The hot row's key.
pub const HOT: &[u8] = b"h";
/// The configuration row's key.
pub const CONFIG: &[u8] = b"c";
/// The delta ring's key prefix.
pub const RING: u8 = b'd';

/// The largest budget genesis may set: under VOS's 4 MiB whole-state refusal, with room for the
/// runtime's own metadata (results, acknowledgements, the directory).
pub const MAX_BUDGET: u64 = 3 * 1024 * 1024;

/// The Agent's bounds, fixed at genesis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// The budget `H`: the most bytes the Agent's state may take (`T-005-02`). At most
    /// [`MAX_BUDGET`].
    pub budget: u64,
    /// The most delta-ring entries the driver may leave unarchived (`T-005-07`).
    pub ring: u64,
    /// The most undelivered notifications the outbox may hold before the Agent refuses new
    /// inputs (`T-005-07`, `DEC-7`).
    pub outbox: u64,
}

impl Limits {
    /// The defaults. The state is the hot row, the configuration and the unarchived ring, so it
    /// does not grow with the book. What an input costs in the guest's 256 KiB heap is set by the
    /// leaves it touches, not by the state (`tests/heap.rs`, `notes/MEASUREMENTS-F005.md` §8).
    ///
    /// The ring holds every input of a batch until the host archives the batch (`T-031-02`): the
    /// acknowledgement rides on the next invocation, so after it the ring holds at most the
    /// inputs of one batch. Sixty-four entries leave room for two batches of the host's default
    /// thirty-two (`crate::host::BatchLimits`), so a host that has not yet archived one batch can
    /// still send the next.
    ///
    /// The budget follows the ring. A ring entry is about 250 B for an ordinary command, plus
    /// 9 B of key (`notes/MEASUREMENTS-F005.md` §8.1: 591 to 638 B of state with the ring
    /// archived after every input, which is the hot row and the configuration), so 64 entries
    /// are about 17 KiB and 32 KiB covers them with room for larger calls. Raising it further
    /// would buy nothing a host archiving each batch uses, and would let a stalled host grow the
    /// state toward the 48 KiB VOS allows one lane's image in one execution
    /// (`vos/src/agent/execution.rs`, `MAX_EXECUTION_STATE_TOTAL_BYTES` at the pinned VOS), which
    /// the guest's 256 KiB heap holds alongside the batch's inputs. Before `T-031-02` the
    /// defaults were 26 KiB and 16 entries, sized for one input per invocation.
    ///
    /// The outbox bound counts every undelivered notification, and a notification whose principal
    /// has no recipient is parked by ingress, never delivered (stress finding F5). At 4 096 the
    /// Agent then refused every input, the whole book stopped by holders who never registered a
    /// recipient. The notifications are service leaves, not Agent state, so the bound protects
    /// nothing in the Agent; it is raised to 2²⁰, and the backlog is watched where it belongs, as
    /// `NFR-11`'s count and age of undelivered notifications (`T-031-15`).
    pub const DEFAULT: Limits = Limits {
        budget: 32 * 1024,
        ring: 64,
        outbox: 1 << 20,
    };
}

/// The most bytes the writer's code fields add to the hot row's encoding (`T-037-02`, `DEC-4`):
/// [`Hot::code`], [`Hot::generation`], [`Hot::construction`], [`Hot::upgrade`],
/// [`Hot::authority`], [`Hot::authority_next`] and [`Hot::continued_at`], each at its widest
/// (postcard: a varint `u64` takes at most 10 bytes, an option one byte more).
pub const MAX_CODE_BYTES: u64 = 32 + 10 + 1 + (1 + 32 + 10) + 32 + (1 + 32 + 10) + (1 + 10);

/// The hot row (module documentation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hot {
    /// [`LAYOUT`].
    pub layout: u32,
    /// The only VOS principal the Agent takes invocations from (`T-005-03`). **Host-local**
    /// (module documentation): the platform's identity for the execution node that drives the
    /// writer, in no root, log hash, checkpoint or anchor.
    pub driver: [u8; 32],
    /// The kernel's six sub-roots.
    pub subroots: [[u8; 32]; 6],
    /// The kernel store's last commit.
    pub last_commit: Option<u64>,
    /// The next posting sequence number.
    pub next_posting: u64,
    /// The service tree's root: every collection of the service store, and the deliveries
    /// recorded, as leaves (`crate::service`).
    pub service_root: [u8; 32],
    /// The audit chain: records emitted.
    pub audit_emitted: u64,
    /// The audit chain: the last record's hash.
    pub audit_head: [u8; 32],
    /// The audit chain: the last record the audit store acknowledged.
    pub audit_shipped: Option<(u64, [u8; 32])>,
    /// The input number of the last committed input.
    pub inputs: u64,
    /// The log head: the last entry's log hash.
    pub log_head: [u8; 32],
    /// The last input number the driver archived; the ring holds `archived + 1 ..= inputs`.
    pub archived: u64,
    /// Each unarchived ring entry's size, in input order.
    pub ring_sizes: Vec<u64>,
    /// How many deliveries are recorded.
    pub delivered: u64,
    /// How many notifications the outbox has taken: with `delivered`, the undelivered backlog the
    /// outbox's backpressure counts (`T-005-07`).
    pub notified: u64,
    /// The Agent's bounds.
    pub limits: Limits,
    /// The configuration row's size.
    pub config_len: u64,
    /// The last slot VOS observed for a committed input (diagnostics; `OQ-36` is open and the
    /// library takes time from the calls). **Host-local** (module documentation): in no root,
    /// log hash, checkpoint or anchor.
    pub observed_slot: u64,
    /// The code version that executes the next input (`REQ-RT-15`): the digest of its manifest
    /// (`ledger_core::code`). *H₀* at founding; zeros for a book founded with no named code
    /// (tests, stress books).
    pub code: [u8; 32],
    /// The code generation in force (`REQ-RT-20`, amendment 0033 D1): 0 at genesis, one more after
    /// each upgrade.
    pub generation: u64,
    /// The construction of the state commitment (`REQ-PR-10`, `DEC-25`): the build's at founding;
    /// it changes only by a migration (`REQ-PR-11`).
    pub construction: u8,
    /// The scheduled upgrade, if any: the code version *H* and the input number `n` it applies
    /// from (`REQ-RT-16`). Its first input moves it into [`Hot::code`] (`T-037-04`).
    pub upgrade: Option<([u8; 32], u64)>,
    /// The digest of the upgrade-authority mode in force (`REQ-RT-18`); the mode itself is a
    /// record in the service tree. Zeros until modes exist (`T-037-05`).
    pub authority: [u8; 32],
    /// A scheduled change of mode: the new mode's digest and the input number it applies from.
    pub authority_next: Option<([u8; 32], u64)>,
    /// The input number at which this writer was founded as a continuation (`REQ-RT-21`), if it
    /// was (`T-037-17`).
    pub continued_at: Option<u64>,
}

impl Hot {
    /// The book state this hot row carries, in the shape a chainspec book is founded on
    /// (`StateGenesis`): the kernel's sub-roots and counters, the service root, the audit chain
    /// and the notification count. Everything an input's effect on the book depends on, beside
    /// the configuration and the limits; the rest of the row is the Agent's own bookkeeping
    /// (inputs, the delta ring, deliveries). `verify-genesis --segments` compares it across a
    /// segment boundary (`F-036`).
    pub fn genesis_state(&self) -> crate::envelope::GenesisState {
        crate::envelope::GenesisState {
            subroots: self.subroots,
            last_commit: self.last_commit,
            next_posting: self.next_posting,
            service_root: self.service_root,
            audit_emitted: self.audit_emitted,
            audit_head: self.audit_head,
            audit_shipped: self.audit_shipped,
            notified: self.notified,
        }
    }

    /// The audit chain's head, to continue from.
    pub fn audit(&self) -> AuditHead {
        AuditHead {
            emitted: self.audit_emitted,
            head: self.audit_head,
            shipped: self.audit_shipped,
        }
    }

    /// The Agent's state in bytes: every row, keys included.
    pub fn bytes(&self) -> u64 {
        let hot = crate::envelope::encode(self).len() as u64 + HOT.len() as u64;
        let config = self.config_len + CONFIG.len() as u64;
        let ring: u64 = self.ring_sizes.iter().map(|n| n + 9).sum();
        hot + config + ring
    }

    /// The book's roots.
    pub fn roots(&self) -> Roots {
        Roots {
            kernel: crate::witness::root_of(&self.subroots),
            service: self.service_root,
            audit: crate::envelope::audit_digest(self.audit_emitted, &self.audit_head),
        }
    }
}

/// A ring entry's key.
pub fn ring_key(seq: u64) -> Vec<u8> {
    let mut k = alloc::vec![RING];
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

/// One committed input in the delta ring (`T-005-07`): what the host needs to bring its leaf
/// store and replica to the Agent's roots, and the hash chain over every input.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RingEntry {
    /// The input number.
    pub seq: u64,
    /// The code version that executed it (`T-037-02`, `REQ-RT-17`): every archived entry names its
    /// code, so the code map reads off the archive, and the log hash covers it.
    pub code: [u8; 32],
    /// The calls, in order.
    pub calls: Vec<Call>,
    /// The deliveries recorded.
    pub deliveries: Vec<([u8; 32], u64)>,
    /// The roots after.
    pub roots: Roots,
    /// The next posting number after.
    pub next_posting: u64,
    /// The previous entry's log hash.
    pub prev: [u8; 32],
}

/// The domain of a ring entry's log hash: version 2 since the entry names its code (`T-037-02`), so
/// that one domain never hashes two entry layouts.
pub const LOG_DOMAIN: &[u8] = b"aledgent/core-agent/log/v2";

impl RingEntry {
    /// Its log hash: `blake2b-256(domain ‖ encoding)`, which chains through `prev`.
    pub fn log_hash(&self) -> [u8; 32] {
        cipher_clerk::crypto::blake2b_256(LOG_DOMAIN, &[&crate::envelope::encode(self)])
    }
}
