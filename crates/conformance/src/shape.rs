// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The refusal-shape assertion every `ERR-*` case uses (`F-015` §7,
//! T-015-06, `REQ-ER-1`, `REQ-CS-4`).
//!
//! A case triggers the condition and hands the command's result to
//! [`expect_refusal`]. It passes only if the result is `Err(Refusal)` (a
//! command returns `Result<_, Refusal>`, so a success body cannot carry an
//! error), the refusal's code is the expected id, its description is the
//! published one in the catalogue, and its serialised shape is
//! `{ "error": <id>, "description": <condition> }` and reads back as itself.

use std::fmt::Debug;

use ledger_core::Refusal;

use crate::case::Outcome;
use crate::spec::Catalogue;

/// Check a command's result against the catalogue entry for `id`.
pub fn expect_refusal<T: Debug>(result: Result<T, Refusal>, id: &str) -> Outcome {
    let cat = Catalogue::checked_in();
    match check_refusal(&cat, result, id) {
        Ok(()) => Outcome::Pass,
        Err(why) => Outcome::Fail(why),
    }
}

/// [`expect_refusal`] against a given catalogue.
pub fn check_refusal<T: Debug>(
    cat: &Catalogue,
    result: Result<T, Refusal>,
    id: &str,
) -> Result<(), String> {
    let entry = cat
        .error(id)
        .ok_or_else(|| format!("{id} is not an error of the catalogue"))?;
    let refusal = match result {
        Ok(v) => return Err(format!("expected {id}, the command succeeded with {v:?}")),
        Err(r) => r,
    };
    if refusal.code() != id {
        return Err(format!("expected {id}, got {}", refusal.code()));
    }
    if refusal.description() != entry.condition {
        return Err(format!(
            "{id}: runtime description {:?} differs from the published {:?}",
            refusal.description(),
            entry.condition
        ));
    }
    let json = serde_json::to_value(refusal).map_err(|e| e.to_string())?;
    let want = serde_json::json!({ "error": id, "description": entry.condition });
    if json != want {
        return Err(format!("{id}: serialised as {json}, expected {want}"));
    }
    let back: Refusal = serde_json::from_value(json).map_err(|e| format!("{id}: {e}"))?;
    if back != refusal {
        return Err(format!("{id}: read back as {}", back.code()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_refusal_has_the_shape_its_catalogue_entry_describes() {
        for r in Refusal::ALL {
            assert_eq!(
                expect_refusal::<()>(Err(r), r.code()),
                Outcome::Pass,
                "{}",
                r.code()
            );
        }
    }

    #[test]
    fn a_success_or_another_refusal_fails() {
        assert!(matches!(
            expect_refusal(Ok(1), "ERR-ZeroAmount"),
            Outcome::Fail(m) if m.contains("succeeded")
        ));
        assert!(matches!(
            expect_refusal::<()>(Err(Refusal::BatchTooLarge), "ERR-ZeroAmount"),
            Outcome::Fail(m) if m.contains("got ERR-BatchTooLarge")
        ));
        assert!(matches!(
            expect_refusal::<()>(Err(Refusal::ZeroAmount), "ERR-Fault"),
            Outcome::Fail(m) if m.contains("not an error of the catalogue")
        ));
    }

    #[test]
    fn a_published_description_the_runtime_disagrees_with_fails() {
        let mut cat = Catalogue::checked_in();
        let e = cat
            .errors
            .iter_mut()
            .find(|e| e.id == "ERR-ZeroAmount")
            .unwrap();
        e.condition = "Zero".into();
        let err =
            check_refusal::<()>(&cat, Err(Refusal::ZeroAmount), "ERR-ZeroAmount").unwrap_err();
        assert!(err.contains("differs from the published"), "{err}");
    }
}
