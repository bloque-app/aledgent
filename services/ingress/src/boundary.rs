// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The ingress-to-core boundary (`DEC-5`, `DEC-6`, `DEC-7`).
//!
//! Ingress is needed whatever issue #38 rules: whether the reservation decision runs inside a VOS
//! Agent (`F-005`, `DEC-4` as planned) or in a native process (`DEC-1`). So everything ingress
//! needs from the deciding component is this one trait, [`Core`], and nothing on ingress's side of
//! it knows which runtime answers:
//!
//! - [`Core::apply_batch`] takes several, in order, and answers each as `apply` would; a core
//!   that can apply them in one step (the core Agent, `F-031`) does (`T-031-04`).
//! - [`Core::apply`] takes one [`CoreInput`] — a command, a tick, or a delivery record — and
//!   returns its [`CoreReply`]: the monotonic **input number** the core gave it, and the
//!   [`Outcome`] (a decision with its state, a refusal, a tick's report, a recorded delivery). It
//!   is idempotent on [`CoreInput::key`]: an input applied before returns its first reply, with
//!   its first input number, and changes nothing (`INV-18`, `REQ-RT-2`). The forwarder relies on
//!   that to retry an ambiguous call with the identical input.
//! - [`Core::outbox`] pages the notifications the core committed with its transitions, and
//!   whether each one's delivery is recorded (`REQ-RT-4`, `REQ-EV-6`).
//! - [`Core::last_tick`] is the time of the last tick the core applied (`RISK-7`).
//! - [`Core::query`] answers a query as a principal; queries are not inputs (`REQ-QR-3`).
//! - [`Core::credential`] gives ingress the public material of one working credential, so it can
//!   check a passkey assertion or an API key before the caller is known (`F-028`, `DEC-19`). It
//!   is not a route: no caller reaches it.
//! - [`Core::party_of`] names the integrating party a principal is tied to, if any: the client
//!   key's domain (`REQ-ID-7`, `REQ-OP-10`, T-009-16). Not a route either.
//!
//! A call that fails says whether the input may have run ([`CoreError::Ambiguous`]) or certainly
//! did not ([`CoreError::Unavailable`]). The forwarder treats both the same way: it retries the
//! identical input until the core answers (`REQ-CS-7`).
//!
//! Every type that crosses the boundary is serialisable, so an implementation behind a network
//! call (a VOS Agent's HTTP ingress) carries the same values an in-process one does. This crate
//! provides one implementation, [`crate::inprocess::InProcessCore`], over `ledger-core` directly.
//! The VOS Agent implementation is `F-005`'s, blocked on issue #38 (`NOTES.md`).

use std::future::Future;

use ledger_core::Refusal;
use ledger_core::account::Time;
use serde::{Deserialize, Serialize};

use crate::query::{
    AssetBody, AssetParams, BalancesBody, BalancesParams, CanSpendBody, MovementsParams, PageBody,
    StatementBody, StatementParams, TrialBalanceBody,
};
use crate::wire::{
    CommandBody, CommandResult, NotificationBody, OperationReply, ShownBody, UndeliveredBody,
};

/// The key an input is idempotent on: the operation reference ingress minted for a command, or
/// the reference of a tick's or a delivery record's row. Hex on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InputKey(#[serde(with = "hex16")] pub [u8; 16]);

mod hex16 {
    use serde::{Deserialize, Deserializer, Serializer, de};

    pub fn serialize<S: Serializer>(b: &[u8; 16], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(b))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 16], D::Error> {
        let s = String::deserialize(d)?;
        let v = hex::decode(s).map_err(de::Error::custom)?;
        v.try_into()
            .map_err(|_| de::Error::custom("an input key is 16 bytes"))
    }
}

/// Who a command is from, as ingress authenticated it (`REQ-PN-1`). The core checks the
/// credential set and authority itself, so a revocation takes effect at once (`REQ-AU-3`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallerBody {
    /// The initiating identity.
    pub principal: String,
    /// The credential it presented (its identifier; never the secret).
    pub credential: String,
    /// The identity it acts for, where different.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
}

/// One input to the core. The core never reads a clock: every time is here (`REQ-RT-7`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum InputBody {
    /// A command.
    Command {
        /// Who sent it.
        caller: CallerBody,
        /// The operation it acts under: the minted reference, or the reservation it captures or
        /// releases.
        operation: String,
        /// When ingress logged it.
        received_at: Time,
        /// When ingress first forwarded it; the same on every retry.
        now: Time,
        /// The command.
        command: Box<CommandBody>,
    },
    /// A tick: move deadlines forward to `now` (`DEC-6`, `REQ-RT-5`, `INV-15`).
    Tick {
        /// The ingress clock at the tick.
        now: Time,
    },
    /// A delivery was acknowledged by its recipient: record it in committed state (`REQ-RT-4`,
    /// `REQ-EV-6`).
    RecordDelivery {
        /// The notification's identity, hex.
        notification: String,
        /// When the recipient acknowledged it.
        at: Time,
    },
    /// One chunk of the chainspec genesis committed (`F-036`, `REQ-RC-6`), applied as the
    /// operator, on a book founded unsealed: the path `aledgent-core-host verify-genesis` replays
    /// in the PVM. No route of ingress's surfaces makes one.
    SpecChunk {
        /// The chunk's canonical bytes, hex.
        chunk: String,
        /// Its path in the seed's tree, each node hex.
        path: Vec<String>,
    },
}

/// An input and the key it is idempotent on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreInput {
    /// Its key.
    pub key: InputKey,
    /// The input.
    pub body: InputBody,
}

/// What an input came to.
// A decision carries the command's whole result; it crosses the boundary once per input, and
// boxing it would change every constructor for no gain (`F-017` added one more boxed field).
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The command was decided and taken.
    Decided {
        /// The operation reference the caller is given.
        operation_reference: String,
        /// Its state, for a command with a lifecycle.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<ShownBody>,
        /// What it produced.
        result: CommandResult,
    },
    /// The command was refused, with the condition of SPEC §10 that names it.
    Refused {
        /// The operation reference the caller is given.
        operation_reference: String,
        /// The refusal.
        error: Refusal,
    },
    /// A tick was applied.
    Ticked {
        /// The time was below the core's high-water mark: nothing changed.
        ignored: bool,
        /// Operations a deadline moved.
        moved: u64,
        /// Operations that joined the review queue.
        queued: u64,
        /// `Received` operations past the decision bound (`REQ-CS-7`).
        incidents: u64,
    },
    /// A delivery is recorded.
    Recorded {
        /// The notification, hex.
        notification: String,
        /// Whether the core knew the notification (an unknown one records nothing).
        known: bool,
    },
    /// A chainspec chunk was applied, or had been (`F-036`).
    SpecApplied {
        /// The chunk's index.
        index: u32,
        /// Chunks applied.
        applied: u32,
        /// Chunks the seed has.
        chunks: u32,
        /// Whether the seed is sealed.
        sealed: bool,
    },
}

/// The core's reply to an input.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreReply {
    /// The input number: monotonic and gapless over every input the core applied, so `F-018`
    /// has an exact replay order. A repeated input gets its first number.
    pub input_seq: u64,
    /// What it came to.
    pub outcome: Outcome,
}

/// Why a call into the core did not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreError {
    /// The core is not reachable. The input did not run.
    Unavailable,
    /// The call failed in a way that does not prove the input did not run (a 503 or a 504 from a
    /// VOS ingress, a dropped connection). Retry the identical input.
    Ambiguous,
}

/// One notification in the core's outbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxItem {
    /// The notification.
    pub notification: NotificationBody,
    /// The principal it is owed to: the operation's initiator, else the operator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient: Option<String>,
    /// When the transition it reports committed.
    pub created_at: Time,
    /// When its delivery was recorded, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<Time>,
}

/// A page of the outbox. Pages are small enough to fit the core's 16 KiB reply limit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxPage {
    /// The notifications, in identity order.
    pub items: Vec<OutboxItem>,
    /// The cursor of the next page, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// The most notifications one outbox page holds, so that a page stays within the 16 KiB reply
/// limit (`notes/FIT-VOS-CIPHERCLERK.md`). A notification at its longest — every identifier at full length, the
/// longest state, the longest refusal with its description, and the anchor it names (amendment
/// 0011 R3) with every number at its maximum — is 887 bytes of JSON (652 without the anchor),
/// measured by `tests::an_outbox_page_at_its_longest_fits_the_reply_limit`: 16 of them, with the
/// cursor, are about 14.3 KiB.
pub const OUTBOX_PAGE: usize = 16;

/// The core's reply limit an outbox page must fit (`notes/FIT-VOS-CIPHERCLERK.md`).
pub const REPLY_LIMIT: usize = 16 * 1024;

/// A query, routed to the core's query entry points (`CTR-QRY`, `F-011`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum Query {
    /// `balances(subject, [asset])`.
    Balances(BalancesParams),
    /// `movements(subject, filter, limit, cursor)`.
    Movements(MovementsParams),
    /// `statement(account, asset, period)`.
    Statement(StatementParams),
    /// `operation(reference)`.
    Operation {
        /// The reference.
        reference: String,
    },
    /// `assets()`.
    Assets,
    /// `trial_balance([asset])`: the operator's.
    TrialBalance(AssetParams),
    /// `can_spend(account, asset, amount)`.
    CanSpend {
        /// The account.
        account: String,
        /// The asset and the amount.
        params: AssetParams,
    },
    /// Undelivered notifications with their age (`REQ-EV-6`): the operator's.
    Undelivered,
    /// The recovery mechanisms a new owned-custody account of `identity` would have (`F-012`).
    RecoveryTerms {
        /// The owner.
        identity: String,
    },
    /// The recovery mechanisms that apply to `account` (`F-012`).
    RecoveryMechanisms {
        /// The account.
        account: String,
    },
    /// The account's custody view (`F-012`).
    Custody {
        /// The account.
        account: String,
    },
    /// The instructions the settlement adapter has not acknowledged (`F-016`): the adapter's.
    Instructions,
    /// The call outbox (`F-027`): every call the executor has not taken over, as envelopes. The
    /// adapter's.
    Calls,
    /// The evidence submitted for operations in `Needs-Review` that the adapter has not checked
    /// yet (`REQ-RC-5`, step 2): the adapter's.
    Evidence,
    /// The checkpoint in flight and whether a comparison is due (`F-017`): the adapter's.
    Anchor,
    /// `verify(disclosure)` (`F-017`, `CTR-DSC`): a query with no side effect.
    VerifyDisclosure {
        /// The disclosure's bytes, hex.
        disclosure: String,
    },
    /// Every checkpoint, the anchoring incidents and the recorded comparisons (`F-017`): the
    /// operator's.
    Anchoring,
    /// The operational signals (`F-018`): the operator's, with the `read_signals` kind.
    Signals,
    /// Every registered integrating party and its principals (`REQ-ID-7`): the operator's.
    IntegratingParties,
    /// `alias(source, name)` (`REQ-RC-7`): the account an alias names, for a caller that reaches
    /// it.
    Alias {
        /// The source, as [`crate::wire::RegisterAliasBody::source`].
        source: String,
        /// The name within it.
        name: String,
    },
    /// `grants(account)` (`REQ-RC-7`): the operator's, or the account owner's.
    Grants {
        /// The account.
        account: String,
    },
    /// `account(account)` (amendment 0028): the account's owner, kind, custody and status, for
    /// whoever reaches it, the operator, the identities principal and a holder of a grant over
    /// its owner.
    Account {
        /// The account.
        account: String,
    },
    /// `grants(identity)` (amendment 0026): every grant over the identity for the identity itself,
    /// the operator and the identities principal; the caller's own for anyone else.
    IdentityGrants {
        /// The identity.
        identity: String,
    },
}

/// A query's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum QueryReply {
    /// Balances.
    Balances(BalancesBody),
    /// A page of movements.
    Page(PageBody),
    /// A statement.
    Statement(StatementBody),
    /// An operation.
    Operation(OperationReply),
    /// The registry.
    Assets {
        /// Every asset.
        items: Vec<AssetBody>,
    },
    /// A trial balance.
    TrialBalance(TrialBalanceBody),
    /// `can_spend` said yes.
    CanSpend(CanSpendBody),
    /// Undelivered notifications.
    Undelivered {
        /// Each, with its age.
        items: Vec<UndeliveredBody>,
    },
    /// Refused.
    Refused {
        /// The refusal.
        error: Refusal,
    },
    /// Recovery mechanisms and their digest (`F-012`).
    RecoveryMechanisms(crate::wire::RecoveryMechanismsBody),
    /// An account's custody view (`F-012`).
    Custody(crate::wire::CustodyViewBody),
    /// The unacknowledged instructions (`F-016`).
    Instructions {
        /// Each, in operation order.
        items: Vec<crate::settlement::InstructionBody>,
    },
    /// The call outbox (`F-027`).
    Calls {
        /// Each, withdrawals in operation order, then the anchor in flight.
        items: Vec<crate::settlement::CallBody>,
    },
    /// The evidence the adapter has not checked (`REQ-RC-5`).
    Evidence {
        /// Each, oldest first.
        items: Vec<crate::settlement::PendingEvidenceBody>,
    },
    /// The adapter's read of anchoring (`F-017`).
    Anchor(crate::settlement::AnchorReadBody),
    /// What `verify(disclosure)` found (`F-017`).
    Verified(crate::anchoring::VerifiedBody),
    /// Anchoring as the operator sees it (`F-017`).
    Anchoring(crate::anchoring::AnchoringBody),
    /// The operational signals (`F-018`).
    Signals(crate::query::SignalsBody),
    /// The integrating parties (`REQ-ID-7`).
    IntegratingParties(crate::wire::IntegratingPartiesBody),
    /// The account an alias names (`REQ-RC-7`).
    Alias(crate::wire::AliasBody),
    /// The grants over an account (`REQ-RC-7`).
    Grants(crate::wire::GrantsBody),
    /// An account as the book holds it (amendment 0028).
    Account(crate::wire::AccountViewBody),
    /// `grants(identity)` (amendment 0026).
    IdentityGrants(crate::wire::IdentityGrantsBody),
}

/// One working credential, as ingress needs it to authenticate a caller (`F-028`): the identity it
/// belongs to, that identity's kind, its scheme and its verifier. Only an **active** credential of
/// an **active** identity is ever returned; anything else is absent, so the check fails closed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialBody {
    /// The identity, `did:`-shaped.
    pub identity: String,
    /// `person`, `organisation` or `agent`.
    pub kind: String,
    /// The scheme (`key`, `webauthn-es256`, `api-key`).
    pub scheme: String,
    /// The verifier: public material, hex.
    pub verifier: String,
}

impl std::fmt::Debug for CredentialBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Public material, but kept out of logs like every verifier (`REQ-AU-8`).
        f.debug_struct("CredentialBody")
            .field("identity", &self.identity)
            .field("kind", &self.kind)
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

/// The deciding component, as ingress sees it. See the module documentation.
pub trait Core: Send + Sync + 'static {
    /// Apply one input, or return its first reply if it was applied before.
    fn apply(&self, input: &CoreInput)
    -> impl Future<Output = Result<CoreReply, CoreError>> + Send;

    /// Apply several inputs in order, as [`Core::apply`] applies each (`T-031-04`, `REQ-CS-7`):
    /// one answer per input, in order. An input never runs before the ones ahead of it: once one
    /// fails, every later one answers [`CoreError::Unavailable`] without running, and the caller
    /// retries from the first failure. A core that batches its own work (the core Agent, `F-031`)
    /// overrides this; the default applies them one by one.
    fn apply_batch(
        &self,
        inputs: &[CoreInput],
    ) -> impl Future<Output = Vec<Result<CoreReply, CoreError>>> + Send {
        async move {
            let mut out = Vec::with_capacity(inputs.len());
            for input in inputs {
                let r = self.apply(input).await;
                let failed = r.is_err();
                out.push(r);
                if failed {
                    break;
                }
            }
            out.resize(inputs.len(), Err(CoreError::Unavailable));
            out
        }
    }

    /// A page of the outbox after `cursor`; only undelivered notifications if `undelivered`.
    fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: Time,
    ) -> impl Future<Output = Result<OutboxPage, CoreError>> + Send;

    /// The time of the last tick the core applied.
    fn last_tick(&self) -> impl Future<Output = Result<Option<Time>, CoreError>> + Send;

    /// When a tick next has work: the earliest deadline, or the next checkpoint's time; `None`
    /// when nothing is scheduled. The ticker skips the ticks before then (`DEC-6`, `F-031`,
    /// stress finding F2). A core that cannot say answers `Unavailable`, and the ticker ticks on
    /// its interval as before.
    fn next_due(&self) -> impl Future<Output = Result<Option<Time>, CoreError>> + Send {
        async { Err(CoreError::Unavailable) }
    }

    /// Answer a query as `caller`, at `now`.
    fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> impl Future<Output = Result<QueryReply, CoreError>> + Send;

    /// The credential `credential` names, if it is active and its identity is active (`F-028`).
    fn credential(
        &self,
        credential: &str,
    ) -> impl Future<Output = Result<Option<CredentialBody>, CoreError>> + Send;

    /// The integrating party `principal` is tied to, `did:`-shaped, if any (`REQ-ID-7`): the
    /// client key's domain (`REQ-OP-10`, T-009-16). Not a route: no caller reaches it. A tie is
    /// never undone, so an answer naming a party never goes stale.
    fn party_of(
        &self,
        principal: &str,
    ) -> impl Future<Output = Result<Option<String>, CoreError>> + Send;

    /// Whether the book takes commands (`F-036` `T-036-25`, `REQ-RC-6`, rulings `a0022-q2` and
    /// `a0022-q2b`). `false` only for a chainspec-founded book whose host holds no passing
    /// `verify-genesis` report for it yet: until then ingress refuses every command with
    /// `ERR-InitialStateNotSealed` before logging it, and answers every read. A core with no such
    /// gate (the in-process core, a book founded without a chainspec) is always open. Once `true`,
    /// it stays `true`.
    fn genesis_open(&self) -> impl Future<Output = Result<bool, CoreError>> + Send {
        async { Ok(true) }
    }

    /// Take a sequenced, signed feed (`F-038`, [`crate::feed`]). The default is a core no link
    /// separates from ingress (the in-process core, a test's): its inputs applied as a batch,
    /// sequences and signatures unread. A core across a link sends the feed (the socket's
    /// [`crate::remote::RemoteCore`], the stream's [`crate::stream::StreamCore`]), and the core
    /// host answers it through a [`crate::feed::Sequencer`].
    fn feed(
        &self,
        feed: &crate::feed::Feed,
    ) -> impl Future<Output = crate::feed::FeedReply> + Send {
        async move {
            match feed.inputs() {
                Ok(inputs) => crate::feed::FeedReply::Answers {
                    answers: self
                        .apply_batch(&inputs)
                        .await
                        .into_iter()
                        .map(crate::remote::BatchAnswer::of)
                        .collect(),
                },
                Err(reason) => crate::feed::FeedReply::Refused { reason },
            }
        }
    }

    /// Take a feed that came by the core host's link `link` (`T-038-21`): the host's sequencer
    /// binds the feed lease's epoch to the link that brought it, so any other stream only reads.
    /// The default ignores the link.
    fn feed_on(
        &self,
        feed: &crate::feed::Feed,
        link: u64,
    ) -> impl Future<Output = crate::feed::FeedReply> + Send {
        let _ = link;
        self.feed(feed)
    }

    /// The core host's link `link` closed: an epoch it brought may come back on another link.
    fn link_closed(&self, link: u64) -> impl Future<Output = ()> + Send {
        let _ = link;
        async {}
    }

    /// Whether this replica can reach a core that writes `partition` now (`F-038` §3): a replica
    /// takes the feed lease and leadership only while it can. A core in process or behind the
    /// socket always can; the stream's only while a core has a stream open to this replica.
    fn can_feed(&self, partition: &str) -> bool {
        let _ = partition;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_cross_the_boundary_as_json() {
        let i = CoreInput {
            key: InputKey([7; 16]),
            body: InputBody::Tick { now: 5 },
        };
        let json = serde_json::to_value(&i).unwrap();
        assert_eq!(json["key"], hex::encode([7u8; 16]));
        assert_eq!(json["body"]["input"], "tick");
        assert_eq!(serde_json::from_value::<CoreInput>(json).unwrap(), i);
        let r = CoreReply {
            input_seq: 3,
            outcome: Outcome::Refused {
                operation_reference: "x".into(),
                error: Refusal::ZeroAmount,
            },
        };
        let back: CoreReply = serde_json::from_slice(&serde_json::to_vec(&r).unwrap()).unwrap();
        assert_eq!(back, r);
    }

    /// The longest notification the core can put in a page: every field at its longest.
    fn longest_item() -> OutboxItem {
        use crate::wire::{AnchorRefBody, CoordinateBody, StateClassBody};
        use ledger_core::id::IdentityId;
        use ledger_core::lifecycle::State;
        let len = |v: &serde_json::Value| serde_json::to_vec(v).unwrap().len();
        let error = Refusal::ALL
            .into_iter()
            .max_by_key(|r| len(&serde_json::to_value(r).unwrap()))
            .unwrap();
        let state = State::ALL
            .into_iter()
            .map(|s| s.name().to_owned())
            .max_by_key(String::len)
            .unwrap();
        // Every identifier has a fixed-length body.
        let op = ledger_core::lifecycle::keys::inbound_reference(&[0xff; 44]);
        OutboxItem {
            notification: NotificationBody {
                id: hex::encode([0xff; 32]),
                operation_reference: op.to_did(),
                state: state.clone(),
                class: StateClassBody::OpenNonRefunding,
                terminal: false,
                outcome: Some(state),
                error: Some(error),
                anchor: Some(AnchorRefBody {
                    number: u64::MAX,
                    entry_sequence: u64::MAX,
                    coordinate: CoordinateBody {
                        instance: u32::MAX,
                        seq: u64::MAX,
                        digest: hex::encode([0xff; 32]),
                    },
                    height: u64::MAX,
                }),
                // The core never carries pass-through data: ingress adds it after the page
                // crossed (`REQ-PO-5`), so it costs the reply limit nothing.
                reference: None,
            },
            recipient: Some(IdentityId::mint(&op, u32::MAX).to_did()),
            created_at: Time::MAX,
            delivered_at: Some(Time::MAX),
        }
    }

    /// `REQ-EV-1`, `AC-M2.4`: a page of [`OUTBOX_PAGE`] notifications at their longest, each
    /// naming an anchor, with its cursor, fits the core's reply limit.
    #[test]
    fn an_outbox_page_at_its_longest_fits_the_reply_limit() {
        let item = longest_item();
        let page = OutboxPage {
            next: Some(item.notification.id.clone()),
            items: vec![item; OUTBOX_PAGE],
        };
        let bytes = serde_json::to_vec(&page).unwrap().len();
        assert!(bytes <= REPLY_LIMIT, "{bytes} B in a page of {OUTBOX_PAGE}");
    }
}
