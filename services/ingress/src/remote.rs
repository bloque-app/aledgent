// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core across a process boundary (`F-005`).
//!
//! The core decides inside a VOS Agent (`DEC-4`, ruled 2026-09-23), and everything that links VOS
//! is AGPL-3.0-or-later. Ingress is Apache-2.0 OR MIT and must not link it, so the Agent lives in
//! a process of its own, `aledgent-core-host` (`agents/core-host`), and ingress reaches it over a
//! local socket. This module is both sides of that socket, and neither links VOS:
//!
//! - [`RemoteCore`] is a [`Core`] whose operations are requests to the host; a batch of inputs
//!   ([`Core::apply_batch`], `T-031-04`) is one request, [`Request::ApplyBatch`];
//! - [`serve`] answers those requests from any [`Core`]: the host serves its Agent core with
//!   it, and the tests serve an in-process one.
//!
//! **Protocol.** One request, one response, as JSON, each framed by its length (four bytes, big
//! endian); a connection carries any number of them in turn. The values are the boundary's own
//! ([`crate::boundary`]), which are serialisable for this.
//!
//! **Failures.** A request that could not be sent (no connection) is [`CoreError::Unavailable`]:
//! the input did not run. An `apply` sent and not answered is [`CoreError::Ambiguous`]: it may
//! have run, and the forwarder retries the identical input, which the core answers once
//! (`INV-18`). A read that fails is `Unavailable` either way: reads change nothing.
//!
//! **Trust** (`F-035`). Every connection is authenticated before a request is read: its first
//! frame is a [`Hello`] carrying the link's token, whose SHA-256 the host's configuration lists
//! ([`LinkAuth`]), compared in constant time; on a Unix socket the peer's user id, from the
//! socket's peer credentials, must also be one the configuration lists. A connection that fails
//! either gets [`Response::Unauthenticated`] and is closed: nothing it sent runs. The token
//! travels in the clear, so a TCP endpoint must be loopback ([`serve`] refuses another); the
//! deployment's link is a Unix socket. Callers of ingress itself are authenticated by ingress
//! before a command becomes an input, as before.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use subtle::ConstantTimeEq as _;
use zeroize::Zeroizing;

use ledger_core::account::Time;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, OutboxPage, Query,
    QueryReply,
};

/// The largest frame either side accepts: well above any reply the core gives (an outbox page
/// is under 16 KiB), well below anything that would let a peer exhaust memory.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

/// A request to the core.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// [`Core::apply`].
    Apply {
        /// The input.
        input: CoreInput,
    },
    /// [`Core::apply_batch`] (`T-031-04`).
    ApplyBatch {
        /// The inputs, in order.
        inputs: Vec<CoreInput>,
    },
    /// [`Core::outbox`].
    Outbox {
        /// After this notification.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        /// Only undelivered ones.
        undelivered: bool,
        /// Now.
        now: Time,
    },
    /// [`Core::last_tick`].
    LastTick,
    /// [`Core::next_due`].
    NextDue,
    /// [`Core::query`].
    Query {
        /// As whom.
        caller: CallerBody,
        /// The query.
        query: Query,
        /// Now.
        now: Time,
    },
    /// [`Core::credential`] (`F-028`).
    Credential {
        /// The credential, `did:`-shaped.
        credential: String,
    },
    /// [`Core::party_of`] (`REQ-ID-7`, T-009-16).
    PartyOf {
        /// The principal, `did:`-shaped.
        principal: String,
    },
    /// [`Core::genesis_open`] (`T-036-25`).
    GenesisOpen,
}

/// The first frame of every connection (`F-035`): the link's token. Never formatted.
#[derive(Clone, Serialize, Deserialize)]
pub struct Hello {
    /// The token.
    pub token: String,
}

impl core::fmt::Debug for Hello {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Hello(<redacted>)")
    }
}

/// How long a connection has to send its [`Hello`].
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// The fewest bytes a link token may have.
pub const MIN_LINK_TOKEN: usize = 32;

/// The link's token, as ingress presents it (`F-035`). Never formatted, wiped when dropped.
#[derive(Clone)]
pub struct LinkToken(Zeroizing<String>);

impl core::fmt::Debug for LinkToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("LinkToken(<redacted>)")
    }
}

impl LinkToken {
    /// A token over `s` (at least [`MIN_LINK_TOKEN`] bytes, surrounding whitespace removed).
    pub fn new(s: &str) -> Result<LinkToken, String> {
        let t = s.trim();
        if t.len() < MIN_LINK_TOKEN {
            return Err(format!(
                "the core link's token has at least {MIN_LINK_TOKEN} bytes"
            ));
        }
        Ok(LinkToken(Zeroizing::new(t.to_owned())))
    }

    /// Its SHA-256, hex: what the host's configuration lists.
    pub fn sha256_hex(&self) -> String {
        hex::encode(Sha256::digest(self.0.as_bytes()))
    }
}

/// Who the host lets in (`F-035`, module documentation): the SHA-256 of each token it accepts
/// (two while one rotates), and on a Unix socket the user ids a peer may run as.
#[derive(Clone, Debug)]
pub struct LinkAuth {
    tokens: Vec<[u8; 32]>,
    uids: BTreeSet<u32>,
}

impl LinkAuth {
    /// The tokens whose SHA-256 (hex) are `token_sha256`, from peers running as one of `uids`
    /// (empty: any user the socket's permissions let connect). At least one token.
    pub fn new(token_sha256: &[String], uids: &[u32]) -> Result<LinkAuth, String> {
        if token_sha256.is_empty() {
            return Err("the core link has no token: refusing to start (F-035)".into());
        }
        let tokens = token_sha256
            .iter()
            .map(|t| {
                hex::decode(t)
                    .ok()
                    .and_then(|b| <[u8; 32]>::try_from(b).ok())
                    .ok_or_else(|| "core_link.token_sha256: 32 bytes of hex".to_owned())
            })
            .collect::<Result<_, _>>()?;
        Ok(LinkAuth {
            tokens,
            uids: uids.iter().copied().collect(),
        })
    }

    /// Whether `token`, from a peer running as `uid` (none over TCP), is let in.
    pub fn admits(&self, uid: Option<u32>, token: &str) -> bool {
        let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let mut ok = subtle::Choice::from(0u8);
        for t in &self.tokens {
            ok |= t.ct_eq(&digest);
        }
        let uid_ok = match uid {
            Some(u) => self.uids.is_empty() || self.uids.contains(&u),
            None => true,
        };
        bool::from(ok) && uid_ok
    }
}

/// The core's response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    /// An input's reply.
    Applied {
        /// The reply.
        reply: CoreReply,
    },
    /// A batch's answers, one per input, in order (`T-031-04`).
    Replies {
        /// Each input's answer.
        replies: Vec<BatchAnswer>,
    },
    /// An outbox page.
    Outbox {
        /// The page.
        page: OutboxPage,
    },
    /// The last tick.
    LastTick {
        /// Its time.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<Time>,
    },
    /// When a tick next has work.
    NextDue {
        /// Its time, if anything is scheduled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<Time>,
    },
    /// A query's answer.
    Query {
        /// The answer.
        reply: QueryReply,
    },
    /// A credential lookup's answer (`F-028`).
    Credential {
        /// The working credential, if there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        found: Option<CredentialBody>,
    },
    /// A principal's integrating party (`REQ-ID-7`).
    PartyOf {
        /// The party, if the principal is tied to one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        party: Option<String>,
    },
    /// Whether the book takes commands (`T-036-25`).
    GenesisOpen {
        /// `false` while a chainspec book waits for its `verify-genesis` report.
        open: bool,
    },
    /// The core did not run the request.
    Unavailable,
    /// The core may have run it.
    Ambiguous,
    /// The connection is not authenticated (`F-035`): nothing it sent ran.
    Unauthenticated,
}

/// One input's answer in [`Response::Replies`].
// A reply is what the boundary carries for every input; boxing it would buy nothing here.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum BatchAnswer {
    /// Its reply.
    Applied {
        /// The reply.
        reply: CoreReply,
    },
    /// The core did not run it.
    Unavailable,
    /// The core may have run it.
    Ambiguous,
}

impl BatchAnswer {
    fn of(r: Result<CoreReply, CoreError>) -> BatchAnswer {
        match r {
            Ok(reply) => BatchAnswer::Applied { reply },
            Err(CoreError::Unavailable) => BatchAnswer::Unavailable,
            Err(CoreError::Ambiguous) => BatchAnswer::Ambiguous,
        }
    }

    fn result(self) -> Result<CoreReply, CoreError> {
        match self {
            BatchAnswer::Applied { reply } => Ok(reply),
            BatchAnswer::Unavailable => Err(CoreError::Unavailable),
            BatchAnswer::Ambiguous => Err(CoreError::Ambiguous),
        }
    }
}

fn error_response(e: CoreError) -> Response {
    match e {
        CoreError::Unavailable => Response::Unavailable,
        CoreError::Ambiguous => Response::Ambiguous,
    }
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("a frame over the limit"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| std::io::Error::other("a frame too large"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(bytes).await?;
    w.flush().await
}

/// Answer one request from `core`.
pub async fn answer<C: Core>(core: &C, request: Request) -> Response {
    match request {
        Request::Apply { input } => match core.apply(&input).await {
            Ok(reply) => Response::Applied { reply },
            Err(e) => error_response(e),
        },
        Request::ApplyBatch { inputs } => Response::Replies {
            replies: core
                .apply_batch(&inputs)
                .await
                .into_iter()
                .map(BatchAnswer::of)
                .collect(),
        },
        Request::Outbox {
            cursor,
            undelivered,
            now,
        } => match core.outbox(cursor.as_deref(), undelivered, now).await {
            Ok(page) => Response::Outbox { page },
            Err(e) => error_response(e),
        },
        Request::LastTick => match core.last_tick().await {
            Ok(at) => Response::LastTick { at },
            Err(e) => error_response(e),
        },
        Request::NextDue => match core.next_due().await {
            Ok(at) => Response::NextDue { at },
            Err(e) => error_response(e),
        },
        Request::Query { caller, query, now } => match core.query(&caller, &query, now).await {
            Ok(reply) => Response::Query { reply },
            Err(e) => error_response(e),
        },
        Request::Credential { credential } => match core.credential(&credential).await {
            Ok(found) => Response::Credential { found },
            Err(e) => error_response(e),
        },
        Request::GenesisOpen => match core.genesis_open().await {
            Ok(open) => Response::GenesisOpen { open },
            Err(e) => error_response(e),
        },
        Request::PartyOf { principal } => match core.party_of(&principal).await {
            Ok(party) => Response::PartyOf { party },
            Err(e) => error_response(e),
        },
    }
}

async fn connection<C: Core, S: AsyncRead + AsyncWrite + Unpin>(
    core: Arc<C>,
    auth: Arc<LinkAuth>,
    uid: Option<u32>,
    mut s: S,
) {
    // F-035: the first frame is the link's token; nothing is read past it until it is checked.
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut s)).await {
        Ok(Ok(frame)) => serde_json::from_slice::<Hello>(&frame).ok(),
        _ => None,
    };
    let admitted = hello.as_ref().is_some_and(|h| auth.admits(uid, &h.token));
    drop(hello);
    if !admitted {
        tracing::warn!(uid, "core link: a connection refused (not authenticated)");
        if let Ok(bytes) = serde_json::to_vec(&Response::Unauthenticated) {
            let _ = write_frame(&mut s, &bytes).await;
        }
        // Close gracefully: what the caller already sent is read and dropped, so the refusal
        // reaches it instead of a reset. Nothing read here runs.
        let _ = s.shutdown().await;
        let mut sink = [0u8; 4096];
        let drain = async {
            let mut left = MAX_FRAME + 8;
            while left > 0 {
                match s.read(&mut sink).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => left = left.saturating_sub(n),
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
        return;
    }
    loop {
        let Ok(frame) = read_frame(&mut s).await else {
            return;
        };
        let response = match serde_json::from_slice::<Request>(&frame) {
            Ok(request) => answer(&*core, request).await,
            // A request the core cannot read did not run.
            Err(_) => Response::Unavailable,
        };
        let Ok(bytes) = serde_json::to_vec(&response) else {
            return;
        };
        if write_frame(&mut s, &bytes).await.is_err() {
            return;
        }
    }
}

/// Where the host listens and ingress connects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    /// A Unix socket at this path.
    Unix(PathBuf),
    /// A TCP address; loopback only in a deployment (module documentation).
    Tcp(String),
}

impl Endpoint {
    /// `unix:<path>` or `tcp:<host:port>`.
    pub fn parse(s: &str) -> Option<Endpoint> {
        if let Some(p) = s.strip_prefix("unix:") {
            return Some(Endpoint::Unix(PathBuf::from(p)));
        }
        s.strip_prefix("tcp:").map(|a| Endpoint::Tcp(a.to_owned()))
    }
}

/// Serve `core` on `endpoint` until the task is dropped: every connection in a task of its own,
/// authenticated by `auth`, every request answered in turn. A TCP endpoint must be loopback.
pub async fn serve<C: Core>(
    endpoint: Endpoint,
    core: Arc<C>,
    auth: LinkAuth,
) -> std::io::Result<()> {
    match endpoint {
        Endpoint::Tcp(addr) => {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            if !listener.local_addr()?.ip().is_loopback() {
                return Err(std::io::Error::other(
                    "the core link listens on loopback or a Unix socket only (F-035)",
                ));
            }
            serve_tcp(listener, core, auth).await
        }
        Endpoint::Unix(path) => {
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path)?;
            let auth = Arc::new(auth);
            loop {
                let (s, _) = listener.accept().await?;
                let uid = s.peer_cred().ok().map(|c| c.uid());
                if uid.is_none() {
                    tracing::warn!("core link: a connection without peer credentials, refused");
                    continue;
                }
                tokio::spawn(connection(Arc::clone(&core), Arc::clone(&auth), uid, s));
            }
        }
    }
}

/// [`serve`] on a TCP listener already bound (tests bind port 0 and read the address back).
pub async fn serve_tcp<C: Core>(
    listener: tokio::net::TcpListener,
    core: Arc<C>,
    auth: LinkAuth,
) -> std::io::Result<()> {
    let auth = Arc::new(auth);
    loop {
        let (s, _) = listener.accept().await?;
        tokio::spawn(connection(Arc::clone(&core), Arc::clone(&auth), None, s));
    }
}

/// The core in another process (module documentation).
#[derive(Clone, Debug)]
pub struct RemoteCore {
    endpoint: Endpoint,
    timeout: Duration,
    token: LinkToken,
}

enum Stream {
    Unix(tokio::net::UnixStream),
    Tcp(tokio::net::TcpStream),
}

impl RemoteCore {
    /// A client of the core at `endpoint`, presenting `token`. `timeout` bounds each request; a
    /// core decision through the Agent takes seconds today (`notes/MEASUREMENTS-F005.md`), so it
    /// is generous.
    pub fn new(endpoint: Endpoint, timeout: Duration, token: LinkToken) -> Self {
        RemoteCore {
            endpoint,
            timeout,
            token,
        }
    }

    async fn connect(&self) -> std::io::Result<Stream> {
        match &self.endpoint {
            Endpoint::Unix(p) => Ok(Stream::Unix(tokio::net::UnixStream::connect(p).await?)),
            Endpoint::Tcp(a) => Ok(Stream::Tcp(tokio::net::TcpStream::connect(a).await?)),
        }
    }

    /// Send one request. `Err(false)`: it was not sent. `Err(true)`: sent and not answered.
    async fn call(&self, request: &Request) -> Result<Response, bool> {
        let bytes = serde_json::to_vec(request).map_err(|_| false)?;
        let hello = Zeroizing::new(
            serde_json::to_vec(&Hello {
                token: self.token.0.to_string(),
            })
            .map_err(|_| false)?,
        );
        let mut stream = self.connect().await.map_err(|_| false)?;
        // F-035: the token first, then the request, in one write each; the host reads the
        // request only once the token is checked.
        let exchange = async {
            // A request that could not be written is still answered if the host refused the
            // token first: read the answer either way.
            match &mut stream {
                Stream::Unix(s) => {
                    write_frame(s, &hello).await?;
                    let sent = write_frame(s, &bytes).await;
                    read_frame(s).await.or_else(|e| sent.and(Err(e)))
                }
                Stream::Tcp(s) => {
                    write_frame(s, &hello).await?;
                    let sent = write_frame(s, &bytes).await;
                    read_frame(s).await.or_else(|e| sent.and(Err(e)))
                }
            }
        };
        let frame = match tokio::time::timeout(self.timeout, exchange).await {
            Ok(Ok(frame)) => frame,
            _ => return Err(true),
        };
        match serde_json::from_slice(&frame) {
            // Refused before the request was read: it did not run.
            Ok(Response::Unauthenticated) => {
                tracing::error!("ALERT core link: the core host refused ingress's token");
                Err(false)
            }
            Ok(r) => Ok(r),
            Err(_) => Err(true),
        }
    }

    /// A read that did not get its answer: reads change nothing, so it did not run.
    fn read_error(_: Response) -> CoreError {
        CoreError::Unavailable
    }
}

impl Core for RemoteCore {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        let request = Request::Apply {
            input: input.clone(),
        };
        match self.call(&request).await {
            Ok(Response::Applied { reply }) => Ok(reply),
            Ok(Response::Unavailable) | Err(false) => Err(CoreError::Unavailable),
            Ok(_) | Err(true) => Err(CoreError::Ambiguous),
        }
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        // One input travels as `apply`, which every host answers.
        if let [one] = inputs {
            return vec![self.apply(one).await];
        }
        let request = Request::ApplyBatch {
            inputs: inputs.to_vec(),
        };
        let all = |e: CoreError| vec![Err(e); inputs.len()];
        match self.call(&request).await {
            Ok(Response::Replies { replies }) if replies.len() == inputs.len() => {
                replies.into_iter().map(BatchAnswer::result).collect()
            }
            Ok(Response::Unavailable) | Err(false) => all(CoreError::Unavailable),
            Ok(_) | Err(true) => all(CoreError::Ambiguous),
        }
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
        match self.call(&request).await {
            Ok(Response::Outbox { page }) => Ok(page),
            Ok(r) => Err(Self::read_error(r)),
            Err(_) => Err(CoreError::Unavailable),
        }
    }

    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        match self.call(&Request::LastTick).await {
            Ok(Response::LastTick { at }) => Ok(at),
            Ok(r) => Err(Self::read_error(r)),
            Err(_) => Err(CoreError::Unavailable),
        }
    }

    async fn next_due(&self) -> Result<Option<Time>, CoreError> {
        match self.call(&Request::NextDue).await {
            Ok(Response::NextDue { at }) => Ok(at),
            Ok(r) => Err(Self::read_error(r)),
            Err(_) => Err(CoreError::Unavailable),
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
        match self.call(&request).await {
            Ok(Response::Query { reply }) => Ok(reply),
            Ok(r) => Err(Self::read_error(r)),
            Err(_) => Err(CoreError::Unavailable),
        }
    }

    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        let request = Request::Credential {
            credential: credential.to_owned(),
        };
        match self.call(&request).await {
            Ok(Response::Credential { found }) => Ok(found),
            Ok(r) => Err(Self::read_error(r)),
            Err(_) => Err(CoreError::Unavailable),
        }
    }

    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        let request = Request::PartyOf {
            principal: principal.to_owned(),
        };
        match self.call(&request).await {
            Ok(Response::PartyOf { party }) => Ok(party),
            Ok(r) => Err(Self::read_error(r)),
            Err(_) => Err(CoreError::Unavailable),
        }
    }

    async fn genesis_open(&self) -> Result<bool, CoreError> {
        match self.call(&Request::GenesisOpen).await {
            Ok(Response::GenesisOpen { open }) => Ok(open),
            Ok(r) => Err(Self::read_error(r)),
            Err(_) => Err(CoreError::Unavailable),
        }
    }
}
