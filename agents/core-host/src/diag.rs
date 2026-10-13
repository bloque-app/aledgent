// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The core host's own report (`T-032-11`, stress findings F14 and F15): every
//! `ALEDGENT_CORE_DIAG_SECS` seconds (unset or 0: never), one line on standard error with the
//! process's memory and threads (`/proc/self/status`, Linux), the allocator's figures when the host
//! is built with `jemalloc`, the state directory's files, and the core's measurements
//! ([`core_binding::core::CoreStats`]). The stress suite keeps the lines in `logs/core-host.log`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_binding::core::CoreStats;

/// The report's period, from the environment.
pub const ENV_DIAG_SECS: &str = "ALEDGENT_CORE_DIAG_SECS";

/// The fields of `/proc/self/status` the report keeps, in kB, and the thread count.
fn status() -> String {
    let Ok(text) = std::fs::read_to_string("/proc/self/status") else {
        return "status=n/a".into();
    };
    let keep = ["VmRSS", "VmHWM", "RssAnon", "RssFile", "VmSize", "Threads"];
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            keep.contains(&k).then(|| {
                let n = v.split_whitespace().next().unwrap_or("?");
                format!("{k}={n}")
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The state directory's files and their sizes, bytes.
fn files(dir: &Path) -> String {
    let mut out = Vec::new();
    for name in ["agent.state", "archive.bin", "replies.jsonl"] {
        if let Ok(m) = std::fs::metadata(dir.join(name)) {
            out.push(format!("{name}={}", m.len()));
        }
    }
    out.join(" ")
}

#[cfg(feature = "jemalloc")]
fn allocator() -> String {
    use tikv_jemalloc_ctl::{epoch, stats};
    if epoch::advance().is_err() {
        return "jemalloc=n/a".into();
    }
    let f = |r: Result<usize, tikv_jemalloc_ctl::Error>| r.map_or(0, |v| v / 1024);
    format!(
        "je_allocated_kib={} je_active_kib={} je_resident_kib={} je_retained_kib={} je_mapped_kib={}",
        f(stats::allocated::read()),
        f(stats::active::read()),
        f(stats::resident::read()),
        f(stats::retained::read()),
        f(stats::mapped::read()),
    )
}

#[cfg(not(feature = "jemalloc"))]
fn allocator() -> String {
    "allocator=system".into()
}

/// Start the report, if the environment asks for one.
pub fn start(stats: Arc<CoreStats>, dir: PathBuf) {
    let secs = std::env::var(ENV_DIAG_SECS)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    if secs == 0 {
        return;
    }
    let started = Instant::now();
    let _ = std::thread::Builder::new()
        .name("core-diag".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(secs));
                eprintln!(
                    "core-diag t={} {} {} {} {}",
                    started.elapsed().as_secs(),
                    status(),
                    allocator(),
                    files(&dir),
                    stats.report()
                );
            }
        });
}
