// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! What an operations job reports when it fails. No variant carries a secret: a sealing key, a
//! key root, a token or a connection string never reaches an error (`REQ-AU-8`).

use std::fmt;

/// Why a job failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The file system refused.
    Io(String),
    /// An artifact's bytes are not the ones its manifest names.
    Checksum {
        /// The artifact.
        artifact: String,
    },
    /// A manifest or a configuration file does not parse, or says something impossible.
    Config(String),
    /// The core refused, or was not running.
    Core(String),
    /// `F-014`'s export or import refused.
    Export(String),
    /// A sealed artifact did not open under the key given.
    Seal,
    /// A PostgreSQL tool failed.
    Postgres(String),
    /// The audit store refused.
    Audit(String),
    /// The restore could not go on: the received-log, the replay or the rebuild disagreed.
    Restore(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o: {e}"),
            Error::Checksum { artifact } => write!(f, "{artifact}: checksum mismatch"),
            Error::Config(e) => write!(f, "configuration: {e}"),
            Error::Core(e) => write!(f, "core: {e}"),
            Error::Export(e) => write!(f, "export: {e}"),
            Error::Seal => f.write_str("a sealed artifact did not open"),
            Error::Postgres(e) => write!(f, "postgres: {e}"),
            Error::Audit(e) => write!(f, "audit store: {e}"),
            Error::Restore(e) => write!(f, "restore: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}
