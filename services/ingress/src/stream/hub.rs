// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Ingress's end of the core stream (`F-038` §1, §6, §9): the listener the core hosts dial, and
//! [`StreamCore`], the [`Core`] whose calls go down the streams they opened.
//!
//! Every connection authenticates before a frame is read past its [`Hello`]: by its client
//! certificate over TLS ([`super::tls`]), or by its peer's user id and the link's token on a Unix
//! socket ([`LinkAuth`]). A session then serves until the stream closes or falls silent past
//! [`Timings::dead_after`].
//!
//! [`StreamCore`] sends a feed down the newest live session whose core writes the feed's partition,
//! and a read down the newest live session that holds the principals role (credentials, ties) or
//! writes the book (the rest). With no session, a feed is unavailable (nothing ran) and a read is
//! unavailable: the replica answers credentials from its copy (`DEC-27` (e″)) and a query
//! `ERR-DecisionUnavailable`. A session lost with a call pending answers a feed as ambiguous and a
//! read as unavailable.
//!
//! **The roster** (`T-038-21`): [`advertise`] keeps this instance in the fleet's roster in
//! ingress's database; every greeting carries the live roster, and a change of it is sent down
//! every live stream ([`StreamCore::set_roster`]), so each core keeps one stream to every instance.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ledger_core::account::Time;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use super::{Greeting, Hello, Msg, RosterEntry, Timings, VERSIONS, negotiate, recv, send};
use crate::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, OutboxPage, Query,
    QueryReply,
};
use crate::feed::{BOOK, Feed, FeedReply};
use crate::remote::{BatchAnswer, LinkAuth, Request, Response};

/// Where ingress listens for cores.
pub enum Listen {
    /// Mutual TLS over TCP.
    Tls {
        /// The bound listener.
        listener: tokio::net::TcpListener,
        /// The acceptor: ingress's certificate, client certificates from the operator's CA.
        acceptor: tokio_rustls::TlsAcceptor,
    },
    /// A Unix socket on this host, with peer credentials and the link's token.
    Unix {
        /// The bound listener.
        listener: tokio::net::UnixListener,
        /// Who it lets in.
        auth: LinkAuth,
    },
}

struct Session {
    id: u64,
    core: String,
    partitions: Vec<String>,
    principals: bool,
    out: mpsc::UnboundedSender<Msg>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Response>>>,
    next_call: AtomicU64,
    closed: AtomicBool,
}

impl Session {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<Response>>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn live(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }
}

struct Hub {
    instance: String,
    timings: Timings,
    sessions: Mutex<Vec<Arc<Session>>>,
    draining: AtomicBool,
    next_session: AtomicU64,
    /// The fleet's live instances as this one last read them ([`advertise`]).
    roster: Mutex<Vec<RosterEntry>>,
}

/// The core behind the streams the core hosts opened to this instance (module documentation).
#[derive(Clone)]
pub struct StreamCore {
    hub: Arc<Hub>,
}

impl std::fmt::Debug for StreamCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamCore")
            .field("instance", &self.hub.instance)
            .field("sessions", &self.sessions())
            .finish()
    }
}

impl StreamCore {
    /// An instance named `instance`, with no core connected yet.
    pub fn new(instance: &str, timings: Timings) -> StreamCore {
        StreamCore {
            hub: Arc::new(Hub {
                instance: instance.to_owned(),
                timings,
                sessions: Mutex::new(Vec::new()),
                draining: AtomicBool::new(false),
                next_session: AtomicU64::new(1),
                roster: Mutex::new(Vec::new()),
            }),
        }
    }

    /// This instance's name.
    pub fn instance(&self) -> &str {
        &self.hub.instance
    }

    /// How many streams cores have opened to this instance since it started.
    pub fn streams_opened(&self) -> u64 {
        self.hub.next_session.load(Ordering::SeqCst) - 1
    }

    /// The roster every greeting carries from now on (`T-038-21`); when it changed, it is sent
    /// down every live stream too, so each core drops the instances that left and dials the ones
    /// that came.
    pub fn set_roster(&self, roster: Vec<RosterEntry>) {
        {
            let mut held = self.hub.roster.lock().unwrap_or_else(|p| p.into_inner());
            if *held == roster {
                return;
            }
            held.clone_from(&roster);
        }
        for s in self.all().iter().filter(|s| s.live()) {
            let _ = s.out.send(Msg::Roster {
                roster: roster.clone(),
            });
        }
    }

    /// The roster greetings carry.
    pub fn roster(&self) -> Vec<RosterEntry> {
        self.hub
            .roster
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn all(&self) -> std::sync::MutexGuard<'_, Vec<Arc<Session>>> {
        self.hub.sessions.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The live sessions, as (core, partitions).
    pub fn sessions(&self) -> Vec<(String, Vec<String>)> {
        self.all()
            .iter()
            .filter(|s| s.live())
            .map(|s| (s.core.clone(), s.partitions.clone()))
            .collect()
    }

    fn newest(&self, f: impl Fn(&Session) -> bool) -> Option<Arc<Session>> {
        self.all()
            .iter()
            .filter(|s| s.live() && f(s))
            .max_by_key(|s| s.id)
            .cloned()
    }

    fn writer_of(&self, partition: &str) -> Option<Arc<Session>> {
        self.newest(|s| s.partitions.iter().any(|p| p == partition))
    }

    fn reader(&self, principals: bool) -> Option<Arc<Session>> {
        if principals && let Some(s) = self.newest(|s| s.principals) {
            return Some(s);
        }
        self.writer_of(BOOK).or_else(|| self.newest(|_| true))
    }

    /// Whether this instance drains.
    pub fn draining(&self) -> bool {
        self.hub.draining.load(Ordering::SeqCst)
    }

    /// Drain (`F-038` §9): refuse new streams as draining, and send every core a go-away, so it
    /// dials a new stream elsewhere. The streams stay open until the cores close them.
    pub fn go_away(&self, reason: &str) {
        self.hub.draining.store(true, Ordering::SeqCst);
        for s in self.all().iter().filter(|s| s.live()) {
            let _ = s.out.send(Msg::GoAway {
                reason: reason.to_owned(),
            });
        }
    }

    /// Send `request` down `s`. `Err(false)`: not sent. `Err(true)`: sent and not answered.
    async fn call(&self, s: &Session, request: Request) -> Result<Response, bool> {
        let id = s.next_call.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        s.pending().insert(id, tx);
        if !s.live() || s.out.send(Msg::Call { id, request }).is_err() {
            s.pending().remove(&id);
            return Err(false);
        }
        match tokio::time::timeout(self.hub.timings.call_timeout, rx).await {
            Ok(Ok(r)) => Ok(r),
            _ => {
                s.pending().remove(&id);
                Err(true)
            }
        }
    }

    async fn read(&self, principals: bool, request: Request) -> Result<Response, CoreError> {
        let s = self.reader(principals).ok_or(CoreError::Unavailable)?;
        match self.call(&s, request).await {
            Ok(Response::Unavailable | Response::Ambiguous | Response::Unauthenticated)
            | Err(_) => Err(CoreError::Unavailable),
            Ok(r) => Ok(r),
        }
    }

    /// Serve cores on `listen` until the future is dropped, which closes every stream it took.
    pub async fn serve(&self, listen: Listen) -> std::io::Result<()> {
        let mut conns = tokio::task::JoinSet::new();
        match listen {
            Listen::Tls { listener, acceptor } => loop {
                while conns.try_join_next().is_some() {}
                let (tcp, peer) = listener.accept().await?;
                let me = self.clone();
                let acceptor = acceptor.clone();
                conns.spawn(async move {
                    let t = me.hub.timings.hello_timeout;
                    match tokio::time::timeout(t, acceptor.accept(tcp)).await {
                        Ok(Ok(tls)) => me.connection(tls, None).await,
                        _ => tracing::warn!(
                            %peer,
                            "core stream: a TLS handshake failed or timed out, refused"
                        ),
                    }
                });
            },
            Listen::Unix { listener, auth } => {
                let auth = Arc::new(auth);
                loop {
                    while conns.try_join_next().is_some() {}
                    let (s, _) = listener.accept().await?;
                    let Some(uid) = s.peer_cred().ok().map(|c| c.uid()) else {
                        tracing::warn!(
                            "core stream: a connection without peer credentials, refused"
                        );
                        continue;
                    };
                    let me = self.clone();
                    let auth = Arc::clone(&auth);
                    conns.spawn(async move { me.connection(s, Some((uid, auth))).await });
                }
            }
        }
    }

    async fn connection<S>(&self, mut s: S, unix: Option<(u32, Arc<LinkAuth>)>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let t = self.hub.timings;
        let hello: Option<Hello> = tokio::time::timeout(t.hello_timeout, recv(&mut s))
            .await
            .ok()
            .and_then(Result::ok);
        let refused = |reason: &str| Greeting::Refused {
            reason: reason.to_owned(),
        };
        let Some(hello) = hello else {
            let _ = send(&mut s, &refused("malformed")).await;
            return;
        };
        if let Some((uid, auth)) = &unix
            && !hello
                .token
                .as_deref()
                .is_some_and(|tok| auth.admits(Some(*uid), tok))
        {
            tracing::warn!(uid, "core stream: a core refused (not authenticated)");
            let _ = send(&mut s, &refused("unauthenticated")).await;
            return;
        }
        let Some(version) = negotiate(VERSIONS, &hello.versions) else {
            let _ = send(&mut s, &refused("version")).await;
            return;
        };
        if hello.partitions.is_empty() {
            let _ = send(&mut s, &refused("malformed")).await;
            return;
        }
        if self.draining() {
            let _ = send(&mut s, &refused("draining")).await;
            return;
        }
        let welcome = Greeting::Welcome {
            version,
            instance: self.hub.instance.clone(),
            roster: self.roster(),
        };
        if send(&mut s, &welcome).await.is_err() || hello.roster_only {
            // A roster read (`T-038-21`): answered, and closed with no stream.
            return;
        }
        let (out, rx) = mpsc::unbounded_channel();
        let session = Arc::new(Session {
            id: self.hub.next_session.fetch_add(1, Ordering::SeqCst),
            core: hello.core.clone(),
            partitions: hello.partitions.clone(),
            principals: hello.principals,
            out,
            pending: Mutex::new(HashMap::new()),
            next_call: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        self.all().push(Arc::clone(&session));
        tracing::info!(core = %hello.core, partitions = ?hello.partitions, version, "core stream open");
        // Unregistered however the session ends, a dropped task included.
        let _registered = Registered {
            core: self.clone(),
            session: Arc::clone(&session),
        };
        self.run(s, Arc::clone(&session), rx).await;
    }

    async fn run<S>(&self, s: S, session: Arc<Session>, mut rx: mpsc::UnboundedReceiver<Msg>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let t = self.hub.timings;
        let (mut r, mut w) = tokio::io::split(s);
        let _writer = super::dial::AbortOnDrop(tokio::spawn(async move {
            let mut ping = tokio::time::interval(t.keepalive);
            loop {
                let msg = tokio::select! {
                    m = rx.recv() => match m { Some(m) => m, None => return },
                    _ = ping.tick() => Msg::Ping,
                };
                if send(&mut w, &msg).await.is_err() {
                    return;
                }
            }
        }));
        loop {
            let msg: Msg = match tokio::time::timeout(t.dead_after, recv(&mut r)).await {
                Ok(Ok(m)) => m,
                Ok(Err(_)) => break,
                Err(_) => {
                    tracing::warn!(core = %session.core, "core stream: silent past dead_after");
                    break;
                }
            };
            match msg {
                Msg::Answer { id, response } => {
                    if let Some(tx) = session.pending().remove(&id) {
                        let _ = tx.send(response);
                    }
                }
                Msg::Ping => {
                    let _ = session.out.send(Msg::Pong);
                }
                Msg::Pong => {}
                // A core never calls ingress, nor sends it away.
                Msg::Call { .. } | Msg::GoAway { .. } | Msg::Roster { .. } => break,
            }
        }
    }
}

/// Keep `core`'s instance in the fleet's roster at `address` (`T-038-21`), renewing its entry every
/// [`Timings::roster`] for [`Timings::roster_ttl`], and hand the live roster to every greeting it
/// sends from then on. Once the instance drains, its entry is withdrawn, so the cores drop its
/// stream, and the task ends. A store that does not answer leaves the last roster in place.
pub async fn advertise<S: crate::store::Store>(store: S, core: StreamCore, address: String) {
    let t = core.hub.timings;
    loop {
        if core.draining() {
            core.set_roster(Vec::new());
            if let Err(e) = store.withdraw(core.instance()).await {
                tracing::warn!(error = %e, "the roster entry was not withdrawn; it lapses instead");
            }
            return;
        }
        match store
            .advertise(core.instance(), &address, t.roster_ttl())
            .await
        {
            Ok(roster) => core.set_roster(roster),
            Err(e) => tracing::warn!(error = %e, "the roster was not renewed"),
        }
        tokio::time::sleep(t.roster).await;
    }
}

/// A session's registration: dropped, the session is closed, its pending calls fail (a feed as
/// ambiguous, a read as unavailable) and it leaves the hub.
struct Registered {
    core: StreamCore,
    session: Arc<Session>,
}

impl Drop for Registered {
    fn drop(&mut self) {
        self.session.closed.store(true, Ordering::SeqCst);
        self.session.pending().clear();
        let id = self.session.id;
        self.core.all().retain(|x| x.id != id);
        tracing::warn!(core = %self.session.core, "core stream closed");
    }
}

impl Core for StreamCore {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        let Some(s) = self.writer_of(BOOK) else {
            return Err(CoreError::Unavailable);
        };
        let request = Request::Apply {
            input: input.clone(),
        };
        match self.call(&s, request).await {
            Ok(Response::Applied { reply }) => Ok(reply),
            Ok(Response::Unavailable) | Err(false) => Err(CoreError::Unavailable),
            Ok(_) | Err(true) => Err(CoreError::Ambiguous),
        }
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        let all = |e: CoreError| vec![Err(e); inputs.len()];
        let Some(s) = self.writer_of(BOOK) else {
            return all(CoreError::Unavailable);
        };
        let request = Request::ApplyBatch {
            inputs: inputs.to_vec(),
        };
        match self.call(&s, request).await {
            Ok(Response::Replies { replies }) if replies.len() == inputs.len() => {
                replies.into_iter().map(BatchAnswer::result).collect()
            }
            Ok(Response::Unavailable) | Err(false) => all(CoreError::Unavailable),
            Ok(_) | Err(true) => all(CoreError::Ambiguous),
        }
    }

    async fn feed(&self, feed: &Feed) -> FeedReply {
        let Some(s) = self.writer_of(&feed.partition) else {
            return FeedReply::Unavailable;
        };
        match self.call(&s, Request::Feed { feed: feed.clone() }).await {
            Ok(Response::Fed { reply }) => reply,
            Ok(Response::Unavailable) | Err(false) => FeedReply::Unavailable,
            Ok(_) | Err(true) => FeedReply::Ambiguous,
        }
    }

    fn can_feed(&self, partition: &str) -> bool {
        !self.draining() && self.writer_of(partition).is_some()
    }

    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: Time,
    ) -> Result<OutboxPage, CoreError> {
        let request = Request::Outbox {
            cursor: cursor.map(str::to_owned),
            undelivered,
            now,
        };
        match self.read(false, request).await? {
            Response::Outbox { page } => Ok(page),
            _ => Err(CoreError::Unavailable),
        }
    }

    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        match self.read(false, Request::LastTick).await? {
            Response::LastTick { at } => Ok(at),
            _ => Err(CoreError::Unavailable),
        }
    }

    async fn next_due(&self) -> Result<Option<Time>, CoreError> {
        match self.read(false, Request::NextDue).await? {
            Response::NextDue { at } => Ok(at),
            _ => Err(CoreError::Unavailable),
        }
    }

    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> Result<QueryReply, CoreError> {
        let request = Request::Query {
            caller: caller.clone(),
            query: query.clone(),
            now,
        };
        match self.read(false, request).await? {
            Response::Query { reply } => Ok(reply),
            _ => Err(CoreError::Unavailable),
        }
    }

    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        let request = Request::Credential {
            credential: credential.to_owned(),
        };
        match self.read(true, request).await? {
            Response::Credential { found } => Ok(found),
            _ => Err(CoreError::Unavailable),
        }
    }

    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        let request = Request::PartyOf {
            principal: principal.to_owned(),
        };
        match self.read(true, request).await? {
            Response::PartyOf { party } => Ok(party),
            _ => Err(CoreError::Unavailable),
        }
    }

    async fn genesis_open(&self) -> Result<bool, CoreError> {
        match self.read(false, Request::GenesisOpen).await? {
            Response::GenesisOpen { open } => Ok(open),
            _ => Err(CoreError::Unavailable),
        }
    }

    async fn stage_code(&self, code: &[u8; 32]) -> Result<crate::boundary::Staged, CoreError> {
        let request = Request::StageCode {
            code: hex::encode(code),
        };
        match self.read(false, request).await? {
            Response::Staged { staged } => Ok(staged),
            _ => Err(CoreError::Unavailable),
        }
    }
}
