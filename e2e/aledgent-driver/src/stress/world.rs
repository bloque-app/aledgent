// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The book a run drives, and the flows it sends: who holds accounts, how they authenticate, and
//! what one unit of the workload is.
//!
//! **Principals.** The operator is the deployment's service principal (its configured bearer
//! token), as an issuing client is: it opens identities and accounts and issues and redeems. Holders act on
//! their own accounts with their own credential: an API key, which is every holder's first
//! credential, or a passkey session (`DEC-19`), for the share the run asks for. A passkey holder
//! signs in again before its 15-minute session runs out, as a client would.
//!
//! **Hot accounts.** Three hubs stand for the accounts every card purchase, fee and top-up touches
//! in a payments deployment, where a few accounts take most transfers: a merchant settlement account (credited by purchases and
//! captures), a fee account (credited by fee legs) and a treasury (debited by top-ups and
//! payouts). The issuer's pool is a fourth: every mint lands there and every burn leaves it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use authn::webauthn::testing::Soft;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::Digest as _;
use tokio::sync::{Mutex, Semaphore};

use super::record::{Completion, Kind, Outcome, Recorder, Sample};
use super::target::{Reply, Target};

/// The relying party a stress deployment configures for its software passkeys.
pub const RP_ID: &str = "stress.aledgent.test";
/// Its origin.
pub const ORIGIN: &str = "https://stress.aledgent.test";
/// Sign in again once a session is this old: its lifetime is 15 minutes (`DEC-19`).
const SESSION_REFRESH: Duration = Duration::from_secs(600);
/// The most passkeys the software authenticator can tell apart (its seed is one byte).
pub const MAX_PASSKEYS: usize = 200;

/// How a holder authenticates.
pub enum Auth {
    /// An API key, presented as it is.
    ApiKey(String),
    /// A passkey, and the session it last signed in for.
    Passkey {
        /// The passkey's credential.
        credential: String,
        /// The authenticator, and the session with when it was issued.
        state: Box<Mutex<(Soft, String, Instant)>>,
    },
}

/// A principal with one account.
pub struct Holder {
    /// Its identity.
    pub identity: String,
    /// Its account.
    pub account: String,
    /// How it authenticates.
    pub auth: Auth,
}

/// The asset every holder is funded in, and the ones the issuer issues and redeems.
#[derive(Clone, Debug)]
pub struct Assets {
    /// The card asset: `DUSD` where registered, else the registry's first.
    pub main: String,
    /// The issuer's assets: `COPM`, `TSTA`, `TSTB`, `TSTC`, those registered.
    pub swap: Vec<String>,
}

/// Everything a run needs.
pub struct World {
    /// The surface.
    pub target: Target,
    /// The operator's token.
    pub operator: String,
    /// The operator's identity.
    pub operator_identity: String,
    /// The assets.
    pub assets: Assets,
    /// The funded holders who pay.
    pub holders: Vec<Arc<Holder>>,
    /// Merchant settlement, fees, treasury.
    pub hubs: [Arc<Holder>; 3],
    /// The issuer's pool, the operator's own account.
    pub pool: String,
    /// What the run recorded.
    pub rec: Arc<Recorder>,
    /// This run's prefix for client keys.
    pub run: String,
    seq: AtomicU64,
    /// Bounds the completion pollers.
    pub pollers: Arc<Semaphore>,
    /// Completion polls patience.
    pub patience: Duration,
    /// Holders onboarded during the run (not payers).
    pub onboarded: AtomicU64,
    next_seed: AtomicU64,
    /// The phase now running.
    pub phase: AtomicU64,
    /// Of the transfers and reservations, how many per cent the operator makes on a holder's
    /// behalf, as an issuing client does (its notifications reach the operator's recipient); the rest the
    /// holder makes with its own credential.
    pub service_pct: u64,
}

/// A small deterministic generator (xorshift64*): the run's mix and amounts, reproducible from
/// its seed. No floating point.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    /// From a seed (zero is replaced).
    pub fn new(seed: u64) -> Rng {
        Rng(seed.max(1))
    }

    /// The next value.
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// What one unit of the workload is, with its weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// One transfer: holder to holder, holder to the merchant hub, or treasury to holder.
    Transfer,
    /// A purchase with its fee: holder to merchant and holder to fees, sometimes a third leg.
    MultiLeg,
    /// A card authorisation, then its settlement: reserve, then capture (one in five for less).
    ReserveCapture,
    /// An authorisation that lapses: reserve, then release.
    ReserveRelease,
    /// A new customer: identity (API key), account, top-up from the treasury.
    Onboard,
    /// The issuer: mint into the pool, or burn out of it, in one of its assets.
    Swap,
    /// A balance read.
    Read,
    /// A passkey holder signs in again.
    SignIn,
}

/// The mix, in hundredths of all flows, and the commands each flow sends. An assumed
/// payments workload (`features/F-032-stress/plan.md` §2): onboarding is about a third of
/// commands, card spend a quarter, plain transfers the rest, issuance a few per cent; reads and sign-ins are added on top, as an assumption (no read volume is recorded).
pub const MIX: &[(Flow, u64, u64)] = &[
    (Flow::Transfer, 35, 1),
    (Flow::MultiLeg, 10, 1),
    (Flow::ReserveCapture, 15, 2),
    (Flow::ReserveRelease, 5, 2),
    (Flow::Onboard, 15, 3),
    (Flow::Swap, 5, 1),
    (Flow::Read, 13, 0),
    (Flow::SignIn, 2, 0),
];

/// Commands per flow, in thousandths, over [`MIX`].
pub fn commands_per_flow_milli() -> u64 {
    let w: u64 = MIX.iter().map(|m| m.1).sum();
    MIX.iter().map(|m| m.1 * m.2).sum::<u64>() * 1000 / w.max(1)
}

/// A flow drawn from [`MIX`].
pub fn draw(rng: &mut Rng) -> Flow {
    let w: u64 = MIX.iter().map(|m| m.1).sum();
    let mut x = rng.below(w);
    for (f, weight, _) in MIX {
        if x < *weight {
            return *f;
        }
        x -= weight;
    }
    Flow::Transfer
}

fn classify(r: &Result<Reply, String>) -> Outcome {
    match r {
        Err(e) => Outcome::Transport(e.clone()),
        Ok(r) if r.status == 202 => Outcome::Acknowledged,
        Ok(r) if r.status == 200 => Outcome::Ok,
        Ok(r) if r.error() == Some("ERR-DecisionUnavailable") => Outcome::DecisionUnavailable,
        Ok(r) if r.status >= 500 => Outcome::ServerError(
            r.error()
                .map(str::to_owned)
                .unwrap_or_else(|| r.status.to_string()),
        ),
        Ok(r) => Outcome::Refused(
            r.error()
                .map(str::to_owned)
                .unwrap_or_else(|| r.status.to_string()),
        ),
    }
}

fn result_str(v: &Value, field: &str) -> Option<String> {
    v["result"][field]
        .as_str()
        .or_else(|| v[field].as_str())
        .map(str::to_owned)
}

/// A presented API key for `credential` (`did:aledgent:cred:…`) and its secret.
pub fn api_key(credential: &str, secret: &[u8; 32]) -> String {
    let body = credential
        .strip_prefix(ledger_core::id::DID_PREFIX)
        .and_then(|c| c.strip_prefix("cred:"))
        .unwrap_or(credential);
    format!(
        "{}{body}.{}",
        authn::token::API_KEY_PREFIX,
        URL_SAFE_NO_PAD.encode(secret)
    )
}

fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    let _ = getrandom::fill(&mut b);
    b
}

impl World {
    /// A client key unique to this run.
    pub fn key(&self) -> String {
        format!("{}-{}", self.run, self.seq.fetch_add(1, Ordering::Relaxed))
    }

    fn phase(&self) -> usize {
        usize::try_from(self.phase.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// Send one command or read, record it, and return the reply.
    pub async fn send(
        &self,
        kind: Kind,
        method: &str,
        path: &str,
        token: &str,
        body: Option<Value>,
        client_key: Option<&str>,
    ) -> Result<Reply, String> {
        let at_ms = self.rec.now_ms();
        let phase = self.phase();
        let headers: Vec<(&str, &str)> = client_key
            .map(|k| vec![("client-key", k)])
            .unwrap_or_default();
        let r = self
            .target
            .call(method, path, Some(token), &headers, body.as_ref())
            .await;
        let outcome = classify(&r);
        let us = match &r {
            Ok(r) => r.us,
            Err(_) => u64::try_from(self.rec.start.elapsed().as_micros())
                .unwrap_or(u64::MAX)
                .saturating_sub(at_ms.saturating_mul(1000)),
        };
        self.rec.sample(Sample {
            phase,
            kind,
            outcome,
            us,
            at_ms,
            auth: super::record::auth_of(token),
        });
        r
    }

    /// A decided command during setup: asked again under the same client key while the core
    /// cannot decide it in time (`ERR-DecisionUnavailable` keeps the command, `REQ-CS-7`), for up
    /// to `patience`. Not recorded: setup is not the load.
    pub async fn setup_call(
        &self,
        path: &str,
        token: &str,
        body: Value,
        patience: Duration,
    ) -> Result<Value, String> {
        let key = self.key();
        let headers = [("client-key", key.as_str())];
        let started = Instant::now();
        loop {
            let r = self
                .target
                .call("POST", path, Some(token), &headers, Some(&body))
                .await;
            match r {
                Ok(r) if r.status == 200 => return Ok(r.body),
                Ok(r) if r.status == 202 => {
                    // Acknowledged: ask again for the decision.
                }
                Ok(r)
                    if r.error() == Some("ERR-DecisionUnavailable")
                        || r.error() == Some("ERR-SettlementLayerUnavailable") => {}
                Ok(r) => return Err(format!("{path}: {} {}", r.status, r.body)),
                Err(e) if started.elapsed() < patience => {
                    eprintln!("[stress] {path}: {e}, again");
                }
                Err(e) => return Err(format!("{path}: {e}")),
            }
            if started.elapsed() > patience {
                return Err(format!(
                    "{path}: not decided within {}s",
                    patience.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// The token a holder presents now: its API key, or a session, signing in again first when
    /// the session is old (that sign-in is part of the load).
    pub async fn token(&self, h: &Holder) -> String {
        match &h.auth {
            Auth::ApiKey(k) => k.clone(),
            Auth::Passkey {
                credential, state, ..
            } => {
                let mut s = state.lock().await;
                if s.2.elapsed() > SESSION_REFRESH {
                    if let Some(session) = self.sign_in(&mut s.0, credential, true).await {
                        s.1 = session;
                        s.2 = Instant::now();
                    }
                }
                s.1.clone()
            }
        }
    }

    /// One passkey ceremony: a challenge, then the assertion. The session, or nothing. With
    /// `record`, both requests are one `SignIn` sample.
    pub async fn sign_in(&self, soft: &mut Soft, credential: &str, record: bool) -> Option<String> {
        let started = Instant::now();
        let at_ms = self.rec.now_ms();
        let ch = self
            .target
            .call(
                "POST",
                "/v1/auth/challenge",
                None,
                &[],
                Some(&json!({ "purpose": "authenticate" })),
            )
            .await;
        let outcome;
        let mut session = None;
        match &ch {
            Ok(c) if c.status == 200 => {
                let challenge = URL_SAFE_NO_PAD
                    .decode(c.body["challenge"].as_str().unwrap_or_default())
                    .unwrap_or_default();
                soft.counter = soft.counter.saturating_add(1);
                let (ad, cd, sig) = soft.assert(&challenge, ORIGIN);
                let body = json!({
                    "credential": credential,
                    "assertion": {
                        "raw_id": URL_SAFE_NO_PAD.encode(&soft.raw_id),
                        "authenticator_data": URL_SAFE_NO_PAD.encode(&ad),
                        "client_data_json": URL_SAFE_NO_PAD.encode(&cd),
                        "signature": URL_SAFE_NO_PAD.encode(&sig),
                    },
                });
                let r = self
                    .target
                    .call("POST", "/v1/auth/passkey", None, &[], Some(&body))
                    .await;
                outcome = classify(&r);
                if let Ok(r) = &r {
                    session = r.body["session"].as_str().map(str::to_owned);
                }
            }
            _ => outcome = classify(&ch),
        }
        if record {
            self.rec.sample(Sample {
                phase: self.phase(),
                kind: Kind::SignIn,
                outcome,
                us: super::target::elapsed_us(started),
                at_ms,
                auth: "none",
            });
        }
        session
    }

    /// Poll `op` until it leaves `Received` (backing off to 2 s), or `patience` passes: its
    /// state, or nothing. Polls are not samples.
    pub async fn until_decided(&self, token: &str, op: &str, patience: Duration) -> Option<String> {
        let started = Instant::now();
        let mut delay = Duration::from_millis(50);
        let path = format!("/v1/operations/{op}");
        loop {
            if let Ok(r) = self.target.call("GET", &path, Some(token), &[], None).await {
                if r.status == 200 {
                    let s = r.body["state"].as_str().unwrap_or_default().to_owned();
                    if !s.is_empty() && s != "Received" {
                        return Some(s);
                    }
                }
            }
            if started.elapsed() > patience {
                return None;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    /// An acknowledged command: keep its reference for the loss check, and sample its
    /// completion if a poller is free.
    async fn acknowledged(self: &Arc<Self>, kind: Kind, token: &str, reply: &Reply, sample: bool) {
        let Some(op) = reply.body["operation_reference"]
            .as_str()
            .map(str::to_owned)
        else {
            return;
        };
        self.rec
            .acknowledged
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(format!("{op} {token}"));
        if !sample {
            return;
        }
        let Ok(permit) = Arc::clone(&self.pollers).try_acquire_owned() else {
            self.rec.unsampled.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let me = Arc::clone(self);
        let token = token.to_owned();
        let phase = self.phase();
        let acked = Instant::now();
        tokio::spawn(async move {
            let _permit = permit;
            if let Some(state) = me.until_decided(&token, &op, me.patience).await {
                me.rec.completion(Completion {
                    phase,
                    kind,
                    us: super::target::elapsed_us(acked),
                    state,
                });
            }
        });
    }

    /// Who makes a command on `h`'s account: the operator (its service principal) for
    /// `service_pct` per cent, the holder itself for the rest.
    async fn actor(&self, h: &Holder, rng: &mut Rng) -> String {
        if rng.below(100) < self.service_pct {
            self.operator.clone()
        } else {
            self.token(h).await
        }
    }

    fn holder(&self, rng: &mut Rng) -> &Arc<Holder> {
        let n = u64::try_from(self.holders.len()).unwrap_or(1);
        &self.holders[usize::try_from(rng.below(n)).unwrap_or(0)]
    }

    fn amount(rng: &mut Rng) -> String {
        (100 + rng.below(9_900)).to_string()
    }

    /// Run one flow.
    pub async fn flow(self: &Arc<Self>, flow: Flow, mut rng: Rng) {
        let main = self.assets.main.clone();
        match flow {
            Flow::Transfer => {
                let pick = rng.below(10);
                let (from, to) = if pick < 5 {
                    let a = Arc::clone(self.holder(&mut rng));
                    let mut b = Arc::clone(self.holder(&mut rng));
                    if Arc::ptr_eq(&a, &b) {
                        b = Arc::clone(&self.hubs[0]);
                    }
                    (a, b.account.clone())
                } else if pick < 8 {
                    (
                        Arc::clone(self.holder(&mut rng)),
                        self.hubs[0].account.clone(),
                    )
                } else {
                    (
                        Arc::clone(&self.hubs[2]),
                        self.holder(&mut rng).account.clone(),
                    )
                };
                let token = self.actor(&from, &mut rng).await;
                let body = json!({ "legs": [{ "from": from.account, "to": to, "asset": main, "amount": Self::amount(&mut rng) }] });
                let key = self.key();
                if let Ok(r) = self
                    .send(
                        Kind::Transfer,
                        "POST",
                        "/v1/transfers",
                        &token,
                        Some(body),
                        Some(&key),
                    )
                    .await
                {
                    if r.status == 202 {
                        let sample = rng.below(4) == 0;
                        self.acknowledged(Kind::Transfer, &token, &r, sample).await;
                    }
                }
            }
            Flow::MultiLeg => {
                let payer = Arc::clone(self.holder(&mut rng));
                let token = self.actor(&payer, &mut rng).await;
                let mut legs = vec![
                    json!({ "from": payer.account, "to": self.hubs[0].account, "asset": main, "amount": Self::amount(&mut rng) }),
                    json!({ "from": payer.account, "to": self.hubs[1].account, "asset": main, "amount": (10 + rng.below(90)).to_string() }),
                ];
                if rng.below(3) == 0 {
                    let other = self.holder(&mut rng);
                    if other.account != payer.account {
                        legs.push(json!({ "from": payer.account, "to": other.account, "asset": main, "amount": Self::amount(&mut rng) }));
                    }
                }
                let key = self.key();
                if let Ok(r) = self
                    .send(
                        Kind::MultiLeg,
                        "POST",
                        "/v1/transfers",
                        &token,
                        Some(json!({ "legs": legs })),
                        Some(&key),
                    )
                    .await
                {
                    if r.status == 202 {
                        let sample = rng.below(4) == 0;
                        self.acknowledged(Kind::MultiLeg, &token, &r, sample).await;
                    }
                }
            }
            Flow::ReserveCapture | Flow::ReserveRelease => {
                let payer = Arc::clone(self.holder(&mut rng));
                let token = self.actor(&payer, &mut rng).await;
                let amount = 100 + rng.below(9_900);
                let key = self.key();
                let Ok(r) = self
                    .send(
                        Kind::Reserve,
                        "POST",
                        "/v1/reservations",
                        &token,
                        Some(json!({ "payer": payer.account, "asset": main, "amount": amount.to_string() })),
                        Some(&key),
                    )
                    .await
                else {
                    return;
                };
                if r.status != 202 {
                    return;
                }
                let Some(op) = r.body["operation_reference"].as_str().map(str::to_owned) else {
                    return;
                };
                self.rec
                    .acknowledged
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(format!("{op} {token}"));
                let acked = Instant::now();
                let phase = self.phase();
                let state = self.until_decided(&token, &op, self.patience).await;
                let Some(state) = state else {
                    self.rec.abandoned.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                self.rec.completion(Completion {
                    phase,
                    kind: Kind::Reserve,
                    us: super::target::elapsed_us(acked),
                    state: state.clone(),
                });
                if state != "Reserved" {
                    return;
                }
                // The settlement of an authorisation comes a moment later.
                tokio::time::sleep(Duration::from_millis(rng.below(500))).await;
                // Whoever reserved settles it.
                let token = if token == self.operator {
                    token
                } else {
                    self.token(&payer).await
                };
                let key = self.key();
                if flow == Flow::ReserveCapture {
                    let cleared = if rng.below(5) == 0 {
                        amount - rng.below(amount / 2)
                    } else {
                        amount
                    };
                    let _ = self
                        .send(
                            Kind::Capture,
                            "POST",
                            &format!("/v1/reservations/{op}/capture"),
                            &token,
                            Some(json!({ "destination": self.hubs[0].account, "cleared": cleared.to_string() })),
                            Some(&key),
                        )
                        .await;
                } else {
                    let _ = self
                        .send(
                            Kind::Release,
                            "POST",
                            &format!("/v1/reservations/{op}/release"),
                            &token,
                            Some(json!({})),
                            Some(&key),
                        )
                        .await;
                }
            }
            Flow::Onboard => {
                let secret = random32();
                let verifier = hex::encode(sha2::Sha256::digest(secret));
                let key = self.key();
                let Ok(r) = self
                    .send(
                        Kind::OpenIdentity,
                        "POST",
                        "/v1/identities",
                        &self.operator,
                        Some(
                            json!({ "kind": "person", "verifier": verifier, "scheme": "api-key" }),
                        ),
                        Some(&key),
                    )
                    .await
                else {
                    return;
                };
                let Some(identity) = result_str(&r.body, "identity").filter(|_| r.status == 200)
                else {
                    return;
                };
                let key = self.key();
                let Ok(r) = self
                    .send(
                        Kind::OpenAccount,
                        "POST",
                        "/v1/accounts",
                        &self.operator,
                        Some(json!({ "owner": identity, "kind": "customer", "custody": "delegated" })),
                        Some(&key),
                    )
                    .await
                else {
                    return;
                };
                let Some(account) = result_str(&r.body, "account").filter(|_| r.status == 200)
                else {
                    return;
                };
                self.onboarded.fetch_add(1, Ordering::Relaxed);
                // The top-up: a transfer from the treasury.
                let treasury = Arc::clone(&self.hubs[2]);
                let token = self.token(&treasury).await;
                let key = self.key();
                if let Ok(r) = self
                    .send(
                        Kind::Transfer,
                        "POST",
                        "/v1/transfers",
                        &token,
                        Some(json!({ "legs": [{ "from": treasury.account, "to": account, "asset": main, "amount": "100000" }] })),
                        Some(&key),
                    )
                    .await
                {
                    if r.status == 202 {
                        self.acknowledged(Kind::Transfer, &token, &r, false).await;
                    }
                }
            }
            Flow::Swap => {
                let n = u64::try_from(self.assets.swap.len()).unwrap_or(0);
                let asset = if n == 0 {
                    main.clone()
                } else {
                    self.assets.swap[usize::try_from(rng.below(n)).unwrap_or(0)].clone()
                };
                let key = self.key();
                if rng.below(2) == 0 {
                    let _ = self
                        .send(
                            Kind::Issue,
                            "POST",
                            "/v1/issuances",
                            &self.operator,
                            Some(json!({ "asset": asset, "destination": self.pool, "amount": Self::amount(&mut rng) })),
                            Some(&key),
                        )
                        .await;
                } else {
                    let _ = self
                        .send(
                            Kind::Redeem,
                            "POST",
                            "/v1/redemptions",
                            &self.operator,
                            Some(json!({ "asset": asset, "source": self.pool, "amount": Self::amount(&mut rng) })),
                            Some(&key),
                        )
                        .await;
                }
            }
            Flow::Read => {
                let h = Arc::clone(self.holder(&mut rng));
                let token = self.token(&h).await;
                let path = format!("/v1/balances?account={}&asset={main}", h.account);
                let _ = self
                    .send(Kind::Read, "GET", &path, &token, None, None)
                    .await;
            }
            Flow::SignIn => {
                let passkeyed: Vec<&Arc<Holder>> = self
                    .holders
                    .iter()
                    .filter(|h| matches!(h.auth, Auth::Passkey { .. }))
                    .collect();
                if passkeyed.is_empty() {
                    return;
                }
                let n = u64::try_from(passkeyed.len()).unwrap_or(1);
                let h = passkeyed[usize::try_from(rng.below(n)).unwrap_or(0)];
                if let Auth::Passkey {
                    credential, state, ..
                } = &h.auth
                {
                    let mut s = state.lock().await;
                    if let Some(session) = self.sign_in(&mut s.0, credential, true).await {
                        s.1 = session;
                        s.2 = Instant::now();
                    }
                }
            }
        }
    }
}

/// How the population is built.
pub struct SetupOptions {
    /// Funded paying holders.
    pub holders: usize,
    /// Of them, how many per cent sign in with a passkey (at most [`MAX_PASSKEYS`]).
    pub passkey_pct: usize,
    /// Setup requests in flight at once.
    pub concurrency: usize,
    /// How long one setup command may take to be decided.
    pub patience: Duration,
    /// Completion pollers at once.
    pub pollers: usize,
    /// See [`World::service_pct`].
    pub service_pct: usize,
}

/// The registry: (main asset, the issuer's assets).
async fn assets(target: &Target, token: &str) -> Result<Assets, String> {
    let r = target
        .call("GET", "/v1/assets", Some(token), &[], None)
        .await?;
    if r.status != 200 {
        return Err(format!("GET /v1/assets: {} {}", r.status, r.body));
    }
    let mut ids: Vec<String> = Vec::new();
    let rows = r.body["assets"]
        .as_array()
        .or_else(|| r.body["rows"].as_array())
        .or_else(|| r.body.as_array())
        .cloned()
        .unwrap_or_default();
    for a in rows {
        if let Some(id) = a["id"].as_str().or_else(|| a["asset"].as_str()) {
            ids.push(id.to_owned());
        }
    }
    if ids.is_empty() {
        return Err(format!("the registry is empty: {}", r.body));
    }
    let main = if ids.iter().any(|a| a == "DUSD") {
        "DUSD".to_owned()
    } else {
        ids[0].clone()
    };
    let swap = ["COPM", "TSTA", "TSTB", "TSTC"]
        .iter()
        .filter(|a| ids.iter().any(|i| i == *a))
        .map(|a| (*a).to_owned())
        .collect();
    Ok(Assets { main, swap })
}

/// Found the population: issuance grants, the hubs, the pool, the holders (some with passkeys),
/// every one funded.
pub async fn setup(
    target: Target,
    operator: String,
    operator_identity: String,
    rec: Arc<Recorder>,
    opts: &SetupOptions,
) -> Result<Arc<World>, String> {
    let assets = assets(&target, &operator).await?;
    rec.note(format!(
        "registry: main {}, swap {:?}",
        assets.main, assets.swap
    ));
    let run = format!("stress-{}", hex::encode(&random32()[..6]));
    let placeholder = || {
        Arc::new(Holder {
            identity: String::new(),
            account: String::new(),
            auth: Auth::ApiKey(String::new()),
        })
    };
    let mut world = World {
        target,
        operator,
        operator_identity,
        assets: assets.clone(),
        holders: Vec::new(),
        hubs: [placeholder(), placeholder(), placeholder()],
        pool: String::new(),
        rec: Arc::clone(&rec),
        run,
        seq: AtomicU64::new(0),
        pollers: Arc::new(Semaphore::new(opts.pollers)),
        patience: Duration::from_secs(900),
        onboarded: AtomicU64::new(0),
        next_seed: AtomicU64::new(1),
        phase: AtomicU64::new(0),
        service_pct: u64::try_from(opts.service_pct).unwrap_or(50),
    };
    let p = opts.patience;
    // Issuance grants, one per asset (an issuer holds one per asset it issues).
    let mut every: Vec<String> = vec![assets.main.clone()];
    every.extend(assets.swap.iter().cloned());
    every.dedup();
    for a in &every {
        world
            .setup_call(
                "/v1/grants",
                &world.operator.clone(),
                json!({
                    "kind": "issuance",
                    "principal": world.operator_identity,
                    "subject": { "asset": a },
                    "ops": ["issue", "redeem"],
                    "per_window": { "length": 86_400_000u64, "limit": "1000000000000000000" },
                    "assets": [a],
                }),
                p,
            )
            .await?;
    }
    rec.note(format!("issuance grants: {every:?}"));
    // The operator acts on accounts in delegated custody, as an issuing client does on its customers'.
    world
        .setup_call(
            "/v1/grants",
            &world.operator.clone(),
            json!({
                "kind": "operator",
                "principal": world.operator_identity,
                "subject": "delegated_accounts",
                "ops": ["transfer", "reserve", "capture", "release"],
                "per_window": { "length": 86_400_000u64, "limit": "1000000000000000000" },
            }),
            p,
        )
        .await?;
    // The issuer's pool: the operator's own account, funded in every issued asset.
    let v = world
        .setup_call(
            "/v1/accounts",
            &world.operator.clone(),
            json!({ "owner": world.operator_identity, "kind": "customer", "custody": "delegated" }),
            p,
        )
        .await?;
    world.pool = result_str(&v, "account").ok_or("the pool has no account")?;
    for a in &every {
        world
            .setup_call(
                "/v1/issuances",
                &world.operator.clone(),
                json!({ "asset": a, "destination": world.pool, "amount": "100000000000000" }),
                p,
            )
            .await?;
    }
    let world = Arc::new(world);
    // The hubs: merchant settlement, fees, treasury (funded deep).
    let mut hubs = Vec::new();
    for (i, funds) in [(0, "0"), (1, "0"), (2, "100000000000000")] {
        let _ = i;
        hubs.push(Arc::new(open_holder(&world, funds, false).await?));
    }
    let passkeys = (opts.holders * opts.passkey_pct / 100).min(MAX_PASSKEYS);
    // The holders, `concurrency` at a time.
    let sem = Arc::new(Semaphore::new(opts.concurrency.max(1)));
    let mut tasks = Vec::new();
    for i in 0..opts.holders {
        let w = Arc::clone(&world);
        let sem = Arc::clone(&sem);
        tasks.push(tokio::spawn(async move {
            let _p = sem.acquire().await;
            open_holder(&w, "1000000000", i < passkeys).await
        }));
    }
    let mut holders = Vec::new();
    for t in tasks {
        holders.push(Arc::new(t.await.map_err(|e| e.to_string())??));
        if holders.len() % 50 == 0 {
            rec.note(format!("{} holders open", holders.len()));
        }
    }
    let mut world = Arc::into_inner(world).ok_or("the world is still shared")?;
    world.hubs = [
        Arc::clone(&hubs[0]),
        Arc::clone(&hubs[1]),
        Arc::clone(&hubs[2]),
    ];
    world.holders = holders;
    rec.note(format!(
        "population: {} holders ({passkeys} with passkeys), 3 hubs, the pool",
        world.holders.len()
    ));
    Ok(Arc::new(world))
}

/// Open one holder: identity with an API key, a customer account, `funds` of the main asset;
/// with `passkey`, a passkey registered through the API key and a first session.
async fn open_holder(world: &World, funds: &str, passkey: bool) -> Result<Holder, String> {
    let p = Duration::from_secs(600);
    let op = world.operator.clone();
    let secret = random32();
    let verifier = hex::encode(sha2::Sha256::digest(secret));
    let v = world
        .setup_call(
            "/v1/identities",
            &op,
            json!({ "kind": "person", "verifier": verifier, "scheme": "api-key" }),
            p,
        )
        .await?;
    let identity = result_str(&v, "identity").ok_or_else(|| format!("no identity: {v}"))?;
    let credential = result_str(&v, "credential").ok_or_else(|| format!("no credential: {v}"))?;
    let key = api_key(&credential, &secret);
    let v = world
        .setup_call(
            "/v1/accounts",
            &op,
            json!({ "owner": identity, "kind": "customer", "custody": "delegated" }),
            p,
        )
        .await?;
    let account = result_str(&v, "account").ok_or_else(|| format!("no account: {v}"))?;
    if funds != "0" {
        world
            .setup_call(
                "/v1/issuances",
                &op,
                json!({ "asset": world.assets.main, "destination": account, "amount": funds }),
                p,
            )
            .await?;
    }
    if !passkey {
        return Ok(Holder {
            identity,
            account,
            auth: Auth::ApiKey(key),
        });
    }
    let seed = u8::try_from(world.next_seed.fetch_add(1, Ordering::Relaxed)).unwrap_or(1);
    let mut soft = Soft::new(seed, RP_ID);
    let ch = world
        .target
        .call(
            "POST",
            "/v1/auth/challenge",
            None,
            &[],
            Some(&json!({ "purpose": "register" })),
        )
        .await?;
    let challenge = URL_SAFE_NO_PAD
        .decode(ch.body["challenge"].as_str().unwrap_or_default())
        .map_err(|_| format!("no challenge: {}", ch.body))?;
    let (cd, att) = soft.register(&challenge, ORIGIN);
    let v = world
        .setup_call(
            "/v1/credentials",
            &key,
            json!({
                "marker": "ordinary",
                "scheme": "webauthn-es256",
                "passkey": {
                    "client_data_json": URL_SAFE_NO_PAD.encode(&cd),
                    "attestation_object": URL_SAFE_NO_PAD.encode(&att),
                },
            }),
            p,
        )
        .await?;
    let pk = result_str(&v, "credential").ok_or_else(|| format!("no passkey: {v}"))?;
    let session = world
        .sign_in(&mut soft, &pk, false)
        .await
        .ok_or("the first passkey sign-in failed")?;
    Ok(Holder {
        identity,
        account,
        auth: Auth::Passkey {
            credential: pk,
            state: Box::new(Mutex::new((soft, session, Instant::now()))),
        },
    })
}

/// The population's size now (for the report).
pub fn describe(w: &World) -> Value {
    json!({
        "holders": w.holders.len(),
        "identities": w.holders.iter().map(|h| h.identity.as_str()).collect::<std::collections::BTreeSet<_>>().len(),
        "passkey_holders": w.holders.iter().filter(|h| matches!(h.auth, Auth::Passkey { .. })).count(),
        "hubs": 3,
        "onboarded_during_run": w.onboarded.load(Ordering::Relaxed),
        "main_asset": w.assets.main,
        "swap_assets": w.assets.swap,
    })
}
