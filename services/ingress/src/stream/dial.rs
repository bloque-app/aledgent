// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core host's end of the core stream (`F-038` §1, §9, `DEC-27` (c), (e)): it dials out to
//! every address it is given and answers ingress's calls from its core, which is the host's
//! [`crate::feed::Sequencer`] in front of the Agent.
//!
//! On a Unix socket the core host checks the peer's user id against the configured
//! `stream.dial.peer_uids` before it sends its token, as ingress checks the core host's.
//!
//! **The roster** (`T-038-21`): the configured addresses are the fleet's stable names; the host
//! reads the roster from them and keeps one stream to every instance in it, dialed directly
//! ([`dial`]). Each stream is a link of its own: a feed is answered through
//! [`Core::feed_on`] with its link, so the host's sequencer refuses the lease's epoch on any
//! stream but the one that brought it, and the others only read.
//!
//! Per address, one stream at a time, kept up for ever: a failed dial or a lost stream is retried
//! with backoff (100 ms, doubling to 5 s); a [`Msg::GoAway`] dials a new stream at once while the
//! old one keeps answering until ingress closes it. Feeds on one stream are answered in the order
//! they arrive, one after the other; reads are answered as they come, side by side.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use super::{Greeting, Hello, Msg, RosterEntry, Timings, VERSIONS, recv, send};
use crate::boundary::Core;
use crate::remote::{LinkToken, Request, Response, answer};

/// One address the core host dials.
#[derive(Clone)]
pub enum Target {
    /// Mutual TLS over TCP.
    Tls {
        /// `host:port`.
        addr: String,
        /// The name ingress's certificate must carry.
        server_name: ServerName<'static>,
        /// This side's TLS.
        connector: tokio_rustls::TlsConnector,
    },
    /// A Unix socket on this host.
    Unix {
        /// Its path.
        path: PathBuf,
        /// The user ids ingress may run as: the socket's peer credentials are checked against
        /// them before the token is sent, so a process that took the socket's path gets nothing.
        /// Empty admits no one.
        peer_uids: Vec<u32>,
        /// The link's token.
        token: LinkToken,
    },
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Tls {
                addr, server_name, ..
            } => write!(f, "tls:{addr} ({})", server_name.to_str()),
            Target::Unix { path, .. } => write!(f, "unix:{}", path.display()),
        }
    }
}

/// How the core host dials.
#[derive(Clone, Debug)]
pub struct DialConfig {
    /// Where.
    pub targets: Vec<Target>,
    /// This core's name, for ingress's logs.
    pub core: String,
    /// The partitions it writes.
    pub partitions: Vec<String>,
    /// Whether it holds the principals role.
    pub principals: bool,
    /// The stream's timings.
    pub timings: Timings,
}

trait Duplex: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Duplex for T {}

impl Target {
    /// The target of the roster's `address`, reached as `self` is: the same TLS connector and
    /// server name (the fleet's certificate names the stable name, whatever address it is dialed
    /// at), or the same token and user ids on a Unix socket. `None` for another transport.
    fn sibling(&self, address: &str) -> Option<Target> {
        match (self, super::Address::parse(address).ok()?) {
            (
                Target::Tls {
                    server_name,
                    connector,
                    ..
                },
                super::Address::Tls(addr),
            ) => Some(Target::Tls {
                addr,
                server_name: server_name.clone(),
                connector: connector.clone(),
            }),
            (
                Target::Unix {
                    peer_uids, token, ..
                },
                super::Address::Unix(path),
            ) => Some(Target::Unix {
                path,
                peer_uids: peer_uids.clone(),
                token: token.clone(),
            }),
            _ => None,
        }
    }
}

/// Dial `target`: the stream, once welcomed, and the roster its greeting carried.
async fn connect(
    target: &Target,
    hello: &Hello,
    t: Timings,
) -> Result<(Box<dyn Duplex>, Vec<RosterEntry>), String> {
    let mut stream: Box<dyn Duplex> = match target {
        Target::Tls {
            addr,
            server_name,
            connector,
        } => {
            let tcp = tokio::net::TcpStream::connect(addr)
                .await
                .map_err(|e| e.to_string())?;
            let tls =
                tokio::time::timeout(t.hello_timeout, connector.connect(server_name.clone(), tcp))
                    .await
                    .map_err(|_| "the TLS handshake timed out".to_owned())?
                    .map_err(|e| format!("the TLS handshake: {e}"))?;
            Box::new(tls)
        }
        Target::Unix {
            path, peer_uids, ..
        } => {
            let s = tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| e.to_string())?;
            // The server side's check, the other way (`DEC-27` (h′)): the token goes only to a
            // peer running as one of ingress's user ids.
            let uid = s
                .peer_cred()
                .map_err(|e| format!("the socket's peer credentials: {e}"))?
                .uid();
            if !peer_uids.contains(&uid) {
                return Err(format!(
                    "the socket's peer runs as user id {uid}, not one of ingress's: no token sent"
                ));
            }
            Box::new(s)
        }
    };
    let roster_only = hello.roster_only;
    let mut hello = hello.clone();
    if let Target::Unix { token, .. } = target {
        hello.token = Some(token.expose().to_owned());
    }
    send(&mut stream, &hello).await.map_err(|e| e.to_string())?;
    drop(hello);
    let greeting: Greeting = tokio::time::timeout(t.hello_timeout, recv(&mut stream))
        .await
        .map_err(|_| "no greeting".to_owned())?
        .map_err(|e| e.to_string())?;
    match greeting {
        Greeting::Welcome {
            version,
            instance,
            roster,
        } => {
            if !roster_only {
                tracing::info!(%instance, version, "core stream open");
            }
            Ok((stream, roster))
        }
        Greeting::Refused { reason } => Err(format!("refused: {reason}")),
    }
}

/// Aborts its task when dropped: a session's writer lives exactly as long as the session.
pub(crate) struct AbortOnDrop(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// The next link id a stream's session takes (`T-038-21`): the host's sequencer binds the feed
/// lease's epoch to the link that brought it.
static NEXT_LINK: AtomicU64 = AtomicU64::new(1);

/// Tells the core a link closed, however its session ends (a dropped task included).
struct LinkGuard<C: Core> {
    core: Arc<C>,
    link: u64,
}

impl<C: Core> Drop for LinkGuard<C> {
    fn drop(&mut self) {
        let (core, link) = (Arc::clone(&self.core), self.link);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { core.link_closed(link).await });
        }
    }
}

/// Answer `request` on link `link`: a feed through [`Core::feed_on`], so the sequencer knows which
/// stream brought it; anything else as the socket link answers it.
async fn answer_on<C: Core>(core: &C, request: Request, link: u64) -> Response {
    match request {
        Request::Feed { feed } => Response::Fed {
            reply: core.feed_on(&feed, link).await,
        },
        other => answer(core, other).await,
    }
}

/// What a stream tells [`dial`]'s roster keeper.
enum Event {
    /// The roster a greeting or a [`Msg::Roster`] carried.
    Roster(Vec<RosterEntry>),
    /// An instance sent this host away: read the roster again from the stable name.
    GoAway,
}

/// Counts a live session, however it ends.
struct Live(Arc<AtomicUsize>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One stream's session: until the stream closes or falls silent. A dropped session closes its
/// stream; a feed already handed to the core finishes there, unanswered (it is resent and answered
/// from the core's replies).
async fn session<C: Core>(
    stream: Box<dyn Duplex>,
    core: Arc<C>,
    t: Timings,
    away: tokio::sync::oneshot::Sender<()>,
    events: mpsc::UnboundedSender<Event>,
    live: Arc<AtomicUsize>,
) {
    live.fetch_add(1, Ordering::SeqCst);
    let _live = Live(live);
    let link = NEXT_LINK.fetch_add(1, Ordering::SeqCst);
    let _link = LinkGuard {
        core: Arc::clone(&core),
        link,
    };
    let (mut r, mut w) = tokio::io::split(stream);
    let (out, mut rx) = mpsc::unbounded_channel::<Msg>();
    let _writer = AbortOnDrop(tokio::spawn(async move {
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
    // Feeds, one after the other, in the order they arrived. The feeder outlives a dropped session
    // only to finish the feed in hand: its queue closes with the session.
    let (feeds, mut queued) = mpsc::unbounded_channel::<(u64, Request)>();
    {
        let core = Arc::clone(&core);
        let out = out.clone();
        tokio::spawn(async move {
            while let Some((id, request)) = queued.recv().await {
                let response = answer_on(&*core, request, link).await;
                let _ = out.send(Msg::Answer { id, response });
            }
        });
    }
    let mut away = Some(away);
    loop {
        let msg: Msg = match tokio::time::timeout(t.dead_after, recv(&mut r)).await {
            Ok(Ok(m)) => m,
            Ok(Err(_)) => break,
            Err(_) => {
                tracing::warn!("core stream: ingress silent past dead_after; dialing again");
                break;
            }
        };
        match msg {
            Msg::Call { id, request } => {
                if matches!(request, Request::Feed { .. }) {
                    let _ = feeds.send((id, request));
                } else {
                    let core = Arc::clone(&core);
                    let out = out.clone();
                    tokio::spawn(async move {
                        let response = answer(&*core, request).await;
                        let _ = out.send(Msg::Answer { id, response });
                    });
                }
            }
            Msg::Ping => {
                let _ = out.send(Msg::Pong);
            }
            Msg::Pong => {}
            Msg::GoAway { reason } => {
                tracing::info!(%reason, "core stream: go-away; dialing a new stream");
                if let Some(a) = away.take() {
                    let _ = a.send(());
                }
            }
            // An empty roster from one instance (it has not advertised yet) changes nothing; only
            // the stable name's answer turns this host back to its configured targets.
            Msg::Roster { roster } if !roster.is_empty() => {
                let _ = events.send(Event::Roster(roster));
            }
            Msg::Roster { .. } => {}
            Msg::Answer { .. } => break,
        }
    }
}

/// Keep one stream to `target` up, for ever. A go-away dials a new one at once, and asks the
/// roster to be read again; every greeting's roster goes to the roster keeper.
async fn maintain<C: Core>(
    target: Target,
    hello: Hello,
    core: Arc<C>,
    t: Timings,
    events: mpsc::UnboundedSender<Event>,
    live: Arc<AtomicUsize>,
) {
    const FIRST: Duration = Duration::from_millis(100);
    const MOST: Duration = Duration::from_secs(5);
    let mut backoff = FIRST;
    // Sessions sent away, answering on until ingress closes them; dropped with this task.
    let mut old = tokio::task::JoinSet::new();
    loop {
        while old.try_join_next().is_some() {}
        match connect(&target, &hello, t).await {
            Ok((stream, roster)) => {
                backoff = FIRST;
                if !roster.is_empty() {
                    let _ = events.send(Event::Roster(roster));
                }
                let (away_tx, mut away_rx) = tokio::sync::oneshot::channel();
                let mut session = Box::pin(session(
                    stream,
                    Arc::clone(&core),
                    t,
                    away_tx,
                    events.clone(),
                    Arc::clone(&live),
                ));
                let away = tokio::select! {
                    r = &mut away_rx => r.is_ok(),
                    _ = &mut session => false,
                };
                if away {
                    let _ = events.send(Event::GoAway);
                    old.spawn(session);
                    continue;
                }
                tracing::warn!(?target, "core stream lost; dialing again");
            }
            Err(e) => tracing::warn!(?target, error = %e, "core stream: dial failed"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MOST);
    }
}

/// The roster, read from the first of `targets` that answers (`T-038-21`): `None` if none did.
async fn read_roster(targets: &[Target], hello: &Hello, t: Timings) -> Option<Vec<RosterEntry>> {
    let mut ask = hello.clone();
    ask.roster_only = true;
    for target in targets {
        match connect(target, &ask, t).await {
            Ok((_, roster)) => return Some(roster),
            Err(e) => tracing::debug!(?target, error = %e, "core stream: the roster not read"),
        }
    }
    None
}

/// Dial ingress and answer from `core`, for ever (module documentation). Dropping the future
/// closes every stream it opened.
///
/// The configured targets are the fleet's stable names. The host dials them at start, and the
/// greeting brings the roster; it reads the roster from them again on every go-away, and every
/// [`Timings::roster`] while no stream is up; in between, every stream's greeting and every
/// [`Msg::Roster`] ingress sends when the roster changes bring it. The
/// host keeps one stream to every instance in the roster, dialed directly at its advertised
/// address, and drops the streams of instances that left (`T-038-21`). While the roster is empty
/// (nothing advertised: one instance, a test), it keeps a stream to each configured target
/// instead, as before; while no target answers, it keeps the streams it has.
pub async fn dial<C: Core>(config: DialConfig, core: Arc<C>) {
    let hello = Hello {
        versions: VERSIONS.to_vec(),
        core: config.core.clone(),
        partitions: config.partitions.clone(),
        principals: config.principals,
        token: None,
        roster_only: false,
    };
    let t = config.timings;
    let (events_tx, mut events) = mpsc::unbounded_channel::<Event>();
    let live = Arc::new(AtomicUsize::new(0));
    let configured: Vec<(String, Target)> = config
        .targets
        .iter()
        .map(|target| (format!("{target:?}"), target.clone()))
        .collect();
    let mut streams: HashMap<String, AbortOnDrop> = HashMap::new();
    // At start, a stream to each configured target: its greeting brings the roster.
    let mut roster: Option<Vec<RosterEntry>> = None;
    loop {
        let wanted: Vec<(String, Target)> = match roster.take() {
            Some(r) if !r.is_empty() => r
                .iter()
                .filter_map(|e| {
                    let sibling = config.targets.iter().find_map(|c| c.sibling(&e.address));
                    if sibling.is_none() {
                        tracing::warn!(
                            instance = %e.instance,
                            address = %e.address,
                            "core stream: a roster address this host cannot dial"
                        );
                    }
                    sibling.map(|s| (e.address.clone(), s))
                })
                .collect(),
            Some(_) => configured.clone(),
            None if streams.is_empty() => configured.clone(),
            None => Vec::new(),
        };
        if !wanted.is_empty() {
            // New streams first, then the old ones go: an instance is never left without one.
            for (key, target) in &wanted {
                if !streams.contains_key(key) {
                    tracing::info!(?target, "core stream: dialing");
                    streams.insert(
                        key.clone(),
                        AbortOnDrop(tokio::spawn(maintain(
                            target.clone(),
                            hello.clone(),
                            Arc::clone(&core),
                            t,
                            events_tx.clone(),
                            Arc::clone(&live),
                        ))),
                    );
                }
            }
            streams.retain(|key, _| {
                let keep = wanted.iter().any(|(k, _)| k == key);
                if !keep {
                    tracing::info!(%key, "core stream: the instance left the roster; dropped");
                }
                keep
            });
        }
        roster = tokio::select! {
            e = events.recv() => match e {
                Some(Event::Roster(r)) => Some(r),
                Some(Event::GoAway) | None => read_roster(&config.targets, &hello, t).await,
            },
            _ = tokio::time::sleep(t.roster) => {
                if live.load(Ordering::SeqCst) == 0 {
                    read_roster(&config.targets, &hello, t).await
                } else {
                    None
                }
            }
        };
    }
}
