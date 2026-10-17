// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The compiled code generation, in a real program (`T-037-01`; amendment 0033, D13, D18).
//!
//! This test program defines its record with `ledger_core::code_generation!`, as a guest does, and
//! shows that the function the fence will call (`code_generation()`) and the record the reader
//! reads (`ledger_core::code::generation_of`) are one symbol: the function reads the exported
//! static `ALEDGENT_CODE_GENERATION` itself, and, in this program's own ELF, that symbol is the
//! section `.aledgent.generation`'s one record. `code_generation_second_constant.rs` is the same
//! program with a second constant beside it, which the reader refuses.
//!
//! The ELF half runs where the test program is an ELF (Linux, CI); elsewhere the in-process half
//! runs alone.

ledger_core::code_generation!(42);

#[test]
fn d18_the_fence_reads_the_exported_record() {
    assert_eq!(code_generation(), 42);
    assert_eq!(
        ALEDGENT_CODE_GENERATION,
        ledger_core::code::generation_record(42)
    );
}

/// D18: the section, the symbol and the fence's read are one record.
#[cfg(target_os = "linux")]
#[test]
fn d18_the_section_and_the_symbol_are_one_record_in_this_programs_elf() {
    let exe = std::fs::read(std::env::current_exe().unwrap()).unwrap();
    assert!(exe.starts_with(b"\x7fELF"));
    // The reader: the section's one record, the symbol naming it, the tag once in the data this
    // program loads.
    assert_eq!(
        ledger_core::code::generation_of(&exe),
        Ok(code_generation())
    );
}
