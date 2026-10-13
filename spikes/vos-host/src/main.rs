// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! F-002 harness for the VOS guest measurements (S2, S3 in the guest, S4).
//!
//! It creates a Local-profile Agent, installs a signed actor package and
//! invokes it through the standard AgentRuntime's canonical ABI — the same
//! `RuntimeWork` → `RuntimeTransition` exchange the Local Agent host drives
//! (`vos/src/agent/local_journal_driver.rs:3325-3364`), with the same
//! preparation cache (predecode once, fresh machine per call). Around each
//! runtime execution it records what that driver logs at `debug`:
//! `elapsed_us`, `preparation_us`, `execution_us`, `input_bytes` and
//! `gas_used`, plus encode/decode time and output bytes. Each invocation is
//! `Invoke` then `Acknowledge`, both whole-state round trips, authorised by an
//! Authority receipt signed with the Agent's authority key.
//!
//! Not measured here: the host's durable journal write, HTTP ingress, the
//! Authority Agent round trip and supervisor queueing (see the S4 notes).
//!
//!     vos-host s4 <state-blob.vos> <sizes,...> <n> <out.csv>
//!     vos-host s2 <kernel-probe.vos> <n> <n-noop> <out.csv> <case.bin>...
//!     vos-host s3 <kernel-probe.vos> <n> <n-noop> <out.csv> <witness.bin>...
//!     vos-host retention <state-blob.vos> <n>
//!     vos-host runtime-pvm <agent_runtime.elf> <out.pvm>
//!     vos-host profile <package.vos> <actor-name> <out.tsv> [payload-bytes]
//!
//! F-025 additions: `VOS_RUNTIME_PVM` selects the AgentRuntime program (the
//! bundled one by default), `runtime-pvm` links a runtime guest ELF built from
//! a VOS checkout exactly as `vosx agent-runtime-pvm` does, and
//! `F002_SEQUENCED=1` gives Linear invocations sequenced identities (the
//! layout of `vos_agent_sdk::SequencedInvocation`, VOS proposal 0001), acks
//! every fill and stops resetting the state between windows.
//!
//! F-025 change 5 (VOS proposal 0005, recompiled Agent execution), against a
//! VOS that has `RefineBackend` (`run.sh` sets `vos_backends`):
//! `F025_BACKEND=recompiler` runs every execution natively instead of on the
//! interpreter, and `F025_LOCKSTEP=1` runs every execution on both engines
//! and requires the same exit, counter, registers, gas and output bytes.
//!
//!     vos-host backends <package.vos> <actor-name> <n> [<case.bin>]
//!     vos-host compile-bench <package.vos> <n>

use std::io::Write as _;
use std::time::Instant;

use ed25519_dalek::{Signer as _, SigningKey};
use vos::Encode as _;
use vos::agent::package_admission::{
    AdmittedActorPackage, AdmittedRuntimePackage, admit_actor_package, admit_runtime_package,
};
use vos::agent_sdk as sdk;
use vos::agent_sdk::authority::{
    AgentAuthorityBinding, AuthorityEvidence, AuthorityIssuer, AuthorityLaneRoots,
    AuthorityOperationKind, AuthorityReceipt, AuthorityReceiptSelector,
};
use vos::agent_sdk::contract::RuntimePackageContract;
use vos::agent_sdk::package::{
    AgentRuntimePackageManifest, PackageArtifact, PackageEnvelope, PackageManifest,
    PackageSigning,
};
use vos::agent_sdk::wire::CanonicalWire as _;
use vos::agent_sdk::{
    ActorDirectoryRecord, ActorEntry, ActorId, AgentDescriptor, AgentId, AgentIdentity,
    AgentProfile, AgentReplica, BlobRef, DeploymentId, Hash, InstallationId,
    InvocationAuthorization, InvocationId, InvocationStatus, InvocationWork, ManagementReply,
    ManagementRequest, MethodMode, NodeId, PrincipalId, ProducerId, ProgramId, ReplicaRole,
    RuntimeBlob, RuntimeCapabilities, RuntimeExecutionContext, RuntimeOutcome, RuntimeState,
    RuntimeTransition, RuntimeWork, SpaceId,
};
use vos_pvm::ExitReason;
use vos_pvm::refine::{MemoryModel, PreparedProgram};
#[cfg(vos_backends)]
use vos_pvm::refine::RefineBackend;
use vos_pvm::refine_host::RefineContext;

const RUNTIME_PVM_PATH: &str = "/vos/vosx/blobs/agent_runtime.pvm";

fn runtime_pvm_path() -> String {
    std::env::var("VOS_RUNTIME_PVM").unwrap_or_else(|_| RUNTIME_PVM_PATH.to_string())
}

/// F-025: sequenced Linear identities (proposal 0001). Built by hand so this
/// harness still compiles against a VOS without the SDK type.
fn sequenced() -> bool {
    std::env::var("F002_SEQUENCED").is_ok_and(|v| v == "1")
}

fn sequenced_linear_id(sequence: u64) -> [u8; 32] {
    let mut id = [0u8; 32];
    id[..16].copy_from_slice(b"vos/sequenced/v1");
    id[16] = 1; // Linear result component
    id[24..].copy_from_slice(&sequence.to_be_bytes());
    id
}
/// The Local host's outer budget: `DEFAULT_MANAGEMENT_GAS`, plus the work's
/// own gas for an Invoke (`vos/src/agent/local_journal_driver.rs:1935-1937`).
const OUTER_GAS: u64 = vos::agent::driver::DEFAULT_MANAGEMENT_GAS;
const ACTOR_GAS: u64 = vos::agent::execution::MAX_EXECUTION_GAS;
const PACKAGE_SEED: [u8; 32] = [0x71; 32];
const AUTHORITY_SEED: [u8; 32] = [0x72; 32];

/// What the host records around one runtime execution.
#[derive(Clone, Copy, Debug, Default)]
struct Exec {
    encode_us: u64,
    preparation_us: u64,
    execution_us: u64,
    decode_us: u64,
    elapsed_us: u64,
    input_bytes: u64,
    output_bytes: u64,
    gas_used: u64,
}

fn sign_package(mut package: PackageEnvelope) -> PackageEnvelope {
    let key = SigningKey::from_bytes(&PACKAGE_SEED);
    let bytes = package.signing_bytes().expect("signing bytes");
    package.manifest.signing_mut().signature = key.sign(&bytes).to_bytes();
    package
}

fn runtime_package(pvm: &[u8]) -> AdmittedRuntimePackage {
    let key = SigningKey::from_bytes(&PACKAGE_SEED);
    let public_key = key.verifying_key().to_bytes();
    let envelope = sign_package(PackageEnvelope {
        manifest: PackageManifest::AgentRuntime(AgentRuntimePackageManifest {
            name: "standard-local-runtime".into(),
            outer_program: BlobRef::of_bytes(pvm),
            contract: RuntimePackageContract::canonical(),
            capabilities: RuntimeCapabilities::standard(),
            signing: PackageSigning {
                producer: ProducerId::of_public_key(&public_key),
                public_key,
                signature: [0; sdk::package::PACKAGE_SIGNATURE_BYTES],
            },
        }),
        artifacts: vec![PackageArtifact {
            identity: BlobRef::of_bytes(pvm),
            bytes: pvm.to_vec(),
        }],
    });
    admit_runtime_package(&envelope.encode().expect("encode")).expect("admit runtime")
}

struct Engine {
    pvm: Vec<u8>,
    prepared: PreparedProgram,
    descriptor: AgentDescriptor,
    authority: SigningKey,
    state: RuntimeState,
    decision: u64,
    slot: u64,
    next_invocation: u64,
    /// F-025 `profile`: the output path for the next Invoke's gas profile.
    profile: Option<String>,
    /// F-025 `compare`: a second runtime every execution is checked against.
    compare: Option<PreparedProgram>,
    /// F-025 change 5: the same runtime on the other engine, in lockstep.
    lockstep: Option<PreparedProgram>,
}

/// F-025 change 5: the engine `F025_BACKEND` selects (the interpreter unless
/// it says `recompiler`).
#[cfg(vos_backends)]
fn backend() -> RefineBackend {
    match std::env::var("F025_BACKEND").as_deref() {
        Ok("recompiler") => RefineBackend::Recompiler,
        Ok("interpreter") | Err(_) => RefineBackend::Interpreter,
        Ok(other) => panic!("F025_BACKEND={other}: interpreter or recompiler"),
    }
}

fn prepare(pvm: &[u8]) -> PreparedProgram {
    #[cfg(vos_backends)]
    return PreparedProgram::with_backend(pvm, backend()).expect("prepare runtime");
    #[cfg(not(vos_backends))]
    PreparedProgram::new(pvm).expect("prepare runtime")
}

fn prepare_lockstep(pvm: &[u8]) -> Option<PreparedProgram> {
    if !std::env::var("F025_LOCKSTEP").is_ok_and(|v| v == "1") {
        return None;
    }
    #[cfg(vos_backends)]
    {
        let other = match backend() {
            RefineBackend::Interpreter => RefineBackend::Recompiler,
            RefineBackend::Recompiler => RefineBackend::Interpreter,
        };
        Some(PreparedProgram::with_backend(pvm, other).expect("prepare the other engine"))
    }
    #[cfg(not(vos_backends))]
    panic!("F025_LOCKSTEP needs a VOS with RefineBackend")
}

/// Everything a caller observes of one execution, to compare engines.
fn observe(inv: &vos_pvm::refine::Invocation) -> (ExitReason, u32, u64, [u64; 13], Option<Vec<u8>>) {
    (
        inv.exit.clone(),
        inv.pc,
        inv.gas_used,
        inv.registers,
        inv.output_bounded(RuntimeTransition::MAX_ENCODED_BYTES),
    )
}

static LOCKSTEP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

static COMPARED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Engine {
    fn new(discriminator: u8) -> Self {
        let pvm = std::fs::read(runtime_pvm_path()).expect("AgentRuntime program");
        println!("runtime {} ({} B)", runtime_pvm_path(), pvm.len());
        let prepared = prepare(&pvm);
        let lockstep = prepare_lockstep(&pvm);
        let runtime = runtime_package(&pvm);
        let authority = SigningKey::from_bytes(&AUTHORITY_SEED);
        let space = SpaceId([0x11; 32]);
        let owner = PrincipalId([discriminator; 32]);
        let creation_nonce = Hash([discriminator.wrapping_add(0x30); 32]);
        let agent = AgentId::derive(space, owner, creation_nonce.as_bytes());
        let public_key = authority.verifying_key().to_bytes();
        let descriptor = AgentDescriptor {
            identity: AgentIdentity {
                space,
                agent,
                owner,
                profile: AgentProfile::Local,
                runtime_deployment: runtime.deployment(),
                runtime_program: runtime.program(),
                runtime_producer: runtime.producer(),
                transition_producer: ProducerId([discriminator.wrapping_add(0x60); 32]),
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
        descriptor.validate().expect("valid descriptor");
        let mut e = Self {
            pvm,
            prepared,
            descriptor,
            authority,
            state: RuntimeState::default(),
            decision: 0,
            slot: 1,
            next_invocation: 1,
            profile: None,
            compare: std::env::var("F025_COMPARE_RUNTIME")
                .ok()
                .map(|path| PreparedProgram::new(&std::fs::read(path).expect("compare runtime")).expect("prepare")),
            lockstep,
        };
        let request = ManagementRequest::Create(Box::new(e.descriptor.clone()));
        let (t, _) = e.manage(request, true);
        assert!(
            matches!(t.outcome, RuntimeOutcome::Management(Ok(ManagementReply::Created(_)))),
            "create: {:?}",
            t.outcome
        );
        e
    }

    fn exec_with(&self, prepared: &PreparedProgram, work: &RuntimeWork) -> (Vec<u8>, u64) {
        let gas = match work {
            RuntimeWork::Invoke { invocation, .. } => OUTER_GAS.saturating_add(invocation.gas),
            _ => OUTER_GAS,
        };
        let input = work.encode().expect("encode RuntimeWork");
        let inv = RefineContext::load_prepared(prepared, &input, gas, MemoryModel::Auto)
            .expect("load runtime")
            .run();
        assert_eq!(inv.exit, ExitReason::Halt, "runtime exit at pc {}", inv.pc);
        (inv.output_bounded(RuntimeTransition::MAX_ENCODED_BYTES).expect("output"), inv.gas_used)
    }

    fn exec(&self, work: &RuntimeWork) -> (RuntimeTransition, Exec) {
        if let Some(other) = &self.compare {
            // F-025 `compare`: every execution also runs on the other
            // runtime from the same input; the transitions must be identical.
            let (mine, _) = self.exec_with(&self.prepared, work);
            let (theirs, _) = self.exec_with(other, work);
            assert_eq!(mine, theirs, "the two runtimes' transitions differ");
            COMPARED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let gas = match work {
            RuntimeWork::Invoke { invocation, .. } => OUTER_GAS.saturating_add(invocation.gas),
            _ => OUTER_GAS,
        };
        let t0 = Instant::now();
        let input = work.encode().expect("encode RuntimeWork");
        let encode_us = t0.elapsed().as_micros() as u64;
        let started = Instant::now();
        let ctx = RefineContext::load_prepared(&self.prepared, &input, gas, MemoryModel::Auto)
            .expect("load runtime");
        let preparation_us = started.elapsed().as_micros() as u64;
        let exec_started = Instant::now();
        let inv = ctx.run();
        let execution_us = exec_started.elapsed().as_micros() as u64;
        let elapsed_us = started.elapsed().as_micros() as u64;
        assert_eq!(
            inv.exit,
            ExitReason::Halt,
            "runtime exit at pc {} after {} gas, input {} B, Agent state {} B",
            inv.pc,
            inv.gas_used,
            input.len(),
            self.state_bytes()
        );
        if let Some(other) = &self.lockstep {
            let theirs = RefineContext::load_prepared(other, &input, gas, MemoryModel::Auto)
                .expect("load runtime on the other engine")
                .run();
            assert_eq!(observe(&theirs), observe(&inv), "the two engines diverge");
            LOCKSTEP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let t1 = Instant::now();
        let out = inv
            .output_bounded(RuntimeTransition::MAX_ENCODED_BYTES)
            .expect("output window");
        let transition = RuntimeTransition::decode(&out).expect("decode transition");
        let decode_us = t1.elapsed().as_micros() as u64;
        (
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
        )
    }

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
        r.signature = self.authority.sign(&r.signing_bytes()).to_bytes();
        r
    }

    fn manage(&mut self, request: ManagementRequest, authorised: bool) -> (RuntimeTransition, Exec) {
        let authority = authorised.then(|| {
            self.decision += 1;
            self.slot += 1;
            let (op, actor, dep) = match &request {
                ManagementRequest::Create(_) => (AuthorityOperationKind::CreateAgent, None, None),
                ManagementRequest::Install(i) => (
                    AuthorityOperationKind::InstallActor,
                    Some(i.entry.actor),
                    Some(i.entry.deployment),
                ),
                other => panic!("unsupported management request {other:?}"),
            };
            Box::new(self.receipt(
                op,
                self.descriptor.identity.runtime_deployment,
                actor,
                dep,
                self.decision,
                request.commitment(),
            ))
        });
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
        let (t, x) = self.exec(&work);
        if authorised {
            self.state = t.state.clone();
        }
        (t, x)
    }

    fn install(&mut self, package: &AdmittedActorPackage, name: &str) -> Actor {
        let schema = sdk::schema::decode(package.state_lane_schema_bytes()).expect("schema");
        let actor = ActorId::top_level(self.descriptor.identity.agent, name);
        let entry = ActorEntry {
            actor,
            name: name.into(),
            parent: None,
            deployment: package.deployment(),
            program: package.program(),
            package: package.package_ref().clone(),
            agent_schema: package.manifest().state_lane_schema.clone(),
            method_policy: package.manifest().method_policy.clone(),
            constructor_abi: schema.constructor_abi().expect("constructor ABI"),
            installation_data: None,
            state_layout: schema.state_layout_hash().expect("state layout"),
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
            constructor_abi: schema.constructor_abi().expect("constructor ABI"),
            installation_data: None,
            state_layout: schema.state_layout_hash().expect("state layout"),
            contract: package.manifest().contract,
            requirements: package.requirements(),
        }));
        let (t, _) = self.manage(request, true);
        assert!(
            matches!(t.outcome, RuntimeOutcome::Management(Ok(ManagementReply::Installed(_)))),
            "install: {:?}",
            t.outcome
        );
        let (t, _) = self.manage(
            ManagementRequest::InspectActors {
                after: None,
                limit: 4,
            },
            false,
        );
        let RuntimeOutcome::Management(Ok(ManagementReply::Actors(page))) = t.outcome else {
            panic!("inspect: {:?}", t.outcome)
        };
        let record = page.entries[0].clone();
        let mut availability = vec![
            RuntimeBlob {
                reference: BlobRef::of_bytes(package.program_bytes()),
                bytes: package.program_bytes().to_vec(),
            },
            RuntimeBlob {
                reference: BlobRef::of_bytes(package.state_lane_schema_bytes()),
                bytes: package.state_lane_schema_bytes().to_vec(),
            },
            RuntimeBlob {
                reference: BlobRef::of_bytes(package.method_policy_bytes()),
                bytes: package.method_policy_bytes().to_vec(),
            },
        ];
        availability.sort_unstable_by(|a, b| a.reference.cmp(&b.reference));
        Actor {
            record,
            availability,
        }
    }

    /// One invocation: Invoke, then Acknowledge. Returns the reply, the
    /// actor's gas, and both executions.
    fn invoke(&mut self, actor: &Actor, message: Vec<u8>) -> Result<(Vec<u8>, u64, Exec, Exec), String> {
        self.invoke_ack(actor, message, true)
    }

    /// The next Invoke of `actor` on the current state, and its parts.
    fn next_invoke(
        &mut self,
        actor: &Actor,
        message: Vec<u8>,
    ) -> (InvocationWork, AuthorityReceipt, RuntimeWork) {
        self.next_invoke_with_gas(actor, message, ACTOR_GAS)
    }

    fn next_invoke_with_gas(
        &mut self,
        actor: &Actor,
        message: Vec<u8>,
        actor_gas: u64,
    ) -> (InvocationWork, AuthorityReceipt, RuntimeWork) {
        self.slot += 1;
        let id = self.next_invocation;
        self.next_invocation += 1;
        let mut inv = [0u8; 32];
        inv[..8].copy_from_slice(&id.to_be_bytes());
        inv[31] = 0x5a;
        if sequenced() {
            inv = sequenced_linear_id(id);
        }
        let work = InvocationWork {
            space: self.descriptor.identity.space,
            agent: self.descriptor.identity.agent,
            runtime_deployment: self.descriptor.identity.runtime_deployment,
            invocation: InvocationId(inv),
            actor: actor.record.entry.actor,
            incarnation: actor.record.incarnation,
            deployment: actor.record.entry.deployment,
            program: actor.record.entry.program,
            mode: MethodMode::Linear,
            origin: sdk::InvocationOrigin::anonymous(),
            roles: sdk::InvocationRoleClaims::none(),
            message,
            installation_data: None,
            availability: actor.availability.clone(),
            gas: actor_gas,
            recovery_only: false,
        };
        let receipt = self.receipt(
            AuthorityOperationKind::InvokeActor,
            work.runtime_deployment,
            Some(work.actor),
            Some(work.deployment),
            0,
            work.commitment(),
        );
        let invoke_work = RuntimeWork::Invoke {
            context: RuntimeExecutionContext::Direct,
            state: self.state.clone(),
            invocation: Box::new(work.clone()),
            authorization: Box::new(InvocationAuthorization::AuthorityReceipt(receipt.clone())),
            observed_slot: self.slot,
        };
        (work, receipt, invoke_work)
    }

    /// F-025 change 5: the next Invoke's work with the actor's gas budget,
    /// without executing it (its identity is not reused).
    fn invoke_work(&mut self, actor: &Actor, message: Vec<u8>, actor_gas: u64) -> RuntimeWork {
        self.next_invoke_with_gas(actor, message, actor_gas).2
    }

    /// Invoke, and acknowledge only when `ack` (an unacknowledged result
    /// stays retained: at most 32 per lane).
    fn invoke_ack(
        &mut self,
        actor: &Actor,
        message: Vec<u8>,
        ack: bool,
    ) -> Result<(Vec<u8>, u64, Exec, Exec), String> {
        let (work, receipt, invoke_work) = self.next_invoke(actor, message);
        if let Some(profile) = self.profile.take() {
            profile_invoke(&self.prepared, &invoke_work, &profile);
        }
        let (t, invoke) = self.exec(&invoke_work);
        let (reply, gas) = match &t.outcome {
            RuntimeOutcome::Completed(Ok(r)) if r.status == InvocationStatus::Done => {
                (r.reply.clone(), ACTOR_GAS.saturating_sub(r.gas_remaining))
            }
            other => return Err(format!("{other:?}")),
        };
        self.state = t.state;
        if !ack {
            return Ok((reply, gas, invoke, Exec::default()));
        }
        let (t, ack) = self.exec(&RuntimeWork::Acknowledge {
            context: RuntimeExecutionContext::Direct,
            state: self.state.clone(),
            invocation: Box::new(sdk::InvocationRetirement::from_work(&work)),
            authorization: Box::new(InvocationAuthorization::AuthorityReceipt(receipt)),
        });
        if !matches!(t.outcome, RuntimeOutcome::Acknowledged(Ok(_))) {
            return Err(format!("ack: {:?}", t.outcome));
        }
        self.state = t.state;
        Ok((reply, gas, invoke, ack))
    }

    /// Invoke, then try to acknowledge; `Ok(false)` when the acknowledgement
    /// is refused (the invocation's result then stays retained).
    fn invoke_no_ack_check(&mut self, actor: &Actor, message: Vec<u8>) -> Result<bool, String> {
        match self.invoke(actor, message) {
            Ok(_) => Ok(true),
            Err(e) if e.starts_with("ack:") => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn state_bytes(&self) -> usize {
        self.state.control.len() + self.state.linear.len() + self.state.merge.len() + self.state.local.len()
    }
}

struct Actor {
    record: ActorDirectoryRecord,
    availability: Vec<RuntimeBlob>,
}

fn msg(name: &str, args: vos::value::Msg) -> Vec<u8> {
    let _ = name;
    let mut m = vec![vos::value::TAG_DYNAMIC];
    m.extend_from_slice(&args.encode());
    m
}

fn load_package(path: &str) -> AdmittedActorPackage {
    admit_actor_package(&std::fs::read(path).expect("package")).expect("admit actor package")
}

struct Csv(std::io::BufWriter<std::fs::File>);

impl Csv {
    fn create(path: &str) -> Self {
        let mut w = std::io::BufWriter::new(std::fs::File::create(path).expect("csv"));
        writeln!(w, "spike,case,variant,size,sample,metric,value,unit").expect("csv");
        Self(w)
    }
    fn row(&mut self, spike: &str, case: &str, variant: &str, size: u64, sample: usize, metric: &str, value: u64, unit: &str) {
        writeln!(self.0, "{spike},{case},{variant},{size},{sample},{metric},{value},{unit}").expect("csv");
    }
    fn exec(&mut self, spike: &str, case: &str, variant: &str, size: u64, sample: usize, x: &Exec) {
        self.row(spike, case, variant, size, sample, "encode_us", x.encode_us, "us");
        self.row(spike, case, variant, size, sample, "preparation_us", x.preparation_us, "us");
        self.row(spike, case, variant, size, sample, "execution_us", x.execution_us, "us");
        self.row(spike, case, variant, size, sample, "decode_us", x.decode_us, "us");
        self.row(spike, case, variant, size, sample, "elapsed_us", x.elapsed_us, "us");
        self.row(spike, case, variant, size, sample, "input_bytes", x.input_bytes, "bytes");
        self.row(spike, case, variant, size, sample, "output_bytes", x.output_bytes, "bytes");
        self.row(spike, case, variant, size, sample, "gas_used", x.gas_used, "gas");
    }
}

fn pct(v: &mut [u64], p: usize) -> u64 {
    v.sort_unstable();
    let rank = (p * v.len()).div_ceil(100).max(1);
    v[rank - 1]
}

fn summary(label: &str, v: &[u64]) {
    let mut s = v.to_vec();
    let (p50, p95, p99) = (pct(&mut s, 50), pct(&mut s, 95), pct(&mut s, 99));
    println!("{label}: n={} p50={p50} p95={p95} p99={p99} max={}", v.len(), s.last().unwrap());
}

/// Positive acknowledgements a Local Agent retains per result lane before it
/// refuses the next one (`vos/src/agent/standard.rs:23-29`, `:4493-4501`).
/// The harness restores the post-setup state every `WINDOW` invocations so
/// that a long series stays inside that cap; see `retention`.
const WINDOW: usize = 30;

/// How far one Agent gets on sequential Linear invocations with no reset:
/// the invocation at which the acknowledgement is first refused, and the one
/// at which the invocation itself is first refused.
fn retention(package: &str, n: usize) {
    let pkg = load_package(package);
    let mut e = Engine::new(0x41);
    let actor = e.install(&pkg, "state-blob");
    let m = msg("fill", vos::value::Msg::new("fill").with("row", 0u32).with("len", 1024u32));
    e.invoke(&actor, m).expect("fill");
    let mut first_ack_refusal = None;
    let started = Instant::now();
    for i in 1..=n {
        if i % 1000 == 0 {
            println!(
                "retention: {i} invocations acknowledged, Agent state {} B, {:.1} s",
                e.state_bytes(),
                started.elapsed().as_secs_f64()
            );
        }
        let m = msg("touch", vos::value::Msg::new("touch").with("row", 0u32).with("at", 0u32));
        match e.invoke_no_ack_check(&actor, m) {
            Ok(acked) => {
                if !acked && first_ack_refusal.is_none() {
                    first_ack_refusal = Some(i + 1);
                    println!("retention: acknowledgement {} refused (ResultCapacity)", i + 1);
                }
            }
            Err(err) => {
                println!("retention: invocation {} refused: {err}", i + 1);
                return;
            }
        }
    }
    println!("retention: {n} invocations accepted, first refused ack {first_ack_refusal:?}");
}

/// S4: invocation cost against state size.
fn s4(package: &str, sizes: &str, n: usize, out: &str) {
    const ROW: u32 = 32 * 1024;
    let pkg = load_package(package);
    let mut csv = Csv::create(out);
    for size in sizes.split(',').map(|s| s.parse::<u64>().expect("size")) {
        let mut e = Engine::new(0x40);
        let actor = e.install(&pkg, "state-blob");
        // Whole 32 KiB rows, two per invocation (four panic the actor: one
        // invocation's writes are bounded). The standard runtime retains at
        // most 32 acknowledgements and 32 unacknowledged results per lane and
        // never evicts either, so past 31 fills the first ones are
        // acknowledged and the rest left as results; the measuring window is
        // what acknowledgement capacity is left.
        let fill_rows: u32 = std::env::var("FILL_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(2);
        let rows = (size / u64::from(ROW)).max(1) as u32;
        let fills_needed = rows.div_ceil(fill_rows) as usize;
        let acked = fills_needed.saturating_sub(31);
        let mut filled = 0u32;
        let mut fills = 0usize;
        while filled < rows {
            let count = fill_rows.min(rows - filled);
            let m = msg(
                "fill_many",
                vos::value::Msg::new("fill_many")
                    .with("first", filled)
                    .with("count", count)
                    .with("len", ROW),
            );
            let ack = fills < acked || fills_needed <= 31 || sequenced();
            match e.invoke_ack(&actor, m, ack) {
                Ok(_) => {
                    filled += count;
                    fills += 1;
                }
                Err(err) => {
                    println!("s4 size {size}: fill stopped at row {filled}: {err}");
                    break;
                }
            }
        }
        let acks_used = if fills_needed <= 31 || sequenced() { fills } else { acked };
        let window = if sequenced() {
            usize::MAX
        } else {
            32usize.saturating_sub(acks_used).max(1)
        };
        println!("s4 size {size}: {fills} fill invocations ({acks_used} acknowledged); reset window {window}");
        // Control (MEASUREMENTS-F002 §11): F002_RETAIN=k leaves k more
        // unacknowledged results retained before measuring, without adding
        // rows, to separate retained results from state bytes.
        let retain: usize = std::env::var("F002_RETAIN").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
        let retained_before = fills - acks_used.min(fills);
        let extra = retain.min(31usize.saturating_sub(retained_before));
        for _ in 0..extra {
            let m = msg("touch", vos::value::Msg::new("touch").with("row", 0u32).with("at", 0u32));
            e.invoke_ack(&actor, m, false).expect("retain");
        }
        println!("s4 size {size}: {} unacknowledged results retained", retained_before + extra);
        csv.row("s4", "state", "filled", size, 0, "retained_results", (retained_before + extra) as u64, "count");
        let state = e.state_bytes() as u64;
        println!("s4 size {size}: {filled}/{rows} rows, Agent state {state} B");
        let snapshot = e.state.clone();
        csv.row("s4", "state", "filled", size, 0, "state_bytes", state, "bytes");
        let mut e2e = Vec::new();
        let mut inv_exec = Vec::new();
        let mut ack_exec = Vec::new();
        let mut inputs = Vec::new();
        let mut gas = Vec::new();
        for i in 0..(n + 10) {
            if i % window == 0 {
                e.state = snapshot.clone();
            }
            let m = msg(
                "touch",
                vos::value::Msg::new("touch")
                    .with("row", (i as u32) % filled.max(1))
                    .with("at", ((i as u32) * 64) % ROW),
            );
            let t = Instant::now();
            let (_, _, a, b) = e.invoke(&actor, m).expect("touch");
            let total = t.elapsed().as_micros() as u64;
            if i >= 10 {
                let k = i - 10;
                csv.row("s4", "touch", "caller", state, k, "e2e_us", total, "us");
                csv.exec("s4", "touch", "invoke", state, k, &a);
                csv.exec("s4", "touch", "acknowledge", state, k, &b);
                e2e.push(total);
                inv_exec.push(a.elapsed_us);
                ack_exec.push(b.elapsed_us);
                inputs.push(a.input_bytes);
                gas.push(a.gas_used);
            }
        }
        println!("== s4 state {state} B ==");
        summary("  caller e2e us (invoke + ack, in-process)", &e2e);
        summary("  invoke elapsed_us", &inv_exec);
        summary("  ack elapsed_us", &ack_exec);
        summary("  invoke input_bytes", &inputs);
        summary("  invoke gas_used", &gas);
    }
}

/// S2 / S3 guest: run the kernel probe's `apply` or `verify` on each input.
fn kernel(spike: &str, package: &str, n: usize, n_noop: usize, out: &str, inputs: &[String]) {
    let pkg = load_package(package);
    let mut csv = Csv::create(out);
    let mut e = Engine::new(0x50);
    let actor = e.install(&pkg, "kernel-probe");
    let snapshot = e.state.clone();
    for path in inputs {
        let bytes = std::fs::read(path).expect("input");
        let case = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("case")
            .to_string();
        let build = |bytes: &[u8]| -> Vec<u8> {
            if spike == "s2" {
                msg("apply", vos::value::Msg::new("apply").with("case", bytes.to_vec()))
            } else {
                let roots_path = path.replace(".bin", ".roots");
                let text = std::fs::read_to_string(&roots_path).expect("roots");
                let mut roots = Vec::new();
                for line in text.lines().take(2) {
                    for i in 0..32 {
                        roots.push(u8::from_str_radix(&line[2 * i..2 * i + 2], 16).expect("hex"));
                    }
                }
                msg(
                    "verify",
                    vos::value::Msg::new("verify")
                        .with("witness", bytes.to_vec())
                        .with("roots", roots),
                )
            }
        };
        let m = build(&bytes);
        println!("{spike} {case}: input {} B, message {} B (limit {} B)", bytes.len(), m.len(), sdk::MAX_INVOCATION_MESSAGE_BYTES);
        csv.row(spike, &case, "message", bytes.len() as u64, 0, "message_bytes", m.len() as u64, "bytes");
        let noop = msg("noop", vos::value::Msg::new("noop").with("payload", vec![0u8; bytes.len()]));
        let mut ok_all = true;
        let mut series: Vec<(Vec<u64>, Vec<u64>, Vec<u64>)> = Vec::new();
        for (variant, message, n) in [("kernel", m.clone(), n), ("noop", noop, n_noop)] {
            if n == 0 {
                continue;
            }
            let mut e2e = Vec::new();
            let mut exec = Vec::new();
            let mut actor_gas = Vec::new();
            for i in 0..(n + 5) {
                if i % WINDOW == 0 && !sequenced() {
                    e.state = snapshot.clone();
                }
                let t = Instant::now();
                match e.invoke(&actor, message.clone()) {
                    Ok((reply, gas, a, b)) => {
                        let total = t.elapsed().as_micros() as u64;
                        if variant == "kernel" && i == 0 {
                            println!("  reply bytes {:02x?}", &reply[..reply.len().min(48)]);
                        }
                        if i >= 5 {
                            let k = i - 5;
                            csv.row(spike, &case, variant, bytes.len() as u64, k, "e2e_us", total, "us");
                            csv.row(spike, &case, variant, bytes.len() as u64, k, "actor_gas", gas, "gas");
                            csv.exec(spike, &case, &format!("{variant}/invoke"), bytes.len() as u64, k, &a);
                            csv.exec(spike, &case, &format!("{variant}/acknowledge"), bytes.len() as u64, k, &b);
                            e2e.push(total);
                            exec.push(a.elapsed_us);
                            actor_gas.push(gas);
                        }
                    }
                    Err(err) => {
                        println!("  {variant} failed: {err}");
                        ok_all = false;
                        break;
                    }
                }
            }
            if !e2e.is_empty() {
                println!("== {spike} {case} {variant} ==");
                summary("  caller e2e us (invoke + ack)", &e2e);
                summary("  invoke elapsed_us", &exec);
                summary("  actor gas", &actor_gas);
            }
            series.push((e2e, exec, actor_gas));
        }
        if !ok_all {
            println!("  {case}: not measured (see failure above)");
        }
    }
}

fn hex32(h: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&h[2 * i..2 * i + 2], 16).expect("hex");
    }
    out
}

/// S4 through `vosx`: write `count` canonical ATQ1 intents for the Local
/// Agent's state-blob actor, one file each, for `vosx space invoke-local
/// --intent`. The origin is the operator's own principal and credential,
/// derived from the public half of `identity.key` (libp2p protobuf Ed25519:
/// `08 01 12 40 ‖ secret ‖ public`) as vosx derives them.
fn atq1(identity: &str, space: &str, agent: &str, kind: &str, rows: u32, start: u64, count: u64, out: &str) {
    use vos::agent::supervisor::AgentRouteKey;
    use vos::agent::supervisor_adapters::{AgentInvocationIntent, AgentTargetedPreparationRequest};
    let key = std::fs::read(identity).expect("identity");
    assert_eq!(&key[..4], &[0x08, 0x01, 0x12, 0x40], "libp2p Ed25519 encoding");
    let mut public = [0u8; 32];
    public.copy_from_slice(&key[36..68]);
    let principal = PrincipalId::of_public_key(&public);
    let credential = sdk::CredentialId::of_public_key(&public);
    let agent = AgentId(hex32(agent));
    let actor = ActorId::top_level(agent, "state-blob");
    std::fs::create_dir_all(out).expect("mkdir");
    for i in start..start + count {
        let call = if kind == "fill" {
            vos::value::Msg::new("fill").with("row", i as u32).with("len", 32u32 * 1024)
        } else {
            vos::value::Msg::new("touch")
                .with("row", (i as u32) % rows.max(1))
                .with("at", ((i as u32) * 64) % (32 * 1024))
        };
        let mut message = vec![vos::value::TAG_DYNAMIC];
        message.extend_from_slice(&call.encode());
        let mut id = [0u8; 32];
        id[..8].copy_from_slice(&(i + 1).to_be_bytes());
        id[8] = if kind == "fill" { 0xF1 } else { 0x70 };
        let intent = AgentTargetedPreparationRequest::new(
            AgentRouteKey::new(SpaceId(hex32(space)), agent, actor).expect("route"),
            AgentInvocationIntent::new(
                InvocationId(id),
                MethodMode::Linear,
                sdk::InvocationOrigin {
                    principal: Some(principal),
                    credential: Some(credential),
                    ..sdk::InvocationOrigin::anonymous()
                },
                sdk::InvocationRoleClaims::none(),
                message,
                vos::agent::execution::MAX_EXECUTION_GAS,
                false,
            )
            .expect("intent"),
        )
        .expect("request");
        std::fs::write(format!("{out}/{kind}-{i:05}.atq1"), intent.encode().expect("encode")).expect("write");
    }
}

/// F-025: interpreted gas of each curve precompile operation, from the
/// `curve-bench` guest (spikes/guest/curve-bench), as the op minus the empty
/// loop over `n` iterations. Linked like the AgentRuntime guest.
fn curve_bench(elf: &str, n: u32) {
    use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
    use curve25519_dalek::scalar::Scalar;
    let pvm = vos_pvm_compiler::link_elf_spi(&std::fs::read(elf).expect("ELF")).expect("link");
    let a = Scalar::from(5u64).to_bytes();
    let b = (Scalar::from(3u64) * RISTRETTO_BASEPOINT_POINT).compress().to_bytes();
    let c = (Scalar::from(7u64) * RISTRETTO_BASEPOINT_POINT).compress().to_bytes();
    let gas = |id: u8, count: u32| {
        let mut input = vec![id];
        input.extend_from_slice(&count.to_le_bytes());
        input.extend_from_slice(&a);
        input.extend_from_slice(&b);
        input.extend_from_slice(&c);
        input.extend_from_slice(&[0xa5; 64]);
        let run = RefineContext::load(&pvm, &input, 100_000_000_000).expect("load").run();
        assert_eq!(run.exit, ExitReason::Halt, "op {id} exit at pc {}", run.pc);
        run.gas_used
    };
    let empty = gas(0, n);
    for (id, name) in [
        (110, "ristretto_scalar_mult"),
        (111, "ristretto_point_add"),
        (112, "scalar_from_bytes_mod_order_wide"),
        (113, "scalar_mul_mod_l"),
        (114, "scalar_add_mod_l"),
    ] {
        let per_op = (gas(id, n) - empty) / u64::from(n);
        println!("curve-bench ecall {id} {name}: {per_op} gas per operation (n={n})");
    }
}

/// F-025 (`T-025-03`): where one Invoke's runtime gas goes. Runs the same
/// Invoke through `RefineContext::run_observed` and attributes the outer
/// runtime's gas to the PVM program counter that charged it (block starts),
/// and host-call gas to its call. Writes `pc<TAB>gas` rows, heaviest first,
/// for VOS's `resolve_observed_standard_program_pcs` diagnostic, and prints
/// the split around the inner-machine calls (9 `machine`, 11 `poke`,
/// 12 `pages`, 13 `invoke`, 14 `expunge`).
fn profile_invoke(prepared: &PreparedProgram, work: &RuntimeWork, out: &str) {
    use std::collections::{BTreeMap, HashMap};
    use vos_pvm::refine_host::{RefineHostPhase, RefineMachineIdentity, RefineObservation};
    let RuntimeWork::Invoke { invocation, .. } = work else { panic!("profile needs an Invoke") };
    let gas = OUTER_GAS.saturating_add(invocation.gas);
    let input = work.encode().expect("encode RuntimeWork");
    let ctx = RefineContext::load_prepared(prepared, &input, gas, MemoryModel::Auto).expect("load");
    let mut per_pc: HashMap<u32, u64> = HashMap::new();
    let mut calls: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
    let mut segments: Vec<(u64, u64)> = Vec::new(); // (call that ends the segment, outer gas in it)
    let mut segment = 0u64;
    let mut inner_gas = 0u64;
    let mut before_call = 0u64;
    let inv = ctx.run_observed(|event| match event {
        RefineObservation::Instruction { identity, instruction } => {
            let charged = instruction.gas_before.saturating_sub(instruction.machine_after.gas);
            match identity {
                RefineMachineIdentity::Outer => {
                    *per_pc.entry(instruction.pc_before).or_default() += charged;
                    segment += charged;
                }
                RefineMachineIdentity::Inner(_) => inner_gas += charged,
            }
        }
        RefineObservation::HostCall { phase: RefineHostPhase::Before, id, outer, .. } => {
            before_call = outer.gas_remaining();
            segments.push((id, segment));
            segment = 0;
        }
        RefineObservation::HostCall { phase: RefineHostPhase::After, id, outer, .. } => {
            let e = calls.entry(id).or_default();
            e.0 += 1;
            e.1 += before_call.saturating_sub(outer.gas_remaining());
        }
        _ => {}
    });
    segments.push((u64::MAX, segment));
    assert_eq!(inv.exit, ExitReason::Halt, "runtime exit at pc {}", inv.pc);
    let attributed: u64 = per_pc.values().sum::<u64>() + calls.values().map(|c| c.1).sum::<u64>();
    println!(
        "profile: input {} B, runtime gas {} ({} attributed to instructions and calls; {} is the initial page charge), actor gas {}",
        input.len(),
        inv.gas_used,
        attributed,
        inv.gas_used.saturating_sub(attributed),
        inner_gas
    );
    for (id, (count, g)) in &calls {
        println!("profile: host call {id}: {count} calls, {g} gas");
    }
    // Before the first `machine` call, the load (machine to the first
    // invoke), the actor's own host calls (between invokes), and after.
    let first_machine = segments.iter().position(|s| s.0 == 9);
    let first_invoke = segments.iter().position(|s| s.0 == 13);
    let last_invoke = segments.iter().rposition(|s| s.0 == 13);
    if let (Some(m), Some(fi), Some(li)) = (first_machine, first_invoke, last_invoke) {
        let sum = |r: std::ops::Range<usize>| segments[r].iter().map(|s| s.1).sum::<u64>();
        println!("profile: outer gas before `machine`: {}", sum(0..m + 1));
        println!("profile: outer gas from `machine` to the first `invoke`: {}", sum(m + 1..fi + 1));
        println!("profile: outer gas between invokes (actor host calls): {}", sum(fi + 1..li + 1));
        println!("profile: outer gas after the last `invoke`: {}", sum(li + 1..segments.len()));
    }
    let mut rows: Vec<_> = per_pc.into_iter().filter(|r| r.1 > 0).collect();
    rows.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut f = std::io::BufWriter::new(std::fs::File::create(out).expect("profile out"));
    for (pc, g) in rows {
        writeln!(f, "{pc}\t{g}").expect("write");
    }
}

/// `vos-host profile <package.vos> <actor-name> <out.tsv> [payload-bytes]`:
/// one warm-up invocation, then the profiled one (`touch` for `state-blob`,
/// `noop` for `kernel-probe`).
fn profile(package: &str, name: &str, out: &str, payload: usize) {
    let pkg = load_package(package);
    let mut e = Engine::new(0x60);
    let actor = e.install(&pkg, name);
    let message = || {
        if name == "state-blob" {
            msg("touch", vos::value::Msg::new("touch").with("row", 0u32).with("at", 0u32))
        } else {
            msg("noop", vos::value::Msg::new("noop").with("payload", vec![0u8; payload]))
        }
    };
    if name == "state-blob" {
        let m = msg("fill", vos::value::Msg::new("fill").with("row", 0u32).with("len", 1024u32));
        e.invoke(&actor, m).expect("fill");
    }
    e.invoke(&actor, message()).expect("warm-up");
    e.profile = Some(out.to_string());
    let (_, gas, invoke, _) = e.invoke(&actor, message()).expect("profiled invoke");
    println!(
        "profile: program {} B; Invoke gas {} (driver), actor gas {}",
        pkg.program_bytes().len(),
        invoke.gas_used,
        gas
    );
}

/// F-025 (`T-025-03`): native cost of preparing an actor's inner machine,
/// which the host pays on every invocation outside the gas schedule:
/// `machine` (deblob) and the first memory operation (predecode).
fn prep_bench(package: &str, n: usize) {
    let pkg = load_package(package);
    let blob = pkg.program_bytes();
    let program = vos_pvm::spi::parse_standard_program(blob).expect("standard program");
    let compact = vos_pvm_program::build_compact_code_blob(&vos_pvm_program::CodeBlob {
        jump_table: program.code.jump_table.clone(),
        code: program.code.code.clone(),
        bitmask: program.code.bitmask.clone(),
    })
    .expect("compact");
    let (mut create, mut first_use) = (Vec::new(), Vec::new());
    for _ in 0..n {
        let mut machines = vos_pvm::inner::InnerMachines::new();
        let t = Instant::now();
        let id = machines.create(&compact, 0).expect("create");
        create.push(t.elapsed().as_micros() as u64);
        let t = Instant::now();
        machines
            .pages(id, 16, 1, vos_pvm::inner::PageMode::AllocateReadWrite)
            .expect("pages");
        first_use.push(t.elapsed().as_micros() as u64);
    }
    println!("prep-bench: program {} B, code {} B", blob.len(), program.code.code.len());
    summary("  machine (deblob) us", &create);
    summary("  first use (predecode) us", &first_use);
}

/// F-025 (VOS proposal 0003): grow an actor package's program to
/// `bytes` (or one byte more) by appending unreachable `trap` instructions after its
/// code (no address, jump-table entry or data moves, so behaviour is
/// unchanged), declare its resources, and re-sign it with the harness key.
/// Only built against a VOS with `ActorResources` (`run.sh` sets the cfg).
#[cfg(vos_declared)]
fn pad_package(input: &str, output: &str, bytes: usize) {
    use sdk::package::{ActorResources, PackageArtifact};
    let mut envelope = sdk::package::PackageEnvelope::decode(&std::fs::read(input).expect("package"))
        .expect("decode package");
    let program = envelope.actor_program_bytes().expect("program").to_vec();
    let mut parsed = vos_pvm_program::parse_standard_program(&program).expect("standard program");
    let base = vos_pvm_program::build_standard_program(&parsed).unwrap().len();
    assert!(bytes >= base, "target {bytes} B is below the program's {base} B");
    // Each trap is one code byte plus one bitmask bit, so the length moves
    // in steps of one or two: take the smallest padding that reaches the
    // target (at most one byte over). At the program's own size this only
    // declares and re-signs.
    let build = |extra: usize| {
        let mut code = parsed.code.clone();
        code.code.resize(parsed.code.code.len() + extra, 0);
        code.bitmask.resize(parsed.code.bitmask.len() + extra, 1);
        vos_pvm_program::build_standard_program(&vos_pvm_program::StandardProgram {
            code,
            ..parsed.clone()
        })
        .unwrap()
    };
    let padded = if bytes == base {
        program.clone()
    } else {
        let mut extra = ((bytes - base) * 8 / 9).saturating_sub(16);
        loop {
            let candidate = build(extra);
            if candidate.len() >= bytes {
                break candidate;
            }
            extra += 1;
        }
    };
    parsed = vos_pvm_program::parse_standard_program(&padded).expect("padded program parses");
    assert!(vos_pvm::spi::parse_standard_program(&padded).is_some(), "executor-valid");
    let reference = BlobRef::of_bytes(&padded);
    let sdk::package::PackageManifest::Actor(manifest) = &mut envelope.manifest else {
        panic!("not an actor package")
    };
    let old = manifest.program.clone();
    manifest.program = reference.clone();
    manifest.resources = Some(ActorResources {
        program_bytes: padded.len() as u32,
        heap_bytes: parsed.heap_pages * ActorResources::PAGE_BYTES,
        stack_bytes: parsed.stack_size,
    });
    let key = SigningKey::from_bytes(&PACKAGE_SEED);
    let public_key = key.verifying_key().to_bytes();
    manifest.signing.public_key = public_key;
    manifest.signing.producer = ProducerId::of_public_key(&public_key);
    envelope.artifacts.retain(|artifact| artifact.identity != old);
    envelope.artifacts.push(PackageArtifact { identity: reference, bytes: padded.clone() });
    envelope.artifacts.sort_unstable_by(|a, b| a.identity.cmp(&b.identity));
    let signed = sign_package(envelope);
    let encoded = signed.encode().expect("encode padded package");
    admit_actor_package(&encoded).expect("admit padded package");
    std::fs::write(output, &encoded).expect("write");
    println!(
        "pad-package: program {} B -> {} B, heap {} B, stack {} B, package {} B",
        program.len(),
        padded.len(),
        parsed.heap_pages * ActorResources::PAGE_BYTES,
        parsed.stack_size,
        encoded.len()
    );
}

/// F-025 (`T-025-03`): replay equivalence of two runtime programs. With
/// `F025_COMPARE_RUNTIME` set, every execution of any command also runs on
/// that runtime from the same input and must return the same transition
/// bytes. This command drives `n` (at most 30) Invoke and Acknowledge pairs
/// on one Agent.
fn compare(package: &str, name: &str, n: usize) {
    assert!(std::env::var("F025_COMPARE_RUNTIME").is_ok(), "set F025_COMPARE_RUNTIME");
    let pkg = load_package(package);
    let mut e = Engine::new(0x61);
    let actor = e.install(&pkg, name);
    for i in 0..n {
        let m = if name == "state-blob" {
            if i % 2 == 0 {
                msg("fill", vos::value::Msg::new("fill").with("row", (i % 8) as u32).with("len", 4096u32))
            } else {
                msg("touch", vos::value::Msg::new("touch").with("row", (i % 8) as u32).with("at", 64u32))
            }
        } else {
            msg("noop", vos::value::Msg::new("noop").with("payload", vec![i as u8; 64]))
        };
        // At most 30: the released runtime keeps 32 acknowledgements.
        e.invoke(&actor, m).expect("invoke");
    }
    println!(
        "compare: {} executions (create, install, {n} invocations and their acknowledgements) returned identical transitions on both runtimes",
        COMPARED.load(std::sync::atomic::Ordering::Relaxed)
    );
}

/// F-025 change 5: what preparing a program costs the host on each engine,
/// for the runtime and for the actor in `package` (median of `n`). The
/// interpreter predecodes; the recompiler compiles to native code. Both are
/// host work outside the gas schedule; this is what ahead-of-time
/// compilation at install would take off the first invocation.
#[cfg(vos_backends)]
fn compile_bench(package: &str, n: usize) {
    let pvm = std::fs::read(runtime_pvm_path()).expect("AgentRuntime program");
    let pkg = load_package(package);
    let actor = vos_pvm::spi::parse_standard_program(pkg.program_bytes()).expect("standard program");
    for (label, blob, code) in [
        ("runtime", &pvm[..], None),
        ("actor", pkg.program_bytes(), Some(&actor.code)),
    ] {
        for backend in [RefineBackend::Interpreter, RefineBackend::Recompiler] {
            let mut us = Vec::new();
            let mut native_bytes = 0;
            for _ in 0..n {
                let t = Instant::now();
                match (code, backend) {
                    (None, _) => {
                        let _ = PreparedProgram::with_backend(blob, backend).expect("prepare");
                    }
                    (Some(code), RefineBackend::Interpreter) => {
                        let _ = vos_pvm::Interpreter::predecode(
                            &code.code, &code.bitmask, &code.jump_table, 25, vos_pvm::IsaMode::Conformance,
                        );
                    }
                    (Some(code), RefineBackend::Recompiler) => {
                        let native = vos_pvm::recompiler::NativeProgram::compile(
                            &code.code, &code.bitmask, &code.jump_table, 25, vos_pvm::IsaMode::Conformance,
                        )
                        .expect("compile");
                        native_bytes = native.native_code_bytes().len();
                    }
                }
                us.push(t.elapsed().as_micros() as u64);
            }
            us.sort_unstable();
            println!(
                "compile-bench: {label} ({} B) {backend:?}: median {} us{}",
                blob.len(),
                us[us.len() / 2],
                if native_bytes > 0 { format!(", native code {native_bytes} B") } else { String::new() }
            );
        }
    }
}

/// F-025 change 5: one Agent through both engines in lockstep (set
/// `F025_LOCKSTEP=1`). Create, install and `n` (at most 30) Invoke and
/// Acknowledge pairs, every execution compared. Then one more Invoke, cut two
/// ways and compared at every cut: by the runtime's outer budget (evenly over
/// the whole range, and at every unit around the least budget that halts), so
/// out-of-gas lands in the runtime's own blocks and in the machine calls; and
/// by the actor's budget (evenly, and at every unit around what the actor
/// uses), so it lands inside the actor.
fn backends(package: &str, name: &str, n: usize, case: Option<&str>) {
    assert!(std::env::var("F025_LOCKSTEP").is_ok_and(|v| v == "1"), "set F025_LOCKSTEP=1");
    let pkg = load_package(package);
    let mut e = Engine::new(0x62);
    let actor = e.install(&pkg, name);
    let payload = case.map(|path| std::fs::read(path).expect("case"));
    let mut last = None;
    let mut actor_used = 0;
    for i in 0..n {
        let m = match (name, &payload) {
            ("state-blob", _) if i % 2 == 0 => {
                msg("fill", vos::value::Msg::new("fill").with("row", (i % 8) as u32).with("len", 4096u32))
            }
            ("state-blob", _) => {
                msg("touch", vos::value::Msg::new("touch").with("row", (i % 8) as u32).with("at", 64u32))
            }
            (_, Some(bytes)) => msg("apply", vos::value::Msg::new("apply").with("case", bytes.clone())),
            _ => msg("noop", vos::value::Msg::new("noop").with("payload", vec![i as u8; 64])),
        };
        last = Some(m.clone());
        actor_used = e.invoke(&actor, m).expect("invoke").1;
    }
    let executions = LOCKSTEP.load(std::sync::atomic::Ordering::Relaxed);
    let message = last.expect("n > 0");
    let other = e.lockstep.take().expect("lockstep engine");
    let mut exits = std::collections::BTreeMap::<String, usize>::new();
    let mut compare = |e: &Engine, work: &RuntimeWork, gas: u64| {
        let input = work.encode().expect("encode");
        let run = |prepared: &PreparedProgram| {
            RefineContext::load_prepared(prepared, &input, gas, MemoryModel::Auto).map(|c| observe(&c.run()))
        };
        let (mine, theirs) = (run(&e.prepared), run(&other));
        assert_eq!(mine, theirs, "the engines diverge at gas {gas}");
        let key = match &mine {
            Ok((exit, ..)) => format!("{exit:?}"),
            Err(err) => format!("load {err:?}"),
        };
        *exits.entry(key).or_default() += 1;
        mine.is_ok_and(|(exit, ..)| exit == ExitReason::Halt)
    };
    // Cut by the outer budget.
    let work = e.invoke_work(&actor, message.clone(), ACTOR_GAS);
    let full = OUTER_GAS.saturating_add(ACTOR_GAS);
    let halts = |gas: u64| {
        let input = work.encode().expect("encode");
        RefineContext::load_prepared(&e.prepared, &input, gas, MemoryModel::Auto)
            .is_ok_and(|c| c.run().exit == ExitReason::Halt)
    };
    let (mut lo, mut hi) = (0u64, full);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if halts(mid) { hi = mid } else { lo = mid }
    }
    let least = hi;
    let mut outer: std::collections::BTreeSet<u64> = (0..=128).map(|i| full / 128 * i).collect();
    outer.extend((0..=128).map(|i| least / 128 * i));
    outer.extend(least.saturating_sub(16)..=least + 2);
    for &gas in &outer {
        compare(&e, &work, gas);
    }
    // Cut by the actor's budget.
    let mut inner: std::collections::BTreeSet<u64> = (1..=128).map(|i| (actor_used / 128 * i).max(1)).collect();
    inner.extend(actor_used.saturating_sub(16).max(1)..=actor_used + 2);
    for &actor_gas in &inner {
        let work = e.invoke_work(&actor, message.clone(), actor_gas);
        compare(&e, &work, OUTER_GAS.saturating_add(actor_gas));
    }
    println!(
        "backends: {executions} executions (create, install, {n} invocations and acknowledgements) identical on both engines; \
         one more Invoke identical at {} outer budgets (least that halts: {least}) and {} actor budgets (actor uses {actor_used}): {exits:?}",
        outer.len(),
        inner.len()
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("atq1") => atq1(
            &args[2],
            &args[3],
            &args[4],
            &args[5],
            args[6].parse().expect("rows"),
            args[7].parse().expect("start"),
            args[8].parse().expect("count"),
            &args[9],
        ),
        #[cfg(vos_declared)]
        Some("pad-package") => pad_package(&args[2], &args[3], args[4].parse().expect("bytes")),
        Some("compare") => compare(&args[2], &args[3], args[4].parse().expect("n")),
        #[cfg(vos_backends)]
        Some("compile-bench") => compile_bench(&args[2], args[3].parse().expect("n")),
        Some("backends") => backends(
            &args[2],
            &args[3],
            args[4].parse().expect("n"),
            args.get(5).map(String::as_str),
        ),
        Some("prep-bench") => prep_bench(&args[2], args[3].parse().expect("n")),
        Some("profile") => profile(
            &args[2],
            &args[3],
            &args[4],
            args.get(5).map_or(0, |v| v.parse().expect("payload")),
        ),
        Some("retention") => retention(&args[2], args[3].parse().expect("n")),
        Some("curve-bench") => curve_bench(&args[2], args[3].parse().expect("n")),
        Some("runtime-pvm") => {
            let elf = std::fs::read(&args[2]).expect("runtime ELF");
            let pvm = vos_pvm_compiler::link_elf_spi(&elf).expect("link runtime ELF");
            vos_pvm::spi::validate_refine_host_calls(&pvm).expect("runtime host-call surface");
            std::fs::write(&args[3], &pvm).expect("write runtime PVM");
            println!(
                "runtime-pvm {} B, program {:02x?}",
                pvm.len(),
                &ProgramId::of_pvm(&pvm).0[..8]
            );
        }
        Some("s4") => s4(&args[2], &args[3], args[4].parse().expect("n"), &args[5]),
        Some(s @ ("s2" | "s3")) => kernel(
            s,
            &args[2],
            args[3].parse().expect("n"),
            args[4].parse().expect("n_noop"),
            &args[5],
            &args[6..],
        ),
        _ => panic!("usage: see the module documentation"),
    }
}
