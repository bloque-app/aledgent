// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter's operational signals (`F-018` T-018-04, `NFR-11`, `REQ-SA-7`, `REQ-IN-5`,
//! `REQ-SN-2`).
//!
//! Only the adapter touches the settlement layer (guardrail 8), so what `F-018`'s exporter needs
//! from the layer the adapter reads and reports: observation completeness (its cursor's lag
//! behind the finalized inbox, the gaps it recorded, the differences its re-scans raised), the
//! liquidity of the positions that fund settlement (the instance's position per asset, each
//! lane's native balance against the fee floor, the liquidity alerts it holds open), and the
//! layer's block production and finality (the finalized and best heights: the gap between them
//! is a finality stall when blocks still come). Amounts are decimal strings of the layer's
//! integer units: no floating point (`INV-7`).
//!
//! The binary writes them to a file after every step when configured to
//! (`ProcessConfig::signals_file`); the exporter reads that file.

use std::collections::BTreeMap;

use ledger_core::account::Time;
use serde::{Deserialize, Serialize};

use crate::adapter::{Adapter, StepResult};
use crate::chain::Chain;
use crate::link::Transport;
use crate::store::Store;

/// One lane's fee balance.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneSignal {
    /// The lane's account, hex.
    pub lane: String,
    /// Its native balance at the finalized head.
    pub native: String,
    /// Whether it is under the fee floor.
    pub under_floor: bool,
}

/// The instance's position in one asset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionSignal {
    /// The book's asset.
    pub asset: String,
    /// The position at the finalized head, in the layer's units.
    pub position: String,
}

/// The adapter's signals at one time. See the module documentation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterSignals {
    /// When they were read.
    pub at: Time,
    /// Whether the adapter serves.
    pub serving: bool,
    /// The instance.
    pub instance: u32,
    /// The finalized height.
    pub finalized: u64,
    /// The best height: with the finalized one, whether blocks come but finality does not.
    /// Zero from an adapter that did not report it.
    #[serde(default)]
    pub best: u64,
    /// The finalized inbox's length.
    pub inbox: u64,
    /// The adapter's cursor: the next inbox position to read.
    pub cursor: u64,
    /// Gaps recorded, ever (`REQ-SA-7`).
    pub gaps: u64,
    /// Open alerts by kind: the part of the key before its first `:` (`position`, `fees`,
    /// `gap`, `incident`, `rescan`, `lane`, `digest`, `refusing`).
    pub alerts: BTreeMap<String, u64>,
    /// Each lane's fee balance.
    pub lanes: Vec<LaneSignal>,
    /// The fee floor.
    pub fee_floor: String,
    /// The instance's position per asset.
    pub positions: Vec<PositionSignal>,
}

impl AdapterSignals {
    /// How far the cursor is behind the finalized inbox.
    pub fn lag(&self) -> u64 {
        self.inbox.saturating_sub(self.cursor)
    }

    /// How many blocks the finalized head is behind the best one.
    pub fn finality_lag(&self) -> u64 {
        self.best.saturating_sub(self.finalized)
    }
}

impl<C: Chain, S: Store, T: Transport> Adapter<C, S, T> {
    /// Read the adapter's signals now (module documentation).
    pub async fn signals(&self, now: Time) -> StepResult<AdapterSignals> {
        let config = self.config();
        let fin = self.chain().finalized().await?;
        let best = self.chain().best().await?;
        let inbox = self
            .chain()
            .instance(&fin, config.instance)
            .await?
            .map_or(0, |i| i.next_seq);
        let cursor = self.store().cursor(config.instance).await?;
        let gaps = self.store().gaps().await?.len() as u64;
        let mut alerts = BTreeMap::new();
        for a in self.store().alerts().await? {
            if a.cleared_at.is_none() {
                let kind = a.key.split(':').next().unwrap_or_default().to_owned();
                *alerts.entry(kind).or_insert(0) += 1;
            }
        }
        let mut lanes = Vec::new();
        for l in self.lanes() {
            let native = self.chain().native_balance(&fin, &l.account()).await?;
            lanes.push(LaneSignal {
                lane: hex::encode(l.account()),
                native: native.to_string(),
                under_floor: native < config.fee_floor,
            });
        }
        let mut positions = Vec::new();
        for a in &config.assets {
            let p = self
                .chain()
                .position(&fin, config.instance, &a.chain)
                .await?;
            positions.push(PositionSignal {
                asset: a.asset.clone(),
                position: p.to_string(),
            });
        }
        Ok(AdapterSignals {
            at: now,
            serving: self.status().0,
            instance: config.instance,
            finalized: fin.number,
            best: best.number.max(fin.number),
            inbox,
            cursor,
            gaps,
            alerts,
            lanes,
            fee_floor: config.fee_floor.to_string(),
            positions,
        })
    }
}
