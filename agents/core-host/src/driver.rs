// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The AgentRuntime driver: one Local-profile Agent, driven through VOS `saga`'s runtime ABI.
//!
//! **Why the ABI and not `vosx`.** The canonical path, `vosx space invoke-local`, runs the system
//! Authority Agent about nine times per application invocation, takes 18–37 s per call and failed
//! at the 13th (`notes/MEASUREMENTS-F002.md` §7, §11.2 (f); `T-025-05` is the upstream change
//! that would fix it). So the host drives the standard AgentRuntime the way the Local host's
//! journal driver does (`vos/src/agent/local_journal_driver.rs`, the `RuntimeWork` →
//! `RuntimeTransition` exchange, the same outer gas, a runtime prepared once), as `F-002`'s
//! harness did (`spikes/vos-host`). The host is then the Local host: it keeps the Agent's runtime
//! state, persists it after every transition, and signs every Authority receipt with the Agent's
//! authority key.
//!
//! **Sequenced identities.** Every Linear (and Query) invocation carries a sequenced identity
//! (`vos_agent_sdk::SequencedInvocation`: a fixed prefix, the result component and a position),
//! so the runtime compacts acknowledged results instead of refusing the 65th invocation
//! (`T-025-01`, `proposals/vos/0001`). The position is the driver's own counter, persisted with
//! the state. A position is used exactly once: a retry of an ambiguous call re-sends the same
//! work under the same identity, and the runtime's exactly-once rule answers it (`INV-18`).

use std::num::NonZeroU64;
use std::time::Instant;

use crate::keys::SharedSigner;
use vos::Encode as _;
use vos::agent::package_admission::{AdmittedActorPackage, AdmittedRuntimePackage};
use vos::agent_sdk as sdk;
use vos::agent_sdk::authority::{
    AgentAuthorityBinding, AuthorityEvidence, AuthorityIssuer, AuthorityLaneRoots,
    AuthorityOperationKind, AuthorityReceipt, AuthorityReceiptSelector,
};
use vos::agent_sdk::contract::ExecutionLimits;
use vos::agent_sdk::wire::CanonicalWire as _;
use vos::agent_sdk::{
    ActorDirectoryRecord, ActorEntry, ActorId, AgentDescriptor, AgentId, AgentIdentity,
    AgentProfile, AgentReplica, BlobRef, DeploymentId, Hash, InstallationId,
    InvocationAuthorization, InvocationId, InvocationOrigin, InvocationResultStorage,
    InvocationStatus, InvocationWork, ManagementReply, ManagementRequest, MethodMode, NodeId,
    PrincipalId, ProducerId, ProgramId, ReplicaRole, RuntimeBlob, RuntimeExecutionContext,
    RuntimeOutcome, RuntimeState, RuntimeTransition, RuntimeWork, SequencedInvocation, SpaceId,
};
use vos_pvm::ExitReason;
use vos_pvm::PvmBackend;
use vos_pvm::refine::{MemoryModel, PreparedProgram};
use vos_pvm::refine_host::RefineContext;

/// The Local host's outer budget for a runtime execution
/// (`vos/src/agent/local_journal_driver.rs`), plus the work's own gas for an Invoke.
pub const OUTER_GAS: u64 = vos::agent::driver::DEFAULT_MANAGEMENT_GAS;
/// The core Agent's execution bounds (VOS `ExecutionLimits`, set right after the Agent is
/// created): 8 000 M of actor gas an invocation, where a batch is bound by its packs (about
/// twenty reservations, `notes/MEASUREMENTS-F031.md` §1) rather than by gas; 1 024 reads, for the
/// committed maps' few reads a leaf (layout 3) over a batch of 32; 1 MiB a lane, for layout 3's
/// rows (layout 2's state is under a kilobyte). VOS's standard bounds are 1 000 M, 16 and 48 KiB.
pub const AGENT_EXECUTION_LIMITS: ExecutionLimits = ExecutionLimits {
    max_invocation_gas: 8_000_000_000,
    max_invocation_reads: 1_024,
    max_lane_state_bytes: 1024 * 1024,
};

/// The core Agent's bounds, with `F025_ACTOR_GAS` (gas units) overriding the gas for a bench;
/// with `F025_SKIP_LIMITS=1`, VOS's standard bounds with that gas.
pub fn agent_limits() -> ExecutionLimits {
    // `F025_MAX_LIMITS=1`: the runtime's own ceilings, for the shard Agents, which keep their
    // books in their rows (`aledgent-core-host shards`, F-031 T-031-21; the stress workflow's
    // shard scenario sets it).
    if std::env::var("F025_MAX_LIMITS").is_ok_and(|v| v == "1") {
        return ExecutionLimits::MAXIMUM;
    }
    if std::env::var("F025_SKIP_LIMITS").is_ok_and(|v| v == "1") {
        let mut standard = ExecutionLimits::STANDARD;
        if let Some(gas) = std::env::var("F025_ACTOR_GAS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            standard.max_invocation_gas = gas;
        }
        return standard;
    }
    let mut limits = AGENT_EXECUTION_LIMITS;
    if let Some(gas) = std::env::var("F025_ACTOR_GAS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        limits.max_invocation_gas = gas;
    }
    limits
}

/// The most gas one actor invocation may be given: the Agent's bound.
pub fn actor_gas() -> u64 {
    agent_limits().max_invocation_gas
}

/// What the host records around one runtime execution (the Local driver's debug fields).
#[derive(Clone, Copy, Debug, Default)]
pub struct Exec {
    /// Encoding the work.
    pub encode_us: u64,
    /// Loading the prepared runtime.
    pub preparation_us: u64,
    /// Running it.
    pub execution_us: u64,
    /// Decoding the transition.
    pub decode_us: u64,
    /// Preparation and execution.
    pub elapsed_us: u64,
    /// The work's encoded size.
    pub input_bytes: u64,
    /// The transition's encoded size.
    pub output_bytes: u64,
    /// The runtime's gas.
    pub gas_used: u64,
}

/// Why the runtime did not complete an invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DriverError {
    /// The runtime refused the work or the actor did not finish (`Panicked`, `OutOfGas`, a
    /// refusal of the invocation): nothing committed.
    Refused(String),
    /// The acknowledgement was refused; the result stays retained.
    Acknowledgement(String),
    /// The runtime program itself failed.
    Runtime(String),
}

impl core::fmt::Display for DriverError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DriverError::Refused(s) => write!(f, "refused: {s}"),
            DriverError::Acknowledgement(s) => write!(f, "acknowledgement refused: {s}"),
            DriverError::Runtime(s) => write!(f, "runtime: {s}"),
        }
    }
}

/// An installed actor, as an invocation names it.
#[derive(Clone, Debug)]
pub struct Actor {
    /// Its directory record.
    pub record: ActorDirectoryRecord,
    /// Its program, schema and policy, staged into every invocation.
    pub availability: Vec<RuntimeBlob>,
}

/// One completed invocation.
#[derive(Clone, Debug)]
pub struct Invoked {
    /// The actor's reply bytes.
    pub reply: Vec<u8>,
    /// The actor's gas.
    pub actor_gas: u64,
    /// The Invoke execution.
    pub invoke: Exec,
    /// The Acknowledge execution.
    pub ack: Exec,
}

/// The seeds of one Agent: its identity and its keys. Test values are the suite's; a deployment
/// supplies its own (the authority key is the Agent owner's, never a default of this crate).
#[derive(Clone)]
pub struct AgentKeys {
    /// Signs runtime packages.
    pub package: SharedSigner,
    /// The Agent's authority: signs every receipt.
    pub authority: SharedSigner,
    /// Distinguishes Agents of one space.
    pub discriminator: u8,
}

/// The standard AgentRuntime, prepared once, and one Local Agent's runtime state.
pub struct Runtime {
    prepared: PreparedProgram,
    /// The runtime program, kept for `F025_LOCKSTEP`'s cold run on the other engine.
    runtime_pvm: Vec<u8>,
    /// Actor programs by ProgramId, each verified natively when attached, for a runtime of
    /// VOS's program-lookup profile (`F025_PROGRAM_LOOKUP=1`): it obtains its actor's
    /// program through Refine `historical_lookup` instead of in the work.
    programs: std::collections::HashMap<[u8; 32], Vec<u8>>,
    descriptor: AgentDescriptor,
    authority: SharedSigner,
    /// The Agent's durable runtime state: the host persists it after every transition.
    pub state: RuntimeState,
    decision: u64,
    slot: u64,
    linear: u64,
    control: u64,
    /// Authority decisions (receipts) this host issued for invocations whose Acknowledge has not
    /// succeeded since it started (`F-018`, `AledgentVosAuthorityBacklog`). The Local host
    /// acknowledges every invocation at once, so it is zero unless acknowledgements fail; the
    /// runtime retains each unacknowledged result, and VOS refuses more than 256 retained
    /// Authority decisions (`MAX_AUTHORITY_OPERATION_ISSUER_RECORDS`).
    unacknowledged: u64,
}

impl Runtime {
    /// Create a Local Agent under the standard runtime `runtime_pvm`.
    pub fn create(runtime_pvm: &[u8], keys: &AgentKeys) -> Result<Self, DriverError> {
        let runtime = crate::package::runtime_package(runtime_pvm, &*keys.package)
            .map_err(|e| DriverError::Runtime(e.to_string()))?;
        Self::create_with(runtime_pvm, &runtime, keys)
    }

    fn create_with(
        runtime_pvm: &[u8],
        runtime: &AdmittedRuntimePackage,
        keys: &AgentKeys,
    ) -> Result<Self, DriverError> {
        let prepared = PreparedProgram::new_with_backend(runtime_pvm, backend())
            .map_err(|e| DriverError::Runtime(format!("prepare runtime: {e:?}")))?;
        let d = keys.discriminator;
        let space = SpaceId([0x11; 32]);
        let owner = PrincipalId([d; 32]);
        let creation_nonce = Hash([d.wrapping_add(0x30); 32]);
        let agent = AgentId::derive(space, owner, creation_nonce.as_bytes());
        let public_key = keys.authority.public();
        let descriptor = AgentDescriptor {
            identity: AgentIdentity {
                space,
                agent,
                owner,
                profile: AgentProfile::Local,
                runtime_deployment: runtime.deployment(),
                runtime_program: runtime.program(),
                runtime_producer: runtime.producer(),
                transition_producer: ProducerId([d.wrapping_add(0x60); 32]),
            },
            creation_nonce,
            authority: AgentAuthorityBinding {
                policy: Hash([0x81; 32]),
                issuer: AuthorityIssuer {
                    principal: PrincipalId([0x82; 32]),
                    actor: ActorId([0x83; 32]),
                    deployment: DeploymentId([0x84; 32]),
                    program: ProgramId([0x85; 32]),
                    producer: ProducerId::of_public_key(&public_key),
                },
                public_key,
                initial_epoch: 1,
            },
            private_recovery: None,
            runtime_package: runtime.package_ref().clone(),
            runtime_contract: runtime.manifest().contract,
            capabilities: runtime.capabilities(),
            replicas: vec![AgentReplica {
                node: NodeId([0x22; 32]),
                principal: owner,
                role: ReplicaRole::Voter,
            }],
        };
        descriptor
            .validate()
            .map_err(|e| DriverError::Runtime(format!("descriptor: {e:?}")))?;
        let mut r = Self {
            prepared,
            runtime_pvm: runtime_pvm.to_vec(),
            programs: std::collections::HashMap::new(),
            descriptor,
            authority: std::sync::Arc::clone(&keys.authority),
            state: RuntimeState::default(),
            decision: 0,
            slot: 1,
            linear: 0,
            control: 0,
            unacknowledged: 0,
        };
        let request = ManagementRequest::Create(Box::new(r.descriptor.clone()));
        let (t, _) = r.manage(request, true)?;
        if !matches!(
            t.outcome,
            RuntimeOutcome::Management(Ok(ManagementReply::Created(_)))
        ) {
            return Err(DriverError::Runtime(format!("create: {:?}", t.outcome)));
        }
        // `F025_SKIP_LIMITS=1`: leave VOS's standard bounds, for a runtime that predates
        // `SetExecutionLimits` or for a bench of the standard bounds.
        if std::env::var("F025_SKIP_LIMITS").is_ok_and(|v| v == "1") {
            return Ok(r);
        }
        let limits = agent_limits();
        let (t, _) = r.manage(ManagementRequest::SetExecutionLimits(limits), true)?;
        match t.outcome {
            RuntimeOutcome::Management(Ok(ManagementReply::ExecutionLimitsSet(set)))
                if set == limits =>
            {
                Ok(r)
            }
            other => Err(DriverError::Runtime(format!("execution limits: {other:?}"))),
        }
    }

    /// The runtime state's size in bytes: every component, all lanes, rows included.
    pub fn state_bytes(&self) -> usize {
        self.state.control.len()
            + self.state.linear.len()
            + self.state.merge.len()
            + self.state.local.len()
    }

    /// The sequenced positions used so far: Linear, then Control (queries).
    pub fn positions(&self) -> (u64, u64) {
        (self.linear, self.control)
    }

    /// Restore the driver's counters with its persisted state (a restart).
    pub fn restore(&mut self, state: RuntimeState, linear: u64, control: u64, slot: u64) {
        self.state = state;
        self.linear = linear;
        self.control = control;
        self.slot = self.slot.max(slot);
        // The authority decisions of management requests are strictly increasing in the runtime's
        // state, and the counter is not persisted: after a restart the first one would sign
        // decision 1 and be refused (`AuthoritySequenceRegressed`). Every management request also
        // takes a slot, so the slot is never below the last decision: start from it.
        self.decision = self.decision.max(self.slot);
    }

    /// The last observed slot.
    pub fn slot(&self) -> u64 {
        self.slot
    }

    /// Authority decisions issued for invocations not yet acknowledged, since this host started.
    pub fn unacknowledged(&self) -> u64 {
        self.unacknowledged
    }

    fn exec(&self, work: &RuntimeWork) -> Result<(RuntimeTransition, Exec), DriverError> {
        let gas = match work {
            RuntimeWork::Invoke { invocation, .. } => OUTER_GAS.saturating_add(invocation.gas),
            _ => OUTER_GAS,
        };
        let t0 = Instant::now();
        let input = work
            .encode()
            .map_err(|e| DriverError::Runtime(format!("encode work: {e:?}")))?;
        let encode_us = t0.elapsed().as_micros() as u64;
        // `F025_DUMP_WORK=<path>`: keep the last Invoke's exact runtime input, for a profile.
        if let (Ok(path), RuntimeWork::Invoke { .. }) = (std::env::var("F025_DUMP_WORK"), work) {
            static DUMPED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = DUMPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = std::fs::write(format!("{path}.{n}"), &input);
        }
        let started = Instant::now();
        let ctx = RefineContext::load_prepared(&self.prepared, &input, gas, MemoryModel::Auto)
            .map_err(|e| DriverError::Runtime(format!("load runtime: {e:?}")))?;
        let preparation_us = started.elapsed().as_micros() as u64;
        let exec_started = Instant::now();
        let programs = &self.programs;
        let serve = |id: u64, machine: &mut vos_pvm::refine::Machine| {
            if id == HISTORICAL_LOOKUP {
                historical_lookup(machine, |hash| programs.get(hash).map(Vec::as_slice))
            } else {
                Err(ExitReason::HostCall(id))
            }
        };
        let inv = ctx
            .try_run_with_host(serve)
            .map_err(|e| DriverError::Runtime(format!("run runtime: {e:?}")))?;
        let execution_us = exec_started.elapsed().as_micros() as u64;
        let elapsed_us = started.elapsed().as_micros() as u64;
        if lockstep() {
            // Outside the timed region: the same input, cold, on the other engine.
            let other = match backend() {
                PvmBackend::ForceRecompiler => PvmBackend::ForceInterpreter,
                _ => PvmBackend::ForceRecompiler,
            };
            let theirs = RefineContext::load_with_backend(
                &self.runtime_pvm,
                &input,
                gas,
                MemoryModel::Auto,
                other,
            )
            .map_err(|e| DriverError::Runtime(format!("load on {other:?}: {e:?}")))?
            .try_run_with_host(serve)
            .map_err(|e| DriverError::Runtime(format!("run on {other:?}: {e:?}")))?;
            let seen = |i: &vos_pvm::refine::Invocation| {
                (
                    i.exit.clone(),
                    i.pc,
                    i.gas_used,
                    i.registers,
                    i.output_bounded(RuntimeTransition::MAX_ENCODED_BYTES),
                )
            };
            if seen(&theirs) != seen(&inv) {
                return Err(DriverError::Runtime("the two engines diverge".into()));
            }
            eprintln!("f025-lockstep: identical ({} gas)", inv.gas_used);
        }
        if inv.exit != ExitReason::Halt {
            return Err(DriverError::Runtime(format!(
                "runtime exit {:?} at pc {}",
                inv.exit, inv.pc
            )));
        }
        let t1 = Instant::now();
        let out = inv
            .output_bounded(RuntimeTransition::MAX_ENCODED_BYTES)
            .ok_or_else(|| DriverError::Runtime("the runtime's output is out of bounds".into()))?;
        let transition = RuntimeTransition::decode(&out)
            .map_err(|e| DriverError::Runtime(format!("decode transition: {e:?}")))?;
        let decode_us = t1.elapsed().as_micros() as u64;
        Ok((
            transition,
            Exec {
                encode_us,
                preparation_us,
                execution_us,
                decode_us,
                elapsed_us,
                input_bytes: input.len() as u64,
                output_bytes: out.len() as u64,
                gas_used: inv.gas_used,
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn receipt(
        &self,
        operation: AuthorityOperationKind,
        runtime_deployment: DeploymentId,
        actor: Option<ActorId>,
        actor_deployment: Option<DeploymentId>,
        decision_sequence: u64,
        request: Hash,
    ) -> AuthorityReceipt {
        let mut r = AuthorityReceipt {
            selector: AuthorityReceiptSelector {
                policy: self.descriptor.authority.policy,
                issuer: self.descriptor.authority.issuer,
                space: self.descriptor.identity.space,
                agent: self.descriptor.identity.agent,
                operation,
                runtime_deployment,
                actor,
                actor_deployment,
                evidence: AuthorityEvidence {
                    package: None,
                    proof: None,
                    commitment: Hash([0x86; 32]),
                },
                lane_roots: AuthorityLaneRoots::default(),
                epoch: 1,
                decision_sequence,
                acknowledged_through: 0,
                valid_from: self.slot,
                expires_at: self.slot,
                request,
            },
            public_key: self.descriptor.authority.public_key,
            signature: [0; sdk::authority::AUTHORITY_SIGNATURE_BYTES],
        };
        r.signature = self.authority.sign(&r.signing_bytes());
        r
    }

    fn manage(
        &mut self,
        request: ManagementRequest,
        authorised: bool,
    ) -> Result<(RuntimeTransition, Exec), DriverError> {
        let authority = if authorised {
            self.decision += 1;
            self.slot += 1;
            let (op, actor, dep) = match &request {
                ManagementRequest::Create(_) => (AuthorityOperationKind::CreateAgent, None, None),
                ManagementRequest::SetExecutionLimits(_) => {
                    (AuthorityOperationKind::SetExecutionLimits, None, None)
                }
                ManagementRequest::Install(i) => (
                    AuthorityOperationKind::InstallActor,
                    Some(i.entry.actor),
                    Some(i.entry.deployment),
                ),
                // The switch of a writer's code (`T-037-14`): the same authority as an install,
                // over the upgrade's target deployment.
                ManagementRequest::UpgradeActor(u) => (
                    AuthorityOperationKind::UpgradeActor,
                    Some(u.actor),
                    Some(u.to_deployment),
                ),
                other => {
                    return Err(DriverError::Runtime(format!(
                        "unsupported management request {other:?}"
                    )));
                }
            };
            Some(Box::new(self.receipt(
                op,
                self.descriptor.identity.runtime_deployment,
                actor,
                dep,
                self.decision,
                request.commitment(),
            )))
        } else {
            None
        };
        let work = RuntimeWork::Manage {
            context: RuntimeExecutionContext::Direct,
            space: self.descriptor.identity.space,
            agent: self.descriptor.identity.agent,
            runtime_deployment: self.descriptor.identity.runtime_deployment,
            state: self.state.clone(),
            request: Box::new(request),
            authority,
            observed_slot: if authorised { self.slot } else { u64::MAX },
        };
        let (t, x) = self.exec(&work)?;
        if authorised {
            self.state = t.state.clone();
        }
        Ok((t, x))
    }

    /// Install an admitted actor package under `name`, with `installation_data` as its
    /// constructor's argument bytes.
    pub fn install(
        &mut self,
        package: &AdmittedActorPackage,
        name: &str,
        installation_data: Option<Vec<u8>>,
    ) -> Result<Actor, DriverError> {
        let schema = sdk::schema::decode(package.state_lane_schema_bytes())
            .map_err(|e| DriverError::Runtime(format!("schema: {e}")))?;
        let actor = ActorId::top_level(self.descriptor.identity.agent, name);
        let constructor_abi = schema
            .constructor_abi()
            .map_err(|e| DriverError::Runtime(format!("constructor ABI: {e:?}")))?;
        let state_layout = schema
            .state_layout_hash()
            .map_err(|e| DriverError::Runtime(format!("state layout: {e:?}")))?;
        let data_ref = installation_data.as_ref().map(|d| BlobRef::of_bytes(d));
        let entry = ActorEntry {
            actor,
            name: name.into(),
            parent: None,
            deployment: package.deployment(),
            program: package.program(),
            package: package.package_ref().clone(),
            agent_schema: package.manifest().state_lane_schema.clone(),
            method_policy: package.manifest().method_policy.clone(),
            constructor_abi,
            installation_data: data_ref.clone(),
            state_layout,
            lanes: package.requirements().lanes,
            suspended: false,
        };
        let request = ManagementRequest::Install(Box::new(sdk::InstallActor {
            installation_id: InstallationId([0x91; 32]),
            registry_reservation: Hash([0x92; 32]),
            entry,
            producer: package.producer(),
            package: package.package_ref().clone(),
            agent_schema: package.manifest().state_lane_schema.clone(),
            method_policy: package.manifest().method_policy.clone(),
            constructor_abi,
            installation_data: installation_data
                .clone()
                .map(|bytes| sdk::InstallationData {
                    reference: BlobRef::of_bytes(&bytes),
                    bytes,
                }),
            state_layout,
            contract: package.manifest().contract,
            requirements: package.requirements(),
        }));
        let (t, _) = self.manage(request, true)?;
        if !matches!(
            t.outcome,
            RuntimeOutcome::Management(Ok(ManagementReply::Installed(_)))
        ) {
            return Err(DriverError::Refused(format!("install: {:?}", t.outcome)));
        }
        self.attach(package, name, installation_data)
    }

    /// Upgrade the installed actor `name` in place to the admitted `package`: the platform half
    /// of `switch(H, n)` on `saga` (`T-037-14`, amendment 0033).
    ///
    /// The runtime's upgrade is a compare-and-swap on the installed deployment: the request names
    /// the deployment this host read ([`ActorEntry::deployment`] of the directory's record), and a
    /// runtime whose actor is another one refuses it and changes nothing. The upgrade is authorised
    /// like an install: a receipt the Agent's authority key signs (through the signer in a
    /// deployment, so no key is in this process); the package's own signature, by the package key,
    /// is checked when it is admitted. The new package must keep the constructor ABI
    /// ([`sdk::UpgradeActor::constructor_abi`]): an in-place upgrade keeps the actor's immutable
    /// installation data.
    pub fn upgrade_actor(
        &mut self,
        package: &AdmittedActorPackage,
        name: &str,
    ) -> Result<Actor, DriverError> {
        let actor = ActorId::top_level(self.descriptor.identity.agent, name);
        let (t, _) = self.manage(
            ManagementRequest::InspectActors {
                after: None,
                limit: 4,
            },
            false,
        )?;
        let RuntimeOutcome::Management(Ok(ManagementReply::Actors(page))) = t.outcome else {
            return Err(DriverError::Runtime(format!("inspect: {:?}", t.outcome)));
        };
        let record = page
            .entries
            .into_iter()
            .find(|r| r.entry.actor == actor)
            .ok_or_else(|| DriverError::Runtime("the installed actor is not listed".into()))?;
        let schema = sdk::schema::decode(package.state_lane_schema_bytes())
            .map_err(|e| DriverError::Runtime(format!("schema: {e}")))?;
        let constructor_abi = schema
            .constructor_abi()
            .map_err(|e| DriverError::Runtime(format!("constructor ABI: {e:?}")))?;
        let state_layout = schema
            .state_layout_hash()
            .map_err(|e| DriverError::Runtime(format!("state layout: {e:?}")))?;
        let request = ManagementRequest::UpgradeActor(Box::new(sdk::UpgradeActor {
            actor,
            from_deployment: record.entry.deployment,
            to_deployment: package.deployment(),
            to_program: package.program(),
            producer: package.producer(),
            package: package.package_ref().clone(),
            agent_schema: package.manifest().state_lane_schema.clone(),
            method_policy: package.manifest().method_policy.clone(),
            constructor_abi,
            state_layout,
            contract: package.manifest().contract,
            requirements: package.requirements(),
        }));
        let (t, _) = self.manage(request, true)?;
        if !matches!(
            t.outcome,
            RuntimeOutcome::Management(Ok(ManagementReply::Upgraded(_)))
        ) {
            return Err(DriverError::Refused(format!("upgrade: {:?}", t.outcome)));
        }
        self.attach(package, name, None)
    }

    /// The actor `package` installed under `name`, as the Agent's directory lists it: after
    /// [`Runtime::install`], or after [`Runtime::restore`] on a restart.
    pub fn attach(
        &mut self,
        package: &AdmittedActorPackage,
        name: &str,
        installation_data: Option<Vec<u8>>,
    ) -> Result<Actor, DriverError> {
        let actor = ActorId::top_level(self.descriptor.identity.agent, name);
        let (t, _) = self.manage(
            ManagementRequest::InspectActors {
                after: None,
                limit: 4,
            },
            false,
        )?;
        let RuntimeOutcome::Management(Ok(ManagementReply::Actors(page))) = t.outcome else {
            return Err(DriverError::Runtime(format!("inspect: {:?}", t.outcome)));
        };
        let record = page
            .entries
            .into_iter()
            .find(|r| r.entry.actor == actor)
            .ok_or_else(|| DriverError::Runtime("the installed actor is not listed".into()))?;
        if record.entry.program != package.program() {
            return Err(DriverError::Refused(
                "the installed actor runs another program than this package".into(),
            ));
        }
        let program = package.program_bytes();
        if ProgramId::of_pvm(program) != record.entry.program {
            return Err(DriverError::Refused("the package's program bytes".into()));
        }
        self.programs
            .insert(record.entry.program.0, program.to_vec());
        let mut availability = vec![
            RuntimeBlob {
                reference: BlobRef::of_bytes(package.state_lane_schema_bytes()),
                bytes: package.state_lane_schema_bytes().to_vec(),
            },
            RuntimeBlob {
                reference: BlobRef::of_bytes(package.method_policy_bytes()),
                bytes: package.method_policy_bytes().to_vec(),
            },
        ];
        if !program_lookup() {
            availability.push(RuntimeBlob {
                reference: BlobRef::of_bytes(program),
                bytes: program.to_vec(),
            });
        }
        if let Some(d) = installation_data {
            availability.push(RuntimeBlob {
                reference: BlobRef::of_bytes(&d),
                bytes: d,
            });
        }
        availability.sort_unstable_by(|a, b| a.reference.cmp(&b.reference));
        Ok(Actor {
            record,
            availability,
        })
    }

    /// The next sequenced identity of `mode`'s result component.
    pub fn next_invocation(&mut self, mode: MethodMode) -> InvocationId {
        let counter = match mode.result_storage() {
            InvocationResultStorage::Control => &mut self.control,
            _ => &mut self.linear,
        };
        *counter += 1;
        let position = NonZeroU64::new(*counter).expect("a counter from 1");
        SequencedInvocation::new(mode.result_storage(), position).invocation_id()
    }

    /// Build the work of one invocation: `message` to `actor` in `mode`, as `origin`, with
    /// `blobs` staged as caller availability (the private inputs and a large witness, read with
    /// `Context::invocation_blob`; never written to the Agent's state).
    pub fn work(
        &self,
        actor: &Actor,
        invocation: InvocationId,
        mode: MethodMode,
        origin: InvocationOrigin,
        message: Vec<u8>,
        blobs: &[Vec<u8>],
    ) -> InvocationWork {
        let mut availability = actor.availability.clone();
        for b in blobs {
            availability.push(RuntimeBlob {
                reference: BlobRef::of_bytes(b),
                bytes: b.clone(),
            });
        }
        availability.sort_unstable_by(|a, b| a.reference.cmp(&b.reference));
        availability.dedup_by(|a, b| a.reference == b.reference);
        InvocationWork {
            space: self.descriptor.identity.space,
            agent: self.descriptor.identity.agent,
            runtime_deployment: self.descriptor.identity.runtime_deployment,
            invocation,
            actor: actor.record.entry.actor,
            incarnation: actor.record.incarnation,
            deployment: actor.record.entry.deployment,
            program: actor.record.entry.program,
            mode,
            origin,
            roles: sdk::InvocationRoleClaims::none(),
            message,
            installation_data: actor.record.entry.installation_data.clone(),
            availability,
            gas: actor_gas(),
            recovery_only: false,
        }
    }

    /// Invoke `work`, then acknowledge it: two whole-state executions, as the Local host does.
    /// On `Refused` nothing committed.
    pub fn invoke(&mut self, work: &InvocationWork) -> Result<Invoked, DriverError> {
        self.slot += 1;
        let receipt = self.receipt(
            AuthorityOperationKind::InvokeActor,
            work.runtime_deployment,
            Some(work.actor),
            Some(work.deployment),
            0,
            work.commitment(),
        );
        let (t, invoke) = self.exec(&RuntimeWork::Invoke {
            context: RuntimeExecutionContext::Direct,
            state: self.state.clone(),
            invocation: Box::new(work.clone()),
            authorization: Box::new(InvocationAuthorization::AuthorityReceipt(receipt.clone())),
            observed_slot: self.slot,
        })?;
        let (reply, actor_gas) = match &t.outcome {
            RuntimeOutcome::Completed(Ok(r)) if r.status == InvocationStatus::Done => {
                (r.reply.clone(), actor_gas().saturating_sub(r.gas_remaining))
            }
            other => return Err(DriverError::Refused(format!("{other:?}"))),
        };
        self.state = t.state;
        // The result is retained until its Acknowledge commits.
        self.unacknowledged += 1;
        let (t, ack) = self.exec(&RuntimeWork::Acknowledge {
            context: RuntimeExecutionContext::Direct,
            state: self.state.clone(),
            invocation: Box::new(sdk::InvocationRetirement::from_work(work)),
            authorization: Box::new(InvocationAuthorization::AuthorityReceipt(receipt)),
        })?;
        if !matches!(t.outcome, RuntimeOutcome::Acknowledged(Ok(_))) {
            return Err(DriverError::Acknowledgement(format!("{:?}", t.outcome)));
        }
        self.state = t.state;
        self.unacknowledged -= 1;
        Ok(Invoked {
            reply,
            actor_gas,
            invoke,
            ack,
        })
    }
}

/// A dynamic VOS message, as `vos::value::Msg` encodes it for an Agent method.
pub fn message(msg: vos::value::Msg) -> Vec<u8> {
    let mut m = vec![vos::value::TAG_DYNAMIC];
    m.extend_from_slice(&msg.encode());
    m
}

/// Whether this build of VOS has the recompiler: x86-64 Linux only.
pub const RECOMPILER_AVAILABLE: bool = cfg!(all(target_arch = "x86_64", target_os = "linux"));

/// The engine a command chose for this process ([`set_engine`]), over `F025_BACKEND`.
static ENGINE: std::sync::OnceLock<PvmBackend> = std::sync::OnceLock::new();

/// Fix the engine for every runtime and actor this process loads from now on, whatever
/// `F025_BACKEND` says: `verify-genesis --engine` (`F-036`). Set once; a second call is ignored.
pub fn set_engine(engine: PvmBackend) {
    let _ = ENGINE.set(engine);
}

/// An engine by name: `interpreter` or `recompiler`.
pub fn engine_named(name: &str) -> Result<PvmBackend, String> {
    match name {
        "interpreter" => Ok(PvmBackend::ForceInterpreter),
        "recompiler" if RECOMPILER_AVAILABLE => Ok(PvmBackend::ForceRecompiler),
        "recompiler" => Err("the recompiler is x86-64 Linux only".into()),
        other => Err(format!("engine {other}: interpreter or recompiler")),
    }
}

/// The name of an engine, as [`engine_named`] takes it.
pub fn engine_name(engine: PvmBackend) -> &'static str {
    match engine {
        PvmBackend::ForceRecompiler => "recompiler",
        _ => "interpreter",
    }
}

/// The engine `F025_BACKEND` selects for the runtime and every actor it starts, unless a command
/// fixed one ([`set_engine`]): the interpreter unless it says `recompiler` (x86-64 Linux only;
/// elsewhere loading fails, never falls back).
pub fn backend() -> PvmBackend {
    if let Some(engine) = ENGINE.get() {
        return *engine;
    }
    match std::env::var("F025_BACKEND").as_deref() {
        Ok("recompiler") => PvmBackend::ForceRecompiler,
        Ok("interpreter") | Err(_) => PvmBackend::ForceInterpreter,
        Ok(other) => panic!("F025_BACKEND={other}: interpreter or recompiler"),
    }
}

/// `F025_LOCKSTEP=1`: every execution also runs on the other engine, and must match.
fn lockstep() -> bool {
    std::env::var("F025_LOCKSTEP").is_ok_and(|v| v == "1")
}

/// `F025_PROGRAM_LOOKUP=1`: the runtime is VOS's program-lookup profile; actor programs are
/// served through `historical_lookup`, not carried in the work.
fn program_lookup() -> bool {
    std::env::var("F025_PROGRAM_LOOKUP").is_ok_and(|v| v == "1")
}

/// Gray Paper v0.8.0 Refine `historical_lookup`.
const HISTORICAL_LOOKUP: u64 = 7;

/// Serve `historical_lookup` (Gray Paper v0.8.0, Refine call 7) from programs this host verified
/// natively: φ7 the service (`2^64 - 1`, this one), φ8 the 32-byte hash, φ9 the output, φ10 the
/// offset, φ11 the most bytes to write; 600 gas plus 248 a KiB of φ11; φ7 returns the preimage's
/// length, or NONE. The same rule as VOS's `refine_host::serve_historical_lookup` (fork branch
/// `exp/runtime-profiles`), kept here so the host builds against every VOS it measures.
fn historical_lookup<'p>(
    machine: &mut vos_pvm::refine::Machine,
    preimage: impl FnOnce(&[u8; 32]) -> Option<&'p [u8]>,
) -> Result<(), ExitReason> {
    const NONE: u64 = u64::MAX;
    let r = *machine.registers();
    let (service, h, o, f, z) = (r[7], r[8], r[9], r[10], r[11]);
    let cost = 600u64.saturating_add(248u64.saturating_mul(z).div_ceil(1024));
    if !machine.charge(cost) {
        return Err(ExitReason::OutOfGas);
    }
    let mut hash = [0u8; 32];
    if !u32::try_from(h)
        .ok()
        .is_some_and(|h| machine.memory().read_bytes_checked(h, &mut hash))
    {
        return Err(ExitReason::Panic);
    }
    let Some(value) = (service == NONE).then(|| preimage(&hash)).flatten() else {
        machine.registers_mut()[7] = NONE;
        return Ok(());
    };
    let start = usize::try_from(f).unwrap_or(usize::MAX).min(value.len());
    let length = usize::try_from(z)
        .unwrap_or(usize::MAX)
        .min(value.len() - start);
    if length > 0
        && !u32::try_from(o).ok().is_some_and(|o| {
            machine
                .memory_mut()
                .write_bytes_checked(o, &value[start..start + length])
        })
    {
        return Err(ExitReason::Panic);
    }
    machine.registers_mut()[7] = value.len() as u64;
    Ok(())
}
