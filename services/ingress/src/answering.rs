// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A principal's answering setting (`REQ-CS-9`, amendment 0013; `T-009-18`).
//!
//! By default a `transfer` or a `reserve` is acknowledged at `Received` and decided later
//! (`REQ-CS-8`). A principal may instead have its transfers and reservations answered with their
//! decision: the reply waits for the core's durable decision, for at most the setting's wait, and
//! past it is `ERR-DecisionUnavailable`, with the command kept and decided like any other
//! (`REQ-CS-7`). Nothing else changes: the row, the forwarder, the operation and its
//! notifications are the same whichever way the reply was given.
//!
//! - **Covers** `transfer` and `reserve` only. A withdrawal stays acknowledged.
//! - **Per row.** Whether a row is answered with its decision is fixed when it is logged
//!   (`synchronous`), so a command keeps the way it was taken in across a change of the setting
//!   and on every retry.
//! - **The wait** is at most the deployment's decision wait (`REQ-CS-7`). Without one stated, it
//!   is `NFR-16`'s 1 000 ms, or the decision wait if that is shorter.
//! - **Who sets it.** The principal, for itself. Another principal of the same integrating party
//!   may set it once `T-009-19` builds it on the parties now on `main`.
//! - **Where it lives.** In ingress's store, with who set it and when (`REQ-AU-7`): it shapes
//!   ingress's replies, not the book.

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ledger_core::Refusal;

use crate::auth::Authenticated;
use crate::boundary::Core;
use crate::command::Command;
use crate::service::Shared;
use crate::store::{Answering, Store};
use crate::wire::{AnswerBody, AnsweringBody, AnsweringReply};

/// `NFR-16`: a command answered with its decision is answered within 1 s at p99.
pub const NFR_16: Duration = Duration::from_millis(1_000);

/// Whether the answering setting applies to `command`: a transfer or a reservation.
pub fn covers(command: &Command) -> bool {
    matches!(command, Command::Transfer(_) | Command::Reserve { .. })
}

/// The wait when a setting states none: `NFR-16`'s, or the decision wait if shorter.
pub fn default_wait<S: Store, C: Core>(shared: &Shared<S, C>) -> Duration {
    NFR_16.min(shared.config.decision_wait)
}

/// How long a decided reply to a transfer or reservation waits, under `setting`. Never above the
/// decision wait, whatever the store holds.
pub fn wait<S: Store, C: Core>(shared: &Shared<S, C>, setting: Option<&Answering>) -> Duration {
    setting
        .map_or_else(
            || default_wait(shared),
            |a| Duration::from_millis(a.wait_ms),
        )
        .min(shared.config.decision_wait)
}

/// The principal's setting. A store that does not answer holds the request open, as an append
/// does: replying acknowledged to a principal that asked for decisions would break its contract.
pub async fn of<S: Store, C: Core>(shared: &Shared<S, C>, principal: &str) -> Option<Answering> {
    loop {
        match shared.store.answering(principal).await {
            Ok(a) => return a,
            Err(e) => {
                tracing::warn!(error = %e, "the store did not answer an answering lookup; retrying");
                tokio::time::sleep(shared.config.poll).await;
            }
        }
    }
}

/// The wait for a reply to `command` from `principal`: its answering wait for a transfer or a
/// reservation, the decision wait for anything else.
pub async fn wait_for<S: Store, C: Core>(
    shared: &Shared<S, C>,
    command: &Command,
    principal: &str,
) -> Duration {
    if covers(command) {
        wait(shared, of(shared, principal).await.as_ref())
    } else {
        shared.config.decision_wait
    }
}

fn reply_of<S: Store, C: Core>(
    shared: &Shared<S, C>,
    principal: &str,
    a: Option<&Answering>,
) -> AnsweringReply {
    AnsweringReply {
        principal: principal.to_owned(),
        answer: if a.is_some_and(|a| a.decided) {
            AnswerBody::Decided
        } else {
            AnswerBody::Acknowledged
        },
        wait_ms: u64::try_from(wait(shared, a).as_millis()).unwrap_or(u64::MAX),
        set_by: a.map(|a| a.set_by.clone()),
        set_at: a.map(|a| a.set_at),
    }
}

/// `GET /v1/principals/self/answering`: the caller's setting, or the default.
pub async fn read<S: Store, C: Core>(shared: &Shared<S, C>, who: &Authenticated) -> Response {
    match shared.store.answering(&who.principal).await {
        Ok(a) => (
            StatusCode::OK,
            axum::Json(reply_of(shared, &who.principal, a.as_ref())),
        )
            .into_response(),
        Err(_) => crate::surface::refusal(Refusal::DecisionUnavailable),
    }
}

/// `POST /v1/principals/self/answering`: set the caller's setting, answered with its result
/// (`REQ-CS-8`). A wait of zero or above the decision wait is refused with `ERR-NotAuthorised`,
/// and so is a body that does not parse (fail closed). Setting the same value again changes only
/// when it was set.
pub async fn set<S: Store, C: Core>(
    shared: &Shared<S, C>,
    who: &Authenticated,
    bytes: &[u8],
) -> Response {
    let Ok(body) = serde_json::from_slice::<AnsweringBody>(bytes) else {
        return crate::surface::refusal(Refusal::MalformedCommand);
    };
    let ceiling = u64::try_from(shared.config.decision_wait.as_millis()).unwrap_or(u64::MAX);
    let wait_ms = match body.wait_ms {
        None => u64::try_from(default_wait(shared).as_millis()).unwrap_or(u64::MAX),
        Some(w) if w == 0 || w > ceiling => {
            return crate::surface::refusal(Refusal::NotAuthorised);
        }
        Some(w) => w,
    };
    let setting = Answering {
        principal: who.principal.clone(),
        decided: body.answer == AnswerBody::Decided,
        wait_ms,
        set_by: who.principal.clone(),
        set_at: shared.clock.now(),
    };
    if shared.store.put_answering(setting.clone()).await.is_err() {
        return crate::surface::refusal(Refusal::DecisionUnavailable);
    }
    tracing::info!(
        principal = %who.principal,
        decided = setting.decided,
        wait_ms = setting.wait_ms,
        "answering setting changed (REQ-CS-9)"
    );
    (
        StatusCode::OK,
        axum::Json(reply_of(shared, &who.principal, Some(&setting))),
    )
        .into_response()
}
