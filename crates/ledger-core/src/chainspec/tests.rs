// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The seed's format (`F-036` T-036-01): determinism, the digest, chunking, the file.

use super::testing::{Synthetic, synthetic};
use super::*;

fn g() -> OperationId {
    OperationId::from_ref(OperationRef([3; 16]))
}

fn small() -> Synthetic {
    synthetic(
        g(),
        40,
        12,
        ChunkLimits {
            max_records: 16,
            max_bytes: 4 * 1024,
            opening_batch: 4,
        },
    )
}

#[test]
fn t_036_01_the_same_records_give_the_same_bytes_and_digest() {
    let a = small();
    let b = small();
    assert_eq!(a.seed.to_bytes(), b.seed.to_bytes());
    assert_eq!(a.seed.digest(), b.seed.digest());
    assert_eq!(a.seed.commitment(), b.seed.commitment());
    // The file reads back to the same seed.
    let back = ChainSpec::from_bytes(&a.seed.to_bytes()).unwrap();
    assert_eq!(back, a.seed);
    assert_eq!(back.digest(), a.seed.digest());
}

#[test]
fn t_036_01_any_change_changes_the_digest() {
    let a = small();
    // One amount one minor unit more: another opening set, another seed.
    let mut records = a.seed.records().to_vec();
    let i = records
        .iter()
        .position(|r| matches!(r, SpecRecord::Opening(_)))
        .unwrap();
    if let SpecRecord::Opening(row) = &mut records[i] {
        row.amount = crate::asset::Minor::new(row.amount.get() + 1);
    }
    let h = a.seed.header();
    let changed = ChainSpec::build(
        h.genesis_op,
        h.at,
        h.assets.clone(),
        records,
        a.seed.reconciliation().cloned(),
        h.history,
        a.limits,
    )
    .unwrap();
    assert_ne!(changed.digest(), a.seed.digest());
    // The same records cut differently: another digest too.
    let h = a.seed.header();
    let recut = ChainSpec::build(
        h.genesis_op,
        h.at,
        h.assets.clone(),
        a.seed.records().to_vec(),
        a.seed.reconciliation().cloned(),
        h.history,
        ChunkLimits {
            max_records: 7,
            ..a.limits
        },
    )
    .unwrap();
    assert_ne!(recut.digest(), a.seed.digest());
    // Another book's genesis: another digest, other identifiers.
    let other = synthetic(
        OperationId::from_ref(OperationRef([4; 16])),
        40,
        12,
        a.limits,
    );
    assert_ne!(other.seed.digest(), a.seed.digest());
    assert_ne!(identity_of(&g(), 0), other.identities[0]);
}

#[test]
fn t_036_01_every_chunk_proves_against_the_commitment_and_no_other_does() {
    let a = small();
    let c = a.seed.commitment();
    let bytes = a.seed.chunk_bytes();
    let paths = a.seed.paths();
    assert_eq!(bytes.len() as u32, c.chunks);
    for (i, (b, p)) in bytes.iter().zip(&paths).enumerate() {
        assert!(chunk_in_spec(&c, i as u32, b, p), "chunk {i}");
        // At another index, or one byte changed, it does not.
        assert!(!chunk_in_spec(&c, (i as u32 + 1) % c.chunks, b, p));
        let mut t = b.clone();
        let last = t.len() - 1;
        t[last] ^= 1;
        assert!(!chunk_in_spec(&c, i as u32, &t, p));
    }
    // The header verifies against the commitment and carries the history's commit.
    let h = verify_header(&c, &bytes[0], &paths[0]).unwrap();
    assert_eq!(&h, a.seed.header());
    assert!(verify_header(&c, &bytes[1], &paths[1]).is_err());
}

#[test]
fn t_036_01_chunks_hold_their_limits_and_keep_opening_batches_whole() {
    let a = small();
    let mut total = 0usize;
    let mut posted = 0u64;
    for i in 1..a.seed.chunk_count() {
        let c = a.seed.chunk(i).unwrap();
        let ChunkBody::Records(rs) = &c.body else {
            panic!("chunk {i} is not records")
        };
        assert!(rs.len() as u32 <= a.limits.max_records);
        assert!(c.to_bytes().len() as u32 <= a.limits.max_bytes);
        total += rs.len();
        posted += rs
            .iter()
            .filter(|r| matches!(r, SpecRecord::Opening(row) if row.amount.get() != 0))
            .count() as u64;
        // A chunk that ends inside the openings ends at a batch boundary.
        let next_is_opening = a
            .seed
            .chunk(i + 1)
            .and_then(|n| match n.body {
                ChunkBody::Records(r) => r.first().cloned(),
                ChunkBody::Header(_) => None,
            })
            .is_some_and(|r| matches!(r, SpecRecord::Opening(_)));
        if matches!(rs.last(), Some(SpecRecord::Opening(_))) && next_is_opening {
            assert_eq!(posted % u64::from(a.limits.opening_batch), 0, "chunk {i}");
        }
    }
    assert_eq!(total, a.seed.records().len());
}

#[test]
fn t_036_01_out_of_order_or_inconsistent_seeds_are_refused() {
    let a = small();
    let h = a.seed.header();
    let build = |records: Vec<SpecRecord>| {
        ChainSpec::build(
            h.genesis_op,
            h.at,
            h.assets.clone(),
            records,
            a.seed.reconciliation().cloned(),
            h.history,
            a.limits,
        )
    };
    // Sections out of order.
    let mut r = a.seed.records().to_vec();
    let n = r.len();
    r.swap(0, n - 1);
    assert_eq!(build(r).unwrap_err(), SpecError::Order);
    // Two opening rows for one pair.
    let mut r = a.seed.records().to_vec();
    let i = r
        .iter()
        .position(|x| matches!(x, SpecRecord::Opening(_)))
        .unwrap();
    r.insert(i, r[i].clone());
    assert_eq!(build(r).unwrap_err(), SpecError::Duplicate);
    // An issuance account.
    let mut r = a.seed.records().to_vec();
    let i = r
        .iter()
        .position(|x| matches!(x, SpecRecord::Account { .. }))
        .unwrap();
    if let SpecRecord::Account { kind, .. } = &mut r[i] {
        *kind = AccountKind::Issuance;
    }
    assert_eq!(build(r).unwrap_err(), SpecError::IssuanceAccount);
    // A file whose reconciliation is not the committed one.
    let mut bytes = a.seed.to_bytes();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    assert!(ChainSpec::from_bytes(&bytes).is_err());
}

#[test]
fn t_036_02_the_gate_runs_f014s_reconciliation() {
    let a = small();
    assert_eq!(a.seed.gate(&a.required), Ok(()));
    // A source that says one minor unit more: divergent, nothing loads.
    let mut rec = a.seed.reconciliation().cloned().unwrap();
    rec.sources[0].rows[0].amount = "999999.99".into();
    rec.sources[0] =
        crate::opening::SourceExtract::new(rec.sources[0].source, rec.sources[0].rows.clone());
    let h = a.seed.header();
    let bad = ChainSpec::build(
        h.genesis_op,
        h.at,
        h.assets.clone(),
        a.seed.records().to_vec(),
        Some(rec),
        h.history,
        a.limits,
    )
    .unwrap();
    assert!(matches!(
        bad.gate(&a.required),
        Err(SpecError::Divergent(_))
    ));
}

#[test]
fn a0023_the_header_names_the_genesis_code() {
    let a = small();
    let manifest = crate::code::sample(b"core.vos", b"runtime.pvm", b"core-host");
    let code = GenesisCode {
        manifest: manifest.clone(),
        construction: 2,
    };
    let named = a.seed.clone().with_code(code.clone()).unwrap();
    // The code is in the header, so in the digest: another code is another chainspec.
    assert_ne!(named.digest(), a.seed.digest());
    let back = ChainSpec::from_bytes(&named.to_bytes()).unwrap();
    assert_eq!(back.header().code.as_ref(), Some(&code));
    assert_eq!(
        back.header().code.as_ref().unwrap().version(),
        manifest.version()
    );
    let mut other = manifest.clone();
    other.artefacts[0].profile.opt_level = "s".into();
    let renamed = a
        .seed
        .clone()
        .with_code(GenesisCode {
            manifest: other,
            construction: 2,
        })
        .unwrap();
    assert_ne!(renamed.digest(), named.digest());
    // A construction the manifest does not read is refused.
    assert_eq!(
        a.seed
            .clone()
            .with_code(GenesisCode {
                manifest,
                construction: 1,
            })
            .unwrap_err(),
        SpecError::Code
    );
    // The settlement base is committed too.
    let based = a.seed.clone().with_settlement(SettlementBase {
        instance: 1,
        last_anchor: 41,
        next_seq: 7,
    });
    assert_ne!(based.digest(), a.seed.digest());
    assert_eq!(ChainSpec::from_bytes(&based.to_bytes()).unwrap(), based);
}

#[test]
fn f036_the_header_commits_the_identifier_maps_digest() {
    let a = small();
    let with = a.seed.clone().with_ids([0xD1; 32]);
    assert_ne!(with.digest(), a.seed.digest());
    let back = ChainSpec::from_bytes(&with.to_bytes()).unwrap();
    assert_eq!(back.header().ids_sha256, Some([0xD1; 32]));
    // Another file is another chainspec.
    assert_ne!(a.seed.clone().with_ids([0xD2; 32]).digest(), with.digest());
}

#[test]
fn au_8_au_19_the_header_commits_the_merge_map_and_the_exclusion_file() {
    let a = small();
    let merged = a.seed.clone().with_merges([0xA8; 32]);
    let excluded = a.seed.clone().with_excluded([0xA8; 32]);
    // Each digest is its own field: the same bytes in either slot are two chainspecs.
    assert_ne!(merged.digest(), a.seed.digest());
    assert_ne!(excluded.digest(), a.seed.digest());
    assert_ne!(merged.digest(), excluded.digest());
    let both = merged.clone().with_excluded([0xE1; 32]);
    let back = ChainSpec::from_bytes(&both.to_bytes()).unwrap();
    assert_eq!(back.header().merges_sha256, Some([0xA8; 32]));
    assert_eq!(back.header().excluded_sha256, Some([0xE1; 32]));
    assert_eq!(back, both);
    // Another file is another chainspec.
    assert_ne!(
        merged.clone().with_merges([0xA9; 32]).digest(),
        merged.digest()
    );
}

#[test]
fn chain_balance_authority_the_header_commits_the_reconciliation_journal() {
    let a = small();
    let journal = a.seed.clone().with_reconcile([0xC0; 32]);
    assert_ne!(journal.digest(), a.seed.digest());
    // Its own field: the same bytes as the merge map's are another chainspec.
    assert_ne!(
        journal.digest(),
        a.seed.clone().with_merges([0xC0; 32]).digest()
    );
    let back = ChainSpec::from_bytes(&journal.to_bytes()).unwrap();
    assert_eq!(back.header().reconcile_sha256, Some([0xC0; 32]));
    assert_eq!(back, journal);
    assert_ne!(
        journal.clone().with_reconcile([0xC1; 32]).digest(),
        journal.digest()
    );
}
