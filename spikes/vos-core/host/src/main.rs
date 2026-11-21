// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The vos-core spike's host (W2, gate G1). See `Cargo.toml`.
//!
//!     aledgent-vos-core-host fit   <guest.elf>
//!     aledgent-vos-core-host serve <guest.elf> <127.0.0.1:port> <samples.csv> [--probe]
//!
//! `fit` prints `key=value` lines: the backend this process runs (`VOS_PVM_BACKEND=recompiler`
//! makes an unavailable recompiler an error, never an interpreter fallback), the ELF's size, the
//! derivation's time (what an install costs the node), the derived program's size and its
//! compact code blob against `budget::MACHINE_BLOB`, its read-only and read-write data and stack.
//!
//! `serve` takes `core_binding::host::RawRequest`s, one fresh agent per connection: the guest
//! installed at instance 1, every `Invoke` a request of `apply(message, blobs)` alone in a batch,
//! `Hot` and `Entry` queries. Each invocation appends a CSV row:
//! `conn,seq,kind,classes,message,blobs,args,gas,outcome,us,peak`. `--probe`: the guest is the
//! `heap-probe` build, and `peak` is its `peak()` query after the invocation.

use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use vos_abi::Encode;
use vos_abi::abi::Failure;
use vos_abi::ids::{AgentId, InstanceId, Principal};
use vos_abi::result::Outcome;
use vos_pvm_compiler::Derived;
use vos_testkit::World;

/// `core_binding::host::RawRequest`, as it is encoded (postcard: variants by index).
#[derive(Debug, Deserialize)]
enum RawRequest {
    Invoke { message: Vec<u8>, blobs: Vec<Vec<u8>> },
    Hot,
    Entry(u64),
    Samples,
}

/// `core_binding::host::PortError`.
#[derive(Debug, Serialize)]
#[allow(dead_code)]
enum PortError {
    Unavailable(String),
    Ambiguous(String),
    Aborted(String),
}

/// `core_binding::host::RawResponse`.
#[derive(Debug, Serialize)]
#[allow(dead_code)]
enum RawResponse {
    Reply(Vec<u8>),
    Hot(Option<Vec<u8>>),
    Entry(Option<Vec<u8>>),
    Samples(Vec<u8>),
    Failed(PortError),
}

const AGENT: AgentId = AgentId(7);
const CORE: InstanceId = InstanceId(1);
/// The core host's session key: the driver principal the book records at genesis.
const DRIVER: Principal = Principal::Client([0xD7; 32]);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("fit") => fit(&args[2]),
        Some("serve") => serve(&args[2], &args[3], &args[4], args.iter().any(|a| a == "--probe")),
        _ => {
            eprintln!("usage: fit <elf> | serve <elf> <addr> <csv> [--probe]");
            std::process::exit(2);
        }
    }
}

fn backend() {
    match vos_pvm::backend::backend_choice() {
        Ok(choice) => {
            println!("backend={:?}", choice.backend);
            println!("backend_requested={}", choice.requested);
            println!(
                "recompiler_unavailable={}",
                choice
                    .unavailable
                    .map_or_else(|| "none".to_string(), |u| u.to_string())
            );
        }
        Err(e) => {
            println!("backend_error={e}");
            std::process::exit(1);
        }
    }
}

fn derive(elf: &[u8]) -> (Derived, u128) {
    let started = Instant::now();
    let derived = vos_pvm_compiler::derive(elf).expect("the guest derives");
    (derived, started.elapsed().as_micros())
}

fn fit(path: &str) {
    backend();
    let elf = std::fs::read(path).expect("the guest ELF");
    println!("elf_bytes={}", elf.len());
    let (derived, first) = derive(&elf);
    // Twice more: the first may include one-time setup.
    let (_, second) = derive(&elf);
    let (_, third) = derive(&elf);
    println!("derive_us={first},{second},{third}");
    println!("actor_program_bytes={}", derived.actor.len());
    println!("kernels={}", derived.kernels.len());
    println!("transpiler_version={}", derived.transpiler_version);
    println!("heap_pages={}", derived.heap);
    let program =
        vos_pvm_program::parse_standard_program(&derived.actor).expect("a standard program");
    let code = program.compact_code().expect("its compact code");
    println!("code_blob_bytes={}", code.len());
    // vos-core 1650c474's bound; a measurement build may raise it (patches/), to run the rest.
    println!("code_blob_bound=2097152");
    println!("code_blob_over_bound={}", code.len() as i64 - 2_097_152);
    println!("machine_blob_this_build={}", vos_pvm::inner::budget::MACHINE_BLOB);
    println!("ro_bytes={}", program.ro_data.len());
    println!("rw_bytes={}", program.rw_data.len());
    println!("stack_bytes={}", program.stack_size);
    // The install as a node takes it: the program added, then `Admin::Install` in a batch.
    let started = Instant::now();
    let mut world = World::new();
    world.agent(AGENT).install(CORE, &derived, &[]);
    println!("install_step_us={}", started.elapsed().as_micros());
    // The first query prepares the actor's code on this backend (the recompiler compiles it).
    let started = Instant::now();
    let hot = world.read::<Vec<u8>>(AGENT, CORE, "hot", ());
    println!("first_query_us={} hot_bytes={}", started.elapsed().as_micros(), hot.len());
    let started = Instant::now();
    let _ = world.read::<Vec<u8>>(AGENT, CORE, "hot", ());
    println!("second_query_us={}", started.elapsed().as_micros());
}

fn classes(message: &[u8]) -> (String, String) {
    use core_binding::envelope::Envelope;
    let Ok(envelope) = core_binding::envelope::decode::<Envelope>(message) else {
        return ("undecoded".into(), String::new());
    };
    match envelope {
        Envelope::Genesis(_) => ("genesis".into(), String::new()),
        Envelope::StateGenesis(_) => ("state-genesis".into(), String::new()),
        Envelope::Archived { .. } => ("archived".into(), String::new()),
        Envelope::Batch(b) => ("batch".into(), format!("{} inputs", b.inputs.len())),
        Envelope::Input(input) => {
            let names: Vec<String> = input
                .calls
                .iter()
                .map(|c| c.method().to_string())
                .collect();
            let mut kind = names.join("+");
            if kind.is_empty() {
                kind = if input.deliveries.is_empty() { "empty".into() } else { "deliveries".into() };
            }
            ("input".into(), kind)
        }
    }
}

fn serve(path: &str, addr: &str, csv: &str, probe: bool) {
    backend();
    let elf = std::fs::read(path).expect("the guest ELF");
    let (derived, us) = derive(&elf);
    eprintln!("derived in {us} us");
    let derived = Arc::new(derived);
    let out = Arc::new(Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(csv)
            .expect("the samples file"),
    ));
    {
        let mut f = out.lock().unwrap();
        if f.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
            writeln!(f, "conn,seq,kind,classes,message,blobs,args,gas,outcome,us,peak").unwrap();
        }
    }
    let listener = TcpListener::bind(addr).expect("bind");
    eprintln!("serving on {addr}");
    let conns = Arc::new(AtomicU64::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let derived = Arc::clone(&derived);
        let out = Arc::clone(&out);
        let conn = conns.fetch_add(1, Ordering::Relaxed);
        std::thread::spawn(move || {
            if let Err(e) = connection(stream, &derived, &out, conn, probe) {
                eprintln!("conn {conn}: {e}");
            }
        });
    }
}

fn read_frame(r: &mut impl std::io::Read) -> std::io::Result<Vec<u8>> {
    let mut n = [0u8; 4];
    r.read_exact(&mut n)?;
    let mut buf = vec![0u8; u32::from_be_bytes(n) as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_frame(w: &mut impl std::io::Write, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_be_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

fn connection(
    mut stream: TcpStream,
    derived: &Derived,
    out: &Mutex<std::fs::File>,
    conn: u64,
    probe: bool,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let mut world = World::new();
    world.agent(AGENT).install(CORE, derived, &[]);
    let mut seq = 0u64;
    loop {
        let frame = match read_frame(&mut stream) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let request: RawRequest = postcard::from_bytes(&frame)
            .map_err(|e| std::io::Error::other(format!("request: {e}")))?;
        let response = match request {
            RawRequest::Invoke { message, blobs } => {
                let (kind, names) = classes(&message);
                let blob_bytes: usize = blobs.iter().map(Vec::len).sum();
                let arguments = (message.clone(), blobs).encode();
                let args = arguments.len();
                let started = Instant::now();
                let request = {
                    let mut on = world.agent(AGENT);
                    let mut r = on.request(DRIVER, CORE, "apply", ());
                    r.arguments = arguments;
                    r
                };
                let run = world
                    .agent(AGENT)
                    .step(vos_wire::batch::Input::Request(request));
                let us = started.elapsed().as_micros();
                let (outcome, response) = match run.outcome {
                    Outcome::Done(bytes) => match <Vec<u8> as vos_abi::Decode>::decode_exact(&bytes)
                    {
                        Some(reply) => ("done".to_string(), RawResponse::Reply(reply)),
                        None => (
                            "undecodable".to_string(),
                            RawResponse::Failed(PortError::Aborted("undecodable reply".into())),
                        ),
                    },
                    Outcome::Failed { code, detail } => {
                        let name = match code {
                            Failure::Handler => "handler".to_string(),
                            other => format!("{other:?}"),
                        };
                        (
                            format!("failed:{name}"),
                            RawResponse::Failed(PortError::Aborted(format!(
                                "{code:?} {} detail bytes",
                                detail.len()
                            ))),
                        )
                    }
                    Outcome::Started(_) => (
                        "started".to_string(),
                        RawResponse::Failed(PortError::Aborted("started".into())),
                    ),
                };
                let peak = if probe {
                    world
                        .read::<u64>(AGENT, CORE, "peak", ())
                        .to_string()
                } else {
                    String::new()
                };
                seq += 1;
                let mut f = out.lock().unwrap();
                writeln!(
                    f,
                    "{conn},{seq},{kind},{names},{},{blob_bytes},{args},{},{outcome},{us},{peak}",
                    message.len(),
                    run.gas_used
                )?;
                response
            }
            RawRequest::Hot => {
                let hot = world.read::<Vec<u8>>(AGENT, CORE, "hot", ());
                RawResponse::Hot((!hot.is_empty()).then_some(hot))
            }
            RawRequest::Entry(n) => {
                let entry = world.read::<Vec<u8>>(AGENT, CORE, "entry", n);
                RawResponse::Entry((!entry.is_empty()).then_some(entry))
            }
            RawRequest::Samples => RawResponse::Samples(Vec::new()),
        };
        let bytes = postcard::to_allocvec(&response)
            .map_err(|e| std::io::Error::other(format!("response: {e}")))?;
        write_frame(&mut stream, &bytes)?;
    }
}
