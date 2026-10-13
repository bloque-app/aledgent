// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! F-002 measurement spikes, native half: shared shapes, statistics and the
//! CSV result format. Never shipped (`spikes/README.md`).

pub mod golden;
pub mod index_store;
pub mod shapes;
pub mod stats;
pub mod witness;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// The directory raw results go to: `spikes/results`.
pub fn results_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("spikes/native has a parent")
        .join("results")
}

/// One raw sample, in the format of `spikes/README.md`.
#[derive(Clone, Debug)]
pub struct Row {
    /// `s1` … `s4`.
    pub spike: &'static str,
    /// The posting shape or scenario.
    pub case: String,
    /// Store, batch or option.
    pub variant: String,
    /// Book size in accounts, or state size in bytes.
    pub size: u64,
    /// 0-based index after warm-up.
    pub sample: u64,
    /// What was measured.
    pub metric: &'static str,
    /// The value, in `unit`.
    pub value: u64,
    /// `ns`, `gas` or `bytes`.
    pub unit: &'static str,
}

/// Writes rows to `results/<name>.csv`, header first.
pub struct Sink {
    out: BufWriter<File>,
}

impl Sink {
    /// Create (truncate) `results/<name>.csv`.
    pub fn create(name: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(results_dir())?;
        let path = results_dir().join(format!("{name}.csv"));
        let mut out = BufWriter::new(File::create(path)?);
        writeln!(out, "spike,case,variant,size,sample,metric,value,unit")?;
        Ok(Self { out })
    }

    /// Append one row.
    pub fn row(&mut self, r: &Row) -> std::io::Result<()> {
        writeln!(
            self.out,
            "{},{},{},{},{},{},{},{}",
            r.spike, r.case, r.variant, r.size, r.sample, r.metric, r.value, r.unit
        )
    }

    /// Append every sample of one series.
    #[allow(clippy::too_many_arguments)]
    pub fn series(
        &mut self,
        spike: &'static str,
        case: &str,
        variant: &str,
        size: u64,
        metric: &'static str,
        unit: &'static str,
        values: &[u64],
    ) -> std::io::Result<()> {
        for (i, v) in values.iter().enumerate() {
            self.row(&Row {
                spike,
                case: case.to_string(),
                variant: variant.to_string(),
                size,
                sample: i as u64,
                metric,
                value: *v,
                unit,
            })?;
        }
        Ok(())
    }

    /// Flush to disk.
    pub fn finish(mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}
