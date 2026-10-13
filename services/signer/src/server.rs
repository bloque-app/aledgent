// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The listener (crate documentation, "Authentication").
//!
//! One task per connection. The first frame must arrive within [`HELLO_TIMEOUT`]; it is checked
//! against the configuration as it stands then, and so is every request after it, so a
//! [`Server::reload`] that removes a token or a grant takes effect on the next request.
//!
//! **Audit.** Every refusal is logged at `warn`. Every signature with a clerk, Ed25519 or
//! secp256k1 key is logged at `info` with the caller, the key and the operation; the key ring's
//! questions (several for every transfer) at `debug`, with a per-caller count at `info` once a
//! minute. A message, a token or an answer is never logged.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use crate::backend::{Backends, Key};
use crate::config::Config;
use crate::proto::{self, Hello, KeyInfo, Op, Refusal, Request, Response, Scheme, Welcome};

/// How long a connection has to say who it is.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

struct Caller {
    uids: BTreeSet<u32>,
    tokens: Vec<[u8; 32]>,
    grants: BTreeSet<String>,
}

struct State {
    socket: PathBuf,
    mode: u32,
    keys: HashMap<String, Arc<Key>>,
    callers: HashMap<String, Caller>,
}

impl State {
    /// Whether `client`, connected as `uid`, holding a token whose SHA-256 is `token`, is a
    /// caller the configuration knows. Every listed hash is compared, in constant time.
    fn authenticated(&self, client: &str, uid: u32, token: &[u8; 32]) -> bool {
        let Some(c) = self.callers.get(client) else {
            return false;
        };
        let mut ok = subtle::Choice::from(0u8);
        for t in &c.tokens {
            ok |= t.ct_eq(token);
        }
        bool::from(ok) && c.uids.contains(&uid)
    }

    fn infos(&self, client: &str) -> Vec<KeyInfo> {
        let Some(c) = self.callers.get(client) else {
            return Vec::new();
        };
        c.grants
            .iter()
            .filter_map(|k| self.keys.get(k))
            .map(|k| KeyInfo {
                id: k.id.clone(),
                scheme: k.scheme,
                public: k.public.clone(),
            })
            .collect()
    }
}

/// The signer (module documentation).
pub struct Server {
    state: RwLock<Arc<State>>,
    backends: Backends,
    counts: Mutex<BTreeMap<String, u64>>,
    /// How many requests it has answered.
    pub answered: AtomicU64,
    /// How many connections or requests it has refused.
    pub refused: AtomicU64,
}

fn token_hash(token: &[u8]) -> [u8; 32] {
    Sha256::digest(token).into()
}

async fn state_of(config: &Config, backends: &Backends) -> Result<State, String> {
    config.check()?;
    let mut keys = HashMap::new();
    for k in &config.keys {
        let key = Key::load(k, backends).await?;
        tracing::info!(key = %key.id, scheme = ?key.scheme, origin = key.origin, "key loaded");
        keys.insert(k.id.clone(), Arc::new(key));
    }
    let mut callers = HashMap::new();
    for c in &config.clients {
        let tokens = c
            .token_sha256
            .iter()
            .map(|t| {
                hex::decode(t)
                    .ok()
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| format!("client {}: token_sha256", c.name))
            })
            .collect::<Result<Vec<[u8; 32]>, String>>()?;
        callers.insert(
            c.name.clone(),
            Caller {
                uids: c.uids.iter().copied().collect(),
                tokens,
                grants: c.keys.iter().cloned().collect(),
            },
        );
    }
    Ok(State {
        socket: config.listen.clone(),
        mode: config.mode(),
        keys,
        callers,
    })
}

async fn read_frame(s: &mut UnixStream) -> std::io::Result<zeroize::Zeroizing<Vec<u8>>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > proto::MAX_FRAME {
        return Err(std::io::Error::other("a frame over the limit"));
    }
    let mut body = zeroize::Zeroizing::new(vec![0u8; len]);
    s.read_exact(&mut body).await?;
    Ok(body)
}

async fn write_frame<T: serde::Serialize>(s: &mut UnixStream, v: &T) -> std::io::Result<()> {
    s.write_all(&proto::frame(v)?).await?;
    s.flush().await
}

impl Server {
    /// A signer over `config`: every key loaded (from Vault, or KMS where the configuration opts
    /// in), or an error. A signer that cannot load a key does not start.
    pub async fn load(config: &Config, backends: Backends) -> Result<Arc<Server>, String> {
        let state = state_of(config, &backends).await?;
        Ok(Arc::new(Server {
            state: RwLock::new(Arc::new(state)),
            backends,
            counts: Mutex::new(BTreeMap::new()),
            answered: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        }))
    }

    /// Replace the configuration (a rotation: a token or a key added or removed). The new one
    /// is loaded whole first; if it does not load, the old one stays.
    pub async fn reload(&self, config: &Config) -> Result<(), String> {
        let state = state_of(config, &self.backends).await?;
        *self.state.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(state);
        tracing::info!("configuration reloaded");
        Ok(())
    }

    fn state(&self) -> Arc<State> {
        Arc::clone(&self.state.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// Bind the configured socket (a stale one is removed) with the configured permissions.
    pub fn bind(&self) -> std::io::Result<UnixListener> {
        use std::os::unix::fs::PermissionsExt as _;
        let st = self.state();
        let _ = std::fs::remove_file(&st.socket);
        let l = UnixListener::bind(&st.socket)?;
        std::fs::set_permissions(&st.socket, std::fs::Permissions::from_mode(st.mode))?;
        Ok(l)
    }

    /// Serve connections on `listener` until the task is dropped.
    pub async fn serve(self: Arc<Self>, listener: UnixListener) -> std::io::Result<()> {
        loop {
            let (s, _) = listener.accept().await?;
            tokio::spawn(Arc::clone(&self).connection(s));
        }
    }

    /// Log the key ring's counts since the last call, per caller and key, at `info`.
    pub fn log_counts(&self) {
        let counts =
            std::mem::take(&mut *self.counts.lock().unwrap_or_else(PoisonError::into_inner));
        for (who, n) in counts {
            tracing::info!(caller = %who, answers = n, "key ring answers in the last period");
        }
    }

    fn refuse(&self, what: &str, client: &str, uid: Option<u32>, r: &Refusal) {
        self.refused.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(client, uid, refusal = %r, "refused: {what}");
    }

    async fn connection(self: Arc<Self>, mut s: UnixStream) {
        let uid = match s.peer_cred() {
            Ok(c) => c.uid(),
            Err(_) => {
                self.refuse("no peer credentials", "", None, &Refusal::Unauthenticated);
                return;
            }
        };
        let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut s)).await {
            Ok(Ok(body)) => match proto::unframe::<Hello>(&body) {
                Ok(h) => h,
                Err(_) => return,
            },
            _ => return,
        };
        let token = token_hash(&hello.token);
        let client = hello.client.clone();
        drop(hello);
        let st = self.state();
        if !st.authenticated(&client, uid, &token) {
            self.refuse("hello", &client, Some(uid), &Refusal::Unauthenticated);
            let _ = write_frame(&mut s, &Welcome::Refused(Refusal::Unauthenticated)).await;
            return;
        }
        tracing::info!(client = %client, uid, "caller authenticated");
        if write_frame(
            &mut s,
            &Welcome::Ok {
                keys: st.infos(&client),
            },
        )
        .await
        .is_err()
        {
            return;
        }
        drop(st);
        loop {
            let Ok(body) = read_frame(&mut s).await else {
                return;
            };
            let response = match proto::unframe::<Request>(&body) {
                Ok(r) => self.answer(&client, uid, &token, r).await,
                Err(_) => Response::Refused(Refusal::Malformed),
            };
            let close = matches!(response, Response::Refused(Refusal::Unauthenticated));
            if write_frame(&mut s, &response).await.is_err() || close {
                return;
            }
        }
    }

    async fn answer(&self, client: &str, uid: u32, token: &[u8; 32], r: Request) -> Response {
        let st = self.state();
        // Checked again on every request: a reload may have revoked this caller.
        if !st.authenticated(client, uid, token) {
            self.refuse("request", client, Some(uid), &Refusal::Unauthenticated);
            return Response::Refused(Refusal::Unauthenticated);
        }
        let Some(key) = st.keys.get(&r.key).cloned() else {
            self.refuse(r.op.kind(), client, Some(uid), &Refusal::UnknownKey);
            return Response::Refused(Refusal::UnknownKey);
        };
        if !st
            .callers
            .get(client)
            .is_some_and(|c| c.grants.contains(&r.key))
        {
            self.refuse(r.op.kind(), client, Some(uid), &Refusal::NotGranted);
            return Response::Refused(Refusal::NotGranted);
        }
        drop(st);
        let kind = r.op.kind();
        match key.answer(&r.op).await {
            Ok(bytes) => {
                self.answered.fetch_add(1, Ordering::Relaxed);
                if key.scheme == Scheme::KeyRing || matches!(r.op, Op::Public) {
                    tracing::debug!(client, key = %key.id, op = kind, "answered");
                    *self
                        .counts
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .entry(format!("{client}/{}", key.id))
                        .or_default() += 1;
                } else {
                    tracing::info!(client, key = %key.id, op = kind, "signed");
                }
                Response::Ok(bytes)
            }
            Err(e) => {
                self.refuse(kind, client, Some(uid), &e);
                Response::Refused(e)
            }
        }
    }
}
