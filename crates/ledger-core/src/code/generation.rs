// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The code generation compiled into a package (amendment 0033, D1, D13, D18; `DEC-26` (a);
//! `REQ-RT-20`).
//!
//! Every package is built with its code generation as a constant: a 16-byte **record**, the tag
//! `ALGGEN01` followed by the generation as a little-endian `u64`. The record is one `#[used]`
//! static, exported as the symbol [`SYMBOL`] (`ALEDGENT_CODE_GENERATION`) in its own section,
//! [`SECTION`] (`.aledgent.generation`), so that the optimiser cannot fold it away. It is defined
//! once, by [`code_generation!`](crate::code_generation), which also defines the one function
//! that reads it inside the guest, `code_generation()`: it reads that very static, through
//! [`record_generation`], the layout [`generation_of`] reads. There is no second constant to
//! compare (D18).
//!
//! One reader, [`generation_of`], reads it from a package's bytes: in `check-code` and the
//! packaging step (the build-time check, `T-038-25`), in `Upgrade` staging, and in the execution
//! node's checks at start and before a switch (`T-037-04`, `T-037-14`).
//!
//! - **An ELF.** The record is the one [`SECTION`] holds, which must be exactly one record, and
//!   the symbol [`SYMBOL`], where the symbol table has it, must name that section's record and
//!   nothing else. A linker script may instead place the section's contents inside another
//!   read-only output section (the core's `agents/core/pvm.ld` puts it first in `.rodata`,
//!   because VOS's PVM linker carries only `.rodata*`, `.srodata` and `.data.rel.ro*` into the
//!   program): then the record is the 16 bytes the symbol names. An ELF with neither the section
//!   nor the symbol (stripped) is read as a package format that keeps no sections.
//! - **A package that keeps no sections** (a VOS3 package, whose program is the PVM the ELF was
//!   linked into): the record is the single occurrence of the tag in its bytes.
//!
//! In every case the tag must occur exactly once in the data the package loads (for an ELF, its
//! allocated sections that are not code; otherwise, all of its bytes). No occurrence is a record
//! folded away ([`GenerationError::Missing`]); more than one, a second constant
//! ([`GenerationError::Duplicated`]): a package whose fence reads a record of its own, beside the
//! section's, is refused this way.
//!
//! The tag is held here as a `u64` ([`TAG`]) and never as eight contiguous bytes in this module's
//! data, so that a program that links this reader (a host, a test) does not carry a second
//! occurrence of the tag in its own read-only data.

/// The record's tag, `ALGGEN01`, as a little-endian `u64`.
pub const TAG: u64 = u64::from_le_bytes(*b"ALGGEN01");

/// The record's length: the tag and the generation.
pub const RECORD_LEN: usize = 16;

/// The section that holds the record in a package's ELF.
pub const SECTION: &str = ".aledgent.generation";

/// The exported symbol that is the record.
pub const SYMBOL: &str = "ALEDGENT_CODE_GENERATION";

/// The record of generation `g`: [`TAG`] then `g`, both little-endian.
pub const fn generation_record(g: u64) -> [u8; RECORD_LEN] {
    let tag = TAG.to_le_bytes();
    let n = g.to_le_bytes();
    let mut r = [0u8; RECORD_LEN];
    let mut i = 0;
    while i < 8 {
        r[i] = tag[i];
        r[8 + i] = n[i];
        i += 1;
    }
    r
}

/// The generation a record holds, read through the record's layout and without checking the tag:
/// the guest's own read of its static (the fence's, `T-037-15`). [`generation_of`] checks the tag.
pub fn record_generation(r: &[u8; RECORD_LEN]) -> u64 {
    let mut n = [0u8; 8];
    n.copy_from_slice(&r[8..]);
    u64::from_le_bytes(n)
}

/// Why a package's compiled generation could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationError {
    /// No record: the constant was folded away, or the package was built without it.
    Missing,
    /// The tag occurs this many times: a second constant beside the section's.
    Duplicated(usize),
    /// The section is not exactly one record, or the symbol does not name it.
    NotTheSection,
    /// The bytes are an ELF this reader cannot read.
    Malformed(&'static str),
}

/// Define the package's code generation (module documentation): the record
/// `ALEDGENT_CODE_GENERATION` in the section `.aledgent.generation`, and `code_generation()`, the
/// one function that reads it. Invoke it once, at the root of the guest crate, with the code
/// generation its manifest names.
///
/// ```ignore
/// ledger_core::code_generation!(0);
/// ```
#[macro_export]
macro_rules! code_generation {
    ($generation:expr) => {
        /// The code generation this package is built as (`ledger_core::code::generation`): the
        /// tag `ALGGEN01` and the generation, little-endian, in the section `.aledgent.generation`.
        #[used]
        #[cfg_attr(
            not(target_vendor = "apple"),
            unsafe(link_section = ".aledgent.generation")
        )]
        #[cfg_attr(
            target_vendor = "apple",
            unsafe(link_section = "__DATA,__aledgent_gen")
        )]
        #[unsafe(no_mangle)]
        pub static ALEDGENT_CODE_GENERATION: [u8; $crate::code::RECORD_LEN] =
            $crate::code::generation_record($generation);

        /// The code generation compiled into this package: the record
        /// `ALEDGENT_CODE_GENERATION` itself, read through `ledger_core::code::record_generation`.
        /// The read goes through `black_box`, so the compiler cannot replace it with the
        /// constant: what it returns is what the section holds.
        #[allow(dead_code)]
        pub fn code_generation() -> u64 {
            $crate::code::record_generation(::core::hint::black_box(&ALEDGENT_CODE_GENERATION))
        }
    };
}

/// The code generation compiled into a package (module documentation).
pub fn generation_of(bytes: &[u8]) -> Result<u64, GenerationError> {
    if bytes.starts_with(b"\x7fELF")
        && let Some(found) = elf::record(bytes)?
    {
        let record: [u8; RECORD_LEN] = found
            .record
            .try_into()
            .map_err(|_| GenerationError::NotTheSection)?;
        if read_u64(&record[..8]) != TAG {
            return Err(GenerationError::NotTheSection);
        }
        let n = found
            .loaded
            .iter()
            .map(|range| occurrences(&bytes[range.clone()]))
            .sum();
        match n {
            // The record is not in anything the package loads.
            0 => return Err(GenerationError::Missing),
            1 => {}
            n => return Err(GenerationError::Duplicated(n)),
        }
        return Ok(record_generation(&record));
    }
    match occurrences(bytes) {
        0 => Err(GenerationError::Missing),
        1 => {
            let at = (0..=bytes.len() - 8)
                .find(|&i| read_u64(&bytes[i..i + 8]) == TAG)
                .ok_or(GenerationError::Missing)?;
            let record: [u8; RECORD_LEN] = bytes
                .get(at..at + RECORD_LEN)
                .and_then(|r| r.try_into().ok())
                .ok_or(GenerationError::Missing)?;
            Ok(record_generation(&record))
        }
        n => Err(GenerationError::Duplicated(n)),
    }
}

fn read_u64(b: &[u8]) -> u64 {
    let mut n = [0u8; 8];
    n.copy_from_slice(&b[..8]);
    u64::from_le_bytes(n)
}

/// How many times the tag occurs in `bytes`.
fn occurrences(bytes: &[u8]) -> usize {
    bytes.windows(8).filter(|w| read_u64(w) == TAG).count()
}

/// The little of ELF the reader needs: section headers, the symbol table, nothing else.
mod elf {
    use alloc::vec::Vec;
    use core::ops::Range;

    use super::{GenerationError, RECORD_LEN, SECTION, SYMBOL};

    const SHT_SYMTAB: u32 = 2;
    const SHT_NOBITS: u32 = 8;
    const SHF_ALLOC: u64 = 0x2;
    const SHF_EXECINSTR: u64 = 0x4;

    struct Section {
        name: u32,
        kind: u32,
        flags: u64,
        addr: u64,
        offset: u64,
        size: u64,
        link: u32,
        entsize: u64,
    }

    /// What an ELF says about its record.
    pub(super) struct Found<'a> {
        /// The record's bytes, as the section or the symbol names them.
        pub(super) record: &'a [u8],
        /// The file ranges of the data it loads that is not code.
        pub(super) loaded: Vec<Range<usize>>,
    }

    fn bad(what: &'static str) -> GenerationError {
        GenerationError::Malformed(what)
    }

    struct Reader<'a> {
        b: &'a [u8],
        wide: bool,
    }

    impl Reader<'_> {
        fn n(&self, at: u64, len: usize) -> Result<u64, GenerationError> {
            let at = usize::try_from(at).map_err(|_| bad("an offset"))?;
            let s = self
                .b
                .get(at..at.checked_add(len).ok_or(bad("an offset"))?)
                .ok_or(bad("past the end"))?;
            let mut v = [0u8; 8];
            v[..len].copy_from_slice(s);
            Ok(u64::from_le_bytes(v))
        }
        fn u32(&self, at: u64) -> Result<u32, GenerationError> {
            Ok(self.n(at, 4)? as u32)
        }
        fn u16(&self, at: u64) -> Result<u16, GenerationError> {
            Ok(self.n(at, 2)? as u16)
        }

        fn section(&self, at: u64) -> Result<Section, GenerationError> {
            Ok(if self.wide {
                Section {
                    name: self.u32(at)?,
                    kind: self.u32(at + 4)?,
                    flags: self.n(at + 8, 8)?,
                    addr: self.n(at + 16, 8)?,
                    offset: self.n(at + 24, 8)?,
                    size: self.n(at + 32, 8)?,
                    link: self.u32(at + 40)?,
                    entsize: self.n(at + 56, 8)?,
                }
            } else {
                Section {
                    name: self.u32(at)?,
                    kind: self.u32(at + 4)?,
                    flags: u64::from(self.u32(at + 8)?),
                    addr: u64::from(self.u32(at + 12)?),
                    offset: u64::from(self.u32(at + 16)?),
                    size: u64::from(self.u32(at + 20)?),
                    link: self.u32(at + 24)?,
                    entsize: u64::from(self.u32(at + 36)?),
                }
            })
        }

        fn name(&self, strtab: &Section, off: u32) -> Result<&[u8], GenerationError> {
            let start = strtab
                .offset
                .checked_add(u64::from(off))
                .ok_or(bad("a name"))?;
            let start = usize::try_from(start).map_err(|_| bad("a name"))?;
            let rest = self.b.get(start..).ok_or(bad("a name"))?;
            let end = rest.iter().position(|&c| c == 0).ok_or(bad("a name"))?;
            Ok(&rest[..end])
        }
    }

    fn range(s: &Section, len: usize) -> Result<Range<usize>, GenerationError> {
        let start = usize::try_from(s.offset).map_err(|_| bad("a section"))?;
        let size = usize::try_from(s.size).map_err(|_| bad("a section"))?;
        let end = start.checked_add(size).ok_or(bad("a section"))?;
        if end > len {
            return Err(bad("a section past the end"));
        }
        Ok(start..end)
    }

    /// The record an ELF names, or `None` if it has neither the section nor the symbol.
    pub(super) fn record(b: &[u8]) -> Result<Option<Found<'_>>, GenerationError> {
        let wide = match b.get(4) {
            Some(1) => false,
            Some(2) => true,
            _ => return Err(bad("the class")),
        };
        if b.get(5) != Some(&1) {
            return Err(bad("not little-endian"));
        }
        let r = Reader { b, wide };
        let (shoff, shentsize, shnum, shstrndx) = if wide {
            (r.n(0x28, 8)?, r.u16(0x3a)?, r.u16(0x3c)?, r.u16(0x3e)?)
        } else {
            (
                u64::from(r.u32(0x20)?),
                r.u16(0x2e)?,
                r.u16(0x30)?,
                r.u16(0x32)?,
            )
        };
        let sections = (0..u64::from(shnum))
            .map(|i| r.section(shoff + i * u64::from(shentsize)))
            .collect::<Result<Vec<_>, _>>()?;
        let shstr = sections
            .get(usize::from(shstrndx))
            .ok_or(bad("the section names"))?;
        let mut own = None;
        let mut loaded = Vec::new();
        for s in &sections {
            if s.flags & SHF_ALLOC != 0 && s.flags & SHF_EXECINSTR == 0 && s.kind != SHT_NOBITS {
                loaded.push(range(s, b.len())?);
            }
            if r.name(shstr, s.name)? == SECTION.as_bytes() {
                if own.is_some() {
                    return Err(GenerationError::NotTheSection);
                }
                own = Some(s);
            }
        }
        let mut symbols = Vec::new();
        for s in sections.iter().filter(|s| s.kind == SHT_SYMTAB) {
            let strtab = sections
                .get(s.link as usize)
                .ok_or(bad("a symbol table's names"))?;
            let entsize = if s.entsize == 0 {
                if wide { 24 } else { 16 }
            } else {
                s.entsize
            };
            for i in 0..s.size / entsize {
                let at = s.offset + i * entsize;
                let (value, size, shndx) = if wide {
                    (r.n(at + 8, 8)?, r.n(at + 16, 8)?, r.u16(at + 6)?)
                } else {
                    (
                        u64::from(r.u32(at + 4)?),
                        u64::from(r.u32(at + 8)?),
                        r.u16(at + 14)?,
                    )
                };
                if shndx != 0 && r.name(strtab, r.u32(at)?)? == SYMBOL.as_bytes() {
                    symbols.push((value, size, shndx));
                }
            }
        }
        if symbols.len() > 1 {
            return Err(GenerationError::Duplicated(symbols.len()));
        }
        let symbol = symbols.first().copied();
        let record = match (own, symbol) {
            (None, None) => return Ok(None),
            (Some(s), symbol) => {
                if s.size != RECORD_LEN as u64 {
                    return Err(GenerationError::NotTheSection);
                }
                if let Some((value, size, _)) = symbol
                    && (value != s.addr || size != RECORD_LEN as u64)
                {
                    return Err(GenerationError::NotTheSection);
                }
                &b[range(s, b.len())?]
            }
            (None, Some((value, size, shndx))) => {
                if size != RECORD_LEN as u64 {
                    return Err(GenerationError::NotTheSection);
                }
                let s = sections
                    .get(usize::from(shndx))
                    .ok_or(bad("the symbol's section"))?;
                let within = value
                    .checked_sub(s.addr)
                    .filter(|o| o + RECORD_LEN as u64 <= s.size)
                    .ok_or(GenerationError::NotTheSection)?;
                if s.kind == SHT_NOBITS || s.flags & SHF_EXECINSTR != 0 {
                    return Err(GenerationError::NotTheSection);
                }
                let at = range(s, b.len())?.start + within as usize;
                &b[at..at + RECORD_LEN]
            }
        };
        Ok(Some(Found { record, loaded }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A minimal little-endian ELF64 with these sections (name, flags, address, contents) and,
    /// optionally, a symbol table naming `SYMBOL` at an address.
    pub(crate) fn elf(sections: &[(&str, u64, u64, &[u8])], symbol: Option<(u64, u64)>) -> Vec<u8> {
        let mut names = vec![0u8];
        let mut name_of = |n: &str| {
            let at = names.len() as u32;
            names.extend_from_slice(n.as_bytes());
            names.push(0);
            at
        };
        // (name, kind, flags, addr, data, link, entsize)
        type Header = (u32, u32, u64, u64, Vec<u8>, u32, u64);
        let mut all: Vec<Header> = Vec::new();
        all.push((0, 0, 0, 0, Vec::new(), 0, 0));
        for (n, flags, addr, data) in sections {
            all.push((name_of(n), 1, *flags, *addr, data.to_vec(), 0, 0));
        }
        if let Some((value, size)) = symbol {
            let strtab_name = name_of(".strtab");
            let symtab_name = name_of(".symtab");
            let mut strs = vec![0u8];
            strs.extend_from_slice(SYMBOL.as_bytes());
            strs.push(0);
            let mut syms = vec![0u8; 24];
            let mut s = Vec::new();
            s.extend_from_slice(&1u32.to_le_bytes());
            s.extend_from_slice(&[0x11, 0]);
            s.extend_from_slice(&1u16.to_le_bytes());
            s.extend_from_slice(&value.to_le_bytes());
            s.extend_from_slice(&size.to_le_bytes());
            syms.extend_from_slice(&s);
            let strtab = all.len() as u32;
            all.push((strtab_name, 3, 0, 0, strs, 0, 0));
            all.push((symtab_name, 2, 0, 0, syms, strtab, 24));
        }
        let shstr_name = name_of(".shstrtab");
        let shstrndx = all.len() as u16;
        all.push((shstr_name, 3, 0, 0, names.clone(), 0, 0));
        let mut out = vec![0u8; 64];
        let mut offsets = Vec::new();
        for (_, _, _, _, data, _, _) in &all {
            offsets.push(out.len() as u64);
            out.extend_from_slice(data);
        }
        let shoff = out.len() as u64;
        for (i, (name, kind, flags, addr, data, link, entsize)) in all.iter().enumerate() {
            out.extend_from_slice(&name.to_le_bytes());
            out.extend_from_slice(&kind.to_le_bytes());
            out.extend_from_slice(&flags.to_le_bytes());
            out.extend_from_slice(&addr.to_le_bytes());
            out.extend_from_slice(&offsets[i].to_le_bytes());
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            out.extend_from_slice(&link.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&1u64.to_le_bytes());
            out.extend_from_slice(&entsize.to_le_bytes());
        }
        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = 2;
        out[5] = 1;
        out[0x28..0x30].copy_from_slice(&shoff.to_le_bytes());
        out[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        out[0x3c..0x3e].copy_from_slice(&(all.len() as u16).to_le_bytes());
        out[0x3e..0x40].copy_from_slice(&shstrndx.to_le_bytes());
        out
    }

    const RO: u64 = 0x2;
    const CODE: u64 = 0x2 | 0x4;

    #[test]
    fn d13_the_record_is_the_tag_and_the_generation() {
        let r = generation_record(7);
        assert_eq!(&r[..8], b"ALGGEN01");
        assert_eq!(&r[8..], &7u64.to_le_bytes());
        assert_eq!(record_generation(&r), 7);
    }

    /// D13: the section in an ELF, with the symbol naming it.
    #[test]
    fn d13_an_elf_is_read_from_its_section() {
        let rec = generation_record(3);
        let b = elf(
            &[
                (".text", CODE, 0x1000, &[0u8; 32]),
                (".rodata", RO, 0x2000, b"other read-only data"),
                (SECTION, RO, 0x3000, &rec),
            ],
            Some((0x3000, 16)),
        );
        assert_eq!(generation_of(&b), Ok(3));
        // Without a symbol table (stripped), the section alone.
        let b = elf(&[(SECTION, RO, 0x3000, &rec)], None);
        assert_eq!(generation_of(&b), Ok(3));
    }

    /// D13: a linker script that puts the record first in `.rodata` (the core's `pvm.ld`): the
    /// symbol names it.
    #[test]
    fn d13_an_elf_whose_record_is_inside_rodata_is_read_by_its_symbol() {
        let mut ro = generation_record(5).to_vec();
        ro.extend_from_slice(b"the rest of .rodata");
        let b = elf(&[(".rodata", RO, 0x40000, &ro)], Some((0x40000, 16)));
        assert_eq!(generation_of(&b), Ok(5));
        // A symbol that does not name a record.
        let b = elf(&[(".rodata", RO, 0x40000, &ro)], Some((0x40004, 16)));
        assert_eq!(generation_of(&b), Err(GenerationError::NotTheSection));
    }

    /// T-037-01: a record folded away, or duplicated, fails the check.
    #[test]
    fn t037_01_a_folded_away_or_duplicated_record_is_refused() {
        // No record anywhere.
        assert_eq!(
            generation_of(b"a package with no record"),
            Err(GenerationError::Missing)
        );
        let b = elf(&[(".rodata", RO, 0x2000, b"nothing")], None);
        assert_eq!(generation_of(&b), Err(GenerationError::Missing));
        // Two records in a package format that keeps no sections.
        let mut two = generation_record(1).to_vec();
        two.extend_from_slice(b"..");
        two.extend_from_slice(&generation_record(1));
        assert_eq!(generation_of(&two), Err(GenerationError::Duplicated(2)));
        // The section's record and a second constant in `.rodata`: the fence would read another
        // number than the section's.
        let rec = generation_record(2);
        let mut other = b"x".to_vec();
        other.extend_from_slice(&generation_record(2));
        let b = elf(
            &[(".rodata", RO, 0x2000, &other), (SECTION, RO, 0x3000, &rec)],
            Some((0x3000, 16)),
        );
        assert_eq!(generation_of(&b), Err(GenerationError::Duplicated(2)));
        // The tag in code is not loaded data, and does not count.
        let mut text = vec![0u8; 8];
        text.extend_from_slice(b"ALGGEN01");
        let b = elf(
            &[(".text", CODE, 0x1000, &text), (SECTION, RO, 0x3000, &rec)],
            Some((0x3000, 16)),
        );
        assert_eq!(generation_of(&b), Ok(2));
        // A section of two records, or a symbol elsewhere.
        let mut double = rec.to_vec();
        double.extend_from_slice(&rec);
        let b = elf(&[(SECTION, RO, 0x3000, &double)], None);
        assert_eq!(generation_of(&b), Err(GenerationError::NotTheSection));
        let b = elf(&[(SECTION, RO, 0x3000, &rec)], Some((0x3008, 16)));
        assert_eq!(generation_of(&b), Err(GenerationError::NotTheSection));
        // A section that holds something else.
        let b = elf(&[(SECTION, RO, 0x3000, &[0u8; 16])], None);
        assert_eq!(generation_of(&b), Err(GenerationError::NotTheSection));
    }

    /// D13: a package format that keeps no sections, the single tagged record in its bytes.
    #[test]
    fn d13_a_package_without_sections_is_read_from_its_single_record() {
        let mut pkg = b"VOS3 header ... ro data: ".to_vec();
        pkg.extend_from_slice(&generation_record(9));
        pkg.extend_from_slice(b" ... code");
        assert_eq!(generation_of(&pkg), Ok(9));
        // A tag at the very end, with no generation after it.
        let mut cut = b"....".to_vec();
        cut.extend_from_slice(b"ALGGEN01");
        assert_eq!(generation_of(&cut), Err(GenerationError::Missing));
    }
}
