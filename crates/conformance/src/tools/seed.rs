// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Seeds of the randomised harnesses, and how a failure of one is replayed.
//!
//! Every randomised harness of the workspace (the kernel's differential, the reservation
//! engine's concurrency run, the unit-atomicity run, the settlement simulator and the
//! re-derivation run) draws its inputs from a fixed seed, so that a run is exactly
//! reproducible and a pull request replays the same inputs as the last. A fixed seed
//! explores nothing beyond its own first cases, so each harness also reads a seed from its own
//! environment variable: unset, it is the harness's constant, byte for byte; set, a fresh
//! seed explores other inputs (the nightly run sets one each night).
//!
//! A failure must name the seed it ran under, or it cannot be replayed: [`Rotation::fail`] panics
//! with the seed, the case count and the command that repeats the run.

use std::fmt::Display;

/// The seed variable of one harness, read once.
#[derive(Clone, Copy, Debug)]
pub struct Rotation {
    var: &'static str,
    seed: Option<u64>,
}

/// What a replay needs besides the seed: where the harness lives and how it is sized.
#[derive(Clone, Copy, Debug)]
pub struct Replay<'a> {
    /// The `cargo test` arguments that select the harness's binary, e.g. `-p ledger-core --test concurrency`.
    pub target: &'a str,
    /// The variable that sizes the run.
    pub size_var: &'a str,
    /// The size the failed run had.
    pub size: u64,
}

impl Rotation {
    /// The rotation of the harness whose seed variable is `var`. A value that is not a number
    /// (decimal, or hexadecimal with `0x`) is an error, not a silent default: a typo in the
    /// nightly workflow must not turn the rotating run into a fixed one.
    pub fn from_env(var: &'static str) -> Self {
        let seed = std::env::var(var).ok().map(|raw| {
            let t = raw.trim();
            let parsed = match t.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => t.parse(),
            };
            parsed.unwrap_or_else(|_| panic!("{var}={raw:?} is not a number (decimal or 0x hex)"))
        });
        Self { var, seed }
    }

    /// The seed set in the environment, if any.
    pub fn seed(&self) -> Option<u64> {
        self.seed
    }

    /// The base of a seed that is a number, `default` unless the variable is set.
    pub fn base(&self, default: u64) -> u64 {
        self.seed.unwrap_or(default)
    }

    /// The ChaCha key of the part `byte` names. Unset, it is `[byte; 32]`, the key the harnesses
    /// have always used. Set, the seed fills the first eight bytes and `byte` the rest, so that
    /// the parts of one run (which run in parallel) still draw different inputs.
    pub fn key(&self, byte: u8) -> [u8; 32] {
        let mut key = [byte; 32];
        if let Some(seed) = self.seed {
            key[..8].copy_from_slice(&seed.to_le_bytes());
        }
        key
    }

    /// The text that says how this run was seeded and how to run it again.
    pub fn replay(&self, replay: Replay<'_>) -> String {
        let test = std::thread::current()
            .name()
            .map_or_else(String::new, |n| format!(" -- {n} --exact"));
        let (seeded, env) = match self.seed {
            Some(s) => (
                format!("seed {s} ({}={s})", self.var),
                format!("{}={s} ", self.var),
            ),
            None => (
                format!("the default seed ({} unset)", self.var),
                String::new(),
            ),
        };
        format!(
            "randomised harness failed under {seeded}, {}={}.\nreplay: {env}{}={} cargo test {}{test}",
            replay.size_var, replay.size, replay.size_var, replay.size, replay.target
        )
    }

    /// Panic with `failure` and the replay of this run.
    pub fn fail(&self, failure: impl Display, replay: Replay<'_>) -> ! {
        panic!("{failure}\n\n{}", self.replay(replay))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_seed_is_the_fixed_key_byte_for_byte() {
        let r = Rotation {
            var: "X",
            seed: None,
        };
        assert_eq!(r.key(3), [3u8; 32]);
        assert_eq!(r.key(0x88), [0x88u8; 32]);
        assert_eq!(r.base(0xF010_0000_0000), 0xF010_0000_0000);
    }

    #[test]
    fn a_set_seed_changes_every_part_and_keeps_the_parts_apart() {
        let r = Rotation {
            var: "X",
            seed: Some(0x0102_0304_0506_0708),
        };
        assert_eq!(&r.key(1)[..8], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_ne!(r.key(1), r.key(2));
        assert_ne!(r.key(1), [1u8; 32]);
        assert_eq!(r.base(5), 0x0102_0304_0506_0708);
    }

    #[test]
    fn a_failure_names_its_seed_and_the_command_that_repeats_it() {
        let replay = Replay {
            target: "-p conformance --lib",
            size_var: "CASES",
            size: 256,
        };
        let seeded = Rotation {
            var: "SEED",
            seed: Some(42),
        };
        let text = seeded.replay(replay);
        assert!(text.contains("seed 42 (SEED=42)"), "{text}");
        assert!(
            text.contains("replay: SEED=42 CASES=256 cargo test -p conformance --lib"),
            "{text}"
        );
        let fixed = Rotation {
            var: "SEED",
            seed: None,
        };
        let text = fixed.replay(replay);
        assert!(text.contains("the default seed (SEED unset)"), "{text}");
        assert!(text.contains("replay: CASES=256 cargo test"), "{text}");
    }
    #[test]
    #[should_panic(expected = "the case shrank to 3\n\nrandomised harness failed under seed 7")]
    fn a_failure_panics_with_the_failure_first_and_then_the_replay() {
        let r = Rotation {
            var: "SEED",
            seed: Some(7),
        };
        r.fail(
            "the case shrank to 3",
            Replay {
                target: "-p x",
                size_var: "CASES",
                size: 9,
            },
        );
    }
}
