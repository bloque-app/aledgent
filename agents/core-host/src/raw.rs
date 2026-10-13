// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `aledgent-core-host agent-server <runtime.pvm> <actor.vos> <host:port>`: the raw Agent port
//! (`core_binding::host::RawRequest`), for the conformance suite's `through-agent/vos` target.
//!
//! Every connection gets a fresh Agent (a new runtime state with the core installed), served one
//! request at a time on its own thread, and dropped when the connection closes. Nothing is
//! persisted: this is a test harness, not a deployment (that is `serve`). Test keys only.

use std::io::{BufReader, BufWriter};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use core_binding::envelope::{decode, encode};
use core_binding::host::{AgentPort, DRIVER, RawRequest, RawResponse, read_frame, write_frame};
use vos::agent::package_admission::AdmittedActorPackage;

use crate::driver::Runtime;
use crate::port::VosPort;
use crate::{keys, package};

fn connection(stream: TcpStream, runtime: Arc<Vec<u8>>, pkg: Arc<AdmittedActorPackage>) {
    let _ = stream.set_nodelay(true);
    let Ok(read) = stream.try_clone() else {
        return;
    };
    let mut r = BufReader::new(read);
    let mut w = BufWriter::new(stream);
    let mut port = match Runtime::create(&runtime, &keys())
        .and_then(|mut rt| rt.install(&pkg, "aledgent-core", None).map(|a| (rt, a)))
    {
        Ok((rt, actor)) => VosPort::new(rt, actor, DRIVER),
        Err(e) => {
            eprintln!("agent-server: the Agent does not start: {e:?}");
            return;
        }
    };
    while let Ok(frame) = read_frame(&mut r) {
        let response = match decode::<RawRequest>(&frame) {
            Err(e) => RawResponse::Failed(core_binding::host::PortError::Aborted(e)),
            Ok(RawRequest::Invoke { message, blobs }) => match port.invoke(message, blobs) {
                Ok(b) => RawResponse::Reply(b),
                Err(e) => RawResponse::Failed(e),
            },
            Ok(RawRequest::Hot) => match port.hot() {
                Ok(h) => RawResponse::Hot(h.map(|h| encode(&h))),
                Err(e) => RawResponse::Failed(e),
            },
            Ok(RawRequest::Entry(seq)) => match port.entry(seq) {
                Ok(e) => RawResponse::Entry(e.map(|e| encode(&e))),
                Err(e) => RawResponse::Failed(e),
            },
            // Three big-endian u64 per invocation: caller µs, the Invoke's gas, the actor's gas.
            Ok(RawRequest::Samples) => RawResponse::Samples(
                port.samples
                    .iter()
                    .flat_map(|s| {
                        [s.caller_us, s.invoke.gas_used, s.actor_gas]
                            .into_iter()
                            .flat_map(u64::to_be_bytes)
                    })
                    .collect(),
            ),
        };
        if write_frame(&mut w, &encode(&response)).is_err() {
            return;
        }
    }
}

/// Serve until the process is stopped.
pub fn agent_server(runtime: &str, actor: &str, addr: &str) -> Result<(), String> {
    let runtime = Arc::new(std::fs::read(runtime).map_err(|e| format!("runtime: {e}"))?);
    let pkg = Arc::new(
        package::admit_actor(&std::fs::read(actor).map_err(|e| format!("actor: {e}"))?)
            .map_err(|e| format!("actor package: {e:?}"))?,
    );
    let listener = TcpListener::bind(addr).map_err(|e| format!("{addr}: {e}"))?;
    eprintln!("agent-server: listening on {addr}");
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| e.to_string())?;
        let (runtime, pkg) = (Arc::clone(&runtime), Arc::clone(&pkg));
        std::thread::spawn(move || connection(stream, runtime, pkg));
    }
    Ok(())
}
