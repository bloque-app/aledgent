// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The readiness listener (`F-038` §10, `T-038-13`): what a load balancer's health check and an
//! orchestrator's probes read, on a port of its own, apart from the client surface.
//!
//! - `GET /ready`: 200 once this replica serves (it starts after the store is connected and its
//!   migrations are applied or verified) and the store answers now; 503 while it drains
//!   ([`crate::service::Replica::drain`]), when the store does not answer, or when the feed key is
//!   held by an API provider (Vault Transit, `F-042`) that does not answer. A release goes out
//!   blue/green on it: the new set is ready before the old one drains (`DEC-27` (a)).
//! - `GET /live`: 200 while the process runs.
//!
//! Neither carries a ledger fact, a credential or a count: one word each. That is why they sit
//! outside the client surface's deny-by-default (`NFR-7`), on a port only the health check reaches;
//! `/metrics` stays on the surface, authenticated.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;

use crate::boundary::Core;
use crate::service::Shared;
use crate::store::Store;

/// Whether `shared`'s replica is ready now.
pub async fn ready<S: Store, C: Core>(shared: &Shared<S, C>) -> bool {
    !shared.draining()
        && shared
            .config
            .feed_signer
            .as_ref()
            .is_none_or(|s| s.0.ready())
        && shared.store.oldest_undecided().await.is_ok()
}

async fn handle_ready<S: Store, C: Core>(
    State(shared): State<Arc<Shared<S, C>>>,
) -> (StatusCode, &'static str) {
    if ready(&shared).await {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

async fn handle_live() -> (StatusCode, &'static str) {
    (StatusCode::OK, "live")
}

/// The readiness routes over `shared`.
pub fn router<S: Store, C: Core>(shared: Arc<Shared<S, C>>) -> Router {
    Router::new()
        .route("/ready", get(handle_ready::<S, C>))
        .route("/live", get(handle_live))
        .with_state(shared)
}
