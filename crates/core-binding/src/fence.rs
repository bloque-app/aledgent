// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The fence, always on, and the way out (`T-037-15`; `REQ-RT-20`, `REQ-RT-18`; amendment 0033
//! D2, D8, D10; `DEC-26` (b)). Part of *H₀*: every package carries it, and no flag turns it off.
//!
//! **The fence.** At every input, the code that receives it compares its own compiled code
//! generation (`ledger_core::code_generation!`, read by the guest's `code_generation()`, the
//! record `generation_of` reads: one number, D18) with the writer's state:
//!
//! - code of the generation in force *g* takes the input, unless a scheduled upgrade applies to it
//!   (`upgrade = Some((H, n))` and the input is numbered `n` or later): then it takes only a way
//!   out, and only one it accepts;
//! - code of generation *g* + 1 takes the input only when a scheduled upgrade applies to it;
//! - any other code refuses it.
//!
//! A refusal is [`AgentRefusal::Fenced`]: nothing is written and the input's number is not taken,
//! so the same input runs once the code the code map names is installed. A missed switch, or a
//! stale package after one, halts the writer at the input instead of running it under the wrong
//! code. The fence identifies code up to its generation; that the bytes running are the code
//! version's own is the execution node's check, by digest (`T-037-14`).
//!
//! **The way out.** `reschedule_upgrade` and `cancel_upgrade` (`ledger_core`'s, authorised under
//! the mode in force) are the only inputs the old code takes once the upgrade applies. A way-out
//! input is exactly one such call and nothing else ([`way_out_input`]); a way out mixed with other
//! calls or deliveries is malformed. Code of either generation decides it the same
//! ([`decide`]): taken, the upgrade does not take effect at its input, a cancel clears it, a
//! reschedule moves it, and the generation stays *g*. Not taken, the old code refuses it as
//! fenced, and the new code takes the upgrade and records the refusal, so the committed state at
//! that input never depends on which package was installed when it arrived.
//!
//! **The upgrade taking effect.** Code of *g* + 1 that takes an input the upgrade applies to,
//! other than a way out it takes, makes *H* and *g* + 1 the ones in force as the input's first
//! effect: the input's checkpoints bind *H* ([`WriterCode::code_for`]) and its ring entry names
//! *H*. `T-037-04` schedules the upgrade (`Upgrade`); this module is where it takes effect.
//!
//! Every function here is pure, `no_std`, and used by both the Agent's transition and the host's
//! driver, which predicts each entry it compares (`crate::host`).

use alloc::string::String;

use ledger_core::call::Call;
use ledger_core::code::WayOut;

use crate::envelope::{AgentRefusal, Fenced};
use crate::hot::Hot;

/// The writer's code, as its hot row holds it: what the fence reads and an input changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriterCode {
    /// The code version in force.
    pub code: [u8; 32],
    /// The code generation in force.
    pub generation: u64,
    /// The scheduled upgrade: the code version and the input number it applies from.
    pub upgrade: Option<([u8; 32], u64)>,
}

impl WriterCode {
    /// The writer's code in `hot`.
    pub fn of(hot: &Hot) -> WriterCode {
        WriterCode {
            code: hot.code,
            generation: hot.generation,
            upgrade: hot.upgrade,
        }
    }

    /// Write it back into `hot`.
    pub fn store(&self, hot: &mut Hot) {
        hot.code = self.code;
        hot.generation = self.generation;
        hot.upgrade = self.upgrade;
    }

    /// Whether the scheduled upgrade applies to input `seq`: scheduled from it or from an input
    /// before it.
    pub fn due(&self, seq: u64) -> bool {
        self.upgrade.is_some_and(|(_, n)| seq >= n)
    }

    /// The code version that executes input `seq`, as the code map names it: the scheduled
    /// upgrade's once it applies, else the code in force. What the input's checkpoints bind
    /// (`T-037-03`); a way out does not make one.
    pub fn code_for(&self, seq: u64) -> [u8; 32] {
        match self.upgrade {
            Some((code, n)) if seq >= n => code,
            _ => self.code,
        }
    }

    fn fenced(&self, running: u64, seq: u64) -> AgentRefusal {
        AgentRefusal::Fenced(Fenced {
            seq,
            code_generation: running,
            generation: self.generation,
            upgrade: self.upgrade,
            refusal: None,
        })
    }
}

/// The way out a call is, if it is one.
pub fn way_out_of(call: &Call) -> Option<WayOut> {
    match call {
        Call::RescheduleUpgrade { from, .. } => Some(WayOut::Reschedule { from: *from }),
        Call::CancelUpgrade { .. } => Some(WayOut::Cancel),
        _ => None,
    }
}

/// Whether an input of `calls` with `deliveries` delivery records is a way-out input: exactly one
/// call, a way out, and no delivery. A way out with anything else is malformed.
pub fn way_out_input(calls: &[Call], deliveries: usize) -> Result<bool, AgentRefusal> {
    let ways = calls.iter().filter(|c| way_out_of(c).is_some()).count();
    match ways {
        0 => Ok(false),
        1 if calls.len() == 1 && deliveries == 0 => Ok(true),
        _ => Err(AgentRefusal::Malformed(String::from(
            "a way out of an upgrade is an input of its own: one call, no delivery",
        ))),
    }
}

/// The fence's admission (module documentation): whether code of generation `running` may decide
/// input `seq` of a writer whose code is `w`. `way_out`: the input is a way-out input. Refused,
/// nothing of the input may run.
pub fn admit(running: u64, w: &WriterCode, seq: u64, way_out: bool) -> Result<(), AgentRefusal> {
    let due = w.due(seq);
    let old = running == w.generation && (!due || way_out);
    let new = due && w.generation.checked_add(1) == Some(running);
    if old || new {
        Ok(())
    } else {
        Err(w.fenced(running, seq))
    }
}

/// After the input's calls ran under code of generation `running` (admitted by [`admit`]): the
/// fence's last word, then [`decide`]. `taken`: the way out the input took
/// (`ledger_core::Ledger::take_way_out`). The old code takes nothing but a way out it accepts
/// once the upgrade applies: a way-out input it does not take is refused as fenced, with nothing
/// written.
pub fn settle(
    running: u64,
    w: &mut WriterCode,
    seq: u64,
    taken: Option<WayOut>,
) -> Result<(), AgentRefusal> {
    if w.due(seq) && taken.is_none() && running == w.generation {
        return Err(w.fenced(running, seq));
    }
    decide(w, seq, taken);
    Ok(())
}

/// What input `seq` does to the writer's code, as the code map reads it and code of either
/// generation decides it (module documentation): a way out taken moves or clears the upgrade and
/// keeps the generation; otherwise, an upgrade that applies to the input takes effect.
pub fn decide(w: &mut WriterCode, seq: u64, taken: Option<WayOut>) {
    match taken {
        Some(t) => w.upgrade = t.apply(w.upgrade),
        None => {
            if let Some((code, n)) = w.upgrade
                && seq >= n
            {
                w.code = code;
                w.generation = w.generation.saturating_add(1);
                w.upgrade = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H0: [u8; 32] = [0xA0; 32];
    const H1: [u8; 32] = [0xA1; 32];

    fn scheduled(n: u64) -> WriterCode {
        WriterCode {
            code: H0,
            generation: 4,
            upgrade: Some((H1, n)),
        }
    }

    fn fenced(r: Result<(), AgentRefusal>) -> bool {
        matches!(r, Err(AgentRefusal::Fenced(_)))
    }

    /// `REQ-RT-20`, D8: the rule at every input, whether or not an upgrade is scheduled.
    #[test]
    fn t037_15_the_fence_admits_only_the_code_the_state_names() {
        let none = WriterCode {
            upgrade: None,
            ..scheduled(0)
        };
        // Nothing scheduled: only g.
        assert!(admit(4, &none, 9, false).is_ok());
        for r in [0, 3, 5, 6, u64::MAX] {
            assert!(fenced(admit(r, &none, 9, false)), "generation {r}");
        }
        // Scheduled from 10: before it, only g; from it, g + 1, and g for a way out only.
        let w = scheduled(10);
        assert!(admit(4, &w, 9, false).is_ok());
        assert!(fenced(admit(5, &w, 9, false)));
        assert!(fenced(admit(4, &w, 10, false)));
        assert!(admit(4, &w, 10, true).is_ok());
        assert!(admit(5, &w, 10, false).is_ok());
        assert!(admit(5, &w, 11, true).is_ok());
        assert!(fenced(admit(6, &w, 10, false)));
        assert!(fenced(admit(3, &w, 10, true)));
        // The refusal names what it saw.
        let Err(AgentRefusal::Fenced(f)) = admit(3, &w, 12, false) else {
            panic!()
        };
        assert_eq!(
            f,
            Fenced {
                seq: 12,
                code_generation: 3,
                generation: 4,
                upgrade: Some((H1, 10)),
                refusal: None
            }
        );
    }

    /// D10: a way out at or after `n` is decided the same by either generation; an input that is
    /// not a way out taken makes the upgrade take effect under the new code, and is fenced under
    /// the old.
    #[test]
    fn t037_15_either_generation_decides_the_way_out_the_same() {
        for taken in [
            Some(WayOut::Cancel),
            Some(WayOut::Reschedule { from: 20 }),
            None,
        ] {
            let (mut old, mut new) = (scheduled(10), scheduled(10));
            let a = settle(4, &mut old, 10, taken);
            let b = settle(5, &mut new, 10, taken);
            match taken {
                Some(_) => {
                    assert!(a.is_ok() && b.is_ok());
                    assert_eq!(old, new);
                    assert_eq!((old.code, old.generation), (H0, 4));
                }
                None => {
                    assert!(fenced(a));
                    assert!(b.is_ok());
                    assert_eq!((new.code, new.generation, new.upgrade), (H1, 5, None));
                }
            }
        }
        let mut w = scheduled(10);
        decide(&mut w, 10, Some(WayOut::Cancel));
        assert_eq!(w.upgrade, None);
        let mut w = scheduled(10);
        decide(&mut w, 10, Some(WayOut::Reschedule { from: 15 }));
        assert_eq!(w.upgrade, Some((H1, 15)));
        // Before `n`, an input changes nothing of the code.
        let mut w = scheduled(10);
        decide(&mut w, 9, None);
        assert_eq!(w, scheduled(10));
        assert_eq!((w.code_for(9), w.code_for(10)), (H0, H1));
    }
}
