// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The spike's actor program. Without `heap-probe`, `vos_sdk::actor_program!` as any vos-core
//! actor has it. With it, the same program with the SDK's heap wrapped so that every block it
//! hands out raises `probe`'s high-water mark: the arena bytes the invocation reached, against
//! `INVOCATION_HEAP` (512 KiB), which is what the SDK's heap itself refuses past.

#![no_std]
#![no_main]

#[cfg(not(feature = "heap-probe"))]
vos_sdk::actor_program!(aledgent_core_vos::aledgent_core);

#[cfg(feature = "heap-probe")]
mod probed {
    use core::alloc::{GlobalAlloc, Layout};
    use vos_sdk::heap::{Arena, Heap};
    use vos_sdk::runtime::PROGRAM_HEAP;

    #[cfg(target_arch = "riscv64")]
    #[unsafe(link_section = ".vos_heap")]
    static VOS_ARENA: Arena<PROGRAM_HEAP> = Arena::new();

    /// The SDK's heap, its high-water mark recorded.
    struct Probed(Heap<PROGRAM_HEAP, true>);

    // SAFETY: a program runs on one thread.
    unsafe impl Sync for Probed {}

    /// The bytes the SDK's heap bumps for `layout`: its power-of-two class up to 128 KiB, whole
    /// pages past it (`vos_sdk::heap`).
    fn bumped(layout: &Layout) -> usize {
        let size = layout.size().max(layout.align()).max(16);
        if size <= 128 << 10 { size.next_power_of_two() } else { layout.size().next_multiple_of(4096) }
    }

    unsafe impl GlobalAlloc for Probed {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: GlobalAlloc's contract, passed on.
            let ptr = unsafe { self.0.alloc(layout) };
            if !ptr.is_null() {
                let base = VOS_ARENA_BASE();
                aledgent_core_vos::probe::reach(ptr as usize - base + bumped(&layout));
            }
            ptr
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: as above.
            unsafe { self.0.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: as above.
            let new = unsafe { self.0.realloc(ptr, layout, new_size) };
            if !new.is_null() {
                // SAFETY: the new layout keeps the alignment.
                let grown = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
                aledgent_core_vos::probe::reach(new as usize - VOS_ARENA_BASE() + bumped(&grown));
            }
            new
        }
    }

    #[allow(non_snake_case)]
    fn VOS_ARENA_BASE() -> usize {
        &VOS_ARENA as *const _ as usize
    }

    #[cfg(target_arch = "riscv64")]
    #[global_allocator]
    static VOS_HEAP: Probed = Probed(Heap::new(&VOS_ARENA));

    #[cfg(target_arch = "riscv64")]
    #[panic_handler]
    fn vos_panic(_: &::core::panic::PanicInfo<'_>) -> ! {
        // SAFETY: `unimp` lowers to a PVM trap, which fails the step.
        unsafe { ::core::arch::asm!("unimp", options(noreturn)) }
    }

    #[cfg(target_arch = "riscv64")]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, len: usize) -> i32 {
        // SAFETY: memcmp's contract.
        unsafe { vos_sdk::mem::compare(a, b, len) }
    }

    #[cfg(target_arch = "riscv64")]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, len: usize) -> i32 {
        // SAFETY: bcmp's contract.
        unsafe { vos_sdk::mem::compare(a, b, len) }
    }

    #[used]
    #[unsafe(link_section = ".vos_schema")]
    static VOS_SCHEMA: [u8; aledgent_core_vos::aledgent_core::SCHEMA.len()] =
        aledgent_core_vos::aledgent_core::SCHEMA;

    #[used]
    #[unsafe(no_mangle)]
    #[unsafe(link_section = ".vos_attest")]
    static VOS_ATTEST: [vos_sdk::runtime::Attestable; aledgent_core_vos::aledgent_core::ATTESTABLES.len()] =
        aledgent_core_vos::aledgent_core::ATTESTABLES;

    #[cfg(target_arch = "riscv64")]
    ::core::arch::global_asm!(
        ".global _start",
        ".type _start, @function",
        "_start:",
        "mv s0, ra",
        "jal ra, vos_entry",
        "mv ra, s0",
        "ret",
    );

    #[cfg(target_arch = "riscv64")]
    #[unsafe(no_mangle)]
    extern "C" fn vos_entry(method: u64) {
        aledgent_core_vos::aledgent_core::dispatch(method as u32);
    }
}
