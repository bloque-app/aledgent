/* Copyright (c) 2026 Pagos Cube LLC */
/* SPDX-License-Identifier: AGPL-3.0-or-later */
/* Canonical bundled agent-runtime layout.
 *
 * The low read-only address reserves a 256 KiB stack. Runtime-owned state is
 * passed in the argument window and returned from the guest allocator; it is
 * never interpreted by the host.
 */

ENTRY(_start)

SECTIONS
{
    .rodata 0x40000 : { *(.rodata .rodata.* .srodata .srodata.*) }
    /* GP lays a <=64 KiB ro payload linked here over [0x10000, 0x50000),
     * making 0x60000 the first canonical read-write address. */
    .data 0x60000 : { *(.data .data.* .sdata .sdata.*) }
    .bss ALIGN(0x1000) : { *(.bss .bss.* .sbss .sbss.* COMMON) }
    /* Keep translated code outside the 32 MiB infrastructure heap's ELF
     * range. Standard-program code has its own instruction address space. */
    .text 0x3000000 : { *(.text .text.* .init .init.*) }

    /DISCARD/ : { *(.eh_frame .eh_frame_hdr .comment .riscv.attributes) }
}
