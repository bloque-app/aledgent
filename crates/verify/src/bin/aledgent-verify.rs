// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `aledgent-verify`: check a disclosure without aledgent (`F-017` T-017-06, `REQ-DS-3`,
//! `AC-I2.1`, `AC-I3.1`). See [`verify::cli`] for the arguments and the output.

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (code, text) = verify::cli::run(&args);
    let _ = if code == 2 {
        std::io::stderr().write_all(text.as_bytes())
    } else {
        std::io::stdout().write_all(text.as_bytes())
    };
    ExitCode::from(code)
}
