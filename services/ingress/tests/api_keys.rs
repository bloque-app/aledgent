// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! API keys without the core on every request (`T-031-13`, finding F4 of `F-032`): ingress reuses
//! what the core answered for an API key's credential for a short TTL, bounded by `NFR-20`, and
//! `NFR-1`'s acknowledgement series starts at the request's arrival, before authentication.

use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use ingress::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, OutboxPage, Query,
    QueryReply,
};
use ingress::credentials::{API_KEY_CACHE_MAX_TTL, API_KEY_CACHE_TTL, authenticate};
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, T0, TestCore, USD, fast_config, genesis, test_clerk};
use ledger_core::Refusal;
use ledger_core::account::Time;
use serde_json::{Value, json};

fn bearer(key: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {key}")).unwrap_or_else(|e| panic!("{e}")),
    );
    h
}

/// Issue an API key for `token`'s identity: the key and its credential.
async fn api_key<S: ingress::store::Store, C: Core + Clone>(
    d: &Deployment<S, C>,
    token: &str,
    client_key: &str,
) -> (String, String) {
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(token),
            &[("client-key", client_key)],
            Some(json!({ "marker": "ordinary", "scheme": "api-key" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let field = |v: &Value, p: &str| {
        v.pointer(p)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no {p} in {v}"))
            .to_owned()
    };
    (field(&v, "/api_key"), field(&v, "/result/credential"))
}

/// `REQ-AU-3`, `NFR-20`, `T-031-13`: a removed API key stops authenticating within the cache's TTL
/// (500 ms by default), well inside `NFR-20`'s 1 s; the TTL cannot be set above that bound; and
/// with a TTL of zero the removal takes effect on the next request.
#[tokio::test(flavor = "multi_thread")]
async fn req_au_3_a_revoked_api_key_stops_authenticating_within_the_bound() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 100).await;
    let shared = &d.replica.shared;
    assert_eq!(d.auth.verifiers().ttl(), API_KEY_CACHE_TTL);
    assert!(
        d.auth
            .verifiers()
            .set_ttl(API_KEY_CACHE_MAX_TTL + Duration::from_millis(1))
            .is_err(),
        "a TTL above NFR-20's bound is refused"
    );

    for (round, ttl) in [(1, API_KEY_CACHE_TTL), (2, Duration::ZERO)] {
        d.auth
            .verifiers()
            .set_ttl(ttl)
            .unwrap_or_else(|e| panic!("{e}"));
        let (key, credential) = api_key(&d, &alice.token, &format!("ak-{round}")).await;
        let who = authenticate(shared, &bearer(&key))
            .await
            .unwrap_or_else(|r| panic!("the key authenticates: {r:?}"));
        assert_eq!(who.principal, alice.identity);
        let (s, v) = d
            .call(
                "POST",
                &format!("/v1/credentials/{credential}/remove"),
                Some(&alice.token),
                &[],
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        // The removal is committed: from here, the key must stop within the TTL.
        let removed = Instant::now();
        let stopped = loop {
            match authenticate(shared, &bearer(&key)).await {
                Err(Refusal::CredentialInvalid) => break removed.elapsed(),
                Err(r) => panic!("refused otherwise: {r:?}"),
                Ok(_) => {
                    assert!(
                        removed.elapsed() < API_KEY_CACHE_MAX_TTL,
                        "the removed key still authenticates after {:?}",
                        removed.elapsed()
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        };
        println!("T-031-13: TTL {ttl:?}, the removed key stopped after {stopped:?}");
        assert!(
            stopped <= ttl + Duration::from_millis(250),
            "stopped after {stopped:?} with a TTL of {ttl:?}"
        );
        // And it stays stopped: the negative answer is cached too.
        assert_eq!(
            authenticate(shared, &bearer(&key)).await.err(),
            Some(Refusal::CredentialInvalid)
        );
    }
}

/// The in-process core, whose credential lookups take `delay`, as the core Agent's did while an
/// invocation was in flight.
#[derive(Clone)]
struct SlowLookups {
    inner: TestCore,
    delay: Duration,
}

impl Core for SlowLookups {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        self.inner.apply(input).await
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        self.inner.apply_batch(inputs).await
    }

    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: Time,
    ) -> Result<OutboxPage, CoreError> {
        self.inner.outbox(cursor, undelivered, now).await
    }

    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        self.inner.last_tick().await
    }

    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> Result<QueryReply, CoreError> {
        self.inner.query(caller, query, now).await
    }

    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        tokio::time::sleep(self.delay).await;
        self.inner.credential(credential).await
    }

    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        self.inner.party_of(principal).await
    }
}

/// `NFR-1`, `T-031-13`: the acknowledgement series is measured from the request's arrival, so it
/// includes authenticating it. A transfer made with an API key whose lookup takes 150 ms is
/// acknowledged no sooner than that, and the series says so; before, it started after
/// authentication and read a few milliseconds while callers waited for the lookup.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_1_the_acknowledgement_series_includes_authentication() {
    let journal = MemJournal::new();
    let inner = InProcessCore::in_memory(genesis(T0), journal.clone()).with_clerk(test_clerk());
    inner
        .start()
        .unwrap_or_else(|e| panic!("the core starts: {e}"));
    let founded = inner
        .founded()
        .unwrap_or_else(|| unreachable!("a started core is founded"));
    let delay = Duration::from_millis(150);
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        SlowLookups { inner, delay },
        founded,
        journal,
    )
    .await;
    // The cache would hide the delay after the first lookup: read the core every time.
    d.auth
        .verifiers()
        .set_ttl(Duration::ZERO)
        .unwrap_or_else(|e| panic!("{e}"));
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let (key, _) = api_key(&d, &alice.token, "ak-nfr-1").await;
    let (s, v) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&key),
            &[("client-key", "t-nfr-1")],
            Some(json!({ "legs": [{
                "from": alice.account, "to": bob.account, "asset": USD, "amount": "1"
            }] })),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let ack = d.replica.shared.metrics.acknowledgements();
    println!(
        "T-031-13: acknowledgement p99 {} µs with a 150 ms lookup",
        ack.p99_us
    );
    assert!(
        ack.p99_us >= u64::try_from(delay.as_micros()).unwrap_or(u64::MAX),
        "the series starts after authentication: p99 {} µs",
        ack.p99_us
    );
}

/// `T-038-10` (`DEC-27` (e″), `REQ-AU-3`): two ingress instances share one database. Instance A
/// copies an API key's verifier; the credential is removed; instance B, which never copied it
/// (another process, its own cache), reads "no working credential" from the book and deletes the
/// copy's row. With no core answering, the key no longer authenticates on A either.
#[tokio::test(flavor = "multi_thread")]
async fn t_038_10_a_revoked_credential_leaves_the_shared_copy_whoever_copied_it() {
    use ingress::store::Store as _;
    let d = Deployment::start().await;
    let alice = d.party("alice", 100).await;
    let (key, credential) = api_key(&d, &alice.token, "ak-shared").await;
    // A authenticates the key while the core answers: the copy keeps its verifier.
    authenticate(&d.replica.shared, &bearer(&key))
        .await
        .unwrap_or_else(|r| panic!("the key authenticates: {r:?}"));
    let copied = || async { d.store.copied_verifier(&credential).await.unwrap() };
    let start = Instant::now();
    while copied().await.is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the copy is written"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let (s, v) = d
        .call(
            "POST",
            &format!("/v1/credentials/{credential}/remove"),
            Some(&alice.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(d.core.credential(&credential).await, Ok(None), "removed");
    // B: another instance over the same database, with an authenticator of its own.
    let b = ingress::service::Replica::start(
        d.store.clone(),
        d.core.clone(),
        std::sync::Arc::new(d.clock.clone()),
        ingress::auth::Authenticator::new(),
        std::sync::Arc::clone(&d.recipients) as std::sync::Arc<dyn ingress::delivery::Transport>,
        fast_config("ingress-b"),
    );
    assert_eq!(
        authenticate(&b.shared, &bearer(&key)).await.err(),
        Some(Refusal::CredentialInvalid)
    );
    let start = Instant::now();
    while copied().await.is_some() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "B forgets the copy A wrote"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // No core answers: A reads the copy, which no longer holds the key.
    d.core.stop();
    d.auth.verifiers().set_ttl(Duration::ZERO).unwrap();
    assert_eq!(
        authenticate(&d.replica.shared, &bearer(&key)).await.err(),
        Some(Refusal::DecisionUnavailable),
        "the revoked key is not authenticated from the copy on A"
    );
}
