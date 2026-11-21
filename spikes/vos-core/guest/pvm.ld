/* Copyright (c) 2026 Pagos Cube LLC */
/* SPDX-License-Identifier: AGPL-3.0-or-later */
/* vos-core's guest layout (pvm/guest/pvm.ld at 1650c474), with one change: the code generation's
 * record (`.aledgent.generation`, T-037-01) is kept in `.rodata`, which is all the linker carries
 * into the read-only data.
 *
 * The production core (agents/core/pvm.ld) starts its read-only data at 0x40000 for a 256 KiB
 * stack: the linker takes the stack's size from the lowest read-only address. vos-core does not
 * allow that: the agent reads every actor's schema at 0x10000 (agent/default/src/refine/admin.rs,
 * SCHEMA_AT), so `.vos_schema` must start there, and the stack is 64 KiB for every actor. */

ENTRY(_start)

SECTIONS
{
    . = 0x10000;
    .vos_schema : { KEEP(*(.vos_schema)) }
    .rodata : { KEEP(*(.vos_attest)) KEEP(*(.aledgent.generation)) *(.rodata .rodata.* .srodata .srodata.*) }
    . = 0x20000 + ((. - 0x10000 + 0xffff) & ~0xffff);
    .data : { *(.data .data.* .sdata .sdata.* .got .got.* .data.rel.ro .data.rel.ro.*) }
    .bss ALIGN(0x1000) : { *(.bss .bss.* .sbss .sbss.* COMMON) }
    .vos_heap ALIGN(0x1000) (NOLOAD) : { KEEP(*(.vos_heap)) }
    .text ALIGN(0x10000) : { *(.text .text.* .init .init.*) }

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .comment .riscv.attributes) }
}
