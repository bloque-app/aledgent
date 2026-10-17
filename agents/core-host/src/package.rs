// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Signed actor and runtime packages, without `vosx`.
//!
//! `vosx actor build` (VOS `vosx/src/commands/build.rs`, `run_with_signer`) links an actor ELF to
//! a PVM, reads the authenticated metadata the actor macros embed, derives the AMP2 method
//! policy, the AAI1 introspection and an empty task set, and signs a VOS3 package. This module
//! does exactly that for an actor with no Task dependencies and no attested method, which is the
//! core's shape. `vosx` itself does not build on macOS and pulls libp2p for its operator key; the
//! host does not need either (`agents/README.md`).
//!
//! The runtime package is the same envelope around the standard AgentRuntime program, as the
//! Local host admits it.

use crate::keys::AgentSigner;
use vos::agent::package_admission::{
    AdmittedActorPackage, AdmittedRuntimePackage, admit_actor_package, admit_runtime_package,
};
use vos::agent_sdk as sdk;
use vos::agent_sdk::contract::RuntimePackageContract;
use vos::agent_sdk::package::{
    AgentRuntimePackageManifest, PackageArtifact, PackageEnvelope, PackageManifest, PackageSigning,
};
use vos::agent_sdk::wire::CanonicalWire as _;
use vos::agent_sdk::{BlobRef, ProducerId, RuntimeCapabilities};

/// Why a package could not be built.
#[derive(Debug)]
pub struct PackageError(pub String);

impl core::fmt::Display for PackageError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err(what: impl core::fmt::Display) -> PackageError {
    PackageError(what.to_string())
}

fn artifact(bytes: Vec<u8>) -> PackageArtifact {
    PackageArtifact {
        identity: BlobRef::of_bytes(&bytes),
        bytes,
    }
}

fn sign(
    key: &dyn AgentSigner,
    mut package: PackageEnvelope,
) -> Result<PackageEnvelope, PackageError> {
    let bytes = package.signing_bytes().map_err(err)?;
    package.manifest.signing_mut().signature = key.sign(&bytes);
    package.validate_shape().map_err(err)?;
    Ok(package)
}

/// Link a guest ELF to a canonical standard PVM, as `vosx` does.
pub fn link(elf: &[u8]) -> Result<Vec<u8>, PackageError> {
    let pvm = vos_pvm_compiler::link_elf_spi(elf).map_err(|e| err(format!("link: {e:?}")))?;
    if pvm.is_empty() || vos_pvm::spi::parse_standard_program(&pvm).is_none() {
        return Err(err("the linked program is not a canonical standard PVM"));
    }
    Ok(pvm)
}

/// The Gray Paper's Refine host calls (GP 0.8.0, eq. B.6 and appendix B.6): gas (0),
/// grow_heap (1), fetch (2), historical_lookup (7), export (8), machine (9), peek (10),
/// poke (11), pages (12), invoke (13), expunge (14). A runtime that issues only these is one
/// artifact for every Refine host: off-chain, a proof, or JAM (proposals/vos/0012).
pub const GRAY_PAPER_REFINE_CALLS: [u64; 11] = [0, 1, 2, 7, 8, 9, 10, 11, 12, 13, 14];

/// The outer host calls a linked runtime issues, sorted and without repeats.
pub fn runtime_host_calls(pvm: &[u8]) -> Result<Vec<u64>, PackageError> {
    let mut calls = vos_pvm::spi::inspect_standard_program_host_calls(pvm)
        .map_err(|e| err(format!("{e:?}")))?;
    calls.sort_unstable();
    calls.dedup();
    Ok(calls)
}

/// Link the standard AgentRuntime guest ELF built from a VOS checkout, as
/// `vosx agent-runtime-pvm` does, and check its outer host-call surface. With `gray_paper_only`
/// (`ALEDGENT_RUNTIME_GP_ONLY=1`, the core image's default) a call outside
/// [`GRAY_PAPER_REFINE_CALLS`] is refused.
pub fn link_runtime(elf: &[u8], gray_paper_only: bool) -> Result<Vec<u8>, PackageError> {
    let pvm = vos_pvm_compiler::link_elf_spi(elf).map_err(|e| err(format!("link: {e:?}")))?;
    // Refine's machine calls (9 to 14), plus what a runtime profile may use: historical_lookup
    // (7), which this host serves from natively verified programs (`driver`), and VOS's BLAKE2b
    // (100) and curve (110-114) precompiles, which Refine serves where VOS has them. Calls 100
    // and 110-114 are outside the Gray Paper (100 is JIP-1's `log`).
    let mut allowed = vos_pvm::spi::REFINE_HOST_CALL_ALLOWLIST.to_vec();
    if gray_paper_only {
        allowed.push(7);
    } else {
        allowed.extend([7, 100, 110, 111, 112, 113, 114]);
    }
    vos_pvm::spi::validate_standard_program_host_calls(&pvm, &allowed)
        .map_err(|e| err(format!("{e:?}")))?;
    Ok(pvm)
}

/// The code generation an actor's ELF and the program it was linked into carry compiled in
/// (`T-037-01`; amendment 0033, D13): read from each by `ledger_core::code::generation_of`, which
/// refuses a record that is missing (folded away) or duplicated, and never counts code. They must
/// agree: a record the linker dropped would leave the guest's fence reading nothing.
pub fn generation(elf: &[u8], program: &[u8]) -> Result<u64, PackageError> {
    let of_elf = ledger_core::code::generation_of(elf)
        .map_err(|e| err(format!("the ELF's code generation record: {e:?}")))?;
    let of_program = ledger_core::code::generation_of(program)
        .map_err(|e| err(format!("the program's code generation record: {e:?}")))?;
    if of_elf != of_program {
        return Err(err(format!(
            "the ELF is built as code generation {of_elf}, its program carries {of_program}"
        )));
    }
    Ok(of_elf)
}

/// The signed VOS3 package of an actor ELF (see the module documentation).
pub fn actor_package(
    elf: &[u8],
    name: Option<&str>,
    key: &dyn AgentSigner,
) -> Result<Vec<u8>, PackageError> {
    let pvm = link(elf)?;
    let metadata_bytes = vos::metadata::raw_section_from_elf(elf)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| err("the ELF has no .vos_meta actor metadata"))?;
    let metadata = vos::metadata::decode(&metadata_bytes)
        .ok_or_else(|| err("the ELF's actor metadata does not decode"))?;
    let schema_bytes = vos::metadata::raw_named_section_from_elf(elf, b".vos_agent")
        .filter(|b| !b.is_empty())
        .ok_or_else(|| err("the ELF has no .vos_agent AAS2 schema"))?;
    let schema = sdk::schema::decode(&schema_bytes).map_err(|e| err(format!("AAS2: {e}")))?;
    let auth_bytes = vos::metadata::raw_agent_authorizations_from_elf(elf)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| err("the ELF has no .vos_agent_auth authorization metadata"))?;
    let authorizations = vos::metadata::decode_agent_authorizations(&auth_bytes)
        .ok_or_else(|| err("the ELF's authorization metadata does not decode"))?;
    if metadata.messages.len() != schema.methods.len()
        || metadata.messages.len() != authorizations.len()
    {
        return Err(err(
            "actor metadata, AAS2 and authorization method counts differ",
        ));
    }

    let schema_reference = BlobRef::of_bytes(&schema_bytes);
    let mut methods = Vec::new();
    let mut introspection_methods = Vec::new();
    for ((m, s), a) in metadata
        .messages
        .iter()
        .zip(&schema.methods)
        .zip(&authorizations)
    {
        if m.name != s.name || m.name != a.name || m.is_query != s.mode.write_lane().is_none() {
            return Err(err(format!(
                "actor metadata surfaces differ at '{}'",
                m.name
            )));
        }
        if m.attested {
            return Err(err("the core has no attested method"));
        }
        let authorization_policy = match &a.selector {
            vos::metadata::ParsedAgentAuthorizationSelector::Public => {
                sdk::method_policy::AuthorizationPolicySelector::Public
            }
            vos::metadata::ParsedAgentAuthorizationSelector::SpaceRole(id) => {
                sdk::method_policy::AuthorizationPolicySelector::SpaceRole(sdk::RoleId(*id))
            }
            vos::metadata::ParsedAgentAuthorizationSelector::ActorRole(id) => {
                sdk::method_policy::AuthorizationPolicySelector::ActorRole(sdk::RoleId(*id))
            }
            vos::metadata::ParsedAgentAuthorizationSelector::Capability(name) => {
                sdk::method_policy::AuthorizationPolicySelector::Capability(
                    sdk::CapabilityId::named(name),
                )
            }
        };
        methods.push(sdk::method_policy::ActorMethodPolicy {
            name: m.name.clone(),
            mode: s.mode,
            arguments: m
                .fields
                .iter()
                .map(|f| sdk::method_policy::MethodArgument {
                    name: f.name.clone(),
                    type_identity: f.ty.clone(),
                })
                .collect(),
            return_type_identity: m.returns.clone(),
            authorization_policy,
            idempotency: sdk::method_policy::IdempotencyRequirement::for_mode(s.mode),
            attestation: sdk::method_policy::AttestationRequirement::None,
        });
        introspection_methods.push(sdk::introspection::ActorMethodIntrospection {
            name: m.name.clone(),
            doc: m.doc.clone(),
            cli_exposure: if m.exposed_to_cli {
                sdk::introspection::CliExposure::Exposed
            } else {
                sdk::introspection::CliExposure::Hidden
            },
            timeout_ms: m.timeout_ms,
            dispatch: match m.mode {
                0 => sdk::introspection::MethodDispatch::Sync,
                1 => sdk::introspection::MethodDispatch::Job,
                v => return Err(err(format!("method '{}' has dispatch mode {v}", m.name))),
            },
        });
    }
    methods.sort_unstable_by(|l, r| l.name.cmp(&r.name));
    introspection_methods.sort_unstable_by(|l, r| l.name.cmp(&r.name));

    let policy = sdk::method_policy::ActorMethodPolicyArtifact {
        actor_schema: schema_reference.clone(),
        methods,
    };
    policy
        .validate_against_schema_bytes(&schema_bytes)
        .map_err(|e| err(format!("AMP2: {e}")))?;
    let policy_bytes = policy.encode().map_err(err)?;
    let policy_reference = BlobRef::of_bytes(&policy_bytes);
    let introspection = sdk::introspection::ActorIntrospectionArtifact {
        actor_schema: schema_reference.clone(),
        method_policy: policy_reference.clone(),
        actor_doc: metadata.doc.clone(),
        methods: introspection_methods,
    };
    introspection
        .validate_against_artifact_bytes(&schema_bytes, &policy_bytes)
        .map_err(|e| err(format!("AAI1: {e}")))?;
    let introspection_bytes = introspection.encode().map_err(err)?;
    let introspection_reference = BlobRef::of_bytes(&introspection_bytes);
    let tasks = sdk::task::TaskDependencySetArtifact {
        dependencies: Vec::new(),
    };
    tasks.validate().map_err(err)?;
    let task_bytes = tasks.encode().map_err(err)?;
    let task_reference = BlobRef::of_bytes(&task_bytes);
    let requirements = sdk::RuntimeRequirements {
        lanes: schema.lanes(),
        scheduling: false,
        proof_systems: policy
            .proof_systems()
            .map_err(err)?
            .union(&tasks.proof_systems().map_err(err)?)
            .map_err(err)?,
    };
    let public_key = key.public();
    let mut artifacts = vec![
        artifact(pvm.clone()),
        artifact(schema_bytes),
        artifact(policy_bytes),
        artifact(introspection_bytes),
        artifact(task_bytes),
    ];
    artifacts.sort_unstable_by(|l, r| l.identity.cmp(&r.identity));
    let package = PackageEnvelope {
        manifest: PackageManifest::Actor(sdk::package::ActorPackageManifest {
            name: name.map_or_else(|| metadata.actor_name.clone(), str::to_owned),
            program: BlobRef::of_bytes(&pvm),
            contract: sdk::contract::ActorPackageContract::canonical(),
            state_lane_schema: schema_reference,
            method_policy: policy_reference,
            introspection: introspection_reference,
            task_dependencies: task_reference,
            scheduling: false,
            requirements,
            // A program above the standard size is a declared, signed choice of its producer
            // (VOS `ActorResources`); the declaration restates the program header exactly.
            resources: (pvm.len() > sdk::STANDARD_ACTOR_PROGRAM_BYTES)
                .then(|| vos::agent::package_admission::actor_program_resources(&pvm))
                .flatten(),
            signing: PackageSigning {
                producer: ProducerId::of_public_key(&public_key),
                public_key,
                signature: [0; sdk::package::PACKAGE_SIGNATURE_BYTES],
            },
        }),
        artifacts,
    };
    sign(key, package)?.encode().map_err(err)
}

/// Admit an actor package, as the Local host does before installing it.
pub fn admit_actor(bytes: &[u8]) -> Result<AdmittedActorPackage, PackageError> {
    admit_actor_package(bytes).map_err(|e| err(format!("{e:?}")))
}

/// The standard AgentRuntime program as a signed, admitted runtime package.
pub fn runtime_package(
    pvm: &[u8],
    key: &dyn AgentSigner,
) -> Result<AdmittedRuntimePackage, PackageError> {
    let public_key = key.public();
    let package = PackageEnvelope {
        manifest: PackageManifest::AgentRuntime(AgentRuntimePackageManifest {
            name: "standard-local-runtime".into(),
            outer_program: BlobRef::of_bytes(pvm),
            // `None` is the byte-identical r19 tag the standard runtime carries (VOS 2bb7876).
            external_state_limits: None,
            contract: RuntimePackageContract::canonical(),
            capabilities: RuntimeCapabilities::standard(),
            signing: PackageSigning {
                producer: ProducerId::of_public_key(&public_key),
                public_key,
                signature: [0; sdk::package::PACKAGE_SIGNATURE_BYTES],
            },
        }),
        artifacts: vec![artifact(pvm.to_vec())],
    };
    let package = sign(key, package)?;
    admit_runtime_package(&package.encode().map_err(err)?).map_err(|e| err(format!("{e:?}")))
}
