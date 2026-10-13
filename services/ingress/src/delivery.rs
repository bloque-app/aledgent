// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Notification delivery (`DEC-7`, `CTR-EVT`, `REQ-EV-1`–`REQ-EV-6`, `REQ-RT-4`).
//!
//! The leader's delivery worker pages through the core's outbox, pushes each undelivered
//! notification to the endpoint registered for the principal it is owed to, and, on a 2xx, logs a
//! delivery record that the forwarder sends to the core like any other input. The core marks the
//! notification delivered in committed state, so after any restart storage alone says who was
//! told (`AC-K3.1`). A failed attempt is kept in the ingress store and retried with exponential
//! backoff, for ever: nothing is dropped. A crash between the recipient's 2xx and the record means
//! one more delivery under the same identity (`AC-K3.2`), which the recipient recognises.
//!
//! The worker never blocks the core, and the core never waits for a recipient (`REQ-EV-5`): an
//! operation reaches its terminal state whether or not anyone is listening (`AC-M3.1`).
//!
//! **Authenticity** (`REQ-EV-4`; `OQ-51` ruled by amendment 0009). Each attempt carries an
//! HMAC-SHA256 under the secret aledgent shares with that recipient, over the notification's
//! identity, the attempt's time and the body. Nothing is signed with a key of aledgent's: the
//! owner ruled signatures out. The destination and secret are registered by the operator for now;
//! the recipient registers them through the API (`T-009-15`, `OQ-66`). A recipient with no secret gets no delivery: the notification
//! stays undelivered and visible with its age (`REQ-EV-4`, `REQ-EV-6`). [`ReferenceVerifier`] is
//! what a recipient runs: it rejects a forged or altered notification, a stale one, and
//! recognises a replayed or redelivered one by its identity (`AC-M2.3`).

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

use hmac::{Hmac, Mac};
use ledger_core::account::Time;
use sha2::Sha256;

/// The header carrying the notification's identity.
pub const HEADER_ID: &str = "aledgent-notification-id";
/// The header carrying the attempt's time, milliseconds since the Unix epoch.
pub const HEADER_TIME: &str = "aledgent-attempt-time";
/// The header carrying the signature, `v1=<hex>`.
pub const HEADER_SIGNATURE: &str = "aledgent-signature";

const SIGNATURE_DOMAIN: &[u8] = b"aledgent/notification/v1";

/// The signature of one attempt: `HMAC-SHA256(secret, domain ‖ id ‖ attempt_time ‖ body)`, each
/// part length-prefixed.
pub fn sign(secret: &[u8], id: &str, attempt_time: Time, body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret)
        .unwrap_or_else(|_| unreachable!("HMAC takes a key of any length"));
    for part in [
        SIGNATURE_DOMAIN,
        id.as_bytes(),
        &attempt_time.to_be_bytes(),
        body,
    ] {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part);
    }
    format!("v1={}", hex::encode(mac.finalize().into_bytes()))
}

/// The headers of one signed attempt.
pub fn signed_headers(
    secret: &[u8],
    id: &str,
    attempt_time: Time,
    body: &[u8],
) -> Vec<(String, String)> {
    vec![
        ("content-type".into(), "application/json".into()),
        (HEADER_ID.into(), id.to_owned()),
        (HEADER_TIME.into(), attempt_time.to_string()),
        (
            HEADER_SIGNATURE.into(),
            sign(secret, id, attempt_time, body),
        ),
    ]
}

/// What a recipient's verifier concluded about an authentic notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// First sight of this notification: process it.
    Fresh,
    /// This identity was seen before: a redelivery or a replay. Acknowledge it, change nothing.
    Duplicate,
}

/// Why a verifier rejected a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    /// A header is missing or malformed.
    Malformed,
    /// The signature does not match: forged, or altered in transit.
    BadSignature,
    /// The attempt is older than the verifier accepts: a replay of an old capture.
    Stale,
    /// The body names another notification than the header.
    Mismatch,
}

/// The reference verifier a recipient runs (`AC-M2.3`).
pub struct ReferenceVerifier {
    secret: Vec<u8>,
    max_age: Time,
    seen: HashSet<String>,
}

impl ReferenceVerifier {
    /// A verifier with the recipient's secret, accepting attempts at most `max_age` old.
    pub fn new(secret: impl Into<Vec<u8>>, max_age: Time) -> ReferenceVerifier {
        ReferenceVerifier {
            secret: secret.into(),
            max_age,
            seen: HashSet::new(),
        }
    }

    /// Verify one received notification at `now`.
    pub fn verify(
        &mut self,
        headers: &[(String, String)],
        body: &[u8],
        now: Time,
    ) -> Result<Verdict, Rejected> {
        let get = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        let id = get(HEADER_ID).ok_or(Rejected::Malformed)?;
        let time: Time = get(HEADER_TIME)
            .and_then(|t| t.parse().ok())
            .ok_or(Rejected::Malformed)?;
        let signature = get(HEADER_SIGNATURE).ok_or(Rejected::Malformed)?;
        let want = sign(&self.secret, id, time, body);
        // Compare in constant time.
        let equal = want.len() == signature.len()
            && want
                .bytes()
                .zip(signature.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0;
        if !equal {
            return Err(Rejected::BadSignature);
        }
        if now.saturating_sub(time) > self.max_age {
            return Err(Rejected::Stale);
        }
        let named: serde_json::Value =
            serde_json::from_slice(body).map_err(|_| Rejected::Malformed)?;
        if named.get("id").and_then(|v| v.as_str()) != Some(id) {
            return Err(Rejected::Mismatch);
        }
        Ok(if self.seen.insert(id.to_owned()) {
            Verdict::Fresh
        } else {
            Verdict::Duplicate
        })
    }
}

/// A boxed future, so [`Transport`] can be held as a trait object.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How a notification reaches its endpoint.
pub trait Transport: Send + Sync + 'static {
    /// POST `body` with `headers` to `endpoint`; the HTTP status, or why there was none.
    fn post<'a>(
        &'a self,
        endpoint: &'a str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<u16, String>>;
}

/// Delivery over HTTPS (feature `http`).
#[cfg(feature = "http")]
pub struct HttpTransport {
    client: reqwest::Client,
}

#[cfg(feature = "http")]
impl HttpTransport {
    /// A transport whose attempts time out after `timeout`.
    pub fn new(timeout: std::time::Duration) -> Result<HttpTransport, String> {
        reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map(|client| HttpTransport { client })
            .map_err(|e| e.to_string())
    }
}

#[cfg(feature = "http")]
impl Transport for HttpTransport {
    fn post<'a>(
        &'a self,
        endpoint: &'a str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<u16, String>> {
        Box::pin(async move {
            let mut req = self.client.post(endpoint).body(body);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            // The error names the kind of failure only: an endpoint URL could carry a secret.
            req.send().await.map(|r| r.status().as_u16()).map_err(|e| {
                if e.is_timeout() {
                    "timeout".to_owned()
                } else if e.is_connect() {
                    "connect".to_owned()
                } else {
                    "transport".to_owned()
                }
            })
        })
    }
}

/// The wait before the next attempt after `failures` failed ones: exponential, capped, never
/// giving up.
pub fn backoff(failures: u64, base: Time, max: Time) -> Time {
    let shift = u32::try_from(failures.min(32)).unwrap_or(32);
    base.saturating_mul(1u64 << shift.min(62)).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"id": id, "state": "Settled"})).unwrap()
    }

    /// `AC-M2.3` at the unit: a recipient verifies authenticity, and a replay changes nothing.
    #[test]
    fn ac_m2_3_the_reference_verifier_rejects_forged_stale_and_replayed() {
        let mut v = ReferenceVerifier::new(b"k".to_vec(), 1_000);
        let b = body("n1");
        let h = signed_headers(b"k", "n1", 10_000, &b);
        assert_eq!(v.verify(&h, &b, 10_500), Ok(Verdict::Fresh));
        // The same request again: recognised, not processed twice.
        assert_eq!(v.verify(&h, &b, 10_600), Ok(Verdict::Duplicate));
        // Captured and replayed later: stale.
        assert_eq!(v.verify(&h, &b, 20_000), Err(Rejected::Stale));
        // Altered body, or another key.
        let other = body("n2");
        assert_eq!(v.verify(&h, &other, 10_500), Err(Rejected::BadSignature));
        let forged = signed_headers(b"not-k", "n1", 10_000, &b);
        assert_eq!(v.verify(&forged, &b, 10_500), Err(Rejected::BadSignature));
        // A signed header naming another notification than the body.
        let h2 = signed_headers(b"k", "n9", 10_000, &b);
        assert_eq!(v.verify(&h2, &b, 10_500), Err(Rejected::Mismatch));
        assert_eq!(v.verify(&[], &b, 0), Err(Rejected::Malformed));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff(0, 100, 10_000), 100);
        assert_eq!(backoff(3, 100, 10_000), 800);
        assert_eq!(backoff(40, 100, 10_000), 10_000);
    }
}
