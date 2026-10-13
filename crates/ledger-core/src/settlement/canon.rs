// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The canonical form of an outbound instruction, and of the settlement adapter's status
//! declaration (`F-016`), in an export (`F-014` T-014-01).

use super::{AdapterStatus, OutboundInstruction};
use crate::export::canon::{Canonical, Dec, DecodeError, Enc};

impl Canonical for OutboundInstruction {
    fn encode(&self, e: &mut Enc) {
        let OutboundInstruction {
            operation_ref,
            asset,
            amount,
            destination,
            expiry,
        } = self;
        e.put(operation_ref)
            .put(asset)
            .put(amount)
            .put(destination)
            .put(expiry);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(OutboundInstruction {
            operation_ref: d.get()?,
            asset: d.get()?,
            amount: d.get()?,
            destination: d.get()?,
            expiry: d.get()?,
        })
    }
}

impl Canonical for AdapterStatus {
    fn encode(&self, e: &mut Enc) {
        let AdapterStatus {
            serving,
            declared_at,
            fresh_until,
        } = self;
        e.put(serving).put(declared_at).put(fresh_until);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AdapterStatus {
            serving: d.get()?,
            declared_at: d.get()?,
            fresh_until: d.get()?,
        })
    }
}
