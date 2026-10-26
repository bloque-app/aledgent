// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The common test suite for signature engines (`F-042`): what every engine must do, run against each
//! built-in, and against any engine added later. An engine is accepted when it passes this.

#![allow(dead_code)]

use std::sync::Arc;

use serde_json::Value;
use signer::client::{
    ClientError, Patience, RemoteClerk, RemoteCustody, RemoteEd25519, RemoteSecp256k1,
};
use signer::engine::{Engine, KeyHandle};
use signer::proto::{Op, Refusal, Scheme};

/// A key an engine is expected to hold.
pub struct Case {
    pub scheme: Scheme,
    pub locator: Value,
    /// The public half it must publish.
    pub public: Vec<u8>,
}

/// What a suite run needs besides the cases.
pub struct Setup<'a> {
    pub engine: &'a dyn Engine,
    pub cases: Vec<Case>,
    /// A locator no key answers to, for any scheme the cases use.
    pub missing: Value,
    /// A text that must appear in no error and no `Debug`.
    pub secret_marker: &'a str,
}

/// Use a key the way its scheme is used, through the typed wrappers: each checks the answer against the
/// public half, so a wrong answer is an error here.
fn exercise(h: &Arc<dyn KeyHandle>) {
    match h.scheme() {
        Scheme::KeyRing => {
            let c = RemoteCustody::from_key(Arc::clone(h), Patience::TEST).unwrap();
            let a = c
                .try_answer(&ledger_kernel::keys::KeyRequest::RegistrarKey)
                .unwrap();
            let _ = a;
        }
        Scheme::Clerk => {
            let c = RemoteClerk::from_key(Arc::clone(h), Patience::TEST).unwrap();
            assert_eq!(c.try_sign(b"a disclosure").unwrap().len(), 64);
        }
        Scheme::Ed25519 => {
            let k = RemoteEd25519::from_key(Arc::clone(h), Patience::TEST).unwrap();
            let sig = k.try_sign(b"a feed").unwrap();
            // Deterministic: the same bytes sign the same way.
            assert_eq!(sig, k.try_sign(b"a feed").unwrap());
        }
        Scheme::Secp256k1 => {
            let k = RemoteSecp256k1::from_key(Arc::clone(h)).unwrap();
            for i in 0..3u8 {
                k.sign_digest([i; 32]).unwrap();
            }
        }
    }
}

/// Run the suite (blocking: engines are).
pub fn run(s: &Setup) {
    for case in &s.cases {
        assert!(
            s.engine.schemes().contains(&case.scheme),
            "{:?} offers {:?}",
            s.engine.kind(),
            case.scheme
        );
        let h = s.engine.open(case.scheme, &case.locator).unwrap();
        assert_eq!(h.scheme(), case.scheme);
        assert_eq!(h.kind(), s.engine.kind());
        assert_eq!(
            h.public(),
            case.public,
            "the public half of {:?}",
            case.scheme
        );
        assert_eq!(
            h.call(&Op::Public)
                .ok()
                .filter(|p| !p.is_empty())
                .unwrap_or(case.public.clone()),
            case.public
        );
        exercise(&h);

        // The wrong call for the scheme is refused, and says so.
        let wrong = match case.scheme {
            Scheme::Secp256k1 => Op::Sign(vec![1]),
            _ => Op::SignDigest([1; 32]),
        };
        assert_eq!(
            h.call(&wrong).unwrap_err(),
            ClientError::Refused(Refusal::WrongScheme)
        );

        // A key that is not there is refused at open, for any scheme; so is a key of another scheme.
        assert!(s.engine.open(case.scheme, &s.missing).is_err());
        let other = if case.scheme == Scheme::Clerk {
            Scheme::Ed25519
        } else {
            Scheme::Clerk
        };
        if let Err(e) = s.engine.open(other, &case.locator) {
            assert!(
                !e.is_transient(),
                "a wrong scheme is not worth waiting for: {e}"
            );
            assert!(!e.to_string().contains(s.secret_marker));
        }

        // No secret in a Debug.
        assert!(!format!("{h:?}").contains(s.secret_marker));
    }
    let e = s.engine.open(s.cases[0].scheme, &s.missing).unwrap_err();
    assert!(!e.to_string().contains(s.secret_marker), "{e}");
    assert!(s.engine.health().ready);
}
