// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! F-025 curve-precompile cost probe. Input: op (u8), count (u32 LE), then
//! scalar (32), point (32), point (32), wide (64). Runs `count` operations
//! exactly as `vos::agent::execution::curve_precompile` computes them and
//! halts with a 32-byte fold of the outputs. Op 0 is the empty loop.
#[cfg(target_arch = "riscv64")]
mod guest {
    use core::arch::global_asm;
    use curve25519_dalek::ristretto::CompressedRistretto;
    use curve25519_dalek::scalar::Scalar;

    const PVM_HALT_ADDR: u64 = 0xffff_0000;

    global_asm!(
        ".global _start",
        ".type _start, @function",
        "_start:",
        "j curve_bench"
    );

    fn scalar(bytes: [u8; 32]) -> Option<Scalar> {
        Scalar::from_canonical_bytes(bytes).into_option()
    }

    fn point(bytes: [u8; 32]) -> Option<curve25519_dalek::ristretto::RistrettoPoint> {
        CompressedRistretto::from_slice(&bytes)
            .ok()
            .and_then(|compressed| compressed.decompress())
    }

    fn op(id: u8, a: [u8; 32], b: [u8; 32], c: [u8; 32], wide: &[u8; 64]) -> [u8; 32] {
        match id {
            110 => match (scalar(a), point(b)) {
                (Some(k), Some(p)) => (k * p).compress().to_bytes(),
                _ => [0; 32],
            },
            111 => match (point(b), point(c)) {
                (Some(p), Some(q)) => (p + q).compress().to_bytes(),
                _ => [0; 32],
            },
            112 => Scalar::from_bytes_mod_order_wide(wide).to_bytes(),
            113 => match (scalar(a), scalar(a)) {
                (Some(x), Some(y)) => (x * y).to_bytes(),
                _ => [0; 32],
            },
            114 => match (scalar(a), scalar(a)) {
                (Some(x), Some(y)) => (x + y).to_bytes(),
                _ => [0; 32],
            },
            _ => a,
        }
    }

    #[unsafe(no_mangle)]
    extern "C" fn curve_bench(arguments: *const u8, len: usize) -> ! {
        // SAFETY: the standard loader maps the argument window read-only.
        let input = unsafe { core::slice::from_raw_parts(arguments, len) };
        let id = input[0];
        let count = u32::from_le_bytes(input[1..5].try_into().unwrap());
        let a: [u8; 32] = input[5..37].try_into().unwrap();
        let b: [u8; 32] = input[37..69].try_into().unwrap();
        let c: [u8; 32] = input[69..101].try_into().unwrap();
        let wide: [u8; 64] = input[101..165].try_into().unwrap();
        let mut fold = [0u8; 32];
        for _ in 0..count {
            let out = op(id, core::hint::black_box(a), b, c, &wide);
            for (f, o) in fold.iter_mut().zip(out) {
                *f ^= o;
            }
        }
        let fold = core::hint::black_box(fold);
        // SAFETY: terminal standard-PVM halt; a0/a1 name the output bytes.
        unsafe {
            core::arch::asm!(
                "jr t0",
                in("a0") fold.as_ptr() as u64,
                in("a1") 32u64,
                in("t0") PVM_HALT_ADDR,
                options(noreturn),
            )
        }
    }

    #[panic_handler]
    fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
        unsafe { core::arch::asm!("ebreak", options(noreturn, nostack)) }
    }
}
