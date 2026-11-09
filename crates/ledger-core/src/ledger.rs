// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! [`Ledger`]: the book and the service store, and the commands built so far (`F-003`, `F-004`,
//! SPEC §8.1).
//!
//! Every command takes a [`Ctx`] — the caller, the operation reference, and the time as an input
//! (`REQ-RT-7`) — and returns `Result<Accepted<_>, Refusal>`. An accepted command writes one
//! [`OperationRecord`] naming every authority it relied on and the principal that used it
//! (`REQ-AU-7`, `REQ-PN-1`, `INV-20`).
//!
//! **One apply unit.** A command validates everything first, then checks its store writes, then
//! applies its kernel unit, and commits its store writes (the record, the window charges) only if
//! the kernel applied. A kernel refusal or a binding fault changes nothing. This is the in-process
//! form of `REQ-RT-1`'s single commit; `F-005` gives it the Agent's.
//!
//! **Idempotency** (`F-007`, `REQ-OP-10`, `REQ-RT-2`). A command's operation reference is derived
//! from (principal, surface, client key) (`crate::lifecycle::keys`). The first acceptance under a
//! reference stores the request's canonical digest beside its record; a repeat of the same
//! request by the same principal returns the original outcome and writes nothing, and anything
//! else under that reference is `ERR-ClientKeyReused`.
//!
//! **Faults** (`REQ-ER-2`, amendment 0006 item 8). A binding fault, or a store write that was
//! checked and still failed, is `ERR-Internal`: no detail reaches the caller, the fault is kept for
//! the operator ([`Ledger::last_fault`]), and it is counted as an `internal_fault` incident on the
//! operator's signals ([`Ledger::internal_faults`]), which `F-018` pages on. It is never a
//! condition about the caller. The count lives in the process, not in the service store: a fault
//! is the host's, and nothing of it may enter the book or the calls a core Agent replays.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::book::{BindingFault, Book};
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use crate::account::{AccountKind, AccountStatus, Custody, LedgerAccount, Time};
use crate::asset::{Asset, AssetId, AssetKind, Minor, NonZeroMinor};
use crate::authority::{
    AuthorityKind, AuthorityUsed, Bounds, Dead, Grant, GrantKind, GrantTerms, OperationKind,
    Subject, Window, effective,
};
use crate::eval::{self, Decision, Request};
use crate::id::{AccountId, CredentialId, GrantId, IdentityId, Mint, OperationId};
use crate::identity::{Identity, IdentityKind, IdentityStatus};
use crate::lifecycle::{LifecycleConfig, Shown};
use crate::operator::OperatorOpKind;
use crate::principal::{Caller, Credential, CredentialMarker, Principal, Verifier, resolve};
use crate::refusal::Refusal;
use crate::set::NonEmptySet;
use crate::state::{Command, OperationRecord, Roles, State, StoreRefusal, Write};

/// What every command is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Ctx {
    /// Who calls.
    pub caller: Caller,
    /// The operation's reference (`F-007` mints it). Identifiers the command mints derive from it.
    pub op: OperationId,
    /// The time input (`REQ-RT-7`).
    pub now: Time,
}

/// An accepted command: its result and its attribution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted<T> {
    /// What the command produced.
    pub value: T,
    /// The operation's record.
    pub record: OperationRecord,
    /// For an operation with a lifecycle (SPEC §5.6), the state the reply shows, labelled with
    /// its class: every open state reads as non-terminal and non-refunding (`REQ-OP-7`). `None`
    /// for a command decided and complete at once (identities, accounts, grants, issuance).
    pub status: Option<Shown>,
}

/// An asset in the genesis registry.
///
/// A deployment's genesis registers no asset (`DEC-20`): its assets are registered by
/// `register_asset`, at the precision the settlement adapter attests for their location. This
/// declaration is the library's, for tests and tools that found a book without a settlement
/// layer. The asset it declares is still bound to its location, and the adapter's first
/// attestation of that location is checked against it like any other (a mismatch holds it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetDecl {
    /// Its id.
    pub id: AssetId,
    /// The settlement-layer location it is bound to from genesis, if any: only for a deployment
    /// whose position on the layer starts with it, empty, as the book does (`F-029` NOTES). An
    /// asset without one is book-only (`REQ-AS-7`), and is bound later by its issuance authority.
    #[cfg_attr(feature = "serde", serde(default))]
    pub location: Option<crate::asset::AssetLocation>,
    /// Its one precision.
    pub precision: u8,
    /// Its kind.
    pub kind: AssetKind,
    /// Whether it is registered already deprecated.
    pub deprecated: bool,
}

/// The reviewed configuration a book starts from (`REQ-AS-2`). Later changes to the registry and to
/// operator authority are `F-013`'s audited commands.
#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Genesis {
    /// The registry.
    pub assets: Vec<AssetDecl>,
    /// The verifier of the operator's first credential.
    pub operator_verifier: Verifier,
    /// Deadlines and review routing (`F-007`).
    pub lifecycle: LifecycleConfig,
    /// The chainspec the book starts from, by its digest and chunk count (`F-036`, `REQ-RC-6`,
    /// owner ruling `genesis-chainspec`). `None` for a book founded empty, as before.
    #[cfg_attr(feature = "serde", serde(default, alias = "seed"))]
    pub chainspec: Option<crate::chainspec::SpecCommitment>,
    /// The chainspec itself, when the genesis state is built from it here: genesis then applies
    /// every chunk, natively, through the same commands, and the book is founded **sealed**
    /// (`build-spec`, the core host's replica). Without it a book that commits a chainspec is
    /// founded unsealed, and takes nothing but the chainspec's chunks, one input each, until the
    /// last seals it: the path `verify-genesis` replays inside the core Agent. Never serialised:
    /// what genesis commits is the digest; the chainspec is a file beside the deployment.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub spec: Option<crate::chainspec::SpecRef>,
}

/// What genesis created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Founded {
    /// The operator's organisation identity.
    pub operator: IdentityId,
    /// Its first credential.
    pub operator_credential: CredentialId,
    /// The issuance counterparty.
    pub issuance: AccountId,
}

/// Why genesis failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenesisError {
    /// The store refused the configuration.
    Store(StoreRefusal),
    /// The recovery configuration fails its start-up checks (`F-012` §10, `REQ-AU-9`,
    /// `REQ-AU-15`): the deployment does not start.
    Recovery(crate::custody::StartupError),
    /// The chainspec is not the one genesis commits, is for another genesis, or one of its chunks
    /// was refused, with the condition (`F-036`): the deployment does not start.
    ChainSpec(Option<Refusal>),
}

/// A new identity and its first credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdentityOpened {
    /// The identity.
    pub identity: IdentityId,
    /// Its first credential.
    pub credential: CredentialId,
}

/// `open_account`'s request.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpenAccount {
    /// The one owning identity (`REQ-LA-1`).
    pub owner: IdentityId,
    /// Its kind.
    pub kind: AccountKind,
    /// Its custody mode.
    pub custody: Custody,
    /// A Treasury account's limits below zero, per asset.
    pub treasury_limits: BTreeMap<AssetId, Minor>,
    /// For owned custody: the digest of the recovery mechanisms the holder was shown
    /// ([`Ledger::recovery_terms`], `REQ-AU-9`, `AC-A5.3`). Missing or stale is refused.
    /// Ignored for delegated custody.
    pub acknowledged: Option<crate::custody::MechanismsDigest>,
}

/// `grant_authority`'s request, as submitted: the bounds may be missing, which is
/// `ERR-UnboundedGrant`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct GrantRequest {
    /// Its kind.
    pub kind: GrantKind,
    /// Who will hold it.
    pub principal: IdentityId,
    /// What it is over.
    pub subject: Subject,
    /// What it may be used for.
    pub ops: NonEmptySet<OperationKind>,
    /// The most one operation may move.
    pub per_operation: Option<NonZeroMinor>,
    /// The most one window may move.
    pub per_window: Option<Window>,
    /// The assets it is limited to.
    pub assets: Option<NonEmptySet<AssetId>>,
    /// The counterparties it is limited to.
    pub counterparties: Option<NonEmptySet<AccountId>>,
    /// The grant it narrows. Required when the grantor acts under a grant.
    pub parent: Option<GrantId>,
    /// When it stops working.
    pub expires: Option<Time>,
}

/// The book and the service store.
///
/// Both change only through the commands (`REQ-CS-5`, `REQ-AT-3`, `F-013` T-013-09): they are
/// handed out by shared reference only, so no write can be called through a ledger.
///
/// ```compile_fail
/// // The service store of a ledger cannot be written except by its commands.
/// type L = ledger_core::Ledger<ledger_kernel::store::MemStore, ledger_kernel::vault::MemVault>;
/// fn raw(l: &mut L) {
///     l.state().commit(&[]).unwrap();
/// }
/// ```
///
/// ```compile_fail
/// // Nor its book.
/// type L = ledger_core::Ledger<ledger_kernel::store::MemStore, ledger_kernel::vault::MemVault>;
/// fn raw(l: &mut L) {
///     let _ = l.book().apply(&Default::default(), 0);
/// }
/// ```
///
/// ```
/// // While the same calls on a shared reference's owner compile: the examples above fail for
/// // the reason they say.
/// type L = ledger_core::Ledger<ledger_kernel::store::MemStore, ledger_kernel::vault::MemVault>;
/// fn read(l: &L) -> usize {
///     l.state().registry().len() + usize::from(l.book().root().is_ok())
/// }
/// ```
pub struct Ledger<S: LeafStore, V: VaultBackend> {
    book: Book<S, V>,
    pub(crate) state: State,
    last_fault: Option<BindingFault>,
    /// Internal faults answered `ERR-Internal` in this process, and when the latest was
    /// (`REQ-ER-2`): the incident the operator's signals carry. Kept outside the service store:
    /// a fault is the host's, and nothing of it may enter the book or its replay.
    internal_faults: (u64, Time),
    config: LifecycleConfig,
    fault: Option<lifecycle::FaultAt>,
    /// The calls recorded while recording is on (`crate::call`, `F-005`).
    pub(crate) calls: Option<crate::call::CallLog>,
    /// The writer's code version and the digest of its upgrade-authority mode, as its hot row
    /// holds them (`T-037-02`): what a checkpoint binds beside the book's genesis (`T-037-03`). Not
    /// service state: whoever runs the ledger as a writer sets it ([`Ledger::set_code`]).
    code: ([u8; 32], [u8; 32]),
    /// The input number the commands that follow take, as the writer that runs the ledger numbers
    /// its inputs ([`Ledger::set_input`]); 0 when nothing says. What a way out of a scheduled
    /// upgrade is checked against (`T-037-15`). Not service state.
    input: u64,
    /// The way out of a scheduled upgrade an input took, until the writer reads it
    /// ([`Ledger::take_way_out`], `T-037-15`). Not service state.
    way_out: Option<crate::code::WayOut>,
    /// The upgrade an input scheduled (the code version and the input number it applies from),
    /// until the writer reads it ([`Ledger::take_upgrade`], `T-037-04`). Not service state.
    upgrade: Option<([u8; 32], u64)>,
    /// The upgrade the writer has scheduled, as its hot row holds it (the code version and the
    /// input number it applies from): what a move of it is checked against
    /// ([`Ledger::set_scheduled_upgrade`], `T-037-05`, owner ruling w6). Not service state.
    scheduled: Option<([u8; 32], u64)>,
    /// How deep the public methods that change the book are nested now, and how many outermost
    /// ones have returned since [`Ledger::set_input`]: the calls of the current input decided so
    /// far (`T-037-15`: a way out must be its input's first call). Kept whether or not calls are
    /// recorded, so the Agent, the replica and a replay count alike. Not service state.
    pub(crate) calls_in_input: (u32, u32),
    /// The writer's upgrade-authority position (`T-037-05`): any scheduled change of mode, and
    /// what this input has decided. Not service state either.
    writer: crate::code::authority::Position,
    /// The code generation in force before the current input, and whether its calls are decided
    /// by the next one ([`Ledger::set_generation`]). Not service state.
    generation: (u64, bool),
    /// Whether the current input, unless it is a way out, switches the writer to
    /// [`crate::settlement::CHAINED_SETTLEMENT`]: the book's incidents and gaps then move to the
    /// audit trail at its end ([`Ledger::switch_settlement`], `T-043-08`). Not service state.
    switching: bool,
    /// Whether the current input has moved its chunk of the incidents an earlier generation kept
    /// in the book's state (`Ledger::move_incidents`, `T-043-08`): at most one chunk an input,
    /// however many adapter calls it holds. Not service state.
    moved_in_input: bool,
}

fn policy_used(p: &Principal) -> AuthorityUsed {
    AuthorityUsed {
        kind: AuthorityKind::Operator,
        grant: None,
        principal: p.identity,
        credential: p.credential,
        on_behalf_of: p.on_behalf_of,
    }
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// Found the book's service state on a freshly bootstrapped book: the registry, the operator's
    /// organisation identity and first credential, and the issuance counterparty.
    pub fn genesis(
        book: Book<S, V>,
        genesis: Genesis,
        op: OperationId,
        now: Time,
    ) -> Result<(Self, Founded), GenesisError> {
        genesis
            .lifecycle
            .recovery
            .check([&genesis.operator_verifier])
            .map_err(GenesisError::Recovery)?;
        let spec = genesis.spec.clone();
        if let Some(sp) = &spec
            && (Some(sp.commitment()) != genesis.chainspec || sp.header().genesis_op != op)
        {
            return Err(GenesisError::ChainSpec(None));
        }
        let mut state = State::new();
        let operator = Identity::new(Mint::new(op, 0), IdentityKind::Organisation, None)
            .map_err(|_| GenesisError::Store(StoreRefusal::Dangling))?;
        let credential = Credential::new(
            Mint::new(op, 1),
            operator.id(),
            CredentialMarker::Ordinary,
            None,
            genesis.operator_verifier,
        );
        let issuance = LedgerAccount::new(
            Mint::new(op, 2),
            operator.id(),
            AccountKind::Issuance,
            Custody::Delegated,
            now,
            BTreeMap::new(),
        )
        .map_err(|_| GenesisError::Store(StoreRefusal::Dangling))?;
        let config = genesis.lifecycle;
        let mut writes: Vec<Write> = Vec::new();
        for a in &genesis.assets {
            writes.push(Write::RegisterAsset {
                id: a.id,
                precision: a.precision,
                kind: a.kind,
            });
            if let Some(l) = a.location {
                writes.push(Write::AssetBinding {
                    id: a.id,
                    binding: crate::asset::AssetBinding::Binding(l),
                });
                writes.push(Write::AssetBinding {
                    id: a.id,
                    binding: crate::asset::AssetBinding::Bound(l),
                });
            }
            if a.deprecated {
                writes.push(Write::DeprecateAsset(a.id));
            }
        }
        let founded = Founded {
            operator: operator.id(),
            operator_credential: credential.id(),
            issuance: issuance.id(),
        };
        writes.push(Write::PutIdentity(operator));
        writes.push(Write::PutCredential(credential));
        writes.push(Write::PutAccount(issuance));
        writes.push(Write::SetRoles(Roles {
            operator: founded.operator,
            issuance: founded.issuance,
        }));
        if let Some(seed) = genesis.chainspec {
            if seed.chunks == 0 {
                return Err(GenesisError::Store(StoreRefusal::ChainSpec(
                    crate::chainspec::SpecRefusal::Commitment,
                )));
            }
            writes.push(Write::ChainSpec(crate::chainspec::SpecWrite::Commit(seed)));
        }
        writes.push(Write::PutRecord(OperationRecord {
            op,
            command: Command::Genesis,
            authorities: vec![AuthorityUsed {
                kind: AuthorityKind::Operator,
                grant: None,
                principal: founded.operator,
                credential: founded.operator_credential,
                on_behalf_of: None,
            }],
            accounts: vec![founded.issuance],
            postings: Vec::new(),
            at: now,
        }));
        state.commit(&writes).map_err(GenesisError::Store)?;
        let mut ledger = Ledger {
            book,
            state,
            last_fault: None,
            internal_faults: (0, 0),
            config,
            fault: None,
            calls: None,
            code: ([0; 32], [0; 32]),
            input: 0,
            way_out: None,
            upgrade: None,
            scheduled: None,
            calls_in_input: (0, 0),
            writer: crate::code::authority::Position::default(),
            generation: (0, false),
            switching: false,
            moved_in_input: false,
        };
        if let Some(sp) = spec {
            ledger.build_genesis_state(&founded, &sp)?;
        }
        Ok((ledger, founded))
    }

    /// The genesis state of a chainspec (`F-036`, owner ruling `genesis-chainspec`): every chunk,
    /// in order, through [`Ledger::apply_spec_chunk`], as the operator, natively. The same code
    /// the chunk-by-chunk path runs inside the core Agent, so the state, and every root, is the
    /// same; the last chunk seals it.
    fn build_genesis_state(
        &mut self,
        founded: &Founded,
        spec: &crate::chainspec::ChainSpec,
    ) -> Result<(), GenesisError> {
        let ctx = Ctx {
            caller: Caller::new(founded.operator, founded.operator_credential),
            op: spec.header().genesis_op,
            now: spec.header().at,
        };
        // One chunk at a time (`ChainSpec::try_for_each_chunk`): never every chunk's bytes at once.
        spec.try_for_each_chunk(|_, bytes, path| {
            self.apply_spec_chunk(&ctx, bytes, path)
                .map(|_| ())
                .map_err(|r| GenesisError::ChainSpec(Some(r)))
        })?;
        if !self.spec_progress().is_some_and(|p| p.sealed) {
            return Err(GenesisError::ChainSpec(None));
        }
        Ok(())
    }

    /// A ledger from its parts: a book already founded, its service store, and the lifecycle
    /// configuration genesis took. The core Agent (`F-005`) builds one per transition, over the
    /// witnessed book and the service store it keeps; nothing is checked here that the parts do
    /// not already hold.
    pub fn from_parts(book: Book<S, V>, state: State, config: LifecycleConfig) -> Self {
        Ledger {
            book,
            state,
            last_fault: None,
            internal_faults: (0, 0),
            config,
            fault: None,
            calls: None,
            code: ([0; 32], [0; 32]),
            input: 0,
            way_out: None,
            upgrade: None,
            scheduled: None,
            calls_in_input: (0, 0),
            writer: crate::code::authority::Position::default(),
            generation: (0, false),
            switching: false,
            moved_in_input: false,
        }
    }

    /// The writer's code version and its upgrade-authority mode's digest, as its hot row holds
    /// them (`T-037-02`): every checkpoint created from here binds them ([`Ledger::code_binding`],
    /// `T-037-03`). The core Agent sets them from its hot row before each input, and the host's
    /// replica from the code each entry names. A ledger whose writer names no code binds zeros.
    pub fn set_code(&mut self, code: [u8; 32], authority: [u8; 32]) {
        self.code = (code, authority);
    }

    /// The input number the commands that follow take (`T-037-15`, `T-037-05`), as the writer that
    /// runs the ledger numbers its inputs: the core Agent sets it from the input before running its
    /// calls, the host's replica from the input it will stage next, a replay from each entry, and
    /// the in-process core from its journal's numbering. A way out of a scheduled upgrade is
    /// decided against it ([`Ledger::reschedule_upgrade`]), a delay of the upgrade-authority mode
    /// counts it, and a scheduled change of mode whose input has come is the mode in force from
    /// it ([`crate::code::authority::Position::begin`], the one place that does so). A writer
    /// that has the hot row gives the scheduled change first ([`Ledger::set_authority_next`]).
    pub fn set_input(&mut self, input: u64) {
        self.input = input;
        self.moved_in_input = false;
        self.calls_in_input.1 = 0;
        self.writer.begin(&mut self.code.1, input);
    }

    /// The code generation that decides the commands that follow (the generation rule, amendment
    /// 0033 and the `code-history` gate, `T-037-12`): `in_force`, the writer's generation before
    /// the input, and `next`, whether the input is decided by the generation after it (an
    /// upgrade applies to it). A change in how the book decides an input comes in under a new
    /// generation and leaves every earlier one as it was, so the same build decides an input of
    /// any generation exactly as the code of that generation did: what the book takes from a
    /// generation on is read here ([`Ledger::generation`]).
    ///
    /// The core Agent sets it from its hot row and the generation compiled into the code that
    /// runs the input, the host's replica from the writer's code before the input it stages, and a
    /// replay from the code map. A ledger never told decides as generation 0.
    pub fn set_generation(&mut self, in_force: u64, next: bool) {
        self.generation = (in_force, next);
        let c = crate::settlement::CHAINED_SETTLEMENT;
        self.switching = next && in_force < c && c <= in_force.saturating_add(1);
    }

    /// Whether the current input, unless it is a way out, switches the writer to the chained
    /// generation (`T-043-08`).
    pub(crate) fn switching_input(&self) -> bool {
        self.switching
    }

    /// The code generation that decides the commands that follow ([`Ledger::set_generation`]).
    pub fn generation(&self) -> u64 {
        self.generation
            .0
            .saturating_add(u64::from(self.generation.1))
    }

    /// Whether the book takes settlement as a chain of entries (amendment 0040, `F-043`): from
    /// [`crate::settlement::CHAINED_SETTLEMENT`] on. Before it, an input is decided as the code
    /// of its generation decided it (the re-scan, the incidents and gaps in the book's state).
    pub(crate) fn chained(&self) -> bool {
        self.generation() >= crate::settlement::CHAINED_SETTLEMENT
    }

    /// Whether the book follows a new sequence of the settlement layer's record (amendment 0041):
    /// from [`crate::settlement::SETTLEMENT_EPOCHS`] on. Before it, the adapter's report of one is
    /// refused and nothing is written.
    pub(crate) fn epochs(&self) -> bool {
        self.generation() >= crate::settlement::SETTLEMENT_EPOCHS
    }

    /// The upgrade the writer has scheduled, as its hot row holds it, before the input that
    /// follows (`T-037-05`, owner ruling w6): the code version and the input number it applies
    /// from. A move of it ([`Ledger::reschedule_upgrade`]) must go strictly later than that input
    /// number, so that one member alone cannot pull an approved upgrade earlier than the input the
    /// approvers and the delay agreed to. Whoever runs the ledger as a writer sets it before
    /// [`Ledger::set_input`], from the code it holds (the core Agent from its hot row, the host's
    /// replica and a replay from the code the inputs before have left); a ledger never told has
    /// nothing scheduled, and the move is checked against its own input alone. It writes nothing
    /// to the book.
    pub fn set_scheduled_upgrade(&mut self, scheduled: Option<([u8; 32], u64)>) {
        self.scheduled = scheduled;
    }

    /// How many calls of the current input were decided before the one running now
    /// ([`Ledger::set_input`] starts an input).
    pub(crate) fn calls_before(&self) -> u32 {
        self.calls_in_input.1
    }

    /// The way out of a scheduled upgrade the last commands took, if one did (`T-037-15`): a
    /// [`Ledger::reschedule_upgrade`] or [`Ledger::cancel_upgrade`] accepted, not the replay of an
    /// earlier one. The writer applies it to its scheduled upgrade, which it holds, not the
    /// ledger.
    pub fn take_way_out(&mut self) -> Option<crate::code::WayOut> {
        self.way_out.take()
    }

    /// The upgrade the last commands scheduled, if one did (`T-037-04`): an [`Ledger::upgrade`]
    /// accepted, not the replay of an earlier one. The writer schedules it, as the one writer step
    /// (`core_binding::fence::decide`); the ledger holds no writer state.
    pub fn take_upgrade(&mut self) -> Option<([u8; 32], u64)> {
        self.upgrade.take()
    }

    /// What a checkpoint created now binds beside a `v2` one (`T-037-03`, `REQ-RT-15`): the book's
    /// genesis (its chainspec's seed digest, or zeros for a book founded without one), and the code
    /// version and mode [`Ledger::set_code`] gave. All of them aledgent's own values (`REQ-RT-19`).
    pub fn code_binding(&self) -> crate::anchor::CodeBinding {
        crate::anchor::CodeBinding {
            genesis: self
                .state
                .chainspec()
                .progress()
                .map_or([0; 32], |p| p.commitment.digest),
            code: self.code.0,
            authority: self.code.1,
        }
    }

    /// The service store, read-only.
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The book, read-only.
    pub fn book(&self) -> &Book<S, V> {
        &self.book
    }

    /// Decide the commands that follow against the book without committing the kernel's leaves
    /// after each: they are committed together by [`Ledger::end_batch`] (`T-031-19`). Every
    /// command decides exactly as it would alone; the service's own state commits per command as
    /// before. A host that batches must treat a failed [`Ledger::end_batch`] as the loss of the
    /// whole batch: it stops and replays its inputs (`REQ-RT-3`).
    pub fn begin_batch(&mut self) {
        self.book.begin_batch();
    }

    /// Commit the kernel's leaves of every command since [`Ledger::begin_batch`] as one commit.
    pub fn end_batch(&mut self) -> Result<Option<(u64, [u8; 32])>, BindingFault> {
        self.book.end_batch()
    }

    /// The deployment's lifecycle configuration.
    pub fn config(&self) -> &LifecycleConfig {
        &self.config
    }

    /// The last binding fault a command failed closed on, for the operator.
    pub fn last_fault(&self) -> Option<&BindingFault> {
        self.last_fault.as_ref()
    }

    /// How many commands this process answered `ERR-Internal`, and when the latest was
    /// (`REQ-ER-2`). The signals raise them as `internal_fault` incidents (`F-018` pages).
    pub fn internal_faults(&self) -> (u64, Time) {
        self.internal_faults
    }

    pub(super) fn roles(&self) -> Result<Roles, Refusal> {
        self.state.roles().ok_or(Refusal::NotAuthorised)
    }

    /// Whether `p` acts with the operator's policy authority: the operator identity, unscoped.
    pub(super) fn is_operator(&self, p: &Principal) -> bool {
        self.state
            .roles()
            .is_some_and(|r| r.operator == p.identity && p.unscoped())
    }

    /// The authority with which `p` acts as the **identities principal** (`REQ-ID-6`, amendment
    /// 0025): the operator's policy authority, or a live operator-surface grant naming
    /// [`OperatorOpKind::OpenIdentity`], with an unscoped credential. `ERR-AuthorityRevoked` when
    /// only revoked grants name it.
    pub(super) fn identities_principal(
        &self,
        p: &Principal,
        now: Time,
    ) -> Result<AuthorityUsed, Refusal> {
        if self.is_operator(p) {
            return Ok(policy_used(p));
        }
        operator::operator_authority_in(&self.state, p, OperatorOpKind::OpenIdentity, now)
    }

    /// The delegated grant over `owner` with which `p` opens an account `owner` will own
    /// (amendment 0025): live along its whole chain, naming [`OperationKind::OpenAccount`]. The
    /// lowest such grant is used; `ERR-AuthorityRevoked` when only revoked ones exist.
    fn open_account_grant(
        &self,
        p: &Principal,
        owner: IdentityId,
        now: Time,
    ) -> Result<AuthorityUsed, Refusal> {
        if !p.unscoped() {
            return Err(Refusal::NotAuthorised);
        }
        let mut revoked = false;
        let mut live: Option<GrantId> = None;
        for g in self.state.held(&p.identity, Subject::Identity(owner)) {
            if g.kind() != GrantKind::Delegated {
                continue;
            }
            match effective(g, now, |id| self.state.grant(id)) {
                Ok(e) if e.ops.contains(&OperationKind::OpenAccount) => {
                    live = Some(live.map_or(g.id(), |x| x.min(g.id())));
                }
                Ok(_) | Err(Dead::Lapsed) => {}
                Err(Dead::Revoked) => revoked = true,
            }
        }
        match live {
            Some(grant) => Ok(AuthorityUsed {
                kind: AuthorityKind::Delegated,
                grant: Some(grant),
                principal: p.identity,
                credential: p.credential,
                on_behalf_of: p.on_behalf_of,
            }),
            None if revoked => Err(Refusal::AuthorityRevoked),
            None => Err(Refusal::NotAuthorised),
        }
    }

    /// Whether `acct` belongs to an identity the identities principal manages (amendment 0025
    /// R2, R4): in delegated custody, owned by an active identity that is not the operator, not an
    /// agent, and holds no account in owned custody.
    pub(super) fn managed_account(&self, acct: &LedgerAccount) -> bool {
        let owner = acct.owner();
        acct.custody() == Custody::Delegated
            && self.state.roles().is_some_and(|r| r.operator != owner)
            && self.state.identity(&owner).is_some_and(|i| {
                i.status() == IdentityStatus::Active && i.kind() != IdentityKind::Agent
            })
            && !self.state.holds_owned_custody(&owner)
    }

    /// A binding fault: `ERR-Internal`, with the fault kept and an incident raised (`REQ-ER-2`).
    pub(super) fn fault(&mut self, f: BindingFault) -> Refusal {
        let key = alloc::format!("{f:?}");
        self.last_fault = Some(f);
        self.internal(key.as_bytes())
    }

    /// An internal fault: count it for the operator's incident (`internal_faults`) and answer
    /// `ERR-Internal`, with no detail (`REQ-ER-2`). `what` names the fault in the host's log only.
    pub(super) fn internal(&mut self, what: &[u8]) -> Refusal {
        let _ = what;
        let at = self.state.operations().clock().unwrap_or(0);
        self.internal_faults = (self.internal_faults.0.saturating_add(1), at);
        Refusal::Internal
    }

    /// Commit a checked batch. The batch was checked against this same state, and nothing has
    /// changed since, so this cannot fail; if it ever did, it is an internal fault (`REQ-ER-2`).
    pub(super) fn commit(&mut self, writes: &[Write]) -> Result<(), Refusal> {
        match self.state.commit(writes) {
            Ok(_) => Ok(()),
            Err(e) => Err(self.internal(alloc::format!("commit: {e:?}").as_bytes())),
        }
    }

    pub(super) fn check(&mut self, writes: &[Write]) -> Result<(), Refusal> {
        self.state.check(writes).map_err(|_| Refusal::NotAuthorised)
    }

    // ---------------------------------------------------------------- identities

    /// `open_identity` (SPEC §8.1, `REQ-AG-1`): a new identity of `kind`, with its first
    /// credential. An agent names the identity it acts for, which must be active. Opening
    /// identities is the identities principal's: the operator's policy authority (`DEP-8`), or
    /// the role the operator grants another principal (`REQ-ID-6`, amendment 0025).
    pub fn open_identity(
        &mut self,
        ctx: &Ctx,
        kind: IdentityKind,
        acts_for: Option<IdentityId>,
        verifier: Verifier,
    ) -> Result<Accepted<IdentityOpened>, Refusal> {
        let top = self.enter(|| crate::call::Call::OpenIdentity {
            ctx: *ctx,
            kind,
            acts_for,
            verifier: verifier.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<IdentityOpened>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::open_identity(&p, kind, acts_for, &verifier);
            let opened = |r: &OperationRecord| IdentityOpened {
                identity: IdentityId::mint(&r.op, 0),
                credential: CredentialId::mint(&r.op, 1),
            };
            if let Some(a) = self.replay(ctx, &p, &digest, opened)? {
                return Ok(a);
            }
            // The identities principal: the operator, or the holder of its role (amendment 0025).
            let used = self.identities_principal(&p, ctx.now)?;
            if let Some(principal) = acts_for {
                match self.state.identity(&principal) {
                    Some(i) if i.status() == IdentityStatus::Active => {}
                    _ => return Err(Refusal::NotAuthorised),
                }
            }
            // A verifier of the scheme it names (`DEC-19`, `F-028`).
            if !verifier.well_formed() {
                return Err(Refusal::CredentialInvalid);
            }
            let identity = Identity::new(Mint::new(ctx.op, 0), kind, acts_for)?;
            let credential = Credential::new(
                Mint::new(ctx.op, 1),
                identity.id(),
                CredentialMarker::Ordinary,
                None,
                verifier,
            );
            let value = IdentityOpened {
                identity: identity.id(),
                credential: credential.id(),
            };
            let record = OperationRecord {
                op: ctx.op,
                command: Command::OpenIdentity,
                authorities: vec![used],
                accounts: Vec::new(),
                postings: Vec::new(),
                at: ctx.now,
            };
            let writes = [
                Write::PutIdentity(identity),
                Write::PutCredential(credential),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, digest, None),
            ];
            self.check(&writes)?;
            self.commit(&writes)?;
            Ok(Accepted {
                value,
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Restrict, reinstate or close an identity: the operator's policy authority. `F-013` wraps it
    /// as an audited operator command. The operator's own identity cannot be changed this way.
    pub fn set_identity_status(
        &mut self,
        ctx: &Ctx,
        identity: IdentityId,
        status: IdentityStatus,
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::SetIdentityStatus {
            ctx: *ctx,
            identity,
            status,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::set_identity_status(&p, identity, status);
            if let Some(a) = self.replay(ctx, &p, &digest, |_| ())? {
                return Ok(a);
            }
            if !self.is_operator(&p) || identity == self.roles()?.operator {
                return Err(Refusal::NotAuthorised);
            }
            let current = self
                .state
                .identity(&identity)
                .ok_or(Refusal::NotAuthorised)?;
            if current.status() == IdentityStatus::Closed && status != IdentityStatus::Closed {
                return Err(Refusal::IdentityRestricted);
            }
            let record = OperationRecord {
                op: ctx.op,
                command: Command::SetIdentityStatus,
                authorities: vec![policy_used(&p)],
                accounts: Vec::new(),
                postings: Vec::new(),
                at: ctx.now,
            };
            let writes = [
                Write::IdentityStatus(identity, status),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, digest, None),
            ];
            self.check(&writes)?;
            self.commit(&writes)?;
            Ok(Accepted {
                value: (),
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    // ---------------------------------------------------------------- accounts

    /// `open_account` (SPEC §8.1, `REQ-LA-1`, `INV-9`). The account exists, is addressable and
    /// can be credited in any registered asset as soon as this returns: its kernel sub-accounts are
    /// created with the first posting that touches them.
    ///
    /// The owner opens a Customer account for itself with an unscoped credential; the identities
    /// principal (the operator's policy authority, or the role it grants, amendment 0025) opens any
    /// kind but Issuance, for any identity; a delegated grant over the owner that names
    /// `OpenAccount` opens a Customer account in delegated custody for it. An agent holds
    /// delegated authority only, so it owns no account (SPEC §3).
    pub fn open_account(
        &mut self,
        ctx: &Ctx,
        request: OpenAccount,
    ) -> Result<Accepted<AccountId>, Refusal> {
        let top = self.enter(|| crate::call::Call::OpenAccount {
            ctx: *ctx,
            request: request.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<AccountId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::open_account(&p, &request);
            if let Some(a) = self.replay(ctx, &p, &digest, |r| AccountId::mint(&r.op, 0))? {
                return Ok(a);
            }
            // Who may open it (`REQ-ID-6`, amendment 0025): the owner, a Customer account for
            // itself; the identities principal (the operator, or the holder of its role), any kind
            // for any identity; or a delegated grant over the owner naming `OpenAccount`, a
            // Customer account in delegated custody.
            let operator = self.is_operator(&p);
            let own = request.owner == p.identity && p.unscoped();
            let authority = if own && (operator || request.kind == AccountKind::Customer) {
                AuthorityUsed {
                    kind: AuthorityKind::Owner,
                    grant: None,
                    principal: p.identity,
                    credential: p.credential,
                    on_behalf_of: p.on_behalf_of,
                }
            } else {
                match self.identities_principal(&p, ctx.now) {
                    Ok(used) => used,
                    Err(_) => {
                        let used = self.open_account_grant(&p, request.owner, ctx.now)?;
                        if request.kind != AccountKind::Customer
                            || request.custody != Custody::Delegated
                        {
                            return Err(Refusal::NotAuthorised);
                        }
                        used
                    }
                }
            };
            let owner = self
                .state
                .identity(&request.owner)
                .ok_or(Refusal::NotAuthorised)?;
            owner.require_active()?;
            if owner.kind() == IdentityKind::Agent || request.kind == AccountKind::Issuance {
                return Err(Refusal::NotAuthorised);
            }
            // Owned custody only with the recovery mechanisms acknowledged (`F-012` §5).
            if request.custody == Custody::Owned {
                self.require_acknowledged(&request.owner, None, request.acknowledged)?;
            }
            let account = LedgerAccount::new(
                Mint::new(ctx.op, 0),
                request.owner,
                request.kind,
                request.custody,
                ctx.now,
                request.treasury_limits,
            )?;
            let value = account.id();
            let record = OperationRecord {
                op: ctx.op,
                command: Command::OpenAccount,
                authorities: vec![authority],
                accounts: vec![value],
                postings: Vec::new(),
                at: ctx.now,
            };
            let writes = [
                Write::PutAccount(account),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, digest, None),
            ];
            self.check(&writes)?;
            self.commit(&writes)?;
            Ok(Accepted {
                value,
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The account's balance in `asset` on the kernel, `0` if it has never held the asset.
    pub(super) fn net(&self, account: &LedgerAccount, asset: &Asset) -> Result<i128, BindingFault> {
        Ok(self
            .book
            .settled_net(&account.sub_account(asset).id())?
            .unwrap_or(0))
    }

    /// `close_account` (SPEC §8.1, `REQ-LA-4`): zero in every asset and balance kind, then
    /// `Closed`. An open reservation is a non-zero reserved balance: `ERR-AccountNotEmpty`. Nothing is deleted, and later postings are `ERR-AccountClosed`.
    pub fn close_account(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::CloseAccount { ctx: *ctx, account });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::close_account(&p, &account);
            if let Some(a) = self.replay(ctx, &p, &digest, |_| ())? {
                return Ok(a);
            }
            let decision = eval::authorise_principal(
                &self.state,
                &p,
                &Request {
                    kind: OperationKind::CloseAccount,
                    account,
                    asset: None,
                    amount: None,
                    counterparty: None,
                },
                ctx.now,
                self.generation(),
            )?;
            let acct = self
                .state
                .account(&account)
                .ok_or(Refusal::UnknownAccount)?
                .clone();
            // Reserved: no open reservation, in any asset (`F-004`). Encumbered value is zero in V0
            // (`DEF-11`). Then available, in every asset the registry has ever held.
            if self.state.reservations().has_open(&account) {
                return Err(Refusal::AccountNotEmpty);
            }
            let assets: Vec<Asset> = self.state.registry().iter().copied().collect();
            for asset in &assets {
                match self.net(&acct, asset) {
                    Ok(0) => {}
                    Ok(_) => return Err(Refusal::AccountNotEmpty),
                    Err(f) => return Err(self.fault(f)),
                }
            }
            let record = OperationRecord {
                op: ctx.op,
                command: Command::CloseAccount,
                authorities: vec![decision.used],
                accounts: vec![account],
                postings: Vec::new(),
                at: ctx.now,
            };
            let writes = [
                Write::AccountStatus(account, AccountStatus::Closed),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, digest, None),
            ];
            self.check(&writes)?;
            self.commit(&writes)?;
            Ok(Accepted {
                value: (),
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    // ---------------------------------------------------------------- authority

    /// `grant_authority` (SPEC §8.1, `REQ-AU-2`, `REQ-AU-4`, `REQ-AU-10`).
    ///
    /// - A **delegated** grant over an account is made by its owner, or narrowed from a delegated
    ///   grant the grantor holds over the same account (`parent`).
    /// - **Operator** and **issuance** grants are made by the operator, or narrowed from one of the
    ///   same kind and subject the grantor holds.
    /// - An issuance grant is over one asset, may name only `Issue` and `Redeem`, and must carry a
    ///   per-window bound and an asset bound on its asset (`REQ-AU-10`).
    /// - Nothing names `Issue` but an issuance grant.
    /// - A **delegated** grant over an **identity** names `OpenAccount`, and nothing else names
    ///   it; it may also name the account kinds its holder delegates. It is made by the identity
    ///   itself or by the identities principal, and never narrowed into another grant over an
    ///   identity (amendment 0025).
    /// - Its holder makes delegated grants over the identity's accounts in delegated custody with
    ///   it as their parent: never wider than it (R4). The identities principal makes them over an
    ///   account of an identity it manages directly.
    ///
    /// Refusals, in order: `ERR-CredentialInvalid`, `ERR-UnboundedGrant`, `ERR-NotAuthorised` for
    /// a malformed kind, subject or operation set, `ERR-UnknownAccount` for an account the grantor
    /// cannot reach, the account's or asset's status, then `ERR-NotAuthorised` or
    /// `ERR-AuthorityRevoked` for the grantor's own authority.
    pub fn grant_authority(
        &mut self,
        ctx: &Ctx,
        request: GrantRequest,
    ) -> Result<Accepted<GrantId>, Refusal> {
        let top = self.enter(|| crate::call::Call::GrantAuthority {
            ctx: *ctx,
            request: request.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<GrantId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::grant_authority(&p, &request);
            if let Some(a) = self.replay(ctx, &p, &digest, |r| GrantId::mint(&r.op, 0))? {
                return Ok(a);
            }
            let bounds = Bounds::new(
                request.per_operation,
                request.per_window,
                request.assets.clone(),
                request.counterparties.clone(),
            )?;
            // The shape of the grant.
            let issue_ops = request
                .ops
                .iter()
                .all(|k| matches!(k, OperationKind::Issue | OperationKind::Redeem));
            let names_issue = request.ops.contains(&OperationKind::Issue);
            // A reversal's authorities live on the operator surface, one per principal
            // (`REQ-OS-1`, `REQ-OS-3`): no grant names the kind.
            if request.ops.contains(&OperationKind::Reversal) {
                return Err(Refusal::NotAuthorised);
            }
            // `OpenAccount` is named by a grant over an identity, and only there; a grant over an
            // identity names it, and may name the account kinds its holder delegates over the
            // accounts it opens (amendment 0025 R4).
            let names_open = request.ops.contains(&OperationKind::OpenAccount);
            if matches!(request.subject, Subject::Identity(_)) != names_open {
                return Err(Refusal::NotAuthorised);
            }
            match (request.kind, request.subject) {
                (GrantKind::Issuance, Subject::Asset(asset)) => {
                    if !issue_ops {
                        return Err(Refusal::NotAuthorised);
                    }
                    if bounds.per_window().is_none()
                        || !bounds.assets().is_some_and(|a| a.contains(&asset))
                    {
                        return Err(Refusal::UnboundedGrant);
                    }
                }
                (GrantKind::Delegated, Subject::Account(_))
                | (GrantKind::Operator, Subject::Account(_) | Subject::DelegatedAccounts) => {
                    if names_issue {
                        return Err(Refusal::NotAuthorised);
                    }
                }
                (GrantKind::Delegated, Subject::Identity(_)) => {
                    if names_issue {
                        return Err(Refusal::NotAuthorised);
                    }
                }
                _ => return Err(Refusal::NotAuthorised),
            }
            // The subject: reach, then status.
            let mut owner_of_subject = false;
            let mut managed_by_role = false;
            match request.subject {
                Subject::Account(a) => {
                    let acct = self.state.account(&a).ok_or(Refusal::UnknownAccount)?;
                    // The parent names this account, or, for a delegated grant, the account's
                    // owner (amendment 0025 R4: an opener's grant over the identity).
                    let parent_names_it = request
                        .parent
                        .and_then(|g| self.state.grant(&g))
                        .is_some_and(|g| {
                            g.principal() == p.identity
                                && (g.subject() == request.subject
                                    || (request.kind == GrantKind::Delegated
                                        && g.subject() == Subject::Identity(acct.owner())
                                        && acct.custody() == Custody::Delegated))
                        });
                    owner_of_subject = acct.owner() == p.identity;
                    managed_by_role = request.kind == GrantKind::Delegated
                        && self.managed_account(acct)
                        && self.identities_principal(&p, ctx.now).is_ok();
                    let reaches = p.in_scope(&a)
                        && (owner_of_subject
                            || parent_names_it
                            || managed_by_role
                            || self.is_operator(&p));
                    if !reaches {
                        return Err(Refusal::UnknownAccount);
                    }
                    acct.require_open()?;
                    if acct.kind() == AccountKind::Issuance {
                        return Err(Refusal::NotAuthorised);
                    }
                }
                Subject::Asset(asset) => {
                    self.state.active_asset(&asset)?;
                }
                Subject::DelegatedAccounts => {}
                // An identity that may own accounts: active, and not an agent (SPEC §3). The
                // identity itself grants it, or the identities principal, or the holder of a
                // grant it narrows.
                // A grant over an identity is never narrowed into another one: its holder
                // delegates only over the accounts of that identity (amendment 0025 R4).
                Subject::Identity(i) => {
                    if request.parent.is_some() {
                        return Err(Refusal::NotAuthorised);
                    }
                    owner_of_subject = i == p.identity && p.unscoped();
                    let reaches =
                        owner_of_subject || self.identities_principal(&p, ctx.now).is_ok();
                    let identity = self.state.identity(&i).filter(|_| reaches);
                    match identity {
                        Some(id)
                            if id.status() == IdentityStatus::Active
                                && id.kind() != IdentityKind::Agent => {}
                        _ => return Err(Refusal::NotAuthorised),
                    }
                }
            }
            match self.state.identity(&request.principal) {
                Some(i) if i.status() == IdentityStatus::Active => {}
                _ => return Err(Refusal::NotAuthorised),
            }
            // The grantor's own authority.
            let authority = match request.parent {
                None => match request.kind {
                    GrantKind::Delegated if owner_of_subject => AuthorityUsed {
                        kind: AuthorityKind::Owner,
                        grant: None,
                        principal: p.identity,
                        credential: p.credential,
                        on_behalf_of: p.on_behalf_of,
                    },
                    GrantKind::Operator | GrantKind::Issuance if self.is_operator(&p) => {
                        policy_used(&p)
                    }
                    // Over an identity, or over an account of an identity it manages, by the
                    // identities principal (amendment 0025, R4).
                    GrantKind::Delegated
                        if matches!(request.subject, Subject::Identity(_)) || managed_by_role =>
                    {
                        self.identities_principal(&p, ctx.now)?
                    }
                    _ => return Err(Refusal::NotAuthorised),
                },
                Some(parent_id) => {
                    let parent = self.state.grant(&parent_id).ok_or(Refusal::NotAuthorised)?;
                    // The same kind and subject; or, for a delegated grant over an account in
                    // delegated custody, a grant over the account's owner (amendment 0025 R4).
                    let over_owner = match request.subject {
                        Subject::Account(a) => {
                            request.kind == GrantKind::Delegated
                                && self.state.account(&a).is_some_and(|acct| {
                                    acct.custody() == Custody::Delegated
                                        && parent.subject() == Subject::Identity(acct.owner())
                                })
                        }
                        _ => false,
                    };
                    if parent.principal() != p.identity
                        || parent.kind() != request.kind
                        || (parent.subject() != request.subject && !over_owner)
                    {
                        return Err(Refusal::NotAuthorised);
                    }
                    if parent.kind() == GrantKind::Issuance && !p.unscoped() {
                        return Err(Refusal::NotAuthorised);
                    }
                    match effective(parent, ctx.now, |g| self.state.grant(g)) {
                        Ok(_) => {}
                        Err(Dead::Revoked) => return Err(Refusal::AuthorityRevoked),
                        Err(Dead::Lapsed) => return Err(Refusal::NotAuthorised),
                    }
                    AuthorityUsed {
                        kind: parent.kind().into(),
                        grant: Some(parent_id),
                        principal: p.identity,
                        credential: p.credential,
                        on_behalf_of: p.on_behalf_of,
                    }
                }
            };
            let grant = Grant::new(
                Mint::new(ctx.op, 0),
                p.identity,
                GrantTerms {
                    kind: request.kind,
                    principal: request.principal,
                    subject: request.subject,
                    ops: request.ops,
                    bounds,
                    parent: request.parent,
                    expires: request.expires,
                },
                ctx.now,
            );
            let value = grant.id();
            let accounts = match request.subject {
                Subject::Account(a) => vec![a],
                _ => Vec::new(),
            };
            let record = OperationRecord {
                op: ctx.op,
                command: Command::GrantAuthority,
                authorities: vec![authority],
                accounts,
                postings: Vec::new(),
                at: ctx.now,
            };
            let writes = [
                Write::PutGrant(grant),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, digest, None),
            ];
            self.check(&writes)?;
            self.commit(&writes)?;
            Ok(Accepted {
                value,
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `revoke_authority` (SPEC §8.1, `REQ-AU-3`): the grant and every grant derived from it stop
    /// at once, because every evaluation walks the chain. Revoked by the owner of the account it
    /// is over, by the holder of any grant it was narrowed from, for operator and issuance
    /// grants by the operator, and for a grant over an identity by that identity or the
    /// identities principal (amendment 0025). Revoking a revoked grant changes nothing but the record.
    pub fn revoke_authority(&mut self, ctx: &Ctx, grant: GrantId) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::RevokeAuthority { ctx: *ctx, grant });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::revoke_authority(&p, &grant);
            if let Some(a) = self.replay(ctx, &p, &digest, |_| ())? {
                return Ok(a);
            }
            let g = self.state.grant(&grant).ok_or(Refusal::NotAuthorised)?;
            let owner = match g.subject() {
                Subject::Account(a) => self
                    .state
                    .account(&a)
                    .is_some_and(|acct| acct.owner() == p.identity && p.in_scope(&a)),
                Subject::Identity(i) => i == p.identity && p.unscoped(),
                _ => false,
            };
            // A grant over an identity is also revoked by the identities principal (amendment
            // 0025), which may grant it.
            let managed = match g.subject() {
                Subject::Identity(_) => true,
                Subject::Account(a) => {
                    g.kind() == GrantKind::Delegated
                        && self
                            .state
                            .account(&a)
                            .is_some_and(|acct| self.managed_account(acct))
                }
                _ => false,
            };
            let identities = if managed {
                self.identities_principal(&p, ctx.now).ok()
            } else {
                None
            };
            let authority = if owner {
                AuthorityUsed {
                    kind: AuthorityKind::Owner,
                    grant: None,
                    principal: p.identity,
                    credential: p.credential,
                    on_behalf_of: p.on_behalf_of,
                }
            } else if matches!(g.kind(), GrantKind::Operator | GrantKind::Issuance)
                && self.is_operator(&p)
            {
                policy_used(&p)
            } else if let Some(used) = identities {
                used
            } else {
                // The holder of an ancestor.
                let mut link = g.parent();
                let mut found = None;
                while let Some(id) = link {
                    let Some(anc) = self.state.grant(&id) else {
                        break;
                    };
                    if anc.principal() == p.identity {
                        found = Some(anc);
                        break;
                    }
                    link = anc.parent();
                }
                let anc = found.ok_or(Refusal::NotAuthorised)?;
                AuthorityUsed {
                    kind: anc.kind().into(),
                    grant: Some(anc.id()),
                    principal: p.identity,
                    credential: p.credential,
                    on_behalf_of: p.on_behalf_of,
                }
            };
            let already = g.revoked().is_some();
            let accounts = match g.subject() {
                Subject::Account(a) => vec![a],
                _ => Vec::new(),
            };
            let record = OperationRecord {
                op: ctx.op,
                command: Command::RevokeAuthority,
                authorities: vec![authority],
                accounts,
                postings: Vec::new(),
                at: ctx.now,
            };
            let mut writes = Vec::with_capacity(2);
            if !already {
                writes.push(Write::Revoke(grant, ctx.now));
            }
            writes.push(Write::PutRecord(record.clone()));
            writes.push(Self::request_entry(ctx, &p, digest, None));
            self.check(&writes)?;
            self.commit(&writes)?;
            Ok(Accepted {
                value: (),
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Evaluate authority for one operation, with no effect: the evaluator every value-moving
    /// command calls (`F-004`, `F-008`). Window charges are returned, not made.
    pub fn authorise(&self, ctx: &Ctx, request: &Request) -> Result<Decision, Refusal> {
        eval::authorise(
            &self.state,
            &ctx.caller,
            request,
            ctx.now,
            self.generation(),
        )
    }
}

mod anchoring;
mod asset_binding;
pub mod auditing;
pub mod custody;
mod disclosure;
mod issuance;
pub mod lifecycle;
pub mod operator;
mod queries;
mod reads;
pub mod requests;
mod reservations;
pub mod seeding;
pub mod settle_adapter;
pub mod settlement;
pub mod transfers;
mod upgrade_authority;

pub use transfers::{Leg, Transfer};

#[cfg(test)]
mod tests;
