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
//!   (`upgrade = Some((H, n))` and the input is numbered `n` or later): then it takes only a
//!   way-out input, and decides every one;
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
//! input is one whose calls are only those, with no delivery ([`way_out_input`]). Code of either
//! generation decides every way-out input the same (owner ruling on #44, D10; [`decide`]): the
//! upgrade does not take effect at it, and the generation stays *g*. A way out taken applies: a
//! cancel clears the upgrade, a reschedule moves it. One not taken (unauthorised, a move not later
//! than its own input, a repeat of an earlier request, or not its input's first call, which the
//! library refuses as `ERR-MalformedCommand`) is a recorded refusal that takes its input number,
//! and the upgrade stays scheduled, to take effect at the next input that is not a way out. So a
//! bad way out at the head of the queue can never wedge the old code, and the committed state at
//! that input never depends on which package was installed when it arrived. An input that mixes a
//! way out with anything else is refused whole, `ERR-MalformedCommand`, before the fence and the
//! same by every generation, with nothing written ([`way_out_input`]); no request makes one, since
//! one command is one call.
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
/// call, a way out, and no delivery. An input with no way out is an ordinary one. An input that
/// holds a way out and anything else (another call, a second way out, a delivery) is refused whole,
/// `ERR-MalformedCommand`, with nothing of it applied, by code of any generation alike, before the
/// fence (owner ruling on #44, N1; amendment 0035).
pub fn way_out_input<'a>(
    calls: impl IntoIterator<Item = &'a Call>,
    deliveries: usize,
) -> Result<bool, AgentRefusal> {
    let (mut n, mut ways) = (0usize, 0usize);
    for c in calls {
        n += 1;
        if way_out_of(c).is_some() {
            ways += 1;
        }
    }
    match (ways, n, deliveries) {
        (0, _, _) => Ok(false),
        (1, 1, 0) => Ok(true),
        _ => Err(AgentRefusal::Malformed(String::from(
            "ERR-MalformedCommand: a way out of an upgrade is an input of its own, one call and \
             nothing else",
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

/// What input `seq` does to the writer's code, as the code map reads it and code of either
/// generation admitted by [`admit`] decides it (module documentation). `way_out`: the input is a
/// way-out input; `taken`: the way out its calls took (`ledger_core::Ledger::take_way_out`).
///
/// An upgrade that applies to an input that is not a way out takes effect first; then a way out
/// taken moves or clears whatever upgrade is still scheduled. A way-out input never makes the
/// upgrade take effect, taken or not, and keeps the generation.
pub fn decide(w: &mut WriterCode, seq: u64, way_out: bool, taken: Option<WayOut>) {
    if !way_out
        && let Some((code, n)) = w.upgrade
        && seq >= n
    {
        w.code = code;
        w.generation = w.generation.saturating_add(1);
        w.upgrade = None;
    }
    if let Some(t) = taken {
        w.upgrade = t.apply(w.upgrade);
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

    /// D10 and the owner's ruling on #44: a way-out input at or after `n` is admitted by either
    /// generation and decided the same, taken or not, and never makes the upgrade take effect;
    /// any other input there is fenced under the old code and makes the upgrade take effect under
    /// the new.
    #[test]
    fn t037_15_either_generation_decides_the_way_out_the_same() {
        let w = scheduled(10);
        for running in [4, 5] {
            assert!(admit(running, &w, 10, true).is_ok());
        }
        assert!(fenced(admit(4, &w, 10, false)));
        for taken in [
            Some(WayOut::Cancel),
            Some(WayOut::Reschedule { from: 20 }),
            None,
        ] {
            let mut x = scheduled(10);
            decide(&mut x, 10, true, taken);
            assert_eq!((x.code, x.generation), (H0, 4));
            assert_eq!(
                x.upgrade,
                taken.map_or(Some((H1, 10)), |t| t.apply(Some((H1, 10))))
            );
        }
        // Not a way out: the upgrade takes effect, and a way out among its calls finds nothing.
        let mut x = scheduled(10);
        decide(&mut x, 11, false, Some(WayOut::Reschedule { from: 30 }));
        assert_eq!((x.code, x.generation, x.upgrade), (H1, 5, None));
        // Before `n`, an input changes nothing of the code but what a way out takes.
        let mut x = scheduled(10);
        decide(&mut x, 9, false, None);
        assert_eq!(x, scheduled(10));
        decide(&mut x, 9, false, Some(WayOut::Reschedule { from: 15 }));
        assert_eq!(x.upgrade, Some((H1, 15)));
        assert_eq!((x.code_for(14), x.code_for(15)), (H0, H1));
    }
}
