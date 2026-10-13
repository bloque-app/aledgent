// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Summaries of raw result files: p50, p95, p99 and max per
//! (spike, case, variant, size, metric), nearest-rank, as the tables in
//! `notes/MEASUREMENTS-F002.md` report them.
//!
//! ```text
//! cargo run --release --manifest-path spikes/Cargo.toml --bin summarize -- spikes/results/s4.csv
//! ```

use std::collections::BTreeMap;

use spikes_native::stats::summarize;

/// (spike, case, variant, size, metric, unit).
type Key = (String, String, String, u64, String, String);

fn main() {
    let mut groups: BTreeMap<Key, Vec<u64>> = BTreeMap::new();
    for path in std::env::args().skip(1) {
        let text = std::fs::read_to_string(&path).expect("read csv");
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split(',').collect();
            if f.len() != 8 {
                continue;
            }
            let (Ok(size), Ok(value)) = (f[3].parse::<u64>(), f[6].parse::<u64>()) else {
                continue;
            };
            groups
                .entry((
                    f[0].into(),
                    f[1].into(),
                    f[2].into(),
                    size,
                    f[5].into(),
                    f[7].into(),
                ))
                .or_default()
                .push(value);
        }
    }
    println!("| spike | case | variant | size | metric | n | p50 | p95 | p99 | max | unit |");
    println!("|---|---|---|---:|---|---:|---:|---:|---:|---:|---|");
    for ((spike, case, variant, size, metric, unit), v) in groups {
        let s = summarize(&v);
        println!(
            "| {spike} | {case} | {variant} | {size} | {metric} | {} | {} | {} | {} | {} | {unit} |",
            s.n, s.p50, s.p95, s.p99, s.max
        );
    }
}
