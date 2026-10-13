// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The embedded light client: smoldot-light 2.x, behind subxt's [`RpcClientT`], so that
//! [`super::rpc::RpcChain`] reads and dry-runs through it exactly as it does over a node's RPC.
//!
//! **Why not subxt's own light client** (`subxt/unstable-light-client`, subxt-lightclient 0.44.3,
//! smoldot-light 0.17.2 and smoldot 0.19.4): F-027 NOTES, decision 42. In short, on the zombienet
//! fleet that client failed every dry run of the contract's `execute`, while smoldot-light 2.1.0
//! and the smoldot 3.3.1 polkadot-api bundles answered the same bytes, byte for byte as the
//! collator, and it panicked in its own threads (decisions 37, 40), here also against Kusama
//! mainnet. subxt's later releases embed smoldot-light 1.3, under an API the adapter would have to
//! be ported to; this module is the smaller change.
//!
//! The workspace builds smoldot and smoldot-light from a fork, pinned by commit
//! (`[patch.crates-io]` in its `Cargo.toml`): released smoldot recalculates, once a runtime call
//! has finished, the root of every child trie the call wrote, which the node's call proof does
//! not cover, and a dry run of a batch of several calls failed (F-027 NOTES, decisions 42, 43).
//!
//! What it does is what subxt-lightclient does: one smoldot client per [`LightRpc`], the relay
//! chain added first and the parachain against it, and a background task that routes each
//! JSON-RPC response to its request and each notification to its subscription. A subscription is
//! registered by that task when its response arrives, before any later message is read, so no
//! notification can arrive for a subscription not yet known; one for a subscription no longer
//! listened to is dropped, and the subscription ended.

use std::collections::HashMap;
use std::num::NonZero;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use smoldot_light::platform::DefaultPlatform;
use subxt::backend::rpc::{RawRpcFuture, RawRpcSubscription, RawValue, RpcClientT};
use subxt::ext::subxt_rpcs::{Error as RpcError, UserError};
use tokio::sync::{mpsc, oneshot};

type Client = smoldot_light::Client<Arc<DefaultPlatform>, ()>;
type Notifications = mpsc::UnboundedSender<Result<Box<RawValue>, RpcError>>;

/// What a request waits for: its response; and, for a subscription, where its notifications go.
struct Pending {
    reply: oneshot::Sender<Result<Box<RawValue>, RpcError>>,
    notifications: Option<Notifications>,
}

struct Inner {
    client: Mutex<Client>,
    chain: smoldot_light::ChainId,
    next_id: AtomicU64,
    pending: Mutex<HashMap<String, Pending>>,
    subscriptions: Mutex<HashMap<String, Notifications>>,
}

/// The light client's JSON-RPC, for subxt's [`subxt::backend::rpc::RpcClient`].
#[derive(Clone)]
pub struct LightRpc {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn client_error(e: impl std::fmt::Display) -> RpcError {
    RpcError::Client(e.to_string().into())
}

/// One message from the light client. A `result` that is `null` is a result (`state_getStorage`
/// of an absent key answers `null`), not a missing one: see [`present`].
#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default, deserialize_with = "present")]
    result: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "present")]
    error: Option<Box<RawValue>>,
    #[serde(default)]
    params: Option<NotificationParams>,
}

#[derive(Deserialize)]
struct NotificationParams {
    subscription: serde_json::Value,
    #[serde(default, deserialize_with = "present")]
    result: Option<Box<RawValue>>,
    #[serde(default, deserialize_with = "present")]
    error: Option<Box<RawValue>>,
}

/// A field that is there, whatever its value, `null` included; `#[serde(default)]` makes an absent
/// one `None`. Plain `Option` would read `null` as absent.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Box<RawValue>>, D::Error> {
    Box::<RawValue>::deserialize(d).map(Some)
}

/// A JSON id, as the key it was sent under: a string as itself, anything else as its JSON text.
fn id_key(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn error_of(raw: &RawValue) -> RpcError {
    match serde_json::from_str::<UserError>(raw.get()) {
        Ok(user) => RpcError::User(user),
        Err(_) => client_error(raw.get()),
    }
}

impl LightRpc {
    /// Start a light client for `chain_spec`: a parachain of `relay_chain_spec` when one is given
    /// (Kreivo on Kusama), or a chain of its own. Both are chain specs' JSON text, with their boot
    /// nodes. Must be called inside a Tokio runtime: the routing task is spawned on it.
    pub fn new(chain_spec: &str, relay_chain_spec: Option<&str>) -> Result<LightRpc, String> {
        let platform =
            DefaultPlatform::new("aledgent-settle".into(), env!("CARGO_PKG_VERSION").into());
        let mut client = Client::new(platform);
        let json_rpc = || smoldot_light::AddChainConfigJsonRpc::Enabled {
            max_pending_requests: NonZero::<u32>::MAX,
            max_subscriptions: u32::MAX,
        };
        let relay = match relay_chain_spec {
            Some(spec) => {
                let added = client
                    .add_chain(smoldot_light::AddChainConfig {
                        user_data: (),
                        specification: spec,
                        database_content: "",
                        potential_relay_chains: std::iter::empty(),
                        // No request is ever made of the relay chain.
                        json_rpc: smoldot_light::AddChainConfigJsonRpc::Disabled,
                        statement_protocol_config: None,
                    })
                    .map_err(|e| format!("the relay chain: {e}"))?;
                Some(added.chain_id)
            }
            None => None,
        };
        let added = client
            .add_chain(smoldot_light::AddChainConfig {
                user_data: (),
                specification: chain_spec,
                database_content: "",
                potential_relay_chains: relay.into_iter(),
                json_rpc: json_rpc(),
                statement_protocol_config: None,
            })
            .map_err(|e| format!("the chain: {e}"))?;
        let mut responses = added
            .json_rpc_responses
            .ok_or("the chain was added without JSON-RPC")?;
        let inner = Arc::new(Inner {
            client: Mutex::new(client),
            chain: added.chain_id,
            next_id: AtomicU64::new(1),
            pending: Mutex::default(),
            subscriptions: Mutex::default(),
        });
        // Holds the client only weakly: dropping the last `LightRpc` stops it.
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            while let Some(text) = responses.next().await {
                let Some(inner) = weak.upgrade() else { break };
                inner.route(&text);
            }
        });
        Ok(LightRpc { inner })
    }
}

impl Inner {
    fn route(&self, text: &str) {
        let Ok(m) = serde_json::from_str::<Message>(text) else {
            tracing::debug!(
                message = text,
                "an unreadable message from the light client"
            );
            return;
        };
        if let Some(id) = &m.id {
            let Some(p) = lock(&self.pending).remove(&id_key(id)) else {
                return;
            };
            let reply = match (m.result, m.error) {
                (Some(result), _) => {
                    // A subscription's id: registered now, before any later message is read.
                    if let Some(tx) = p.notifications {
                        match serde_json::from_str::<serde_json::Value>(result.get()) {
                            Ok(sub) => {
                                lock(&self.subscriptions).insert(id_key(&sub), tx);
                            }
                            Err(e) => {
                                let _ = p.reply.send(Err(RpcError::Deserialization(e)));
                                return;
                            }
                        }
                    }
                    Ok(result)
                }
                (None, Some(error)) => Err(error_of(&error)),
                (None, None) => Err(client_error("a response with neither result nor error")),
            };
            let _ = p.reply.send(reply);
            return;
        }
        let Some(params) = m.params else { return };
        let key = id_key(&params.subscription);
        let item = match (params.result, params.error) {
            (Some(r), _) => Ok(r),
            (None, Some(e)) => Err(error_of(&e)),
            (None, None) => return,
        };
        let mut subs = lock(&self.subscriptions);
        if let Some(tx) = subs.get(&key) {
            if tx.send(item).is_err() {
                subs.remove(&key);
            }
        }
    }

    fn send(
        &self,
        method: &str,
        params: Option<Box<RawValue>>,
        notifications: Option<Notifications>,
    ) -> oneshot::Receiver<Result<Box<RawValue>, RpcError>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).to_string();
        let (reply, rx) = oneshot::channel();
        lock(&self.pending).insert(
            id.clone(),
            Pending {
                reply,
                notifications,
            },
        );
        let request = format!(
            r#"{{"jsonrpc":"2.0","id":"{id}","method":{},"params":{}}}"#,
            serde_json::Value::String(method.to_owned()),
            params.as_ref().map_or("[]", |p| p.get())
        );
        if let Err(e) = lock(&self.client).json_rpc_request(request, self.chain) {
            if let Some(p) = lock(&self.pending).remove(&id) {
                let _ = p.reply.send(Err(client_error(format!(
                    "the light client refused {method}: {e:?}"
                ))));
            }
        }
        rx
    }
}

/// A subscription's notifications; dropping it unsubscribes.
struct Subscription {
    rx: mpsc::UnboundedReceiver<Result<Box<RawValue>, RpcError>>,
    inner: Arc<Inner>,
    id: String,
    unsubscribe: String,
}

impl subxt::ext::futures::Stream for Subscription {
    type Item = Result<Box<RawValue>, RpcError>;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        lock(&self.inner.subscriptions).remove(&self.id);
        let params = serde_json::value::to_raw_value(&[&self.id]).ok();
        // Its answer is not waited for.
        drop(self.inner.send(&self.unsubscribe, params, None));
    }
}

impl RpcClientT for LightRpc {
    fn request_raw<'a>(
        &'a self,
        method: &'a str,
        params: Option<Box<RawValue>>,
    ) -> RawRpcFuture<'a, Box<RawValue>> {
        let rx = self.inner.send(method, params, None);
        Box::pin(async move {
            rx.await
                .map_err(|_| client_error("the light client stopped"))?
        })
    }

    fn subscribe_raw<'a>(
        &'a self,
        sub: &'a str,
        params: Option<Box<RawValue>>,
        unsub: &'a str,
    ) -> RawRpcFuture<'a, RawRpcSubscription> {
        let (tx, notifications) = mpsc::unbounded_channel();
        let rx = self.inner.send(sub, params, Some(tx));
        let inner = self.inner.clone();
        Box::pin(async move {
            let result = rx
                .await
                .map_err(|_| client_error("the light client stopped"))??;
            let id = id_key(
                &serde_json::from_str::<serde_json::Value>(result.get())
                    .map_err(RpcError::Deserialization)?,
            );
            Ok(RawRpcSubscription {
                id: Some(id.clone()),
                stream: Box::pin(Subscription {
                    rx: notifications,
                    inner,
                    id,
                    unsubscribe: unsub.to_owned(),
                }),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_result_is_a_result_and_an_absent_one_is_not() {
        let m: Message =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"3","result":null}"#).unwrap();
        assert_eq!(
            m.result.map(|r| r.get().to_owned()),
            Some("null".to_owned())
        );
        assert!(m.error.is_none());
        let m: Message = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":"4","error":{"code":-32000,"message":"x"}}"#,
        )
        .unwrap();
        assert!(m.result.is_none() && m.error.is_some());
        let m: Message = serde_json::from_str(
            r#"{"jsonrpc":"2.0","method":"chain_finalizedHead","params":{"subscription":"s","result":null}}"#,
        )
        .unwrap();
        let p = m.params.unwrap();
        assert_eq!(
            p.result.map(|r| r.get().to_owned()),
            Some("null".to_owned())
        );
        assert_eq!(id_key(&p.subscription), "s");
    }

    #[test]
    fn ids_are_keyed_as_sent() {
        assert_eq!(id_key(&serde_json::json!("7")), "7");
        assert_eq!(id_key(&serde_json::json!(7)), "7");
        assert_eq!(id_key(&serde_json::json!("abc")), "abc");
    }

    #[test]
    fn a_user_error_is_reported_as_one() {
        let raw = serde_json::value::to_raw_value(
            &serde_json::json!({"code": -32000, "message": "no node available"}),
        )
        .unwrap();
        let e = error_of(&raw);
        assert!(
            matches!(e, RpcError::User(ref u) if u.code == -32000),
            "{e}"
        );
        assert!(e.to_string().contains("no node available"));
    }
}
