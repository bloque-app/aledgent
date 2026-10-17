/* Copyright (c) 2026 Pagos Cube LLC */
/* SPDX-License-Identifier: AGPL-3.0-or-later */
/* The portable AgentActor layout `vosx actor new` writes at the pinned VOS
 * (vosx/src/commands/new_project.rs, PVM_LINKER_SCRIPT), with one change: the
 * read-only data starts at 0x40000 instead of 0x10000.
 *
 * The linker takes the stack size from the lowest read-only address
 * (pvm/compiler/src/linker.rs, link_elf_spi): 0x10000 gives a 64 KiB stack,
 * which the library's deepest command overflows. 0x40000 gives 256 KiB, the
 * size the standard AgentRuntime's own layout uses.
 *
 * Read-only and read-write data occupy distinct GP zones. The read-write
 * base follows the complete read-only image instead of assuming a fixed
 * maximum size.
 *
 * The code generation's record (`.aledgent.generation`, T-037-01, amendment
 * 0033 D13) is kept (KEEP) and placed first in `.rodata`: VOS's PVM linker
 * (pvm/compiler/src/linker.rs, link_elf_spi) carries only `.rodata*`,
 * `.srodata` and `.data.rel.ro*` into the program's read-only data, so a
 * section of its own would be dropped and the guest would read nothing there.
 * The record stays the one the exported symbol ALEDGENT_CODE_GENERATION names.
 */

ENTRY(_start)

SECTIONS
{
    .rodata 0x40000 : { KEEP(*(.aledgent.generation)) *(.rodata .rodata.* .srodata .srodata.*) }
    .data (0x50000 + ((SIZEOF(.rodata) + 0xffff) & ~0xffff)) :
        { *(.data .data.* .sdata .sdata.*) }
    .bss ALIGN(0x1000) : { *(.bss .bss.* .sbss .sbss.* COMMON) }
    .text 0x900000 : { *(.text .text.* .init .init.*) }

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .comment .riscv.attributes) }
}
