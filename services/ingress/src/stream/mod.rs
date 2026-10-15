// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core stream (`F-038`, `DEC-27`): each core host dials out to ingress over one long-lived
//! stream, and ingress feeds it and reads from it down that stream.
//!
//! Ingress runs as a fleet of its own, apart from every execution node (`DEC-27` (a)), and needs
//! no core to acknowledge (`NFR-1`, `REQ-CS-2`). A core host listens on no network port: it dials
//! the stable address of ingress's core listener ([`hub`]), and a lost or replaced core, node or
//! ingress instance is a reconnect, never a refusal (`NFR-5`, `NFR-17`, `NFR-21(b)`).
//!
//! **Framing and messages.** The socket link's frame ([`crate::remote`]): four bytes of length,
//! big endian, then one JSON message, at most [`crate::remote::MAX_FRAME`]. The core host's first
//! frame is a [`Hello`]; ingress answers a [`Greeting`]: [`Greeting::Welcome`] with the version
//! both speak ([`VERSION`] is this module's), or [`Greeting::Refused`] and the connection closes.
//! After it, [`Msg`]s both ways, several in flight: ingress's [`Msg::Call`]s carry the socket
//! link's own [`crate::remote::Request`]s (the reads and the sequenced [`crate::feed::Feed`]), the
//! core's [`Msg::Answer`]s their [`crate::remote::Response`]s, so the host answers both links with
//! one function and its calls into the Agent are untouched (`DEC-26(d)`, `DEC-27` (i)).
//!
//! **Keepalive.** Each side sends [`Msg::Ping`] every [`Timings::keepalive`] and drops a peer it
//! has heard nothing from for [`Timings::dead_after`]. A dropped stream fails its pending calls: a
//! feed as ambiguous (it may have run), a read as unavailable (reads change nothing).
//!
//! **Go-away** (`DEC-27` (e)). A draining instance sends [`Msg::GoAway`] and refuses new
//! handshakes; the core dials a new stream at once and keeps answering the old one until it closes.
//!
//! **Transports** ([`tls`]): mutual TLS over TCP under the operator's own CA (`tls:<host:port>`),
//! or a Unix socket on the same host, authenticated by the peer's user id and a token as the socket
//! link is (`unix:<path>`, `DEC-27` (h′)).

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::remote::{Request, Response};

pub mod dial;
pub mod hub;
pub mod tls;

pub use dial::{DialConfig, Target, dial};
pub use hub::{Listen, StreamCore};

/// The protocol version this build speaks.
pub const VERSION: u32 = 1;

/// The versions this build speaks, oldest first.
pub const VERSIONS: &[u32] = &[VERSION];

/// The core host's first frame.
#[derive(Clone, Serialize, Deserialize)]
pub struct Hello {
    /// The versions it speaks.
    pub versions: Vec<u32>,
    /// Its name, for the logs.
    pub core: String,
    /// The partitions it writes.
    pub partitions: Vec<String>,
    /// Whether it holds the principals role (credentials and ties are read from it).
    #[serde(default)]
    pub principals: bool,
    /// On a Unix socket, the link's token; never sent over TLS, where the certificate
    /// authenticates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

impl core::fmt::Debug for Hello {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hello")
            .field("versions", &self.versions)
            .field("core", &self.core)
            .field("partitions", &self.partitions)
            .field("principals", &self.principals)
            .finish_non_exhaustive()
    }
}

/// Ingress's answer to a [`Hello`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "greeting", rename_all = "snake_case")]
pub enum Greeting {
    /// The stream is open.
    Welcome {
        /// The version both speak.
        version: u32,
        /// The instance's name, for the logs.
        instance: String,
    },
    /// The stream is refused, and closes.
    Refused {
        /// Why: `unauthenticated`, `version`, `draining` or `malformed`.
        reason: String,
    },
}

/// A message after the handshake.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum Msg {
    /// Ingress asks the core.
    Call {
        /// Repeated by its answer.
        id: u64,
        /// The request.
        request: Request,
    },
    /// The core answers a call.
    Answer {
        /// The call's.
        id: u64,
        /// The response.
        response: Response,
    },
    /// Keepalive.
    Ping,
    /// Keepalive's answer.
    Pong,
    /// Ingress is leaving: dial a new stream.
    GoAway {
        /// Why.
        reason: String,
    },
}

/// The stream's timings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timings {
    /// How often each side pings.
    pub keepalive: Duration,
    /// How long a side waits on silence before it drops the stream.
    pub dead_after: Duration,
    /// How long ingress waits for one call's answer. A decision through the core Agent takes
    /// seconds at worst (`notes/MEASUREMENTS-F005.md`), so it is generous.
    pub call_timeout: Duration,
    /// How long a connection has to complete its handshake.
    pub hello_timeout: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Timings {
            keepalive: Duration::from_secs(1),
            dead_after: Duration::from_secs(3),
            call_timeout: Duration::from_secs(60),
            hello_timeout: Duration::from_secs(5),
        }
    }
}

/// The highest version both `ours` and `theirs` speak.
pub fn negotiate(ours: &[u32], theirs: &[u32]) -> Option<u32> {
    ours.iter().filter(|v| theirs.contains(v)).max().copied()
}

/// Write `msg` as one frame.
pub(crate) async fn send<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    crate::remote::write_frame(w, &bytes).await
}

/// Read one frame as a `T`.
pub(crate) async fn recv<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    let frame = crate::remote::read_frame(r).await?;
    serde_json::from_slice(&frame).map_err(std::io::Error::other)
}

/// Where a listener or a dialer is: `tls:<host:port>` or `unix:<path>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Address {
    /// Mutual TLS over TCP.
    Tls(String),
    /// A Unix socket on this host.
    Unix(std::path::PathBuf),
}

impl Address {
    /// `tls:<host:port>` or `unix:<path>`.
    pub fn parse(s: &str) -> Result<Address, String> {
        if let Some(a) = s.strip_prefix("tls:") {
            return Ok(Address::Tls(a.to_owned()));
        }
        if let Some(p) = s.strip_prefix("unix:") {
            return Ok(Address::Unix(p.into()));
        }
        Err(format!("{s}: tls:<host:port> or unix:<path> (F-038)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `T-038-01`: the highest common version, or none.
    #[test]
    fn t_038_01_versions_negotiate_to_the_highest_common_one() {
        assert_eq!(negotiate(&[1], &[1]), Some(1));
        assert_eq!(negotiate(&[1, 2], &[1, 2, 3]), Some(2));
        assert_eq!(negotiate(&[1], &[2]), None);
        assert_eq!(negotiate(&[1], &[]), None);
    }

    /// `T-038-01`: every message round-trips, and a hello never shows its token.
    #[test]
    fn t_038_01_messages_round_trip_and_a_token_is_never_formatted() {
        let hello = Hello {
            versions: vec![1],
            core: "core-0".into(),
            partitions: vec!["book".into()],
            principals: true,
            token: Some("secret-token".into()),
        };
        assert!(!format!("{hello:?}").contains("secret"));
        let back: Hello = serde_json::from_slice(&serde_json::to_vec(&hello).unwrap()).unwrap();
        assert_eq!(back.token.as_deref(), Some("secret-token"));
        for m in [
            Msg::Ping,
            Msg::Pong,
            Msg::GoAway {
                reason: "draining".into(),
            },
            Msg::Call {
                id: 7,
                request: Request::LastTick,
            },
            Msg::Answer {
                id: 7,
                response: Response::LastTick { at: Some(5) },
            },
        ] {
            let bytes = serde_json::to_vec(&m).unwrap();
            let back: Msg = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(serde_json::to_vec(&back).unwrap(), bytes);
        }
        assert_eq!(
            Address::parse("tls:ingress.internal:7443"),
            Ok(Address::Tls("ingress.internal:7443".into()))
        );
        assert!(Address::parse("tcp:1.2.3.4:5").is_err());
    }
}
