// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The anchor store's rules, whoever writes (`F-017`): numbers in order, one in flight, a later
//! entry sequence, the commitment its fields produce, and status only forward.

use super::*;
use crate::settlement::Coordinate;
use alloc::vec;

fn cp(number: u64, entry_sequence: u64) -> Checkpoint {
    let kernel_root = [number as u8; 32];
    Checkpoint {
        number,
        instance: 1,
        entry_sequence,
        kernel_root,
        commitment: commitment(
            super::construction(),
            1,
            number,
            entry_sequence,
            &kernel_root,
        ),
        created_at: 0,
        status: CheckpointStatus::Pending,
        coordinate: None,
        venue: None,
        height: None,
    }
}

fn at(seq: u64) -> Coordinate {
    Coordinate {
        instance: 1,
        seq,
        digest: [9; 32],
    }
}

#[test]
fn inv_19_the_store_takes_checkpoints_in_order_one_in_flight_over_later_entries() {
    let mut s = AnchorStore::new();
    assert_eq!(
        s.apply(&AnchorWrite::Create(cp(2, 5))).err(),
        Some(AnchorRefusal::OutOfOrder)
    );
    s.apply(&AnchorWrite::Create(cp(1, 5))).unwrap();
    // One in flight.
    assert_eq!(
        s.apply(&AnchorWrite::Create(cp(2, 9))).err(),
        Some(AnchorRefusal::InFlight)
    );
    s.apply(&AnchorWrite::Acknowledge(1)).unwrap();
    s.apply(&AnchorWrite::Final {
        number: 1,
        coordinate: at(0),
        venue: None,
        height: 7,
    })
    .unwrap();
    // A later entry sequence only.
    assert_eq!(
        s.apply(&AnchorWrite::Create(cp(2, 5))).err(),
        Some(AnchorRefusal::NotAfter)
    );
    // Only the commitment its own fields produce.
    let mut forged = cp(2, 9);
    forged.commitment = [0; 32];
    assert_eq!(
        s.apply(&AnchorWrite::Create(forged)).err(),
        Some(AnchorRefusal::Commitment)
    );
    let mut born_final = cp(2, 9);
    born_final.status = CheckpointStatus::Final;
    assert_eq!(
        s.apply(&AnchorWrite::Create(born_final)).err(),
        Some(AnchorRefusal::NotNew)
    );
    s.apply(&AnchorWrite::Create(cp(2, 9))).unwrap();
    assert_eq!(s.in_flight().map(|c| c.number), Some(2));
    assert_eq!(s.latest_final().map(|c| c.number), Some(1));
}

#[test]
fn req_pr_8_status_moves_forward_and_final_absorbs() {
    let mut s = AnchorStore::new();
    s.apply(&AnchorWrite::Create(cp(1, 3))).unwrap();
    assert_eq!(
        s.apply(&AnchorWrite::Reemit(1)).err(),
        Some(AnchorRefusal::Status)
    );
    s.apply(&AnchorWrite::Acknowledge(1)).unwrap();
    let u = s.apply(&AnchorWrite::Reemit(1)).unwrap();
    assert_eq!(s.checkpoint(1).unwrap().status, CheckpointStatus::Pending);
    s.undo(u);
    assert_eq!(s.checkpoint(1).unwrap().status, CheckpointStatus::Submitted);
    s.apply(&AnchorWrite::Final {
        number: 1,
        coordinate: at(4),
        venue: None,
        height: 7,
    })
    .unwrap();
    for w in [
        AnchorWrite::Acknowledge(1),
        AnchorWrite::Reemit(1),
        AnchorWrite::Final {
            number: 1,
            coordinate: at(5),
            venue: None,
            height: 7,
        },
    ] {
        assert_eq!(s.apply(&w).err(), Some(AnchorRefusal::Status));
    }
    assert_eq!(s.checkpoint(1).unwrap().coordinate, Some(at(4)));
    assert_eq!(
        s.apply(&AnchorWrite::Acknowledge(7)).err(),
        Some(AnchorRefusal::Missing)
    );
}

#[test]
fn ac_j3_1_a_comparison_names_only_final_checkpoints_and_every_final_one_is_owed_one() {
    let mut s = AnchorStore::new();
    s.apply(&AnchorWrite::Create(cp(1, 3))).unwrap();
    let c = Comparison {
        cut: 2,
        height: 7,
        at: 1,
        anchors: vec![1],
        requested: None,
        rows: Vec::new(),
    };
    assert_eq!(
        s.apply(&AnchorWrite::Compare(c.clone())).err(),
        Some(AnchorRefusal::NotFinal)
    );
    assert!(s.uncompared().is_empty());
    s.apply(&AnchorWrite::Final {
        number: 1,
        coordinate: at(1),
        venue: None,
        height: 7,
    })
    .unwrap();
    assert_eq!(s.uncompared(), vec![1]);
    s.apply(&AnchorWrite::Compare(c)).unwrap();
    assert!(s.uncompared().is_empty());
}

#[test]
fn a_store_restores_through_its_own_rules() {
    let mut s = AnchorStore::new();
    s.apply(&AnchorWrite::Create(cp(1, 3))).unwrap();
    s.apply(&AnchorWrite::Final {
        number: 1,
        coordinate: at(1),
        venue: None,
        height: 7,
    })
    .unwrap();
    s.apply(&AnchorWrite::Create(cp(2, 8))).unwrap();
    s.apply(&AnchorWrite::Acknowledge(2)).unwrap();
    let back = AnchorStore::restore(
        s.checkpoints().copied().collect(),
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(back, s);
    // A forged record is refused on the way in, as on any write.
    let mut forged = cp(1, 3);
    forged.kernel_root = [0xEE; 32];
    assert!(
        AnchorStore::restore(
            vec![forged],
            Vec::new(),
            Vec::new(),
            None,
            Vec::new(),
            Vec::new()
        )
        .is_err()
    );
}

#[test]
fn t_027_04_a_checkpoint_records_the_venue_it_became_final_in() {
    use crate::export::canon::{Canonical, Dec, Enc};
    let contract = Venue::Contract { account: [7; 32] };
    let mut s = AnchorStore::new();
    // A new checkpoint has no venue yet, and cannot be created with one.
    let mut early = cp(1, 3);
    early.venue = Some(contract);
    assert_eq!(
        s.apply(&AnchorWrite::Create(early)).err(),
        Some(AnchorRefusal::NotNew)
    );
    s.apply(&AnchorWrite::Create(cp(1, 3))).unwrap();
    assert_eq!(
        s.checkpoint(1).unwrap().anchored_in(),
        Venue::Pallet { instance: 1 }
    );
    let undo = s
        .apply(&AnchorWrite::Final {
            number: 1,
            coordinate: at(1),
            venue: Some(contract),
            height: 7,
        })
        .unwrap();
    assert_eq!(s.checkpoint(1).unwrap().venue, Some(contract));
    assert_eq!(s.checkpoint(1).unwrap().anchored_in(), contract);
    // Undone with the rest of the write.
    s.undo(undo);
    assert_eq!(s.checkpoint(1).unwrap().venue, None);
    s.apply(&AnchorWrite::Final {
        number: 1,
        coordinate: at(1),
        venue: Some(contract),
        height: 7,
    })
    .unwrap();
    // Exported and restored with it.
    let c = *s.checkpoint(1).unwrap();
    let mut e = Enc::new();
    c.encode(&mut e);
    let bytes = e.into_bytes();
    let mut d = Dec::new(&bytes);
    assert_eq!(Checkpoint::decode(&mut d).unwrap(), c);
    let back = AnchorStore::restore(
        s.checkpoints().copied().collect(),
        Vec::new(),
        Vec::new(),
        None,
        Vec::new(),
        Vec::new(),
    )
    .unwrap();
    assert_eq!(back, s);
}

#[test]
fn ac_i1_1_the_anchor_instruction_is_a_number_and_32_bytes() {
    let c = cp(3, 11);
    let i = c.instruction();
    assert_eq!((i.number(), i.commitment()), (3, c.commitment));
    assert_eq!(core::mem::size_of::<AnchorInstruction>(), 40);
}

/// `AC-M2.4`: the latest final checkpoint is the last one created, or the one before it while the
/// last is in flight; a final checkpoint keeps the height of its record's block.
#[test]
fn ac_m2_4_latest_final_is_the_last_or_the_one_before_it() {
    let mut s = AnchorStore::new();
    assert_eq!(s.latest_final(), None);
    s.apply(&AnchorWrite::Create(cp(1, 3))).unwrap();
    assert_eq!(s.latest_final(), None);
    s.apply(&AnchorWrite::Final {
        number: 1,
        coordinate: at(0),
        venue: None,
        height: 42,
    })
    .unwrap();
    assert_eq!(
        s.latest_final().map(|c| (c.number, c.height)),
        Some((1, Some(42)))
    );
    s.apply(&AnchorWrite::Create(cp(2, 5))).unwrap();
    assert_eq!(s.latest_final().map(|c| c.number), Some(1));
    s.apply(&AnchorWrite::Acknowledge(2)).unwrap();
    assert_eq!(s.latest_final().map(|c| c.number), Some(1));
    s.apply(&AnchorWrite::Final {
        number: 2,
        coordinate: at(1),
        venue: None,
        height: 43,
    })
    .unwrap();
    assert_eq!(s.latest_final().map(|c| c.number), Some(2));
    // A new checkpoint carrying a height is refused: only finality records one.
    let mut c = cp(3, 7);
    c.height = Some(1);
    assert_eq!(
        s.apply(&AnchorWrite::Create(c)).err(),
        Some(AnchorRefusal::NotNew)
    );
}
