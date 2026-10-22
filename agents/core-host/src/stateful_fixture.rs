// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Test fixtures for the stateful program change (`T-037-19`): small signed actor packages that
//! have a Linear lane, as the core has lanes, and no more than a few PVM instructions of program.
//!
//! The program of generation `tag` writes one byte to its Linear lane and replies one byte, both
//! its own, so each tag is a different program over the same schema (the same state layout, lanes,
//! constructor ABI and producer: what the runtime requires to keep an actor's state across a
//! program change). The producer signs with the package key the `package` subcommand uses.

use crate::keys::AgentSigner as _;
use crate::package::seal;
use vos::agent_sdk as sdk;
use vos::agent_sdk::contract::ActorPackageContract;
use vos::agent_sdk::introspection::{
    ActorIntrospectionArtifact, ActorMethodIntrospection, CliExposure, MethodDispatch,
};
use vos::agent_sdk::method_policy::{
    ActorMethodPolicy, ActorMethodPolicyArtifact, AttestationRequirement,
    AuthorizationPolicySelector, IdempotencyRequirement,
};
use vos::agent_sdk::package::{
    ActorPackageManifest, PackageArtifact, PackageEnvelope, PackageManifest, PackageSigning,
};
use vos::agent_sdk::schema::{
    ConstructorContract, ParsedField, ParsedInlineField, ParsedMethod, ParsedSchema,
};
use vos::agent_sdk::task::TaskDependencySetArtifact;
use vos::agent_sdk::wire::CanonicalWire as _;
use vos::agent_sdk::{
    BlobRef, LaneSet, MethodMode, ProducerId, ProgramId, RuntimeRequirements, StateLane,
};
use vos_pvm_compiler::assembler::{Assembler, Reg};

/// The package key's seed: the `package` subcommand's.
pub const PACKAGE_SEED: u8 = 0x71;

/// The byte generation `tag` replies with.
pub fn reply_of(tag: u8) -> u8 {
    0x62 + tag
}

/// The program of generation `tag`: it writes one Linear byte and replies with [`reply_of`].
fn program(tag: u8) -> Vec<u8> {
    let mut output = vec![vos::actors::STATUS_DONE];
    output.extend_from_slice(&1u32.to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.extend_from_slice(&0u32.to_le_bytes());
    output.push(0x29 + tag);
    output.push(reply_of(tag));
    let mut actor = Assembler::new();
    actor
        .set_rw_data(output.clone())
        .load_imm_64(Reg::A0, 2 * u64::from(vos_pvm::PVM_ZONE_SIZE))
        .load_imm_64(Reg::A1, output.len() as u64)
        .jump_ind(Reg::RA, 0);
    actor.build_standard()
}

fn artifact(bytes: &[u8]) -> PackageArtifact {
    PackageArtifact {
        identity: BlobRef::of_bytes(bytes),
        bytes: bytes.to_vec(),
    }
}

/// The signed package bytes of generation `tag`, its producer claiming it reads the state of the
/// `claim` programs, sealed as the `package` subcommand seals a core.
pub fn package(tag: u8, claim: &[ProgramId]) -> Vec<u8> {
    let key = ed25519_dalek::SigningKey::from_bytes(&[PACKAGE_SEED; 32]);
    let program = program(tag);
    let schema = ParsedSchema {
        constructor: ConstructorContract::Forbidden,
        fields: vec![ParsedField::Inline(ParsedInlineField {
            source_index: 0,
            name: "value".into(),
            type_identity: "core::primitive::u8".into(),
            persistence: sdk::FieldPersistence::State(StateLane::Linear),
        })],
        methods: vec![ParsedMethod {
            source_index: 0,
            name: "write".into(),
            mode: MethodMode::Linear,
            explicit: true,
        }],
    };
    let schema_bytes = schema.encode().expect("encode the schema");
    let policy_bytes = ActorMethodPolicyArtifact {
        actor_schema: BlobRef::of_bytes(&schema_bytes),
        methods: vec![ActorMethodPolicy {
            name: "write".into(),
            mode: MethodMode::Linear,
            arguments: Vec::new(),
            return_type_identity: "core::primitive::u8".into(),
            authorization_policy: AuthorizationPolicySelector::Public,
            idempotency: IdempotencyRequirement::Required,
            attestation: AttestationRequirement::None,
        }],
    }
    .encode()
    .expect("encode the method policy");
    let introspection_bytes = ActorIntrospectionArtifact {
        actor_schema: BlobRef::of_bytes(&schema_bytes),
        method_policy: BlobRef::of_bytes(&policy_bytes),
        actor_doc: "T-037-19 fixture".into(),
        methods: vec![ActorMethodIntrospection {
            name: "write".into(),
            doc: String::new(),
            cli_exposure: CliExposure::Exposed,
            timeout_ms: 0,
            dispatch: MethodDispatch::Sync,
        }],
    }
    .encode()
    .expect("encode the introspection");
    let task_bytes = TaskDependencySetArtifact {
        dependencies: Vec::new(),
    }
    .encode()
    .expect("encode the task set");
    let mut artifacts = vec![
        artifact(&program),
        artifact(&schema_bytes),
        artifact(&policy_bytes),
        artifact(&introspection_bytes),
        artifact(&task_bytes),
    ];
    artifacts.sort_unstable_by(|l, r| l.identity.cmp(&r.identity));
    let public_key = key.public();
    let envelope = PackageEnvelope {
        manifest: PackageManifest::Actor(ActorPackageManifest {
            name: "counter".into(),
            program: BlobRef::of_bytes(&program),
            contract: ActorPackageContract::canonical(),
            state_lane_schema: BlobRef::of_bytes(&schema_bytes),
            method_policy: BlobRef::of_bytes(&policy_bytes),
            introspection: BlobRef::of_bytes(&introspection_bytes),
            task_dependencies: BlobRef::of_bytes(&task_bytes),
            scheduling: false,
            requirements: RuntimeRequirements {
                lanes: LaneSet::of(StateLane::Linear),
                scheduling: false,
                proof_systems: sdk::ProofSystemSet::EMPTY,
            },
            resources: None,
            state_compatible_with: Vec::new(),
            signing: PackageSigning {
                producer: ProducerId::of_public_key(&public_key),
                public_key,
                signature: [0; sdk::package::PACKAGE_SIGNATURE_BYTES],
            },
        }),
        artifacts,
    };
    seal(envelope, claim, &key).expect("seal the fixture package")
}
