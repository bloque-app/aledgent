// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The conformance suite's runner (`F-015` §2): every registered case on
//! every target, judged by the current gate profile. See
//! `conformance::runner`.

fn main() {
    conformance::runner::main();
}
