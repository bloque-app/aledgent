// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Agent port over VOS's runtime ABI: `core_binding`'s driver, through the real runtime.

use std::time::Instant;

#[cfg(feature = "composite")]
use core_binding::envelope::{AgentRefusal, BatchEnvelope, Envelope, Reply, encode};
use core_binding::envelope::{BlobKey, decode};
use core_binding::host::{AgentPort, PortError};
use core_binding::hot::{Hot, RingEntry};
use vos::Decode as _;
#[cfg(feature = "composite")]
use vos::agent::composite::{Composite, CompositeMode, CompositeReply, CompositeStep};
use vos::agent_sdk::{InvocationOrigin, MethodMode, PrincipalId};

use crate::driver::{Actor, DriverError, Exec, Runtime, message};

/// One invocation's measurements, for `F-002`'s method (`notes/MEASUREMENTS-F002.md` §3).
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    /// Invoke then acknowledge, at the caller.
    pub caller_us: u64,
    /// The Invoke execution.
    pub invoke: Exec,
    /// The Acknowledge execution.
    pub ack: Exec,
    /// The actor's own gas.
    pub actor_gas: u64,
    /// The message's size.
    pub message_bytes: u64,
    /// The caller blobs' size.
    pub blob_bytes: u64,
}

/// The core Agent, reached through the runtime ABI.
pub struct VosPort {
    runtime: Runtime,
    actor: Actor,
    driver: PrincipalId,
    /// Every invocation's measurements, in order, while [`VosPort::keep_samples`] is on (the
    /// default; the benches read them). `serve` turns it off: a long-running host would keep one
    /// for every invocation for ever (`T-032-15`, stress finding F15).
    pub samples: Vec<Sample>,
    keep: bool,
}

impl VosPort {
    /// The port over an Agent with the core installed; invocations come from `driver`.
    pub fn new(runtime: Runtime, actor: Actor, driver: [u8; 32]) -> Self {
        VosPort {
            runtime,
            actor,
            driver: PrincipalId(driver),
            samples: Vec::new(),
            keep: true,
        }
    }

    /// Whether to keep every invocation's measurements in [`VosPort::samples`].
    pub fn keep_samples(&mut self, keep: bool) {
        self.keep = keep;
        if !keep {
            self.samples = Vec::new();
        }
    }

    fn sample(&mut self, s: Sample) {
        if self.keep {
            self.samples.push(s);
        }
    }

    /// The runtime.
    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// Invoke as another principal (the principal-check test).
    pub fn invoke_as(
        &mut self,
        principal: [u8; 32],
        envelope: Vec<u8>,
        blobs: Vec<Vec<u8>>,
    ) -> Result<Vec<u8>, PortError> {
        let saved = self.driver;
        self.driver = PrincipalId(principal);
        let r = self.invoke(envelope, blobs);
        self.driver = saved;
        r
    }

    /// A query method of the actor, through the runtime: a Query-mode invocation, which reads
    /// the Linear lane and writes nothing but its own retired result.
    fn query(&mut self, msg: vos::value::Msg) -> Result<Vec<u8>, PortError> {
        let id = self.runtime.next_invocation(MethodMode::Query);
        let m = message(msg);
        let origin = self.origin();
        let w = self
            .runtime
            .work(&self.actor, id, MethodMode::Query, origin, m, &[]);
        let i = self.runtime.invoke(&w).map_err(map_err)?;
        reply_bytes(&i.reply)
    }

    fn origin(&self) -> InvocationOrigin {
        InvocationOrigin {
            principal: Some(self.driver),
            ..InvocationOrigin::anonymous()
        }
    }
}

fn map_err(e: DriverError) -> PortError {
    match e {
        DriverError::Refused(s) | DriverError::Acknowledgement(s) => PortError::Aborted(s),
        DriverError::Runtime(s) => PortError::Unavailable(s),
    }
}

/// The actor's `Vec<u8>` return value, as the runtime carries it.
pub(crate) fn reply_bytes(reply: &[u8]) -> Result<Vec<u8>, PortError> {
    if let Some(v) = Vec::<u8>::try_decode(reply) {
        return Ok(v);
    }
    match vos::value::Value::try_decode(reply) {
        Some(vos::value::Value::Bytes(b)) => Ok(b),
        _ => Err(PortError::Aborted(format!(
            "the actor's reply is not bytes: {:02x?}",
            &reply[..reply.len().min(32)]
        ))),
    }
}

/// `F033_COMPOSITE=isolated|atomic` (`F-033`): a batch of inputs reaches the Agent as one
/// composite invocation of aledgent's runtime (`agents/aledgent-runtime`), one input a step, rather
/// than as one `Envelope::Batch` the actor walks itself. Each step is the input alone, a batch of
/// one naming the pack its blobs travel in; every step carries the core's one order key, so a step
/// that does not apply defers every later one, as the actor's prefix commit does. Needs a runtime
/// built with VOS's `runtime-composite`; the standard runtime ignores the composite blob.
#[cfg(feature = "composite")]
pub fn composite_mode() -> Option<CompositeMode> {
    match std::env::var("F033_COMPOSITE").as_deref() {
        Ok("isolated") => Some(CompositeMode::Isolated),
        Ok("atomic") => Some(CompositeMode::Atomic),
        Ok("") | Ok("off") | Err(_) => None,
        Ok(other) => panic!("F033_COMPOSITE={other}: isolated, atomic or off"),
    }
}

/// Whether batches go to the Agent as composites (`composite_mode`); never without the
/// `composite` feature, which needs a VOS with `vos::agent::composite` (`F-033`).
pub fn composite_on() -> bool {
    #[cfg(feature = "composite")]
    return composite_mode().is_some();
    #[cfg(not(feature = "composite"))]
    false
}

/// The actor gas one composite step may use (`F033_STEP_GAS`, default 400 M: above every
/// ordinary command, `notes/MEASUREMENTS-F005.md` §8.2), drawn from the invocation's.
#[cfg(feature = "composite")]
fn composite_step_gas() -> u64 {
    std::env::var("F033_STEP_GAS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400_000_000)
}

/// The core's inputs are one ordered sequence: one order key for all of them.
#[cfg(feature = "composite")]
const CORE_INPUTS: [u8; 32] = *b"aledgent/core/inputs/v1\0\0\0\0\0\0\0\0\0";

#[cfg(feature = "composite")]
impl VosPort {
    /// A batch as a composite (`composite_mode`); answers as the actor's own batch would.
    fn invoke_composite(
        &mut self,
        mode: CompositeMode,
        batch: BatchEnvelope,
        mut blobs: Vec<Vec<u8>>,
    ) -> Result<Vec<u8>, PortError> {
        let apply =
            |envelope: Vec<u8>| message(vos::value::Msg::new("apply").with("envelope", envelope));
        let mut messages = Vec::with_capacity(batch.inputs.len());
        let mut inputs = batch.inputs.into_iter();
        for (pack, count) in &batch.packs {
            for input in inputs.by_ref().take(*count as usize) {
                messages.push(apply(encode(&Envelope::Batch(BatchEnvelope {
                    inputs: vec![input],
                    packs: vec![(*pack, 1)],
                }))));
            }
        }
        let mut messages = messages.into_iter();
        let primary = messages
            .next()
            .ok_or_else(|| PortError::Aborted("an empty batch".into()))?;
        let actor = self.actor.record.entry.actor.0;
        let gas = composite_step_gas();
        let composite = Composite {
            mode,
            steps: messages
                .map(|message| CompositeStep {
                    actor,
                    gas,
                    order: Some(CORE_INPUTS),
                    stream: None,
                    message,
                })
                .collect(),
        }
        .encode();
        let message_bytes = primary.len() as u64 + composite.len() as u64;
        blobs.push(composite);
        let blob_bytes = blobs.iter().map(|b| b.len() as u64).sum();
        let id = self.runtime.next_invocation(MethodMode::Linear);
        let origin = self.origin();
        let w = self
            .runtime
            .work(&self.actor, id, MethodMode::Linear, origin, primary, &blobs);
        let t = Instant::now();
        let i = self.runtime.invoke(&w).map_err(|e| {
            eprintln!(
                "composite refused: primary message {} B, {} caller blobs of {blob_bytes} B, the composite {} B: {e:?}",
                w.message.len(),
                blobs.len(),
                blobs.last().map_or(0, Vec::len),
            );
            map_err(e)
        })?;
        self.sample(Sample {
            caller_us: t.elapsed().as_micros() as u64,
            invoke: i.invoke,
            ack: i.ack,
            actor_gas: i.actor_gas,
            message_bytes,
            blob_bytes,
        });
        let report = CompositeReply::decode(&i.reply)
            .map_err(|_| PortError::Aborted("not a composite reply".into()))?;
        eprintln!(
            "composite: {} steps after the primary, {} applied, actor gas {} M",
            report.steps.len(),
            report
                .steps
                .iter()
                .filter(|s| s.outcome.is_applied())
                .count(),
            i.actor_gas / 1_000_000
        );
        if report.digested {
            return Err(PortError::Aborted(
                "the composite's replies did not fit the retained result".into(),
            ));
        }
        // One reply per input, as the actor's batch answers: the applied prefix, then at most one
        // refusal, then `Skipped` (a step after a refused input is refused too, for its number).
        let mut replies = Vec::new();
        let mut stopped = false;
        let parts = core::iter::once(Some(report.primary)).chain(
            report
                .steps
                .into_iter()
                .map(|step| step.outcome.is_applied().then_some(step.reply)),
        );
        for part in parts {
            let reply = match part {
                Some(bytes) if !stopped => match decode::<Reply>(&reply_bytes(&bytes)?) {
                    Ok(Reply::Batch(mut one)) if one.len() == 1 => one.remove(0),
                    Ok(other) => {
                        return Err(PortError::Aborted(format!("a step's reply: {other:?}")));
                    }
                    Err(e) => return Err(PortError::Aborted(e)),
                },
                _ => Reply::Refused(AgentRefusal::Skipped),
            };
            if stopped {
                replies.push(Reply::Refused(AgentRefusal::Skipped));
                continue;
            }
            stopped = matches!(reply, Reply::Refused(_));
            replies.push(reply);
        }
        Ok(encode(&Reply::Batch(replies)))
    }
}

impl AgentPort for VosPort {
    fn unacknowledged(&self) -> Option<u64> {
        Some(self.runtime.unacknowledged())
    }

    fn invoke(&mut self, envelope: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        #[cfg(feature = "composite")]
        if let Some(mode) = composite_mode()
            && let Ok(Envelope::Batch(batch)) = decode::<Envelope>(&envelope)
            && batch.inputs.len() > 1
        {
            return self.invoke_composite(mode, batch, blobs);
        }
        let id = self.runtime.next_invocation(MethodMode::Linear);
        let message_bytes = envelope.len() as u64;
        let blob_bytes = blobs.iter().map(|b| b.len() as u64).sum();
        let m = message(vos::value::Msg::new("apply").with("envelope", envelope));
        let origin = self.origin();
        let w = self
            .runtime
            .work(&self.actor, id, MethodMode::Linear, origin, m, &blobs);
        let t = Instant::now();
        let i = self.runtime.invoke(&w).map_err(map_err)?;
        self.sample(Sample {
            caller_us: t.elapsed().as_micros() as u64,
            invoke: i.invoke,
            ack: i.ack,
            actor_gas: i.actor_gas,
            message_bytes,
            blob_bytes,
        });
        reply_bytes(&i.reply)
    }

    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        let r = vos::agent_sdk::BlobRef::of_bytes(bytes);
        BlobKey {
            hash: r.hash.0,
            len: r.len,
        }
    }

    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        let b = self.query(vos::value::Msg::new("hot"))?;
        if b.is_empty() {
            return Ok(None);
        }
        decode::<Hot>(&b).map(Some).map_err(PortError::Unavailable)
    }

    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        let b = self.query(vos::value::Msg::new("entry").with("seq", seq))?;
        if b.is_empty() {
            return Ok(None);
        }
        decode::<RingEntry>(&b)
            .map(Some)
            .map_err(PortError::Unavailable)
    }
}
