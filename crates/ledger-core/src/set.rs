// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! [`NonEmptySet`]: a set that cannot be empty, so "no restriction" is never spelled as an empty
//! set (`REQ-AU-2`).

use alloc::collections::BTreeSet;

/// An ordered set with at least one member.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NonEmptySet<T: Ord>(BTreeSet<T>);

impl<T: Ord> NonEmptySet<T> {
    /// The set, or `None` if it is empty.
    pub fn new(set: BTreeSet<T>) -> Option<Self> {
        (!set.is_empty()).then_some(Self(set))
    }

    /// A set of one.
    pub fn one(member: T) -> Self {
        let mut s = BTreeSet::new();
        s.insert(member);
        Self(s)
    }

    /// Whether `member` is in the set.
    pub fn contains(&self, member: &T) -> bool {
        self.0.contains(member)
    }

    /// The members.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.0.iter()
    }

    /// How many members.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always `false`.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The members as a plain set.
    pub fn as_set(&self) -> &BTreeSet<T> {
        &self.0
    }
}

impl<T: Ord + Clone> NonEmptySet<T> {
    /// The members in both sets, or `None` if there are none.
    pub fn intersection(&self, other: &NonEmptySet<T>) -> Option<NonEmptySet<T>> {
        NonEmptySet::new(self.0.intersection(&other.0).cloned().collect())
    }

    /// Whether every member of `self` is in `other`.
    pub fn is_subset(&self, other: &NonEmptySet<T>) -> bool {
        self.0.is_subset(&other.0)
    }
}
