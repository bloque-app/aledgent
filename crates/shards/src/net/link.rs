// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The relay's links between hosts: a full mesh of authenticated sessions ([`super::noise`]),
//! one per pair of hosts, re-established whenever one ends.
//!
//! The higher host index dials the lower one, so each pair has one dialler; the coordinator
//! (host 0) only answers. A dialler retries until its peer answers. Every session begins with each
//! side's [`Msg::Hello`] (from [`Handler::hello`]), so the peer can send again what the previous
//! session may have lost. Messages for a peer wait in its queue while it is unreachable and are
//! sent, after the Hello, once a session is up; one written into a session that then fails is
//! lost, and the Hello's resumption covers it (every stream is idempotent at its receiver).
//!
//! A session idle for [`HEARTBEAT`] sends an empty frame; a session that has received nothing for
//! [`DEAD_AFTER`] is ended: a host that vanished without closing its sockets is found within it.

use std::collections::{BTreeMap, VecDeque};
use std::format;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::string::String;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use std::vec::Vec;

use super::noise::{self, Accept, Keypair, Session};
use super::wire::{self, Msg, NetCounters};

/// What the links ask of the host they serve.
pub trait Handler: Send + Sync + 'static {
    /// This host's Hello for `peer`, sent first in every session with it.
    fn hello(&self, peer: u16) -> Msg;
    /// A message from `peer`.
    fn on_msg(&self, peer: u16, msg: Msg);
    /// A session with `peer` began (`true`) or ended (`false`).
    fn on_session(&self, _peer: u16, _up: bool) {}
}

/// How often an idle session says it is alive.
pub const HEARTBEAT: Duration = Duration::from_secs(1);
/// How long a silent session lives.
pub const DEAD_AFTER: Duration = Duration::from_secs(5);

struct Peer {
    addr: Option<String>,
    key: [u8; 32],
    queue: VecDeque<Arc<Vec<u8>>>,
    /// The current session's number, 0 when none.
    session: u64,
    closer: Option<TcpStream>,
    dialling: bool,
}

struct Inner {
    me: u16,
    key: Keypair,
    net_id: Vec<u8>,
    handler: Arc<dyn Handler>,
    peers: Mutex<BTreeMap<u16, Peer>>,
    cv: Condvar,
    stop: AtomicBool,
    next_session: AtomicU64,
    sent: AtomicU64,
    sent_bytes: AtomicU64,
    received: AtomicU64,
    received_bytes: AtomicU64,
    sessions: AtomicU64,
}

/// One host's links (module documentation).
#[derive(Clone)]
pub struct Net {
    inner: Arc<Inner>,
    addr: SocketAddr,
}

impl Net {
    /// Listen on `listen` as host `me` with `key`, for the book's relay `net_id`.
    pub fn start(
        me: u16,
        key: Keypair,
        listen: SocketAddr,
        net_id: &[u8],
        handler: Arc<dyn Handler>,
    ) -> io::Result<Net> {
        let listener = TcpListener::bind(listen)?;
        let addr = listener.local_addr()?;
        let inner = Arc::new(Inner {
            me,
            key,
            net_id: net_id.to_vec(),
            handler,
            peers: Mutex::new(BTreeMap::new()),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            next_session: AtomicU64::new(1),
            sent: AtomicU64::new(0),
            sent_bytes: AtomicU64::new(0),
            received: AtomicU64::new(0),
            received_bytes: AtomicU64::new(0),
            sessions: AtomicU64::new(0),
        });
        let i = Arc::clone(&inner);
        std::thread::Builder::new()
            .name(String::from("relay-accept"))
            .spawn(move || accept_loop(i, listener))?;
        Ok(Net { inner, addr })
    }

    /// Where this host listens.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Know host `index`, at `addr` if this host dials it, with static key `key`. A higher index
    /// than this host's is only answered; a lower one is dialled.
    pub fn add_peer(&self, index: u16, addr: Option<String>, key: [u8; 32]) {
        if index == self.inner.me {
            return;
        }
        let dial = {
            let mut peers = self.inner.peers.lock().expect("peers");
            let p = peers.entry(index).or_insert_with(|| Peer {
                addr: None,
                key,
                queue: VecDeque::new(),
                session: 0,
                closer: None,
                dialling: false,
            });
            p.key = key;
            if addr.is_some() {
                p.addr = addr;
            }
            let dial = index < self.inner.me && p.addr.is_some() && !p.dialling;
            if dial {
                p.dialling = true;
            }
            dial
        };
        if dial {
            let i = Arc::clone(&self.inner);
            let _ = std::thread::Builder::new()
                .name(format!("relay-dial-{index}"))
                .spawn(move || dial_loop(i, index));
        }
    }

    /// Queue `msg` for `peer`.
    pub fn send(&self, peer: u16, msg: &Msg) {
        self.send_bytes(peer, Arc::new(wire::encode(msg)));
    }

    fn send_bytes(&self, peer: u16, bytes: Arc<Vec<u8>>) {
        let mut peers = self.inner.peers.lock().expect("peers");
        if let Some(p) = peers.get_mut(&peer) {
            p.queue.push_back(bytes);
        }
        drop(peers);
        self.inner.cv.notify_all();
    }

    /// Queue `msg` for every known peer.
    pub fn broadcast(&self, msg: &Msg) {
        let bytes = Arc::new(wire::encode(msg));
        let ids: Vec<u16> = self.peers();
        for p in ids {
            self.send_bytes(p, Arc::clone(&bytes));
        }
    }

    /// The known peers.
    pub fn peers(&self) -> Vec<u16> {
        self.inner
            .peers
            .lock()
            .expect("peers")
            .keys()
            .copied()
            .collect()
    }

    /// Whether a session with `peer` is up.
    pub fn connected(&self, peer: u16) -> bool {
        self.inner
            .peers
            .lock()
            .expect("peers")
            .get(&peer)
            .is_some_and(|p| p.session != 0)
    }

    /// End every session now (a fault, in tests): each is re-established by its dialler.
    pub fn drop_sessions(&self) {
        let mut peers = self.inner.peers.lock().expect("peers");
        for p in peers.values_mut() {
            if let Some(c) = p.closer.take() {
                let _ = c.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    /// The relay's traffic so far.
    pub fn counters(&self) -> NetCounters {
        let i = &self.inner;
        NetCounters {
            sent: i.sent.load(Ordering::Relaxed),
            sent_bytes: i.sent_bytes.load(Ordering::Relaxed),
            received: i.received.load(Ordering::Relaxed),
            received_bytes: i.received_bytes.load(Ordering::Relaxed),
            sessions: i.sessions.load(Ordering::Relaxed),
        }
    }

    /// Stop: end every session, dial no more.
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.drop_sessions();
        self.inner.cv.notify_all();
        // Unblock the acceptor.
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
    }
}

fn accept_loop(i: Arc<Inner>, listener: TcpListener) {
    for s in listener.incoming() {
        if i.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(s) = s else { continue };
        let i = Arc::clone(&i);
        let _ = std::thread::Builder::new()
            .name(String::from("relay-answer"))
            .spawn(move || {
                let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                let keys = Arc::clone(&i);
                let accept: Accept = Arc::new(move |k: &[u8; 32]| {
                    keys.peers
                        .lock()
                        .expect("peers")
                        .iter()
                        .find(|(_, p)| p.key == *k)
                        .map(|(id, _)| *id)
                });
                if let Ok(session) = noise::respond(s, &i.key, i.me, &accept, &i.net_id) {
                    run_session(&i, session);
                }
            });
    }
}

fn dial_loop(i: Arc<Inner>, peer: u16) {
    let mut backoff = Duration::from_millis(100);
    while !i.stop.load(Ordering::SeqCst) {
        let (addr, key) = {
            let peers = i.peers.lock().expect("peers");
            let p = &peers[&peer];
            (p.addr.clone().unwrap_or_default(), p.key)
        };
        let up = addr
            .parse::<SocketAddr>()
            .ok()
            .and_then(|a| TcpStream::connect_timeout(&a, Duration::from_secs(3)).ok())
            .and_then(|s| {
                let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                noise::initiate(s, &i.key, i.me, peer, &key, &i.net_id).ok()
            });
        match up {
            Some(session) => {
                backoff = Duration::from_millis(100);
                run_session(&i, session);
            }
            None => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// Serve one session until it ends: the writer on its own thread, the reader on this one.
fn run_session(i: &Arc<Inner>, session: Session) {
    let Session {
        peer,
        mut writer,
        mut reader,
    } = session;
    let id = i.next_session.fetch_add(1, Ordering::SeqCst);
    {
        let mut peers = i.peers.lock().expect("peers");
        let Some(p) = peers.get_mut(&peer) else {
            return;
        };
        // A newer session replaces an older one.
        if let Some(c) = p.closer.take() {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
        p.session = id;
        p.closer = writer.closer().ok();
    }
    i.sessions.fetch_add(1, Ordering::Relaxed);
    i.cv.notify_all();
    i.handler.on_session(peer, true);
    if let Ok(c) = writer.closer() {
        let _ = c.set_read_timeout(Some(DEAD_AFTER));
    }
    let wi = Arc::clone(i);
    let w = std::thread::Builder::new()
        .name(format!("relay-write-{peer}"))
        .spawn(move || {
            let hello = wire::encode(&wi.handler.hello(peer));
            if writer.send(&hello).is_err() {
                writer.shutdown();
                return;
            }
            let mut last = std::time::Instant::now();
            loop {
                let next = {
                    let mut peers = wi.peers.lock().expect("peers");
                    loop {
                        if wi.stop.load(Ordering::SeqCst) {
                            break None;
                        }
                        let p = peers.get_mut(&peer).expect("a known peer");
                        if p.session != id {
                            break None;
                        }
                        if let Some(m) = p.queue.pop_front() {
                            break Some(m);
                        }
                        if last.elapsed() >= HEARTBEAT {
                            break Some(Arc::new(Vec::new()));
                        }
                        peers = wi
                            .cv
                            .wait_timeout(peers, Duration::from_millis(200))
                            .expect("peers")
                            .0;
                    }
                };
                let Some(m) = next else { break };
                if writer.send(&m).is_err() {
                    break;
                }
                last = std::time::Instant::now();
                wi.sent.fetch_add(1, Ordering::Relaxed);
                wi.sent_bytes.fetch_add(m.len() as u64, Ordering::Relaxed);
            }
            writer.shutdown();
        });
    while let Ok(frame) = reader.recv() {
        i.received.fetch_add(1, Ordering::Relaxed);
        i.received_bytes
            .fetch_add(frame.len() as u64, Ordering::Relaxed);
        if frame.is_empty() {
            continue;
        }
        match wire::decode(&frame) {
            Ok(msg) => i.handler.on_msg(peer, msg),
            Err(_) => break,
        }
    }
    {
        let mut peers = i.peers.lock().expect("peers");
        if let Some(p) = peers.get_mut(&peer) {
            if p.session == id {
                p.session = 0;
                if let Some(c) = p.closer.take() {
                    let _ = c.shutdown(std::net::Shutdown::Both);
                }
            }
        }
    }
    i.cv.notify_all();
    if let Ok(w) = w {
        let _ = w.join();
    }
    i.handler.on_session(peer, false);
}
