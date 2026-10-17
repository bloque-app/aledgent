// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A program whose fence would read another constant than the section's (`T-037-01`; amendment
//! 0033, D13, D18): beside the record `ledger_core::code_generation!` defines, a second record
//! that a function reads. The reader refuses the program: the tag occurs twice in what it loads.
//! `code_generation.rs` is the same program without the second constant.

ledger_core::code_generation!(7);

/// A second constant, as a fence written against its own copy would carry.
static ANOTHER: [u8; 16] = ledger_core::code::generation_record(8);

fn another_fence() -> u64 {
    ledger_core::code::record_generation(core::hint::black_box(&ANOTHER))
}

#[test]
fn t037_01_a_second_constant_is_two_records() {
    assert_eq!(code_generation(), 7);
    assert_eq!(another_fence(), 8);
}

/// T-037-01: "a package whose fence reads another constant is detected".
#[cfg(target_os = "linux")]
#[test]
fn t037_01_a_program_with_a_second_constant_is_refused() {
    let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    assert_eq!(
        ledger_core::code::generation_of(&exe),
        Err(ledger_core::code::GenerationError::Duplicated(2))
    );
}
