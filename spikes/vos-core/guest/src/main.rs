// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The spike's actor program. Without `heap-probe`, `vos_sdk::actor_program!` as any vos-core
//! actor has it. With it, the same program with the SDK's heap wrapped so that every block it
//! hands out raises `probe`'s high-water mark: the arena bytes the invocation reached, against
//! `INVOCATION_HEAP` (512 KiB), which is what the SDK's heap itself refuses past.

#![no_std]
#![no_main]

#[cfg(not(any(feature = "heap-probe", feature = "be-memcmp")))]
vos_sdk::actor_program!(aledgent_core_vos::aledgent_core);

#[cfg(any(feature = "heap-probe", feature = "be-memcmp"))]
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

    #[cfg(all(target_arch = "riscv64", feature = "heap-probe"))]
    #[global_allocator]
    static VOS_HEAP: Probed = Probed(Heap::new(&VOS_ARENA));

    #[cfg(all(target_arch = "riscv64", not(feature = "heap-probe")))]
    #[global_allocator]
    static VOS_HEAP: Heap<PROGRAM_HEAP, true> = Heap::new(&VOS_ARENA);

    /// `be-memcmp`: the SDK's word compare (`vos_sdk::mem::compare`), with the first differing
    /// word's order taken from its big-endian value instead of from its lowest differing byte
    /// (`trailing_zeros` and a shift), and a result of -1, 0 or 1. At vos-core 1650c474 the SDK's
    /// compare orders some keys wrongly in the guest (W2: genesis's service root).
    #[cfg(feature = "be-memcmp")]
    unsafe fn compare(a: *const u8, b: *const u8, len: usize) -> i32 {
        const WORD: usize = 8;
        let mut i = 0;
        if (a as usize ^ b as usize).is_multiple_of(WORD) {
            while i < len && !(a as usize + i).is_multiple_of(WORD) {
                // SAFETY: `i < len`.
                let (x, y) = unsafe { (*a.add(i), *b.add(i)) };
                if x != y {
                    return if x < y { -1 } else { 1 };
                }
                i += 1;
            }
            while i + WORD <= len {
                // SAFETY: both sides are word-aligned here and `i + WORD <= len`.
                let (x, y) = unsafe { (*a.add(i).cast::<u64>(), *b.add(i).cast::<u64>()) };
                if x != y {
                    let (x, y) = (u64::from_be(x), u64::from_be(y));
                    return if x < y { -1 } else { 1 };
                }
                i += WORD;
            }
        }
        while i < len {
            // SAFETY: memcmp's contract.
            let (x, y) = unsafe { (*a.add(i), *b.add(i)) };
            if x != y {
                return if x < y { -1 } else { 1 };
            }
            i += 1;
        }
        0
    }
    #[cfg(not(feature = "be-memcmp"))]
    use vos_sdk::mem::compare;

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
        unsafe { compare(a, b, len) }
    }

    #[cfg(target_arch = "riscv64")]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn bcmp(a: *const u8, b: *const u8, len: usize) -> i32 {
        // SAFETY: bcmp's contract.
        unsafe { compare(a, b, len) }
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
