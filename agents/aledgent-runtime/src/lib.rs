// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! aledgent's AgentRuntime (`F-033`, `DEC-23`).
//!
//! VOS's standard runtime, built with composite invocations: an ordinary Linear invocation may
//! carry a composite (`vos::agent::composite`), whose steps are Linear methods of the Agent's own
//! actors, run after the primary in its one invocation and its one Linear transition. Atomic
//! composites commit every step or none; isolated ones commit each step alone, defer what the
//! gas cannot cover, keep each order key's steps in order, and apply each numbered message once,
//! at its stream's next sequence. Every other work is the standard runtime's, byte for byte.
//!
//! What this program fixes, and the standard one does not:
//!
//! - **Its rules** ([`RULES`]): at most 128 steps a composite and 16 an atomic one, and every step
//!   ordered or numbered, so that no command of a payer's and no message of a sender's runs
//!   outside its order.
//! - **Its public I/O**, with `direct-public-io`: the by-reference commitment only for work a
//!   proof will consume; Direct work, which no executor reads it for, gets a constant.
//!
//! What it cannot change: the host-call allowlist of its contract (calls 9 to 14, and 7, 100,
//! 110 to 114 for the profiles), that actor programs arrive as availability or by
//! `historical_lookup`, the actor gas cap the host and the Agent's limits set, and Raft. The
//! entry is VOS's `services/agent-runtime/src/lib.rs`, with the rules set first.
//!
//! **JAM-compatible by default.** Built without features, this program issues only the Gray
//! Paper's Refine calls 9 to 14 (`aledgent-core-host runtime-pvm` lists them, and refuses any
//! other with `ALEDGENT_RUNTIME_GP_ONLY=1`). Composites add no host call: their steps are inner
//! machines the runtime already runs through `machine`/`invoke`, their stream heads live in the
//! control image, and their ordering is decided in the guest (proposals/vos/0012).

/// The composite rules this runtime admits.
#[cfg(target_arch = "riscv64")]
pub const RULES: vos::agent::wire::composite_engine::CompositeRules =
    vos::agent::wire::composite_engine::CompositeRules {
        max_steps: 128,
        max_atomic_steps: 16,
        require_order_or_stream: true,
    };

#[cfg(target_arch = "riscv64")]
mod guest {
    extern crate alloc;

    use core::arch::global_asm;

    use vos::agent_sdk::wire::CanonicalWire as _;

    const PVM_HALT_ADDR: u64 = 0xffff_0000;

    global_asm!(
        ".global _start",
        ".type _start, @function",
        "_start:",
        "j vos_agent_manage",
    );

    #[unsafe(no_mangle)]
    extern "C" fn vos_agent_manage(arguments: *const u8, arguments_len: usize) -> ! {
        vos::agent::wire::composite_engine::set_rules(super::RULES);
        // SAFETY: the standard PVM loader maps the complete argument window read-only and
        // supplies its base/length in a0/a1.
        let input = unsafe { core::slice::from_raw_parts(arguments, arguments_len) };
        #[cfg(not(feature = "direct-public-io"))]
        let (output, public_io) = {
            let (transition, work) = vos::agent::wire::apply_standard_runtime_input_committed(input)
                .unwrap_or_else(|_| fail_closed());
            let output = transition.encode().unwrap_or_else(|_| fail_closed());
            let public_io =
                vos::agent_sdk::runtime_transition_public_io_by_reference(work, &output);
            (output, public_io)
        };
        #[cfg(feature = "direct-public-io")]
        let (output, public_io) = {
            let (transition, work) =
                vos::agent::wire::apply_standard_runtime_input_committed_unless_direct(input)
                    .unwrap_or_else(|_| fail_closed());
            let output = transition.encode().unwrap_or_else(|_| fail_closed());
            let public_io = match work {
                Some(work) => {
                    vos::agent_sdk::runtime_transition_public_io_by_reference(work, &output)
                }
                // Never zero (a proof statement refuses a zero public I/O), and never a value
                // a committed one could take: a domain of its own.
                None => vos::agent_sdk::Hash::digest(
                    b"aledgent/runtime/direct-public-io/v1",
                    &[vos::agent_sdk::RUNTIME_ABI_ID.as_bytes()],
                ),
            };
            (output, public_io)
        };
        halt_with_output_bound(&output, public_io.as_bytes())
    }

    fn halt_with_output_bound(output: &[u8], public_io: &[u8; 32]) -> ! {
        let mut words = [0u64; 4];
        for (word, bytes) in words.iter_mut().zip(public_io.chunks_exact(8)) {
            *word = u64::from_le_bytes(bytes.try_into().expect("exact hash word"));
        }
        // SAFETY: terminal standard-PVM halt. The output allocation remains live, a0/a1
        // designate its exact bytes, and a2..a5 carry the public commitment.
        unsafe {
            core::arch::asm!(
                "jr t0",
                in("a0") output.as_ptr() as u64,
                in("a1") output.len() as u64,
                in("a2") words[0],
                in("a3") words[1],
                in("a4") words[2],
                in("a5") words[3],
                in("t0") PVM_HALT_ADDR,
                options(noreturn),
            )
        }
    }

    fn fail_closed() -> ! {
        // An invalid management call is a deterministic guest trap, never an empty or
        // partially decoded success.
        unsafe { core::arch::asm!("ebreak", options(noreturn, nostack)) }
    }
}
