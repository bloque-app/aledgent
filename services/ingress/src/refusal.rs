// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Refusals that only ingress sends (amendment 0037, R2; `T-040-10`).
//!
//! `ledger_core::Refusal` is the closed list compiled into the core program, and a conformance
//! check holds it equal to `SPEC.md` §10 in both directions (`REQ-ER-1`). `ERR-CommandApprovalRequired`
//! is in §10 from amendment 0037, and the core's list gets it with the next core (`T-041-06`), so
//! until then ingress answers it from this type. The surface publishes it beside the core's list
//! (the OpenAPI `Refusal` schema, [`crate::openapi::refusal_schema`]), in the same shape
//! (`{"error", "description"}`, `REQ-CS-4`), and the conformance suite's one named allowance
//! (`conformance::closure::SPEC_AHEAD_OF_PROGRAM`) names it and expires with that core.
//!
//! When the core's `Refusal` has the line, this type goes: its one variant moves to the core in
//! the same change that removes the allowance, and the surface maps it like any other refusal.
//!
//! Nothing here reaches the core: Feature A never sends it this refusal (amendment 0037, R2).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::ser::{Serialize, SerializeStruct, Serializer};

/// A refusal ingress sends and the core's list does not have yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IngressRefusal {
    /// `ERR-CommandApprovalRequired`: a command that needs a credential's approval was submitted
    /// without one (`REQ-AU-23`).
    CommandApprovalRequired,
}

impl IngressRefusal {
    /// Every refusal of this type, published beside `ledger_core::Refusal::ALL`.
    pub const ALL: [IngressRefusal; 1] = [IngressRefusal::CommandApprovalRequired];

    /// The identifier, as `SPEC.md` §10 spells it.
    pub const fn code(self) -> &'static str {
        match self {
            IngressRefusal::CommandApprovalRequired => "ERR-CommandApprovalRequired",
        }
    }

    /// The published condition, verbatim from `SPEC.md` §10 (`REQ-ER-1`).
    pub const fn description(self) -> &'static str {
        match self {
            IngressRefusal::CommandApprovalRequired => {
                "A command that needs a credential's approval of that one command (`REQ-AU-23`) \
                 was submitted without one. A presented approval that does not verify is \
                 `ERR-CredentialInvalid`"
            }
        }
    }

    /// The HTTP status. The owner's ruling (amendment 0037, R2): the same as `ERR-NotAuthorised`.
    pub const fn status(self) -> StatusCode {
        match self {
            IngressRefusal::CommandApprovalRequired => StatusCode::FORBIDDEN,
        }
    }

    /// The refusal as the surface sends it: its status and a body of the shape every refusal has.
    pub fn response(self) -> Response {
        (self.status(), axum::Json(self)).into_response()
    }
}

impl Serialize for IngressRefusal {
    /// `{"error": <code>, "description": <condition>}`, the shape of `ledger_core::Refusal`.
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Refusal", 2)?;
        st.serialize_field("error", self.code())?;
        st.serialize_field("description", self.description())?;
        st.end()
    }
}
