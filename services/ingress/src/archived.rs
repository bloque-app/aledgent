// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Archived identifiers (`REQ-RC-7`, amendment 0024; owner ruling `au4-q1`, `au-19`): a prior
//! system's identifiers that the book's initial state deliberately left out answer
//! `ERR-IdentifierArchived` (410), for good, and never resolve.
//!
//! The list is the deployment's, fixed with its genesis: the migration's `excluded.jsonl` (format
//! `aledgent/excluded/v1`, one canonical JSON line per identifier, `{"legacy", "kind", "reason"}`,
//! `kind` `urn` or `ledger_account_id`), named by the deployment file's `archived_identifiers`
//! beside the book's chainspec. Its SHA-256 must be the one the chainspec's header commits
//! (`excluded_sha256`, format 3), and the chainspec must be the one genesis commits
//! (`T-036-29`): the list is the one genesis committed, not one a deployment file names. A file
//! of another digest, a chainspec that commits none, or a line that does not parse stops the
//! start: a list that is not the reviewed one must not decide what a caller is told.
//! Identifiers only, no personal data (`INV-17`); the book never sees the list.
//!
//! **Where ingress answers it.** Whenever a request names one: a path segment, a query parameter
//! (`GET /v1/aliases?source=…&name=…` among them), or any string of a command's body except its
//! pass-through data (`reference`, `REQ-PO-5`), which is the caller's own and never read. Nothing
//! refused is logged. A caller learns that the identifier existed and was left behind on purpose,
//! which is the point: "archived" is not "unknown".

use std::collections::HashSet;

use ledger_core::Refusal;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// The list's line format.
pub const FORMAT: &str = "aledgent/excluded/v1";

/// One line of the list.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    legacy: String,
    kind: String,
    #[allow(dead_code)]
    reason: String,
}

/// The archived identifiers a deployment names (module documentation). Empty: none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArchivedIds {
    ids: HashSet<String>,
}

impl ArchivedIds {
    /// Read the list from `bytes`, which must hash to `sha256`: the chainspec header's
    /// `excluded_sha256` (`T-036-29`).
    pub fn parse(bytes: &[u8], sha256: &[u8; 32]) -> Result<ArchivedIds, String> {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if digest != *sha256 {
            return Err(format!(
                "the archived identifiers hash to {}, not to the {} the chainspec's genesis commits",
                hex::encode(digest),
                hex::encode(sha256)
            ));
        }
        let text = std::str::from_utf8(bytes)
            .map_err(|_| "the archived identifiers are not UTF-8".to_string())?;
        let mut ids = HashSet::new();
        for (n, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let l: Line = serde_json::from_str(line).map_err(|e| {
                format!(
                    "the archived identifiers, line {}: not {FORMAT} ({e})",
                    n + 1
                )
            })?;
            if l.legacy.is_empty() || !matches!(l.kind.as_str(), "urn" | "ledger_account_id") {
                return Err(format!(
                    "the archived identifiers, line {}: not {FORMAT}",
                    n + 1
                ));
            }
            ids.insert(l.legacy);
        }
        Ok(ArchivedIds { ids })
    }

    /// A list of `ids`, for tests and tools.
    pub fn of<I: IntoIterator<Item = S>, S: Into<String>>(ids: I) -> ArchivedIds {
        ArchivedIds {
            ids: ids.into_iter().map(Into::into).collect(),
        }
    }

    /// How many.
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Whether `id` is archived.
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// `ERR-IdentifierArchived` if the request names an archived identifier: its path segment,
    /// any value of its query string, or any string of its JSON body but the top-level
    /// pass-through `reference`. A body that is not JSON names nothing here; its own refusal
    /// follows.
    pub fn check(
        &self,
        param: Option<&str>,
        query: Option<&str>,
        body: &[u8],
    ) -> Result<(), Refusal> {
        if self.ids.is_empty() {
            return Ok(());
        }
        let hit = param.is_some_and(|p| self.contains(p))
            || query.is_some_and(|q| {
                serde_urlencoded::from_str::<Vec<(String, String)>>(q)
                    .is_ok_and(|pairs| pairs.iter().any(|(_, v)| self.contains(v)))
            })
            || (!body.is_empty()
                && serde_json::from_slice::<serde_json::Value>(body).is_ok_and(|v| match v {
                    serde_json::Value::Object(m) => m
                        .iter()
                        .filter(|(k, _)| k.as_str() != "reference")
                        .any(|(_, v)| self.in_value(v)),
                    other => self.in_value(&other),
                }));
        if hit {
            Err(Refusal::IdentifierArchived)
        } else {
            Ok(())
        }
    }

    fn in_value(&self, v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::String(s) => self.contains(s),
            serde_json::Value::Array(a) => a.iter().any(|x| self.in_value(x)),
            serde_json::Value::Object(m) => m.values().any(|x| self.in_value(x)),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "{\"legacy\":\"urn:example:qa:1\",\"kind\":\"urn\",\"reason\":\"test-identity-zero-balance\"}\n\
                        {\"legacy\":\"3f0c\",\"kind\":\"ledger_account_id\",\"reason\":\"test-identity-zero-balance\"}\n";

    fn sha(b: &[u8]) -> [u8; 32] {
        Sha256::digest(b).into()
    }

    #[test]
    fn the_list_is_the_one_the_deployment_names() {
        let a = ArchivedIds::parse(FILE.as_bytes(), &sha(FILE.as_bytes())).unwrap();
        assert_eq!(a.len(), 2);
        assert!(a.contains("3f0c") && a.contains("urn:example:qa:1"));
        assert!(ArchivedIds::parse(FILE.as_bytes(), &sha(b"other")).is_err());
        let bad = "{\"legacy\":\"x\",\"kind\":\"phone\",\"reason\":\"r\"}\n";
        assert!(ArchivedIds::parse(bad.as_bytes(), &sha(bad.as_bytes())).is_err());
    }

    /// `REQ-RC-7`: a path segment, a query value or a body string naming an archived identifier
    /// is `ERR-IdentifierArchived`; the pass-through data is the caller's and is not read.
    #[test]
    fn req_rc_7_an_archived_identifier_is_answered_wherever_a_request_names_it() {
        let a = ArchivedIds::of(["3f0c"]);
        let gone = Err(Refusal::IdentifierArchived);
        assert_eq!(a.check(Some("3f0c"), None, b""), gone);
        assert_eq!(a.check(None, Some("source=x&name=3f0c"), b""), gone);
        assert_eq!(
            a.check(
                None,
                None,
                br#"{"payer":"3f0c","asset":"USD","amount":"1"}"#
            ),
            gone
        );
        assert_eq!(a.check(None, None, br#"{"legs":[{"payee":"3f0c"}]}"#), gone);
        assert_eq!(
            a.check(
                None,
                None,
                br#"{"payer":"did:x","reference":{"id":"3f0c"}}"#
            ),
            Ok(())
        );
        assert_eq!(
            a.check(Some("did:x"), Some("asset=USD"), b"not json"),
            Ok(())
        );
        assert_eq!(
            ArchivedIds::default().check(Some("3f0c"), None, b""),
            Ok(())
        );
    }
}
