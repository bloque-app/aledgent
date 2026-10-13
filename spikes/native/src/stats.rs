// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Nearest-rank percentiles over integer samples. No floating point.

use std::time::Instant;

/// p50, p95, p99 and max of one series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Number of samples.
    pub n: usize,
    /// Median.
    pub p50: u64,
    /// 95th percentile.
    pub p95: u64,
    /// 99th percentile.
    pub p99: u64,
    /// Maximum.
    pub max: u64,
}

/// The nearest-rank `pct`-th percentile of `sorted` (ascending, non-empty).
pub fn nearest_rank(sorted: &[u64], pct: usize) -> u64 {
    assert!(!sorted.is_empty(), "no samples");
    // rank = ceil(pct/100 * n), 1-based.
    let n = sorted.len();
    let rank = (pct * n).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// Summarise a series.
pub fn summarize(samples: &[u64]) -> Summary {
    let mut s = samples.to_vec();
    s.sort_unstable();
    Summary {
        n: s.len(),
        p50: nearest_rank(&s, 50),
        p95: nearest_rank(&s, 95),
        p99: nearest_rank(&s, 99),
        max: *s.last().expect("non-empty"),
    }
}

/// Nanoseconds as microseconds with one decimal, for tables: `1234567` → `1234.6`.
pub fn us(ns: u64) -> String {
    let tenths = (ns + 50) / 100;
    format!("{}.{}", tenths / 10, tenths % 10)
}

/// Run `f` `warmup` times unrecorded, then `n` times, returning each run's
/// wall time in nanoseconds.
pub fn time_n(warmup: usize, n: usize, mut f: impl FnMut()) -> Vec<u64> {
    for _ in 0..warmup {
        f();
    }
    (0..n)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_nanos() as u64
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_on_one_to_hundred() {
        let s: Vec<u64> = (1..=100).collect();
        assert_eq!(nearest_rank(&s, 50), 50);
        assert_eq!(nearest_rank(&s, 99), 99);
        assert_eq!(nearest_rank(&s, 100), 100);
        let one = [7];
        assert_eq!(nearest_rank(&one, 1), 7);
        assert_eq!(nearest_rank(&one, 99), 7);
    }

    #[test]
    fn microseconds_round_to_a_tenth() {
        assert_eq!(us(1_234_567), "1234.6");
        assert_eq!(us(49), "0.0");
        assert_eq!(us(50), "0.1");
    }
}
