// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # authn
//!
//! aledgent's credential schemes at the boundary (`DEC-19`, `F-028`). Ingress links it; the core
//! does not: the core's guest carries no P-256, JSON or CBOR, and checks only the shape of a
//! verifier and `key` proofs (`ledger_core::scheme`).
//!
//! - [`webauthn`]: passkeys. Assertion verification (`rpIdHash`, flags, client data, signature,
//!   sign count), registration from an attestation, and Kreivo `Pass`'s stored key.
//! - [`token`]: what ingress issues. Stateless challenges and session tokens under one MAC key,
//!   and API keys, issued once and kept as their hash.
//!
//! Nothing here formats a secret: the token types implement neither `Debug` nor `Display`
//! (`REQ-AU-8`).

pub mod token;
pub mod webauthn;
