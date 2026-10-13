// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The native Agent with its runtime state saved after every invocation, as the VOS host saves
//! `agent.state` (`agents/core-host/src/serve.rs`, `Persisting`): what the drill and the tests run
//! the archive over without VOS. Its state file is the rows, length-prefixed; a checkpoint of it
//! restores into [`SavingPort::restore`].

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use core_binding::envelope::BlobKey;
use core_binding::host::{AgentPort, NativePort, PortError};
use core_binding::hot::{Hot, RingEntry};
use core_binding::transition::MemRows;

/// The state file's name: the VOS host's.
pub const STATE_FILE: &str = "agent.state";

/// The rows as bytes: each key and value with a 4-byte big-endian length.
pub fn encode_rows(rows: &MemRows) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in &rows.0 {
        for part in [k, v] {
            out.extend_from_slice(&(part.len() as u32).to_be_bytes());
            out.extend_from_slice(part);
        }
    }
    out
}

/// [`encode_rows`] read back.
pub fn decode_rows(bytes: &[u8]) -> Result<MemRows, String> {
    let mut rows = MemRows::default();
    let mut rest = bytes;
    let take = |rest: &mut &[u8]| -> Result<Vec<u8>, String> {
        let (n, r) = rest.split_at_checked(4).ok_or("truncated rows")?;
        let n = u32::from_be_bytes([n[0], n[1], n[2], n[3]]) as usize;
        let (v, r) = r.split_at_checked(n).ok_or("truncated rows")?;
        *rest = r;
        Ok(v.to_vec())
    };
    while !rest.is_empty() {
        let k = take(&mut rest)?;
        let v = take(&mut rest)?;
        rows.0.insert(k, v);
    }
    Ok(rows)
}

/// The native Agent, saving its rows to `dir/agent.state` after every invocation (written,
/// synced, renamed).
pub struct SavingPort {
    /// The native Agent.
    pub inner: NativePort,
    dir: PathBuf,
}

impl SavingPort {
    /// A fresh Agent saving into `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> SavingPort {
        SavingPort {
            inner: NativePort::new(),
            dir: dir.into(),
        }
    }

    /// The Agent saved in `dir`, if any; a fresh one otherwise.
    pub fn restore(dir: impl Into<PathBuf>) -> Result<SavingPort, String> {
        let dir = dir.into();
        let mut port = SavingPort::new(&dir);
        match fs::read(dir.join(STATE_FILE)) {
            Ok(b) => port.inner.rows = decode_rows(&b)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        Ok(port)
    }

    fn save(&self) -> Result<(), PortError> {
        let tmp = self.dir.join(format!("{STATE_FILE}.tmp"));
        let saved = File::create(&tmp)
            .and_then(|mut f| {
                f.write_all(&encode_rows(&self.inner.rows))?;
                f.sync_all()
            })
            .and_then(|_| fs::rename(&tmp, self.dir.join(STATE_FILE)));
        saved.map_err(|e| PortError::Ambiguous(format!("{STATE_FILE}: {e}")))
    }

    /// A reader of the saved state, for [`crate::ArchivingLog`]'s checkpoints.
    pub fn reader(dir: &Path) -> crate::archiver::StateReader {
        let path = dir.join(STATE_FILE);
        Box::new(move || fs::read(&path).map_err(|e| e.to_string()))
    }
}

impl AgentPort for SavingPort {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        let r = self.inner.invoke(message, blobs);
        self.save()?;
        r
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
}
