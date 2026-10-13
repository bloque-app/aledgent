// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Shared tools for cases (`F-015` §8, T-015-07).
//!
//! - [`strategies`]: `proptest` strategies for books and move schedules,
//!   and their translation to `F-001` units.
//! - [`ShadowBook`]: a plaintext model of every entry per (account, asset),
//!   for `INV-5` and `INV-6`.
//! - [`book`]: a random book driven through the command surface (`F-014`).
//! - [`rederive()`]: balances rebuilt from the committed delta log alone
//!   (`NFR-4`, `INV-6`), and the per-asset commitment sum (`INV-5`).

pub mod book;
pub mod rederive;
pub mod shadow;
pub mod strategies;

pub use rederive::{Rederived, rederive};
pub use shadow::ShadowBook;
