// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The canonical encoding of the export (`F-014` T-014-01).
//!
//! Every value is written in one way only: integers big-endian and fixed width, byte strings and
//! sequences length-prefixed, an optional value behind a presence byte, an enum behind a stable
//! tag, a map or a set in key order. Decoding refuses anything that is not that one way: an
//! unknown tag, a presence byte other than 0 or 1, a map or set whose keys are not strictly
//! increasing, bytes left over. So two exports of the same state are the same bytes, and a
//! decoded value re-encodes to the bytes it came from.
//!
//! [`Canonical`] is implemented here for the building blocks, and beside each record type for the
//! records (the `canon` modules of `state`, `lifecycle::store` and `settlement::store`, and
//! the private `export::records` module).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::num::NonZeroU64;

/// Why bytes did not decode. Names what was being read; carries no data from the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodeError(pub &'static str);

/// A canonical writer.
#[derive(Clone, Debug, Default)]
pub struct Enc(Vec<u8>);

impl Enc {
    /// An empty writer.
    pub fn new() -> Enc {
        Enc(Vec::new())
    }

    /// Raw bytes, no prefix: for fixed-width fields.
    pub fn raw(&mut self, b: &[u8]) -> &mut Enc {
        self.0.extend_from_slice(b);
        self
    }

    /// A length-prefixed byte string.
    pub fn bytes(&mut self, b: &[u8]) -> &mut Enc {
        self.len(b.len());
        self.raw(b)
    }

    /// A length or a count, as a `u32`.
    pub fn len(&mut self, n: usize) -> &mut Enc {
        let n = u32::try_from(n).expect("an exported sequence never holds 2^32 items");
        self.raw(&n.to_be_bytes())
    }

    /// A one-byte tag.
    pub fn tag(&mut self, t: u8) -> &mut Enc {
        self.raw(&[t])
    }

    /// Any canonical value.
    pub fn put<T: Canonical>(&mut self, v: &T) -> &mut Enc {
        v.encode(self);
        self
    }

    /// The bytes written.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    /// The bytes written so far.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A canonical reader over one byte string.
#[derive(Clone, Debug)]
pub struct Dec<'a> {
    buf: &'a [u8],
}

impl<'a> Dec<'a> {
    /// A reader over `buf`.
    pub fn new(buf: &'a [u8]) -> Dec<'a> {
        Dec { buf }
    }

    /// `n` raw bytes.
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.buf.len() < n {
            return Err(DecodeError("truncated"));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    /// A fixed-width array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.raw(N)?);
        Ok(out)
    }

    /// A length or a count.
    pub fn len(&mut self) -> Result<usize, DecodeError> {
        let n = u32::from_be_bytes(self.array()?);
        usize::try_from(n).map_err(|_| DecodeError("length"))
    }

    /// A length-prefixed byte string.
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.len()?;
        self.raw(n)
    }

    /// A one-byte tag.
    pub fn tag(&mut self) -> Result<u8, DecodeError> {
        Ok(self.raw(1)?[0])
    }

    /// Any canonical value.
    pub fn get<T: Canonical>(&mut self) -> Result<T, DecodeError> {
        T::decode(self)
    }

    /// Whether everything was read.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Fail unless everything was read.
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(DecodeError("trailing bytes"))
        }
    }
}

/// A value with one canonical encoding.
pub trait Canonical: Sized {
    /// Write the value.
    fn encode(&self, e: &mut Enc);
    /// Read a value, refusing any encoding but the canonical one.
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError>;
}

/// The canonical bytes of `v`.
pub fn to_bytes<T: Canonical>(v: &T) -> Vec<u8> {
    let mut e = Enc::new();
    v.encode(&mut e);
    e.into_bytes()
}

/// Decode `bytes` as exactly one `T`.
pub fn from_bytes<T: Canonical>(bytes: &[u8]) -> Result<T, DecodeError> {
    let mut d = Dec::new(bytes);
    let v = T::decode(&mut d)?;
    d.finish()?;
    Ok(v)
}

macro_rules! int {
    ($($t:ty),+) => {$(
        impl Canonical for $t {
            fn encode(&self, e: &mut Enc) {
                e.raw(&self.to_be_bytes());
            }
            fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
                Ok(<$t>::from_be_bytes(d.array()?))
            }
        }
    )+};
}

int!(u8, u16, u32, u64, u128, i128);

impl Canonical for bool {
    fn encode(&self, e: &mut Enc) {
        e.tag(u8::from(*self));
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError("bool")),
        }
    }
}

impl Canonical for NonZeroU64 {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.get());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        NonZeroU64::new(d.get()?).ok_or(DecodeError("non-zero"))
    }
}

impl<const N: usize> Canonical for [u8; N] {
    fn encode(&self, e: &mut Enc) {
        e.raw(self);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        d.array()
    }
}

impl<T: Canonical> Canonical for Option<T> {
    fn encode(&self, e: &mut Enc) {
        match self {
            None => {
                e.tag(0);
            }
            Some(v) => {
                e.tag(1).put(v);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            0 => Ok(None),
            1 => Ok(Some(d.get()?)),
            _ => Err(DecodeError("option")),
        }
    }
}

impl<T: Canonical> Canonical for Vec<T> {
    fn encode(&self, e: &mut Enc) {
        e.len(self.len());
        for v in self {
            e.put(v);
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let n = d.len()?;
        // Never trust a count to size an allocation: grow as items actually decode.
        let mut out = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            out.push(d.get()?);
        }
        Ok(out)
    }
}

impl<K: Canonical + Ord, V: Canonical> Canonical for BTreeMap<K, V> {
    fn encode(&self, e: &mut Enc) {
        e.len(self.len());
        for (k, v) in self {
            e.put(k).put(v);
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let n = d.len()?;
        let mut out = BTreeMap::new();
        for _ in 0..n {
            let k: K = d.get()?;
            if out.last_key_value().is_some_and(|(last, _)| *last >= k) {
                return Err(DecodeError("map keys out of order"));
            }
            let v: V = d.get()?;
            out.insert(k, v);
        }
        Ok(out)
    }
}

impl<T: Canonical + Ord> Canonical for BTreeSet<T> {
    fn encode(&self, e: &mut Enc) {
        e.len(self.len());
        for v in self {
            e.put(v);
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let n = d.len()?;
        let mut out = BTreeSet::new();
        for _ in 0..n {
            let v: T = d.get()?;
            if out.last().is_some_and(|last| *last >= v) {
                return Err(DecodeError("set members out of order"));
            }
            out.insert(v);
        }
        Ok(out)
    }
}

impl<A: Canonical, B: Canonical> Canonical for (A, B) {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.0).put(&self.1);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok((d.get()?, d.get()?))
    }
}

impl<A: Canonical, B: Canonical, C: Canonical> Canonical for (A, B, C) {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.0).put(&self.1).put(&self.2);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok((d.get()?, d.get()?, d.get()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_and_reencode_to_the_same_bytes() {
        let mut m: BTreeMap<u64, Option<Vec<u8>>> = BTreeMap::new();
        m.insert(3, None);
        m.insert(1, Some(alloc::vec![1, 2]));
        let b = to_bytes(&m);
        let back: BTreeMap<u64, Option<Vec<u8>>> = from_bytes(&b).unwrap();
        assert_eq!(back, m);
        assert_eq!(to_bytes(&back), b);
    }

    #[test]
    fn non_canonical_encodings_are_refused() {
        // Keys out of order.
        let mut e = Enc::new();
        e.len(2).put(&2u64).put(&true).put(&1u64).put(&true);
        assert!(from_bytes::<BTreeMap<u64, bool>>(e.as_bytes()).is_err());
        // A repeated set member.
        let mut e = Enc::new();
        e.len(2).put(&1u8).put(&1u8);
        assert!(from_bytes::<BTreeSet<u8>>(e.as_bytes()).is_err());
        // A presence byte of 2, a bool of 2, trailing bytes, truncation.
        assert!(from_bytes::<Option<u8>>(&[2, 0]).is_err());
        assert!(from_bytes::<bool>(&[2]).is_err());
        assert!(from_bytes::<u8>(&[1, 2]).is_err());
        assert!(from_bytes::<u64>(&[1, 2]).is_err());
        assert!(from_bytes::<NonZeroU64>(&[0; 8]).is_err());
    }
}
