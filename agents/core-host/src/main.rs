// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `aledgent-core-host` (F-005): the core Agent's host process.
//!
//! ```text
//! aledgent-core-host serve --config <file> --runtime <runtime.pvm> --actor <actor.vos> --state <dir> [--chainspec <file> [--genesis-manifest <file>] [--unnamed-code] [--unverified-genesis]] [--budget <bytes>] [--batch N]
//!                     [--archive <spec> [--archive-interval-ms MS] [--checkpoint-every N] [--checkpoint-every-secs S]]
//!                     [--code-config <file>]
//!                                    the core Agent behind ingress's boundary::Core, streaming every
//!                                    decision to the off-node archive (serve.rs)
//! aledgent-core-host restore --config <file> --runtime <runtime.pvm> --actor <actor.vos> --state <dir> [--budget <bytes>] [--code-config <file>]
//!                                    finish a restore `opsctl restore core --from` laid out: the Agent
//!                                    re-sent every archived input after its checkpoint (T-005-16)
//! aledgent-core-host measure <runtime.pvm> <actor.vos> <out.csv> [--n N] [--warmup W] [--mix reserve|mixed|card|transfers|tick|open|custody|head|positions] [--batch N] [--mapped]
//!                                    the reservation path, or reservations, captures and
//!                                    transfers, through the Agent (T-005-10, T-005-14), N
//!                                    inputs per invocation (T-031-05); `--mix open` opens
//!                                    identities and accounts (profiling, measure.rs)
//! aledgent-core-host code-manifest --actor <vos> --runtime <pvm> --profile <json> --out <file>
//!                                    the genesis code's manifest (amendment 0023's `aledgent/code/v1`)
//!                                    of this host, package and runtime (F-036, code.rs)
//! aledgent-core-host check-code --actor <vos> --runtime <pvm> (--chainspec <file> | --manifest <file>) [--code <hex>] [--write-manifest <file>] [--out <file>]
//!                                    refuse unless the package and runtime are the genesis code's,
//!                                    as a restart checks them; for a core image on a deployment's
//!                                    genesis bytes (F-038 §12 step 0, DEC-27 (i), code.rs)
//! aledgent-core-host build-spec --config <file> --chainspec <file> --actor <vos> --runtime <pvm> --out <dir> [--unnamed-code]
//!                                    a chainspec's genesis state, built natively: the leaf store
//!                                    and its manifest (F-036, chainspec.rs)
//! aledgent-core-host verify-genesis --config <file> --chainspec <file> --manifest <file> --runtime <pvm> --actor <vos> [--segments K] [--engine interpreter|recompiler] [--out <report>]
//!                                    the chainspec executed chunk by chunk inside the PVM: its
//!                                    roots must be the manifest's; --out writes the report
//! aledgent-core-host install-verified --config <file> --state <dir> --report <file> [--genesis-manifest <file>] [--archive <spec>]
//!                                    open a chainspec book: install a passing verify-genesis report
//!                                    in the core host's state directory (T-036-25, chainspec.rs)
//! aledgent-core-host spec-measure <runtime.pvm> <actor.vos> <chainspec.bin> <out.csv> [--batch N]
//!                                    the chunk-by-chunk path's gas, with keys in process
//! aledgent-core-host batch-curve <runtime.pvm> <actor.vos> <out.csv> [--sizes 1,8,32,128,512,2048] [--rounds R] [--accounts A]
//!                                    one invocation's cost against the commands it carries (F-032)
//! aledgent-core-host shards <runtime.pvm> <shard.vos> <out.csv> [--counts 1,2,4,8] [--accounts A] [--ops M] [--batch B]
//!                                    N book-shard Agents and a principals Agent in one Space,
//!                                    decisions per second against N (F-031, DEC-22, sharded.rs)
//! aledgent-core-host agent-server <runtime.pvm> <actor.vos> <host:port>
//!                                    the raw Agent port, one Agent per connection (conformance)
//! aledgent-core-host runtime-pvm <agent_runtime.elf> <out.pvm>   link VOS's standard runtime
//! aledgent-core-host package <actor.elf> <out.vos>               package the core actor
//! aledgent-core-host smoke <runtime.pvm> <actor.vos>             a few commands, printed
//! ```
//!
//! AGPL-3.0-or-later: it links VOS (`agents/Cargo.toml`).

mod batch;
mod diag;

/// The host's allocator with `jemalloc` (`T-032-11`, `diag`).
#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
mod chainspec;
mod code;
mod code_source;
mod driver;
mod fleet;
mod keys;
mod measure;
#[allow(dead_code)]
mod package;
mod port;
mod r03gas;
mod raw;
mod restart;
mod serve;
mod sharded;
#[cfg(test)]
mod stateful_fixture;
mod switch;
mod upgrade_check;

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::time::Instant;

use core_binding::host::{DRIVER, Driver};
use ledger_core::account::{AccountKind, Custody};
use ledger_core::asset::{AssetId, AssetKind, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::id::OperationId;
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{AssetDecl, Ctx, Genesis, GrantRequest, OpenAccount};
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_kernel::posting::OperationRef;

fn usage() -> ! {
    eprintln!(
        "usage:\n  aledgent-core-host serve --config <file> --runtime <runtime.pvm> --actor <actor.vos> --state <dir> [--chainspec <file> [--genesis-manifest <file>] [--unnamed-code] [--unverified-genesis]] [--budget <bytes>] [--batch N] [--archive <spec> [--archive-interval-ms MS] [--checkpoint-every N] [--checkpoint-every-secs S]] [--code-config <file>]\n  \
         aledgent-core-host restore --config <file> --runtime <runtime.pvm> --actor <actor.vos> --state <dir> [--budget <bytes>]\n  \
         aledgent-core-host check-upgrade --runtime <pvm> --previous <vos> --package <vos> --history <dir> [--out <file>]\n  \
         aledgent-core-host code-manifest --actor <vos> --runtime <pvm> --profile <json> --out <file>\n  \
         aledgent-core-host check-code --actor <vos> --runtime <pvm> (--chainspec <file> | --manifest <file>) [--code <hex>] [--write-manifest <file>] [--out <file>]\n  \
         aledgent-core-host build-spec --config <file> --chainspec <file> --actor <vos> --runtime <pvm> --out <dir> [--unnamed-code] [--max-memory <bytes>]\n  \
         aledgent-core-host verify-genesis --config <file> --chainspec <file> --manifest <file> --runtime <pvm> --actor <vos> [--unnamed-code] [--segments K] [--engine interpreter|recompiler] [--out <report>]\n  \
         aledgent-core-host install-verified --config <file> --state <dir> --report <file> [--genesis-manifest <file>] [--archive <spec>]\n  \
         aledgent-core-host measure <runtime.pvm> <actor.vos> <out.csv> [--n N] [--warmup W] [--mix reserve|mixed|card|transfers|tick|open|custody|head|positions] [--batch N] [--mapped]\n  \
         aledgent-core-host batch-curve <runtime.pvm> <actor.vos> <out.csv> [--sizes ...] [--rounds R] [--accounts A]\n  \
         aledgent-core-host shards <runtime.pvm> <shard.vos> <out.csv> [--counts 1,2,4,8] [--accounts A] [--ops M] [--batch B]\n  \
         aledgent-core-host fleet <runtime.pvm> <shard.vos> <out.csv> --index I --nodes N --peers F --port P --key K [--per-node K] [--phases ...]\n  \
         aledgent-core-host fleet-key <path>\n  \
         aledgent-core-host agent-server <runtime.pvm> <actor.vos> <host:port>\n  \
         aledgent-core-host runtime-pvm <agent_runtime.elf> <out.pvm>\n  \
         aledgent-core-host package <actor.elf> <out.vos> [--previous <previous.vos>]\n  \
         aledgent-core-host smoke <runtime.pvm> <actor.vos>"
    );
    std::process::exit(2)
}

fn keys() -> driver::AgentKeys {
    driver::AgentKeys {
        package: keys::test_key(0x71),
        authority: keys::test_key(0x72),
        discriminator: 0x40,
    }
}

fn op(n: u64) -> OperationId {
    let mut b = [0u8; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    OperationId::from_ref(OperationRef(b))
}

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn genesis() -> Genesis {
    Genesis {
        assets: vec![AssetDecl {
            id: usd(),
            location: Some(ledger_core::asset::AssetLocation::parse("Here(1)").unwrap()),
            precision: 2,
            kind: AssetKind::Fiat,
            deprecated: false,
        }],
        operator_verifier: Verifier::new(vec![0x0F]),
        lifecycle: LifecycleConfig::new([7; 16], 60, 3_600)
            .with_default_deadline(OperationKind::Reserve, 1_000),
        chainspec: None,
        spec: None,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("runtime-pvm") if args.len() == 4 => {
            let elf = std::fs::read(&args[2]).expect("runtime ELF");
            let gray_paper_only = std::env::var("ALEDGENT_RUNTIME_GP_ONLY").is_ok_and(|v| v == "1");
            let pvm = package::link_runtime(&elf, gray_paper_only).expect("link runtime");
            std::fs::write(&args[3], &pvm).expect("write");
            let calls = package::runtime_host_calls(&pvm).expect("host calls");
            let outside: Vec<u64> = calls
                .iter()
                .copied()
                .filter(|c| !package::GRAY_PAPER_REFINE_CALLS.contains(c))
                .collect();
            println!(
                "runtime {} B; host calls {calls:?}; {}",
                pvm.len(),
                if outside.is_empty() {
                    "Gray Paper only".to_string()
                } else {
                    format!("outside the Gray Paper: {outside:?}")
                }
            );
        }
        Some("package") if args.len() == 4 || (args.len() == 6 && args[4] == "--previous") => {
            let elf = std::fs::read(&args[2]).expect("actor ELF");
            let key = ed25519_dalek::SigningKey::from_bytes(&[0x71; 32]);
            // T-037-19: a generation after the first claims to read the state of the previous
            // generation's program, which the runtime requires to change a stateful actor's
            // program in place. The genesis (no `--previous`) claims nothing.
            let previous = args
                .get(5)
                .map(|p| std::fs::read(p).expect("previous package"));
            let claim = match package::state_claim(previous.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("aledgent-core-host: package: {e}");
                    std::process::exit(1);
                }
            };
            let pkg = match package::actor_package(&elf, None, &key, &claim) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("aledgent-core-host: package: {e}");
                    std::process::exit(1);
                }
            };
            let pvm = package::link(&elf).expect("link");
            // T-037-01: the build-time check of the compiled code generation. The ELF's record
            // (`ALEDGENT_CODE_GENERATION`) must be there once, and must survive linking: the
            // program the package carries, which keeps no sections, holds the same one record in
            // its read-only data.
            let generation = match package::generation(&elf, &pvm) {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("aledgent-core-host: package: {e}");
                    std::process::exit(1);
                }
            };
            std::fs::write(&args[3], &pkg).expect("write");
            println!(
                "package {} B, program {} B, code generation {generation}, state claim {}",
                pkg.len(),
                pvm.len(),
                if claim.is_empty() {
                    "none (a genesis)".to_string()
                } else {
                    claim
                        .iter()
                        .map(|p| hex::encode(p.as_bytes()))
                        .collect::<Vec<_>>()
                        .join(",")
                }
            );
        }
        Some("smoke") if args.len() == 4 => smoke(&args[2], &args[3]),
        Some("measure") if args.len() >= 5 => measure::measure(&args),
        Some("build-spec") => {
            if let Err(e) = chainspec::build_spec(&args) {
                eprintln!("aledgent-core-host: {e}");
                std::process::exit(1);
            }
        }
        Some("code-manifest") => {
            if let Err(e) = code::code_manifest(&args) {
                eprintln!("aledgent-core-host: {e}");
                std::process::exit(1);
            }
        }
        Some("check-code") => {
            if let Err(e) = code::check_code(&args) {
                eprintln!("aledgent-core-host: check-code: {e}");
                std::process::exit(1);
            }
        }
        Some("verify-genesis") => {
            if let Err(e) = chainspec::verify_genesis(&args) {
                eprintln!("aledgent-core-host: {e}");
                std::process::exit(1);
            }
        }
        Some("install-verified") => {
            if let Err(e) = chainspec::install_verified(&args) {
                eprintln!("aledgent-core-host: install-verified refused: {e}");
                std::process::exit(1);
            }
        }
        Some("spec-measure" | "seed-measure") if args.len() >= 6 => {
            if let Err(e) = chainspec::measure(&args) {
                eprintln!("aledgent-core-host: {e}");
                std::process::exit(1);
            }
        }
        Some("batch-curve") if args.len() >= 5 => batch::batch_curve(&args),
        Some("shards") if args.len() >= 5 => sharded::shards(&args),
        Some("fleet") if args.len() >= 5 => fleet::fleet(&args),
        Some("fleet-key") if args.len() == 3 => fleet::fleet_key(&args),
        Some("agent-server") if args.len() == 5 => {
            if let Err(e) = raw::agent_server(&args[2], &args[3], &args[4]) {
                eprintln!("aledgent-core-host: {e}");
                std::process::exit(1);
            }
        }
        Some("serve") => {
            if let Err(e) = serve::serve(&args) {
                eprintln!("aledgent-core-host: {e}");
                std::process::exit(1);
            }
        }
        Some("check-upgrade") => {
            if let Err(e) = upgrade_check::check_upgrade(&args) {
                eprintln!("aledgent-core-host: check-upgrade refused: {e}");
                std::process::exit(1);
            }
        }
        Some("restore") => {
            if let Err(e) = serve::restore(&args) {
                eprintln!("aledgent-core-host: restore refused: {e}");
                std::process::exit(1);
            }
        }
        _ => usage(),
    }
}

fn smoke(runtime: &str, actor: &str) {
    let runtime = std::fs::read(runtime).expect("runtime");
    let pkg = package::admit_actor(&std::fs::read(actor).expect("package")).expect("admit");
    let mut rt = driver::Runtime::create(&runtime, &keys()).expect("create");
    let installed = rt.install(&pkg, "aledgent-core", None).expect("install");
    let port = port::VosPort::new(rt, installed, DRIVER);
    let t = Instant::now();
    let (mut driver, mut replica, founded) = Driver::found(
        port,
        core_binding::host::HostKeys::local([0xC5; 32], None),
        genesis(),
        op(0),
        1,
        core_binding::hot::Limits::DEFAULT,
    )
    .unwrap_or_else(|e| panic!("genesis: {e:?}"));
    println!("genesis {} ms", t.elapsed().as_millis());
    let operator = Caller::new(founded.operator, founded.operator_credential);
    let mut n = 0;
    let mut ctx = |c: Caller| {
        n += 1;
        Ctx {
            caller: c,
            op: op(1_000 + n),
            now: 10,
        }
    };
    let step = |what: &str,
                replica: &mut core_binding::host::Replica,
                driver: &mut Driver<port::VosPort>| {
        let t = Instant::now();
        let r = driver.sync(replica, Vec::new());
        let s = driver.port().samples.last().copied().unwrap_or_default();
        println!(
            "{what}: {:?} caller {} ms, invoke {} ms ({} M gas), ack {} ms, actor gas {} M, message {} B, blobs {} B, state {} B",
            r.as_ref().map(|c| c.as_ref().map(|c| c.summaries.clone())),
            t.elapsed().as_millis(),
            s.invoke.elapsed_us / 1000,
            s.invoke.gas_used / 1_000_000,
            s.ack.elapsed_us / 1000,
            s.actor_gas / 1_000_000,
            s.message_bytes,
            s.blob_bytes,
            driver.port().runtime().state_bytes(),
        );
        r.expect("the Agent commits");
    };
    replica
        .grant_authority(
            &ctx(operator),
            GrantRequest {
                kind: GrantKind::Issuance,
                principal: operator.principal,
                subject: Subject::Asset(usd()),
                ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                    .unwrap(),
                per_operation: None,
                per_window: Some(Window {
                    length: NonZeroU64::new(1_000_000).unwrap(),
                    limit: NonZeroMinor::new(Minor::new(u64::MAX / 2)).unwrap(),
                }),
                assets: Some(NonEmptySet::one(usd())),
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
    step("grant issuance", &mut replica, &mut driver);
    let o = replica
        .open_identity(
            &ctx(operator),
            IdentityKind::Person,
            None,
            Verifier::new(vec![1]),
        )
        .unwrap()
        .value;
    step("open identity", &mut replica, &mut driver);
    let person = Caller::new(o.identity, o.credential);
    let a = replica
        .open_account(
            &ctx(person),
            OpenAccount {
                owner: person.principal,
                kind: AccountKind::Customer,
                custody: Custody::Delegated,
                treasury_limits: BTreeMap::new(),
                acknowledged: None,
            },
        )
        .unwrap()
        .value;
    step("open account", &mut replica, &mut driver);
    replica
        .issue(&ctx(operator), usd(), a, Minor::new(1_000_000))
        .unwrap();
    step("issue", &mut replica, &mut driver);
    for i in 0..3 {
        replica
            .reserve(&ctx(person), a, usd(), Minor::new(100 + i))
            .unwrap();
        step("reserve", &mut replica, &mut driver);
    }
    // REQ-PN-1 through VOS: an invocation from another principal is refused, and nothing
    // commits (the replica has nothing to send: the refusal is the Agent's own).
    let before = driver.seq();
    let envelope = core_binding::envelope::encode(&core_binding::envelope::Envelope::Archived {
        through: 0,
        log_hash: [0; 32],
    });
    let reply = driver
        .port_mut()
        .invoke_as([0x0B; 32], envelope, Vec::new())
        .expect("the runtime answers");
    let reply: core_binding::envelope::Reply =
        core_binding::envelope::decode(&reply).expect("a reply");
    println!("another principal: {reply:?}");
    assert!(
        matches!(
            reply,
            core_binding::envelope::Reply::Refused(core_binding::envelope::AgentRefusal::NotDriver)
        ),
        "another principal is refused"
    );
    assert_eq!(driver.seq(), before);
}
