// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The front door refuses a code version the switch cannot install (`T-037-22`; `REQ-RT-16`,
//! `REQ-RT-20`; owner ruling w16, proposal 4): the core asks its port, before it stages the
//! version, whether the switch could install it, and a port that says no leaves nothing staged
//! and the door answers `ERR-InvalidTransition` (`T-037-04`), so the version is never recorded.
//!
//! The port here is boxed, as the core host's is: the answer must travel through the wrapper that
//! really holds the port, or the check would be a silent yes in production.

use std::sync::Arc;

use core_binding::core::{AgentCore, Founding, MemLog};
use core_binding::envelope::BlobKey;
use core_binding::host::{AgentPort, NativePort, PortError};
use core_binding::hot::{Hot, Limits, MAX_BUDGET, RingEntry};
use core_binding::staging::Checked;
use ingress::boundary::{Core as _, Staged};

mod common;
use common::HeldCode;

/// A native port that answers the question of the door as the test says, and is the native port
/// in everything else.
struct Picky {
    inner: NativePort,
    /// The reason it gives, or `None` to say yes.
    refuse: Option<String>,
}

impl AgentPort for Picky {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        self.inner.invoke(message, blobs)
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.inner.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        self.inner.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.inner.entry(seq)
    }
    fn switch_to(&mut self, code: [u8; 32]) -> Result<(), String> {
        self.inner.switch_to(code)
    }
    fn stage_code(&mut self, checked: Checked) -> Result<(), core_binding::staging::StageRefusal> {
        self.inner.stage_code(checked)
    }
    fn can_install(&self, checked: &Checked) -> Result<(), String> {
        let _ = checked;
        self.refuse.clone().map_or(Ok(()), Err)
    }
}

fn core(refuse: Option<&str>) -> (AgentCore<Box<Picky>>, HeldCode) {
    let g = common::genesis();
    let founding = Founding {
        genesis: g,
        op: common::op(0),
        at: 1,
        keys: common::keys(),
        limits: Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        genesis_roots: None,
    };
    let port = Box::new(Picky {
        inner: NativePort::new(),
        refuse: refuse.map(str::to_owned),
    });
    let core = AgentCore::open(port, founding, MemLog::default())
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let code = HeldCode::default();
    core.set_code_source(Arc::new(code.clone())).unwrap();
    (core, code)
}

fn staged(core: &AgentCore<Box<Picky>>) -> bool {
    core.with_port(|p| !p.inner.code_map.is_empty())
        .unwrap_or_default()
}

/// `T-037-22`: a port that says the switch could not install the version makes the core refuse it,
/// with the port's reason in the refusal, and nothing is staged.
#[tokio::test(flavor = "multi_thread")]
async fn t037_22_a_version_the_port_cannot_install_is_refused_and_not_staged() {
    let (core, code) = core(Some("it names another runtime than the one this host runs"));
    let h1 = code.version(1);
    let Staged::Refused { reason } = core.stage_code(&h1).await.unwrap() else {
        panic!("a version the switch could not install was staged");
    };
    assert!(
        reason.contains("could not install") && reason.contains("another runtime"),
        "{reason}"
    );
    assert!(!staged(&core), "nothing was staged");
}

/// `T-037-22`: the same version, with a port that says yes, is staged as it was before the check
/// existed (a port that does not override the question says yes: `ac_k7_1` in `upgrade_input.rs`
/// stages through the native port).
#[tokio::test(flavor = "multi_thread")]
async fn t037_22_a_version_the_port_can_install_is_staged_as_before() {
    let (core, code) = core(None);
    let h1 = code.version(1);
    assert!(matches!(
        core.stage_code(&h1).await.unwrap(),
        Staged::Staged
    ));
    assert!(staged(&core));
}

/// A wrapper that forwards staging and forgets the question must not turn the door off: the
/// default of `can_install` says no, so the version is refused and nothing is staged (`T-037-22`).
struct Forgetful(NativePort);

impl AgentPort for Forgetful {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        self.0.invoke(message, blobs)
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.0.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        self.0.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.0.entry(seq)
    }
    fn stage_code(&mut self, checked: Checked) -> Result<(), core_binding::staging::StageRefusal> {
        self.0.stage_code(checked)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn t037_22_a_wrapper_that_forgets_the_question_refuses_everything() {
    let founding = Founding {
        genesis: common::genesis(),
        op: common::op(0),
        at: 1,
        keys: common::keys(),
        limits: Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        genesis_roots: None,
    };
    let core = AgentCore::open(Forgetful(NativePort::new()), founding, MemLog::default()).unwrap();
    let code = HeldCode::default();
    core.set_code_source(Arc::new(code.clone())).unwrap();
    let Staged::Refused { reason } = core.stage_code(&code.version(1)).await.unwrap() else {
        panic!("a port that does not answer the question staged a version");
    };
    assert!(reason.contains("does not say"), "{reason}");
    assert!(
        !core
            .with_port(|p| !p.0.code_map.is_empty())
            .unwrap_or_default()
    );
}
