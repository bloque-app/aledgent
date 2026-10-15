// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core host's end of the core stream (`F-038` §1, §9, `DEC-27` (c), (e)): it dials out to
//! every address it is given and answers ingress's calls from its core, which is the host's
//! [`crate::feed::Sequencer`] in front of the Agent.
//!
//! Per address, one stream at a time, kept up for ever: a failed dial or a lost stream is retried
//! with backoff (100 ms, doubling to 5 s); a [`Msg::GoAway`] dials a new stream at once while the
//! old one keeps answering until ingress closes it. Feeds on one stream are answered in the order
//! they arrive, one after the other; reads are answered as they come, side by side.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rustls_pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use super::{Greeting, Hello, Msg, Timings, VERSIONS, recv, send};
use crate::boundary::Core;
use crate::remote::{LinkToken, Request, answer};

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

async fn connect(target: &Target, hello: &Hello, t: Timings) -> Result<Box<dyn Duplex>, String> {
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
        Target::Unix { path, .. } => Box::new(
            tokio::net::UnixStream::connect(path)
                .await
                .map_err(|e| e.to_string())?,
        ),
    };
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
        Greeting::Welcome { version, instance } => {
            tracing::info!(%instance, version, "core stream open");
            Ok(stream)
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

/// One stream's session: until the stream closes or falls silent. A dropped session closes its
/// stream; a feed already handed to the core finishes there, unanswered (it is resent and answered
/// from the core's replies).
async fn session<C: Core>(
    stream: Box<dyn Duplex>,
    core: Arc<C>,
    t: Timings,
    away: tokio::sync::oneshot::Sender<()>,
) {
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
                let response = answer(&*core, request).await;
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
            Msg::Answer { .. } => break,
        }
    }
}

async fn maintain<C: Core>(target: Target, hello: Hello, core: Arc<C>, t: Timings) {
    const FIRST: Duration = Duration::from_millis(100);
    const MOST: Duration = Duration::from_secs(5);
    let mut backoff = FIRST;
    // Sessions sent away, answering on until ingress closes them; dropped with this task.
    let mut old = tokio::task::JoinSet::new();
    loop {
        while old.try_join_next().is_some() {}
        match connect(&target, &hello, t).await {
            Ok(stream) => {
                backoff = FIRST;
                let (away_tx, mut away_rx) = tokio::sync::oneshot::channel();
                let mut live = Box::pin(session(stream, Arc::clone(&core), t, away_tx));
                let away = tokio::select! {
                    r = &mut away_rx => r.is_ok(),
                    _ = &mut live => false,
                };
                if away {
                    old.spawn(live);
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

/// Dial every target of `config` and answer from `core`, for ever (module documentation).
/// Dropping the future closes every stream it opened.
pub async fn dial<C: Core>(config: DialConfig, core: Arc<C>) {
    let hello = Hello {
        versions: VERSIONS.to_vec(),
        core: config.core.clone(),
        partitions: config.partitions.clone(),
        principals: config.principals,
        token: None,
    };
    let mut set = tokio::task::JoinSet::new();
    for target in config.targets {
        set.spawn(maintain(
            target,
            hello.clone(),
            Arc::clone(&core),
            config.timings,
        ));
    }
    while set.join_next().await.is_some() {}
}
