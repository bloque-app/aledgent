// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Native decide, VOS verify (`F-034`, feature `native-decide`): a shard's host decides and
//! commits each round on its native shard, answers from that, and hands the committed round to
//! a **replay worker** that runs it through the shard Agent afterwards, off the commit's critical
//! path.
//!
//! - **The worker** owns the Agent's [`Port`] on a thread of its own. It takes the rounds in the
//!   order they committed, sends up to `batch` of them as one [`Envelope::Replay`] (one
//!   invocation, one private blob, one digest per round), and reports each round
//!   [`Event::Confirmed`] when the Agent's reply has the native reply's digest.
//! - **A divergence** is the Agent's own finding: it commits its reply for the first round whose
//!   digest differs, applies nothing after it, and sets its fence. The worker reports
//!   [`Event::Diverged`] and drops every replay after it; the host halts the shard
//!   ([`crate::host::Cluster::recover_shard`] spells out the recovery).
//! - **Everything else the Agent is asked** (a read, a round decided through it, the fence's lift)
//!   goes through the same thread, after the replays queued before it.
//!
//! The host keeps what the worker needs to be told nothing more: the round, its private input
//! (the openings read from the native shard before the round, `REQ-RT-6`; zeroised on drop) and
//! the native reply's digest.

use std::string::{String, ToString};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::vec::Vec;
use std::{format, vec};

use crate::agent::{self, Envelope, Fence, Private, Reply};
use crate::decide::Round;
use crate::host::{Port, PortStats};

/// One committed round, to replay.
pub struct ReplayJob {
    /// Its audit sequence (`Heads::seq`): the shard's rounds committed through it.
    pub seq: u64,
    /// The round, as the native shard ran it.
    pub round: Round,
    /// Its private input, read from the native shard before the round.
    pub private: Private,
    /// The digest of the native reply ([`agent::digest`]).
    pub expected: [u8; 32],
}

/// What the worker reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The Agent reached the native reply for round `seq`.
    Confirmed {
        /// The round's audit sequence.
        seq: u64,
        /// When the Agent's reply came back.
        at: Instant,
    },
    /// The Agent's reply to a round differed from the native one: the Agent is fenced.
    Diverged {
        /// What differed.
        fence: Fence,
        /// When the Agent's reply came back.
        at: Instant,
    },
    /// The Agent refused a replay, or its port failed: nothing more is replayed.
    Failed(String),
}

type Back<T> = Sender<Result<T, String>>;
type Build = std::boxed::Box<dyn FnOnce(&dyn Port) -> (Vec<u8>, Vec<Vec<u8>>) + Send>;

enum Job {
    Replay(std::boxed::Box<ReplayJob>),
    Invoke { build: Build, back: Back<Vec<u8>> },
    Read { query: Vec<u8>, back: Back<Vec<u8>> },
    Stats { back: Back<PortStats> },
}

/// The handle of a shard's replay worker.
pub struct Replayer<P> {
    tx: Option<Sender<Job>>,
    events: Receiver<Event>,
    handle: Option<JoinHandle<P>>,
    abort: Arc<AtomicBool>,
    /// Rounds sent and not yet reported.
    pub outstanding: u64,
}

impl<P: Port + Send + 'static> Replayer<P> {
    /// Start a worker over `port`, replaying at most `batch` rounds an invocation.
    pub fn spawn(port: P, batch: usize, name: String) -> Self {
        let (tx, rx) = channel::<Job>();
        let (etx, events) = channel::<Event>();
        let abort = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&abort);
        let handle = std::thread::Builder::new()
            .name(name)
            .spawn(move || work(port, &rx, &etx, batch.max(1), &stop))
            .expect("a replay worker's thread");
        Replayer {
            tx: Some(tx),
            events,
            handle: Some(handle),
            abort,
            outstanding: 0,
        }
    }
}

impl<P> Replayer<P> {
    fn send(&self, job: Job) -> Result<(), String> {
        self.tx
            .as_ref()
            .ok_or_else(|| String::from("the replay worker has stopped"))?
            .send(job)
            .map_err(|_| String::from("the replay worker has stopped"))
    }

    /// Queue a committed round for replay.
    pub fn replay(&mut self, job: ReplayJob) -> Result<(), String> {
        self.outstanding += 1;
        self.send(Job::Replay(std::boxed::Box::new(job)))
    }

    /// An invocation of the Agent, after every replay queued before it; `build` makes the
    /// message and its caller blobs with the port's blob keys.
    pub fn invoke(
        &self,
        build: impl FnOnce(&dyn Port) -> (Vec<u8>, Vec<Vec<u8>>) + Send + 'static,
    ) -> Result<Vec<u8>, String> {
        let (back, rx) = channel();
        self.send(Job::Invoke {
            build: std::boxed::Box::new(build),
            back,
        })?;
        rx.recv()
            .map_err(|_| String::from("the replay worker has stopped"))?
    }

    /// A read of the Agent's committed rows, after every replay queued before it.
    pub fn read(&self, query: Vec<u8>) -> Result<Vec<u8>, String> {
        let (back, rx) = channel();
        self.send(Job::Read { query, back })?;
        rx.recv()
            .map_err(|_| String::from("the replay worker has stopped"))?
    }

    /// The port's measurements, after every replay queued before.
    pub fn stats(&self) -> Result<PortStats, String> {
        let (back, rx) = channel();
        self.send(Job::Stats { back })?;
        rx.recv()
            .map_err(|_| String::from("the replay worker has stopped"))?
    }

    /// The events reported so far, without waiting.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(e) = self.events.try_recv() {
            out.push(e);
        }
        self.count(&out);
        out
    }

    /// Wait up to `timeout` for one event, then take every other already reported.
    pub fn wait(&mut self, timeout: Duration) -> Vec<Event> {
        let mut out = Vec::new();
        match self.events.recv_timeout(timeout) {
            Ok(e) => out.push(e),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return out,
        }
        while let Ok(e) = self.events.try_recv() {
            out.push(e);
        }
        self.count(&out);
        out
    }

    fn count(&mut self, events: &[Event]) {
        for e in events {
            match e {
                Event::Confirmed { .. } | Event::Diverged { .. } => {
                    self.outstanding = self.outstanding.saturating_sub(1);
                }
                Event::Failed(_) => self.outstanding = 0,
            }
        }
    }

    /// Drop every replay queued and not yet sent (a measurement that ends before the replay has
    /// caught up). Nothing is confirmed after it.
    pub fn abort(&mut self) {
        self.abort.store(true, Ordering::SeqCst);
        self.outstanding = 0;
    }

    /// Stop the worker and take its port back (every job queued is done first).
    pub fn stop(mut self) -> Option<P> {
        self.tx = None;
        self.handle.take().and_then(|h| h.join().ok())
    }
}

impl<P> Drop for Replayer<P> {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn work<P: Port>(
    mut port: P,
    rx: &Receiver<Job>,
    events: &Sender<Event>,
    batch: usize,
    abort: &AtomicBool,
) -> P {
    // Set once a replay diverged or failed: later replays are dropped, as the Agent would
    // refuse them; the host re-decides those rounds from its log.
    let mut stopped = false;
    let mut carry: Option<Job> = None;
    loop {
        let job = match carry.take() {
            Some(j) => j,
            None => match rx.recv() {
                Ok(j) => j,
                Err(_) => return port,
            },
        };
        match job {
            Job::Replay(first) => {
                let mut jobs = vec![*first];
                while jobs.len() < batch {
                    match rx.try_recv() {
                        Ok(Job::Replay(j)) => jobs.push(*j),
                        Ok(other) => {
                            carry = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                if stopped || abort.load(Ordering::SeqCst) {
                    continue;
                }
                for e in replay(&mut port, &jobs) {
                    stopped |= !matches!(e, Event::Confirmed { .. });
                    if events.send(e).is_err() {
                        return port;
                    }
                }
            }
            Job::Invoke { build, back } => {
                let (message, blobs) = build(&port);
                let r = port.invoke(message, blobs);
                if let Ok(bytes) = &r {
                    // The fence's lift lets replays through again.
                    if matches!(agent::decode::<Reply>(bytes), Ok(Reply::Resumed)) {
                        stopped = false;
                    }
                }
                let _ = back.send(r);
            }
            Job::Read { query, back } => {
                let _ = back.send(port.read(query));
            }
            Job::Stats { back } => {
                let _ = back.send(Ok(port.stats()));
            }
        }
    }
}

/// One replay invocation of `jobs`, and what it came to, one event per round reported.
fn replay<P: Port>(port: &mut P, jobs: &[ReplayJob]) -> Vec<Event> {
    let privates: Vec<Private> = jobs.iter().map(|j| j.private.clone()).collect();
    let blob = agent::encode(&privates);
    let key = port.blob_key(&blob);
    let message = agent::encode(&Envelope::Replay {
        rounds: jobs.iter().map(|j| j.round.clone()).collect(),
        private: key,
        expected: jobs.iter().map(|j| j.expected).collect(),
    });
    let reply = port.invoke(message, vec![blob]);
    let at = Instant::now();
    let replayed = match reply.and_then(|b| agent::decode::<Reply>(&b)) {
        Ok(Reply::Replayed(r)) => r,
        Ok(Reply::Refused(why)) => return vec![Event::Failed(format!("refused: {why}"))],
        Ok(other) => return vec![Event::Failed(format!("{other:?}"))],
        Err(e) => return vec![Event::Failed(e.to_string())],
    };
    let mut out = Vec::with_capacity(jobs.len());
    for (j, got) in jobs.iter().zip(&replayed.digests) {
        if *got == j.expected {
            out.push(Event::Confirmed { seq: j.seq, at });
        } else {
            let fence = replayed.diverged.unwrap_or(Fence {
                round: j.seq,
                expected: j.expected,
                got: *got,
            });
            out.push(Event::Diverged { fence, at });
            return out;
        }
    }
    if replayed.digests.len() != jobs.len() {
        out.push(Event::Failed(format!(
            "the Agent replayed {} rounds of {}",
            replayed.digests.len(),
            jobs.len()
        )));
    }
    out
}
