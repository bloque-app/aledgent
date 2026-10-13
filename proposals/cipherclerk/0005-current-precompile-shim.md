<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# CipherClerk 0005 — the current precompile shim

**Draft pull request description, 2026-10-03.** Not yet opened upstream. Patch `0005-current-precompile-shim.patch`, against `e9a7b08` with 0001 applied;
`tools/cipherclerk-patched.sh` applies it for aledgent's guest. The series: `0003-0006-guest-performance.md`.

**Why.** In a VOS PVM guest, a reservation in aledgent's core costs about 295 M gas of actor work. A
per-PC profile of one 16-reservation invocation (`notes/MEASUREMENTS-F031.md` §8.3) puts about 46 % of
it in CipherClerk's `crypto::hash::blake2b_256`. That is **8 470 one-block hashes per reservation**,
at about 16 k gas each: the `blake2` crate's compression plus the `digest` wrapper's per-hash buffer
copies (`memcpy` and `memset` are 26 % of the actor's gas). Another 17 % is curve arithmetic.
`merkle::build_empty_chain` alone is 21 % of the hashes: 14 rebuilds a reservation, 128 hashes each.

| Patch | Subject | Actor gas per reservation (core, VOS-native) |
|---|---|---:|
| | before | 293.5 M |
| 0004 | `perf(merkle): the empty-subtree chain as a static table` | 256.5 M (−12.6 %) |
| 0003 | `perf(crypto): hash with VOS's BLAKE2b behind vos-blake2b` | 227.5 M (−22.5 %) |
| 0003 + 0004 | | 204.4 M (−30.4 %) |
| 0005 | `build(precompile): the current VOS precompile shim for pvm-precompile`, with VOS's native curve precompiles (VOS 0010) | 154.0 M |
| | and the guest at `opt-level = 3` (aledgent) | 104.8 M (−64 %) |

## The change

`pvm-precompile` pinned `zkpvm-precompiles` at VOS `9f5337f`, whose shim emits a bare `ecall`. Today's
PVM compiler translates that into a trap, so a guest built with the feature panicked at its first curve
operation (VOS 0002 found this). The feature now uses `vos-pvm-precompiles`, the current shim with the
same API, which marks its ECALLs. Served natively by the runtime (VOS 0010), a Ristretto scalar mult
costs 250 k gas instead of about 3.9 M. `vos-protocol` and the shim are pinned at the same VOS
revision as the consumer's (aledgent pins `perf/agent-stack`).
