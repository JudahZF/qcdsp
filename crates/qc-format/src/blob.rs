//! The decrypted `ModuleBlob` (`blobs/core1/<name>_blob.bin`).
//!
//! All integers are little-endian `u32`:
//!
//! ```text
//! group 0: count, count × { section, length, bytes[length] }      contents
//! group 1: count, count × { section, length, u32[length] }        relocations
//! group 2: count, count × { section, 2, start, end }              link ranges (end inclusive)
//! trailer: entry address (code space)
//! ```
//!
//! [`ModuleBlob::parse`] keeps the section order and every relocation word,
//! so [`ModuleBlob::to_bytes`] reproduces stock blobs byte for byte.

use crate::{FormatError, Reader, Result, put_u32, wire_len};

/// A link-time section. The base is the placeholder address ZenUI relocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// Second byte-addressed data region; reported as `pm` in `moduleReport.xml`.
    Data0 = 0,
    /// DM data: constants, tables, function-pointer tables.
    Dm = 1,
    /// SHARC+ VISA code, addressed in 16-bit parcels. Every stock blob has it.
    Code = 2,
    /// ISA-mode code or PM data, addressed in 48-bit words.
    Isa = 3,
}

impl Section {
    pub const ALL: [Section; 4] = [Self::Data0, Self::Dm, Self::Code, Self::Isa];

    pub fn from_id(id: u32) -> Result<Self> {
        Self::ALL
            .get(id as usize)
            .copied()
            .ok_or(FormatError::UnknownSection(id))
    }

    pub const fn id(self) -> u32 {
        self as u32
    }

    /// Placeholder base address of the section's link range.
    pub const fn base(self) -> u32 {
        match self {
            Self::Data0 => 0x6040_0000,
            Self::Dm => 0x6030_0000,
            Self::Code => 0x00F8_0000,
            Self::Isa => 0x007A_AAAB,
        }
    }

    /// Bytes per address unit.
    pub const fn unit_bytes(self) -> u32 {
        match self {
            Self::Data0 | Self::Dm => 1,
            Self::Code => 2,
            Self::Isa => 6,
        }
    }
}

/// Group 0 entry: one section's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionBytes {
    pub section: Section,
    pub bytes: Vec<u8>,
}

/// Group 1 entry: one section's relocation words, kept raw so unknown
/// record layouts survive a round trip. Decode with
/// [`SectionWords::code_relocations`] or [`SectionWords::data_relocations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionWords {
    pub section: Section,
    pub words: Vec<u32>,
}

/// Group 2 entry: a section's address range. `end` is inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkRange {
    pub section: Section,
    pub start: u32,
    pub end: u32,
}

impl LinkRange {
    /// The range stock blobs use for `len` content bytes:
    /// `start = base`, `end = base + len / unit - 1`.
    pub fn for_contents(section: Section, len: usize) -> Result<Self> {
        let unit = section.unit_bytes() as usize;
        if len == 0 || !len.is_multiple_of(unit) {
            return Err(FormatError::InvalidBlob(format!(
                "{section:?} holds {len} bytes; need a non-zero multiple of {unit}"
            )));
        }
        let units = wire_len(len / unit, "section")?;
        let end = section.base().checked_add(units - 1).ok_or_else(|| {
            FormatError::InvalidBlob(format!("{section:?} overflows the address space"))
        })?;
        Ok(Self {
            section,
            start: section.base(),
            end,
        })
    }

    pub fn contains(&self, address: u32) -> bool {
        (self.start..=self.end).contains(&address)
    }
}

/// Code relocation (sections 2 and 3), 4 words. `offset` is in section
/// units. Known kinds: 11 = 24-bit absolute CALL/JUMP target, 8 = 32-bit
/// absolute address in `ureg = imm32` (stock targets every section),
/// 7 = return address pushed by `cjump`, 4 = DM data address (inferred).
/// 0, 5 and 9 are not decoded yet; most kind 0 targets lie outside the blob
/// (kernel-relative calls).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeRelocation {
    pub kind: u32,
    pub offset: u32,
    pub target_base: u32,
    pub target_offset: u32,
}

impl CodeRelocation {
    pub const WORDS: usize = 4;

    pub fn words(&self) -> [u32; 4] {
        [self.kind, self.offset, self.target_base, self.target_offset]
    }
}

/// Data relocation (section 1), 3 words: an absolute pointer stored at
/// `byte_offset`, for example a function-pointer table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRelocation {
    pub byte_offset: u32,
    pub target_base: u32,
    pub target_offset: u32,
}

impl DataRelocation {
    pub const WORDS: usize = 3;

    pub fn words(&self) -> [u32; 3] {
        [self.byte_offset, self.target_base, self.target_offset]
    }
}

impl SectionWords {
    pub fn code_relocations(&self) -> Result<Vec<CodeRelocation>> {
        self.records(&[Section::Code, Section::Isa], CodeRelocation::WORDS)
            .map(|chunks| {
                chunks
                    .map(|w| CodeRelocation {
                        kind: w[0],
                        offset: w[1],
                        target_base: w[2],
                        target_offset: w[3],
                    })
                    .collect()
            })
    }

    pub fn data_relocations(&self) -> Result<Vec<DataRelocation>> {
        self.records(&[Section::Dm], DataRelocation::WORDS)
            .map(|chunks| {
                chunks
                    .map(|w| DataRelocation {
                        byte_offset: w[0],
                        target_base: w[1],
                        target_offset: w[2],
                    })
                    .collect()
            })
    }

    fn records(
        &self,
        sections: &[Section],
        width: usize,
    ) -> Result<std::slice::ChunksExact<'_, u32>> {
        if !sections.contains(&self.section) || !self.words.len().is_multiple_of(width) {
            return Err(FormatError::InvalidBlob(format!(
                "{:?} relocations ({} words) are not {width}-word records",
                self.section,
                self.words.len()
            )));
        }
        Ok(self.words.chunks_exact(width))
    }
}

/// A decrypted block file. Field order is wire order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleBlob {
    pub contents: Vec<SectionBytes>,
    pub relocations: Vec<SectionWords>,
    pub ranges: Vec<LinkRange>,
    /// Absolute entry address; stock entries lie in the code range.
    pub entry: u32,
}

impl ModuleBlob {
    /// Parses the wire layout. Checks structure only (counts, unique known
    /// sections, exact length); [`ModuleBlob::validate`] checks meaning.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);

        let contents = read_group(&mut r, |r, section, len| {
            Ok(SectionBytes {
                section,
                bytes: r.bytes(len as usize)?.to_vec(),
            })
        })?;
        let relocations = read_group(&mut r, |r, section, len| {
            Ok(SectionWords {
                section,
                words: r.words(len)?,
            })
        })?;
        let ranges = read_group(&mut r, |r, section, len| {
            if len != 2 {
                return Err(FormatError::Malformed {
                    offset: r.pos - 4,
                    reason: "link range length is not 2",
                });
            }
            Ok(LinkRange {
                section,
                start: r.u32()?,
                end: r.u32()?,
            })
        })?;
        let entry = r.u32()?;
        r.finish()?;

        let blob = Self {
            contents,
            relocations,
            ranges,
            entry,
        };
        blob.check_structure()?;
        Ok(blob)
    }

    /// Writes the wire layout. Fails on anything [`ModuleBlob::parse`] would reject.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.check_structure()?;
        let mut out = Vec::new();
        put_u32(&mut out, self.contents.len() as u32);
        for c in &self.contents {
            put_u32(&mut out, c.section.id());
            put_u32(&mut out, wire_len(c.bytes.len(), "section")?);
            out.extend_from_slice(&c.bytes);
        }
        put_u32(&mut out, self.relocations.len() as u32);
        for rel in &self.relocations {
            put_u32(&mut out, rel.section.id());
            put_u32(&mut out, wire_len(rel.words.len(), "relocation group")?);
            for &word in &rel.words {
                put_u32(&mut out, word);
            }
        }
        put_u32(&mut out, self.ranges.len() as u32);
        for range in &self.ranges {
            for word in [range.section.id(), 2, range.start, range.end] {
                put_u32(&mut out, word);
            }
        }
        put_u32(&mut out, self.entry);
        Ok(out)
    }

    /// Checks the invariants every stock blob meets:
    /// - each content section has a link range `[base, base + len/unit - 1]` and vice versa;
    /// - relocation groups refer to present sections and decode as whole records;
    /// - the entry lies in the code range.
    pub fn validate(&self) -> Result<()> {
        self.check_structure()?;
        if self.ranges.len() != self.contents.len() {
            return Err(FormatError::InvalidBlob(
                "link ranges do not match content sections".into(),
            ));
        }
        for c in &self.contents {
            let expected = LinkRange::for_contents(c.section, c.bytes.len())?;
            if self.range(c.section) != Some(&expected) {
                return Err(FormatError::InvalidBlob(format!(
                    "{:?} range is not {:#x}..={:#x}",
                    c.section, expected.start, expected.end
                )));
            }
        }
        for rel in &self.relocations {
            if self.content(rel.section).is_none() {
                return Err(FormatError::InvalidBlob(format!(
                    "relocations for absent {:?}",
                    rel.section
                )));
            }
            match rel.section {
                Section::Code | Section::Isa => rel.code_relocations().map(drop)?,
                Section::Dm => {
                    let size = self.content(Section::Dm).expect("present section").len();
                    for relocation in rel.data_relocations()? {
                        let offset = relocation.byte_offset as usize;
                        if !offset.is_multiple_of(4)
                            || offset.checked_add(4).is_none_or(|end| end > size)
                        {
                            return Err(FormatError::InvalidBlob(format!(
                                "DM relocation pointer at {offset} is unaligned or outside {size} bytes"
                            )));
                        }
                    }
                }
                Section::Data0 => {
                    return Err(FormatError::InvalidBlob("relocations for Data0".into()));
                }
            }
        }
        match self.range(Section::Code) {
            Some(code) if code.contains(self.entry) => Ok(()),
            _ => Err(FormatError::InvalidBlob(format!(
                "entry {:#x} is outside the code range",
                self.entry
            ))),
        }
    }

    pub fn content(&self, section: Section) -> Option<&[u8]> {
        self.contents
            .iter()
            .find(|c| c.section == section)
            .map(|c| c.bytes.as_slice())
    }

    /// Mutable content bytes. Changing the length breaks the link range;
    /// run [`ModuleBlob::validate`] afterwards.
    pub fn content_mut(&mut self, section: Section) -> Option<&mut Vec<u8>> {
        self.contents
            .iter_mut()
            .find(|c| c.section == section)
            .map(|c| &mut c.bytes)
    }

    pub fn relocation_words(&self, section: Section) -> Option<&[u32]> {
        self.relocations
            .iter()
            .find(|r| r.section == section)
            .map(|r| r.words.as_slice())
    }

    pub fn range(&self, section: Section) -> Option<&LinkRange> {
        self.ranges.iter().find(|r| r.section == section)
    }

    /// Wire-level rules: at most one entry per section in each group, at
    /// least one content section, and lengths that fit a `u32`.
    fn check_structure(&self) -> Result<()> {
        if self.contents.is_empty() {
            return Err(FormatError::InvalidBlob("no content sections".into()));
        }
        check_unique(self.contents.iter().map(|c| c.section), "content")?;
        check_unique(self.relocations.iter().map(|r| r.section), "relocation")?;
        check_unique(self.ranges.iter().map(|r| r.section), "link range")
    }
}

/// Reads one group: a count, then that many `{section, length, payload}` entries.
fn read_group<T>(
    r: &mut Reader<'_>,
    mut entry: impl FnMut(&mut Reader<'_>, Section, u32) -> Result<T>,
) -> Result<Vec<T>> {
    let at = r.pos;
    let count = r.u32()?;
    if count > Section::ALL.len() as u32 {
        return Err(FormatError::Malformed {
            offset: at,
            reason: "more group entries than sections",
        });
    }
    (0..count)
        .map(|_| {
            let section = Section::from_id(r.u32()?)?;
            let len = r.u32()?;
            entry(r, section, len)
        })
        .collect()
}

fn check_unique(sections: impl Iterator<Item = Section>, group: &str) -> Result<()> {
    let mut seen = [false; 4];
    for section in sections {
        if std::mem::replace(&mut seen[section.id() as usize], true) {
            return Err(FormatError::InvalidBlob(format!(
                "duplicate {group} entry for {section:?}"
            )));
        }
    }
    Ok(())
}

/// Checks a blob name for use in file names and diff records:
/// 1–80 ASCII letters, digits or `_` (every stock name fits).
pub fn validate_blob_name(name: &str) -> Result<()> {
    let ok = (1..=80).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(FormatError::InvalidName(name.into()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A small blob with every group populated, shaped like a stock blob.
    pub(crate) fn sample() -> ModuleBlob {
        let dm = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let code: Vec<u8> = (0..24).collect();
        ModuleBlob {
            ranges: vec![
                LinkRange::for_contents(Section::Dm, dm.len()).unwrap(),
                LinkRange::for_contents(Section::Code, code.len()).unwrap(),
            ],
            contents: vec![
                SectionBytes {
                    section: Section::Dm,
                    bytes: dm,
                },
                SectionBytes {
                    section: Section::Code,
                    bytes: code,
                },
            ],
            relocations: vec![
                SectionWords {
                    section: Section::Dm,
                    words: vec![4, Section::Code.base(), 3],
                },
                SectionWords {
                    section: Section::Code,
                    words: vec![7, 1, Section::Code.base(), 3, 99, 2, 3, 4],
                },
            ],
            entry: Section::Code.base() + 2,
        }
    }

    #[test]
    fn round_trips_and_keeps_unknown_relocation_kinds() {
        let blob = sample();
        let bytes = blob.to_bytes().unwrap();
        let parsed = ModuleBlob::parse(&bytes).unwrap();
        assert_eq!(parsed, blob);
        parsed.validate().unwrap();
        let code = parsed.relocations[1].code_relocations().unwrap();
        assert_eq!(code[1].kind, 99);
        assert_eq!(
            parsed.relocations[0].data_relocations().unwrap()[0].byte_offset,
            4
        );
    }

    #[test]
    fn keeps_section_order() {
        let mut blob = sample();
        blob.contents.reverse();
        blob.ranges.reverse();
        let parsed = ModuleBlob::parse(&blob.to_bytes().unwrap()).unwrap();
        assert_eq!(parsed.contents[0].section, Section::Code);
        assert_eq!(parsed.ranges[0].section, Section::Code);
    }

    #[test]
    fn inclusive_ranges() {
        let isa = LinkRange::for_contents(Section::Isa, 12).unwrap();
        assert_eq!((isa.start, isa.end), (0x7A_AAAB, 0x7A_AAAC));
        assert!(LinkRange::for_contents(Section::Code, 3).is_err());
        assert!(LinkRange::for_contents(Section::Dm, 0).is_err());
    }

    #[test]
    fn rejects_malformed_wire_data() {
        let bytes = sample().to_bytes().unwrap();
        for len in 0..bytes.len() {
            assert!(
                ModuleBlob::parse(&bytes[..len]).is_err(),
                "truncated to {len}"
            );
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(matches!(
            ModuleBlob::parse(&trailing),
            Err(FormatError::Trailing { .. })
        ));

        let mut unknown = bytes.clone();
        unknown[4..8].copy_from_slice(&4u32.to_le_bytes());
        assert!(matches!(
            ModuleBlob::parse(&unknown),
            Err(FormatError::UnknownSection(4))
        ));

        let mut huge = bytes.clone();
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            ModuleBlob::parse(&huge),
            Err(FormatError::Truncated { .. })
        ));

        let mut duplicate = sample();
        duplicate.contents[0].section = Section::Code;
        assert!(duplicate.to_bytes().is_err());

        let empty = [0u8; 16];
        assert!(ModuleBlob::parse(&empty).is_err());
    }

    #[test]
    fn validate_catches_semantic_errors() {
        let mut blob = sample();
        blob.entry = Section::Code.base() + 12;
        assert!(blob.validate().is_err());

        let mut blob = sample();
        blob.ranges[1].end += 1;
        assert!(blob.validate().is_err());

        let mut blob = sample();
        blob.relocations[1].words.pop();
        assert!(blob.validate().is_err());

        for invalid_offset in [1, 6, 8, u32::MAX] {
            let mut blob = sample();
            blob.relocations[0].words[0] = invalid_offset;
            assert!(blob.validate().is_err(), "DM offset {invalid_offset}");
        }
    }

    #[test]
    fn blob_names() {
        validate_blob_name("adaptive_gate2").unwrap();
        for bad in ["", "a/b", "a b", "naïve", &"x".repeat(81)] {
            assert!(validate_blob_name(bad).is_err(), "{bad:?}");
        }
    }
}
