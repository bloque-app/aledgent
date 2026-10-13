// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Where the load goes: a running ingress over HTTP (scenarios a, b, d, e), or an in-process
//! ingress's router (scenario c, inside the zombienet driver, whose core is in its memory).

use std::time::{Duration, Instant};

use serde_json::Value;

/// One reply: its HTTP status, its JSON body (a string for a body that is not JSON), and how long
/// it took, in microseconds, measured at the client.
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: Value,
    /// Client-side latency, µs.
    pub us: u64,
}

impl Reply {
    /// The refusal's code (`ERR-…`), if the body carries one.
    pub fn error(&self) -> Option<&str> {
        self.body["error"].as_str()
    }
}

/// The surface the harness drives.
#[derive(Clone)]
pub enum Target {
    /// Ingress over HTTP at `base` (no trailing slash).
    Http {
        /// The client, with its connection pool.
        client: reqwest::Client,
        /// `http://host:port`.
        base: String,
    },
    /// An in-process ingress's router.
    Router(axum::Router),
}

impl Target {
    /// Ingress over HTTP at `base`. Each request may take up to `timeout`: a command answered with
    /// its decision waits for it up to ingress's `decision_wait` (30 s by default).
    pub fn http(base: &str, timeout: Duration) -> Result<Target, String> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .pool_max_idle_per_host(1024)
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Target::Http {
            client,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    /// Send one request. `Err` only when no reply came back (connection refused, reset, timed
    /// out): a transport failure, which the harness counts as an error.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Reply, String> {
        let started = Instant::now();
        match self {
            Target::Router(router) => {
                let (s, v) =
                    ingress::testkit::call(router, method, path, token, headers, body.cloned())
                        .await;
                Ok(Reply {
                    status: s.as_u16(),
                    body: v,
                    us: elapsed_us(started),
                })
            }
            Target::Http { client, base } => {
                let url = format!("{base}{path}");
                let mut req = match method {
                    "GET" => client.get(&url),
                    "POST" => client.post(&url),
                    m => return Err(format!("method {m} is not used")),
                };
                if let Some(t) = token {
                    req = req.header("authorization", format!("Bearer {t}"));
                }
                for (k, v) in headers {
                    req = req.header(*k, *v);
                }
                if let Some(b) = body {
                    req = req
                        .header("content-type", "application/json")
                        .body(serde_json::to_vec(b).unwrap_or_default());
                }
                let resp = req.send().await.map_err(|e| transport(&e))?;
                let status = resp.status().as_u16();
                let bytes = resp.bytes().await.map_err(|e| transport(&e))?;
                let body = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
                    Value::String(String::from_utf8_lossy(&bytes).into_owned())
                });
                Ok(Reply {
                    status,
                    body,
                    us: elapsed_us(started),
                })
            }
        }
    }

    /// `GET /metrics` as text (Prometheus format), or nothing.
    pub async fn metrics(&self, token: &str) -> Option<String> {
        let r = self
            .call("GET", "/metrics", Some(token), &[], None)
            .await
            .ok()?;
        (r.status == 200).then(|| match r.body {
            Value::String(s) => s,
            v => v.to_string(),
        })
    }
}

fn transport(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".into()
    } else if e.is_connect() {
        "connect".into()
    } else {
        "transport".into()
    }
}

/// Microseconds since `t`, saturating.
pub fn elapsed_us(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX)
}
