// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Partitions of the book (`REQ-RT-9`, amendment 0012, `DEC-22`, `T-031-09`).
//!
//! The book's state may be held in **partitions** of accounts, each committing on its own. Every
//! ledger account belongs to exactly one, fixed when it is opened by a function of its identifier:
//! the first two bytes of `blake2b-256("aledgent/partition/v1" ‖ id)`. There are 2¹⁶ partitions.
//!
//! The partition is written into the **top sixteen bits** of every key that is ordered against an
//! account: its kernel accounts' ids ([`crate::account::kernel_account_id_in`]), the transfer id
//! of every posting whose first debit is one of them ([`crate::posting`]), and the service keys
//! of the kinds keyed by a ledger account (`ledger_core::delta::service_key`). A sparse Merkle
//! tree reads key bits most significant first, so everything of one partition is one subtree at
//! depth sixteen, and a contiguous range of partitions is a set of whole subtrees. A shard that
//! holds a range of partitions holds those subtrees; splitting a shard moves subtrees and renames
//! nothing (`REQ-RT-9`: no identifier changes when the book is re-partitioned).
//!
//! Trade-off, stated plainly: sixteen bits of every such key are no longer a hash, so a key keeps
//! 112 bits of its digest. The partition is a function of public identifiers and carries no
//! personal data (`INV-17`).

use cipher_clerk::crypto::blake2b_256;

/// Domain of the partition function. Changing it moves every account.
const PARTITION_DOMAIN: &[u8] = b"aledgent/partition/v1";

/// How many bits of a key the partition takes.
pub const PARTITION_BITS: u32 = 16;

/// How many partitions there are.
pub const PARTITIONS: u32 = 1 << PARTITION_BITS;

/// One partition of the book.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Partition(pub u16);

impl Partition {
    /// The partition of the ledger account (or other subject) whose 16-byte identifier is `id`.
    pub fn of(id: &[u8; 16]) -> Self {
        let d = blake2b_256(PARTITION_DOMAIN, &[id]);
        Partition(u16::from_be_bytes([d[0], d[1]]))
    }

    /// The partition a placed key belongs to: its top sixteen bits.
    pub const fn of_key(key: &[u8; 16]) -> Self {
        Partition(u16::from_be_bytes([key[0], key[1]]))
    }

    /// `digest`'s first fourteen bytes, behind this partition: a key placed in it.
    pub fn place(self, digest: &[u8]) -> [u8; 16] {
        let mut key = [0u8; 16];
        key[..2].copy_from_slice(&self.0.to_be_bytes());
        key[2..].copy_from_slice(&digest[..14]);
        key
    }

    /// The two bytes of the partition, most significant first.
    pub const fn to_be_bytes(self) -> [u8; 2] {
        self.0.to_be_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn req_rt_9_a_partition_is_a_fixed_function_of_the_identifier() {
        let a = Partition::of(&[7; 16]);
        assert_eq!(a, Partition::of(&[7; 16]));
        assert_ne!(a, Partition::of(&[8; 16]));
        let key = a.place(&[0xAB; 32]);
        assert_eq!(Partition::of_key(&key), a);
        assert_eq!(&key[2..], &[0xAB; 14]);
    }

    #[test]
    fn req_rt_9_partitions_spread_accounts() {
        // 4 096 identifiers over 16 buckets of the top four bits: none is empty or holds more than
        // twice its share.
        let mut buckets = [0u32; 16];
        for n in 0u32..4096 {
            let mut id = [0u8; 16];
            id[..4].copy_from_slice(&n.to_be_bytes());
            buckets[(Partition::of(&id).0 >> 12) as usize] += 1;
        }
        assert!(buckets.iter().all(|b| *b > 0 && *b < 512), "{buckets:?}");
    }
}
