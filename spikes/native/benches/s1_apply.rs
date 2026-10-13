// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! S1 as Criterion benches: `Book::apply` over each posting shape at batch
//! sizes 1, 4, 10 and 64, on `FileStore` with redb's in-memory backend.
//! The percentile tables in `notes/MEASUREMENTS-F002.md` come from the `s1`
//! binary, which keeps every raw sample; these benches are for regression.
//!
//! ```text
//! cargo bench --manifest-path spikes/Cargo.toml --bench s1_apply
//! ```

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use spikes_native::shapes::{
    CUSTOMERS, Ops, Shape, assert_applied, file_mem_book, sample_units, set_up,
};

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("book_apply");
    group.sample_size(20);
    for shape in Shape::ALL {
        let batches: &[usize] = if shape.batched() {
            &[1, 4, 10, 64]
        } else {
            &[1]
        };
        for &b in batches {
            let mut book = file_mem_book();
            set_up(&mut book, CUSTOMERS);
            let mut ops = Ops::new(0x52);
            let mut ts = 100_000u64;
            group.bench_with_input(BenchmarkId::new(shape.name(), b), &b, |bench, &b| {
                bench.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        let (prep, unit) = sample_units(shape, b, &mut ops);
                        if let Some(p) = prep {
                            ts += 1_000;
                            assert_applied(&book.apply(&p, ts).expect("prep"));
                        }
                        ts += 1_000;
                        let t = Instant::now();
                        let a = book.apply(&unit, ts).expect("apply");
                        total += t.elapsed();
                        assert_applied(&a);
                    }
                    total
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
