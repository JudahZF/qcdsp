//! Links caller-supplied sections into a Quad Cortex block.
//!
//! Input: section bytes, explicit relocation records, the entry offset, and
//! references to kernel services together with a kernel address map. Output:
//! the core 1 [`ModuleBlob`], one [`DiffRecord`] each for cores 2–4, and the
//! exact static section sizes.
//!
//! The linker checks; it does not guess. It rejects:
//! - sections whose length is not a whole number of address units;
//! - an entry outside section 2 (every stock entry lies there);
//! - relocation kinds whose meaning is not known (only 4, 7, 8 and 11 are
//!   accepted; stock 0, 5 and 9 are not), offsets outside their section,
//!   targets outside a present section's inclusive link range, and targets
//!   in a section that kind never uses in stock blobs;
//! - kernel references that are not one of the two VISA forms seen in stock
//!   per-core diffs, that overlap each other or a relocation, whose address
//!   is not in the map, or whose code bytes hold a different address.
//!
//! It does not know the kernel ABI. It never writes callback tables, shims
//! or kernel calls of its own; the caller's code must do that, and a block
//! that links is not thereby known to run.

use std::collections::BTreeMap;

use qc_format::FormatError;
use qc_format::blob::{
    CodeRelocation, DataRelocation, LinkRange, ModuleBlob, Section, SectionBytes, SectionWords,
    validate_blob_name,
};
use qc_format::diff::{DiffRecord, Run, SectionRuns};

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error("section 2 (code) is required")]
    MissingCode,
    #[error("entry parcel {entry} is outside the {parcels}-parcel code section")]
    Entry { entry: u32, parcels: u32 },
    #[error("relocation {index} in {section:?}: {reason}")]
    Relocation {
        section: Section,
        index: usize,
        reason: String,
    },
    #[error("kernel reference at parcel {parcel}: {reason}")]
    KernelRef { parcel: u32, reason: String },
    #[error("kernel map: {0}")]
    KernelMap(String),
}

pub type Result<T> = std::result::Result<T, LinkError>;

/// Kernel addresses on core 1 and their counterparts on cores 2, 3 and 4.
/// Build it from the user's own device data; it is never shipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KernelMap {
    entries: BTreeMap<u32, [u32; 3]>,
}

impl KernelMap {
    /// Adds one mapping. Re-adding the same mapping is allowed; a different
    /// mapping for the same core 1 address is an error.
    pub fn insert(&mut self, core1: u32, others: [u32; 3]) -> Result<()> {
        match self.entries.entry(core1) {
            std::collections::btree_map::Entry::Occupied(entry) if *entry.get() != others => {
                Err(LinkError::KernelMap(format!(
                    "{core1:#x} maps to both {:x?} and {others:x?}",
                    entry.get()
                )))
            }
            std::collections::btree_map::Entry::Occupied(_) => Ok(()),
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(others);
                Ok(())
            }
        }
    }

    pub fn get(&self, core1: u32) -> Option<[u32; 3]> {
        self.entries.get(&core1).copied()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Mappings in ascending core 1 address order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, [u32; 3])> + '_ {
        self.entries.iter().map(|(&core1, &others)| (core1, others))
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Adds the kernel calls of a stock blob, read from the user's own data: every
    /// [`KernelForm::Call24`] site in section 2 that has no relocation and whose address field
    /// a per-core record changes. `diffs` are the blob's records for cores 2, 3 and 4. A core
    /// whose record leaves the field alone keeps the core 1 address. Sites are found by
    /// parcel scan, so a misaligned match is possible in principle; a conflicting mapping
    /// is an error, not an overwrite.
    pub fn add_stock_calls(&mut self, core1: &ModuleBlob, diffs: &[DiffRecord; 3]) -> Result<()> {
        let code = core1.content(Section::Code).ok_or(LinkError::MissingCode)?;
        let relocated: Vec<u32> = core1
            .relocations
            .iter()
            .filter(|group| group.section == Section::Code)
            .map(|group| group.code_relocations())
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .map(|r| r.offset)
            .collect();
        let others = diffs
            .iter()
            .map(|diff| diff.apply(core1))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let others: Vec<&[u8]> = others
            .iter()
            .map(|blob| blob.content(Section::Code).ok_or(LinkError::MissingCode))
            .collect::<Result<_>>()?;
        let parcel = |code: &[u8], p: usize| u16::from_le_bytes([code[2 * p], code[2 * p + 1]]);
        let address = |code: &[u8], p: usize| {
            (u32::from(parcel(code, p + 1)) << 16) | u32::from(parcel(code, p + 2))
        };
        for p in 0..(code.len() / 2).saturating_sub(2) {
            if parcel(code, p) != 0x1804
                || parcel(code, p + 1) >> 8 != 0
                || relocated.contains(&(p as u32))
                || others.iter().any(|other| parcel(other, p) != 0x1804)
            {
                continue;
            }
            let core1_address = address(code, p);
            let mapped: [u32; 3] = std::array::from_fn(|core| address(others[core], p));
            if mapped.iter().any(|&other| other != core1_address) {
                self.insert(core1_address, mapped)?;
            }
        }
        Ok(())
    }

    /// Reads the research map format: objects `calls` and `data`, each
    /// `{"0x<core1>": {"core2": "0x…", "core3": "0x…", "core4": "0x…"}}`.
    /// Other top-level keys (such as `note`) are ignored.
    pub fn from_json(text: &str) -> Result<Self> {
        let root: MapDocument =
            serde_json::from_str(text).map_err(|error| LinkError::KernelMap(error.to_string()))?;
        let hex = |value: &str| {
            value
                .strip_prefix("0x")
                .filter(|digits| {
                    !digits.is_empty() && digits.bytes().all(|digit| digit.is_ascii_hexdigit())
                })
                .and_then(|digits| u32::from_str_radix(digits, 16).ok())
                .ok_or_else(|| LinkError::KernelMap(format!("{value} is not a 0x-prefixed u32")))
        };
        let mut map = Self::default();
        for entries in [root.calls, root.data] {
            for (core1, cores) in entries {
                map.insert(
                    hex(&core1)?,
                    [hex(&cores.core2)?, hex(&cores.core3)?, hex(&cores.core4)?],
                )?;
            }
        }
        Ok(map)
    }
}

#[derive(serde::Deserialize)]
struct MapDocument {
    #[serde(default, deserialize_with = "unique_addresses")]
    calls: BTreeMap<String, CoreAddresses>,
    #[serde(default, deserialize_with = "unique_addresses")]
    data: BTreeMap<String, CoreAddresses>,
}

#[derive(serde::Deserialize)]
struct CoreAddresses {
    core2: String,
    core3: String,
    core4: String,
}

// A Value or ordinary map would silently discard repeated JSON address keys.
fn unique_addresses<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, CoreAddresses>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Addresses;
    impl<'de> serde::de::Visitor<'de> for Addresses {
        type Value = BTreeMap<String, CoreAddresses>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("an object with unique kernel address keys")
        }

        fn visit_map<M>(self, mut access: M) -> std::result::Result<Self::Value, M::Error>
        where
            M: serde::de::MapAccess<'de>,
        {
            let mut entries = BTreeMap::new();
            while let Some((key, value)) = access.next_entry::<String, CoreAddresses>()? {
                if entries.insert(key.clone(), value).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate kernel address {key}"
                    )));
                }
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_map(Addresses)
}

/// Instruction forms that carry an absolute kernel address in section 2.
/// Both appear in stock per-core diffs; each spans three 16-bit parcels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelForm {
    /// `CALL addr24`: parcels `0x1804`, `addr >> 16` (high byte 0), `addr & 0xFFFF`.
    Call24,
    /// `Rn = imm32` (n = 0–15): parcels `0x0F0n`, `addr >> 16`, `addr & 0xFFFF`.
    Imm32,
}

/// A kernel address used by the code. The linker writes the core 1 address
/// into the instruction and emits the other cores' addresses as diff runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelRef {
    /// Parcel offset of the instruction's first parcel in section 2.
    pub parcel: u32,
    pub form: KernelForm,
    pub core1_address: u32,
}

/// Everything needed to build a block. An empty section is left out.
#[derive(Debug, Clone, Default)]
pub struct LinkInput {
    /// Blob name: 1–80 ASCII letters, digits or `_`.
    pub name: String,
    /// Section 0 bytes.
    pub data0: Vec<u8>,
    /// Section 1 (DM) bytes.
    pub dm: Vec<u8>,
    /// Section 2 VISA code: 16-bit parcels, each little-endian. Required.
    pub code: Vec<u8>,
    /// Section 3: 48-bit words, each little-endian (6 bytes).
    pub isa: Vec<u8>,
    pub dm_relocations: Vec<DataRelocation>,
    pub code_relocations: Vec<CodeRelocation>,
    pub isa_relocations: Vec<CodeRelocation>,
    /// Entry point as a parcel offset into section 2.
    pub entry_parcel: u32,
    pub kernel_refs: Vec<KernelRef>,
}

/// Exact section sizes in bytes, named after the `moduleReport.xml` fields
/// they feed. The report adds pads (`dm_pad`, `sw_pad`, `nw_pad`) and the
/// run-time heaps, which the caller must supply; stock reports are often
/// stale, so do not expect every stock entry to match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticResources {
    /// Section 0.
    pub pm: u32,
    /// Section 1.
    pub dm: u32,
    /// Section 2.
    pub sw: u32,
    /// Section 3.
    pub nw: u32,
}

impl StaticResources {
    pub fn of(blob: &ModuleBlob) -> Self {
        let len = |s| blob.content(s).map_or(0, |b| b.len() as u32);
        Self {
            pm: len(Section::Data0),
            dm: len(Section::Dm),
            sw: len(Section::Code),
            nw: len(Section::Isa),
        }
    }
}

/// The linker output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedBlock {
    /// The core 1 blob, ready to encrypt.
    pub blob: ModuleBlob,
    /// Diff records for cores 2, 3 and 4, named like the blob.
    pub diffs: [DiffRecord; 3],
    pub resources: StaticResources,
}

/// Lays out the sections in stock order (ascending section id), checks every
/// relocation and kernel reference, and builds the per-core diff records.
pub fn link(input: &LinkInput, kernel: &KernelMap) -> Result<LinkedBlock> {
    validate_blob_name(&input.name)?;
    if input.code.is_empty() {
        return Err(LinkError::MissingCode);
    }

    let mut contents = Vec::new();
    let mut ranges = Vec::new();
    for (section, bytes) in [
        (Section::Data0, &input.data0),
        (Section::Dm, &input.dm),
        (Section::Code, &input.code),
        (Section::Isa, &input.isa),
    ] {
        if !bytes.is_empty() {
            ranges.push(LinkRange::for_contents(section, bytes.len())?);
            contents.push(SectionBytes {
                section,
                bytes: bytes.clone(),
            });
        }
    }

    let parcels = (input.code.len() / 2) as u32;
    if input.entry_parcel >= parcels {
        return Err(LinkError::Entry {
            entry: input.entry_parcel,
            parcels,
        });
    }

    check_data_relocations(&input.dm_relocations, &ranges)?;
    check_code_relocations(Section::Code, &input.code_relocations, &ranges)?;
    check_code_relocations(Section::Isa, &input.isa_relocations, &ranges)?;

    let mut relocations = Vec::new();
    if !input.dm_relocations.is_empty() {
        relocations.push(SectionWords {
            section: Section::Dm,
            words: input
                .dm_relocations
                .iter()
                .flat_map(|r| r.words())
                .collect(),
        });
    }
    for (section, records) in [
        (Section::Code, &input.code_relocations),
        (Section::Isa, &input.isa_relocations),
    ] {
        if !records.is_empty() {
            relocations.push(SectionWords {
                section,
                words: records.iter().flat_map(|r| r.words()).collect(),
            });
        }
    }

    let mut refs = input.kernel_refs.clone();
    refs.sort_by_key(|r| r.parcel);
    let code = contents
        .iter_mut()
        .find(|c| c.section == Section::Code)
        .map(|c| &mut c.bytes)
        .ok_or(LinkError::MissingCode)?;
    let per_core = place_kernel_refs(code, &refs, &input.code_relocations, kernel)?;

    let blob = ModuleBlob {
        contents,
        relocations,
        ranges,
        entry: Section::Code.base() + input.entry_parcel,
    };
    blob.validate()?;

    fn empty<T>(section: Section) -> SectionRuns<T> {
        SectionRuns {
            section,
            runs: Vec::new(),
        }
    }
    let diffs = per_core.map(|code_runs| DiffRecord {
        name: input.name.clone(),
        contents: blob
            .contents
            .iter()
            .map(|c| {
                if c.section == Section::Code {
                    SectionRuns {
                        section: Section::Code,
                        runs: code_runs.clone(),
                    }
                } else {
                    empty(c.section)
                }
            })
            .collect(),
        relocations: blob.relocations.iter().map(|r| empty(r.section)).collect(),
        ranges: blob.ranges.iter().map(|r| empty(r.section)).collect(),
    });
    for diff in &diffs {
        diff.apply(&blob)?;
    }

    Ok(LinkedBlock {
        resources: StaticResources::of(&blob),
        blob,
        diffs,
    })
}

/// Units in a present section's link range, or `None` when absent.
fn units(ranges: &[LinkRange], section: Section) -> Option<u32> {
    ranges
        .iter()
        .find(|r| r.section == section)
        .map(|r| r.end - r.start + 1)
}

/// Checks that `base + offset` lies in a present section; returns that section.
fn target(ranges: &[LinkRange], base: u32, offset: u32) -> std::result::Result<Section, String> {
    let range = ranges
        .iter()
        .find(|r| r.start == base)
        .ok_or_else(|| format!("target base {base:#x} is not a present section's base"))?;
    if offset > range.end - range.start {
        return Err(format!(
            "target offset {offset:#x} is outside {:?}",
            range.section
        ));
    }
    Ok(range.section)
}

fn check_code_relocations(
    section: Section,
    records: &[CodeRelocation],
    ranges: &[LinkRange],
) -> Result<()> {
    let fail = |index, reason: String| LinkError::Relocation {
        section,
        index,
        reason,
    };
    let size = match (units(ranges, section), records.is_empty()) {
        (_, true) => return Ok(()),
        (None, false) => return Err(fail(0, "section is absent".into())),
        (Some(size), false) => size,
    };
    for (index, r) in records.iter().enumerate() {
        if r.offset >= size {
            return Err(fail(
                index,
                format!("offset {} is outside the section", r.offset),
            ));
        }
        // Allowed targets follow the stock corpus (kind 7 only ever targets section 2).
        let wanted: &[Section] = match r.kind {
            7 => &[Section::Code],
            11 => &[Section::Code, Section::Isa],
            8 => &Section::ALL,
            4 => &[Section::Dm],
            kind => return Err(fail(index, format!("kind {kind} has no verified meaning"))),
        };
        let to = target(ranges, r.target_base, r.target_offset).map_err(|e| fail(index, e))?;
        if !wanted.contains(&to) {
            return Err(fail(index, format!("kind {} cannot target {to:?}", r.kind)));
        }
    }
    Ok(())
}

fn check_data_relocations(records: &[DataRelocation], ranges: &[LinkRange]) -> Result<()> {
    let fail = |index, reason: String| LinkError::Relocation {
        section: Section::Dm,
        index,
        reason,
    };
    let size = units(ranges, Section::Dm);
    for (index, r) in records.iter().enumerate() {
        let size = size.ok_or_else(|| fail(index, "section is absent".into()))?;
        if r.byte_offset % 4 != 0 || r.byte_offset.checked_add(4).is_none_or(|end| end > size) {
            return Err(fail(
                index,
                format!(
                    "pointer at byte {} is unaligned or does not fit",
                    r.byte_offset
                ),
            ));
        }
        target(ranges, r.target_base, r.target_offset).map_err(|e| fail(index, e))?;
    }
    Ok(())
}

/// Writes each core 1 address into the code and returns, per other core,
/// the byte runs that replace it with that core's address.
fn place_kernel_refs(
    code: &mut [u8],
    refs: &[KernelRef],
    relocations: &[CodeRelocation],
    kernel: &KernelMap,
) -> Result<[Vec<Run<u8>>; 3]> {
    let mut per_core: [Vec<Run<u8>>; 3] = Default::default();
    let parcels = (code.len() / 2) as u32;
    let mut previous_end = 0;
    for (i, r) in refs.iter().enumerate() {
        let fail = |reason: String| LinkError::KernelRef {
            parcel: r.parcel,
            reason,
        };
        let end = r
            .parcel
            .checked_add(3)
            .filter(|&end| end <= parcels)
            .ok_or_else(|| fail("instruction runs past the code".into()))?;
        if i > 0 && r.parcel < previous_end {
            return Err(fail("overlaps the previous kernel reference".into()));
        }
        previous_end = end;
        if relocations
            .iter()
            .any(|rel| (r.parcel..end).contains(&rel.offset))
        {
            return Err(fail("a relocation patches the same instruction".into()));
        }
        let others = kernel
            .get(r.core1_address)
            .ok_or_else(|| fail(format!("{:#x} is not in the kernel map", r.core1_address)))?;

        let at = r.parcel as usize * 2;
        let parcel = |k: usize| u16::from_le_bytes([code[at + 2 * k], code[at + 2 * k + 1]]);
        let (opcode, high, low) = (parcel(0), parcel(1), parcel(2));
        let opcode_ok = match r.form {
            KernelForm::Call24 => opcode == 0x1804,
            KernelForm::Imm32 => opcode & 0xFFF0 == 0x0F00,
        };
        if !opcode_ok {
            return Err(fail(format!(
                "opcode parcel {opcode:#06x} is not {:?}",
                r.form
            )));
        }
        let field = |address: u32| -> Result<[u8; 4]> {
            if r.form == KernelForm::Call24 && address > 0xFF_FFFF {
                return Err(fail(format!("{address:#x} does not fit 24 bits")));
            }
            let [a, b] = ((address >> 16) as u16).to_le_bytes();
            let [c, d] = (address as u16).to_le_bytes();
            Ok([a, b, c, d])
        };
        let current = (u32::from(high) << 16) | u32::from(low);
        if current != 0 && current != r.core1_address {
            return Err(fail(format!(
                "code holds {current:#x}, not {:#x}",
                r.core1_address
            )));
        }
        code[at + 2..at + 6].copy_from_slice(&field(r.core1_address)?);
        for (runs, address) in per_core.iter_mut().zip(others) {
            let bytes = field(address)?;
            if address != r.core1_address {
                runs.push(Run {
                    offset: (at + 2) as u32,
                    values: bytes.to_vec(),
                });
            }
        }
    }
    Ok(per_core)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALL: u32 = 0x16_FFC0;
    const PTR: u32 = 0x16_1ABB;

    fn map() -> KernelMap {
        KernelMap::from_json(
            r#"{"note": "synthetic", "calls": {"0x16ffc0": {"core2": "0x170688", "core3": "0x16ffc0", "core4": "0x170001"}},
                "data": {"0x161abb": {"core2": "0x161cf1", "core3": "0x161103", "core4": "0x160b15"}}}"#,
        )
        .unwrap()
    }

    fn parcels(words: &[u16]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// Code: nop, CALL <placeholder>, R4 = <core1 address>, nop, nop.
    fn input() -> LinkInput {
        LinkInput {
            name: "qcdsp_test".into(),
            dm: vec![0; 8],
            code: parcels(&[0x0001, 0x1804, 0, 0, 0x0F04, 0x0016, 0x1ABB, 0x0001, 0x0001]),
            dm_relocations: vec![DataRelocation {
                byte_offset: 4,
                target_base: Section::Code.base(),
                target_offset: 7,
            }],
            code_relocations: vec![CodeRelocation {
                kind: 4,
                offset: 8,
                target_base: Section::Dm.base(),
                target_offset: 0,
            }],
            entry_parcel: 0,
            kernel_refs: vec![
                KernelRef {
                    parcel: 4,
                    form: KernelForm::Imm32,
                    core1_address: PTR,
                },
                KernelRef {
                    parcel: 1,
                    form: KernelForm::Call24,
                    core1_address: CALL,
                },
            ],
            ..LinkInput::default()
        }
    }

    #[test]
    fn links_blob_and_per_core_diffs() {
        let linked = link(&input(), &map()).unwrap();
        let blob = &linked.blob;
        assert_eq!(blob.entry, Section::Code.base());
        assert_eq!(
            blob.content(Section::Code).unwrap()[2..8],
            parcels(&[0x1804, 0x0016, 0xFFC0])[..]
        );
        assert_eq!(ModuleBlob::parse(&blob.to_bytes().unwrap()).unwrap(), *blob);
        assert_eq!(
            linked.resources,
            StaticResources {
                pm: 0,
                dm: 8,
                sw: 18,
                nw: 0
            }
        );

        let core2 = linked.diffs[0].apply(blob).unwrap();
        let code2 = core2.content(Section::Code).unwrap();
        assert_eq!(code2[2..8], parcels(&[0x1804, 0x0017, 0x0688])[..]);
        assert_eq!(code2[8..14], parcels(&[0x0F04, 0x0016, 0x1CF1])[..]);

        // Core 3 shares the CALL address, so only the pointer changes.
        assert_eq!(linked.diffs[1].contents[1].runs.len(), 1);
        // Every section appears in every group, like stock records.
        assert_eq!(linked.diffs[0].contents.len(), 2);
        assert_eq!(linked.diffs[0].relocations.len(), 2);
        assert_eq!(linked.diffs[0].ranges.len(), 2);
    }

    /// A linked block is a stock-shaped blob with per-core records, so harvesting it gives back
    /// the CALL mapping. The Rn = imm32 pointer is data, not a call, and is not harvested.
    #[test]
    fn stock_calls_round_trip_through_per_core_records() {
        let linked = link(&input(), &map()).unwrap();
        let mut harvested = KernelMap::default();
        harvested
            .add_stock_calls(&linked.blob, &linked.diffs)
            .unwrap();
        assert_eq!(harvested.len(), 1);
        assert_eq!(harvested.get(CALL), map().get(CALL));

        let mut conflicting = KernelMap::default();
        conflicting.insert(CALL, [CALL + 1; 3]).unwrap();
        assert!(matches!(
            conflicting.add_stock_calls(&linked.blob, &linked.diffs),
            Err(LinkError::KernelMap(_))
        ));
    }

    #[test]
    fn rejects_bad_layout_and_entry() {
        let kernel = map();
        let mut i = input();
        i.entry_parcel = 9;
        assert!(matches!(link(&i, &kernel), Err(LinkError::Entry { .. })));

        let mut i = input();
        i.code.push(0);
        assert!(link(&i, &kernel).is_err(), "odd code length");

        let mut i = input();
        i.isa = vec![0; 7];
        assert!(link(&i, &kernel).is_err(), "partial 48-bit word");

        let mut i = input();
        i.code.clear();
        assert!(matches!(link(&i, &kernel), Err(LinkError::MissingCode)));

        let mut i = input();
        i.name = "bad/name".into();
        assert!(link(&i, &kernel).is_err());
    }

    #[test]
    fn rejects_bad_relocations() {
        let kernel = map();
        let cases: [fn(&mut LinkInput); 7] = [
            |i| i.code_relocations[0].kind = 0,
            |i| i.code_relocations[0].offset = 9,
            |i| i.code_relocations[0].target_offset = 8,
            |i| i.code_relocations[0].target_base = Section::Isa.base(),
            |i| i.code_relocations[0].target_base = Section::Code.base(),
            |i| i.dm_relocations[0].byte_offset = 2,
            |i| {
                i.isa_relocations = vec![CodeRelocation {
                    kind: 11,
                    offset: 0,
                    target_base: Section::Code.base(),
                    target_offset: 0,
                }]
            },
        ];
        for (n, case) in cases.iter().enumerate() {
            let mut i = input();
            case(&mut i);
            assert!(
                matches!(link(&i, &kernel), Err(LinkError::Relocation { .. })),
                "case {n}"
            );
        }
    }

    #[test]
    fn rejects_bad_kernel_refs() {
        let kernel = map();
        let cases: [fn(&mut LinkInput); 6] = [
            |i| i.kernel_refs[1].core1_address = 0x16_0000,
            |i| i.kernel_refs[1].form = KernelForm::Imm32,
            |i| i.kernel_refs[0].parcel = 3,
            |i| i.kernel_refs[0].parcel = 7,
            |i| i.kernel_refs[0].core1_address = CALL, // code holds a different address
            |i| i.code_relocations[0].offset = 5,
        ];
        for (n, case) in cases.iter().enumerate() {
            let mut i = input();
            case(&mut i);
            assert!(
                matches!(link(&i, &kernel), Err(LinkError::KernelRef { .. })),
                "case {n}"
            );
        }
        let mut wide = KernelMap::default();
        wide.insert(CALL, [0x0100_0000, CALL, CALL]).unwrap();
        wide.insert(PTR, [PTR; 3]).unwrap();
        assert!(
            matches!(link(&input(), &wide), Err(LinkError::KernelRef { .. })),
            "24-bit overflow"
        );
    }

    #[test]
    fn kernel_map_conflicts() {
        let mut m = map();
        m.insert(CALL, [0x170688, 0x16FFC0, 0x170001]).unwrap();
        assert!(m.insert(CALL, [1, 2, 3]).is_err());
        assert_eq!(m.get(CALL), Some([0x170688, 0x16FFC0, 0x170001]));
        for invalid in [
            r#"{"calls":{"0x1":{"core2":"0x2","core3":"0x3","core4":"0x4"},"0x1":{"core2":"0x9","core3":"0x9","core4":"0x9"}}}"#,
            r#"{"calls":{},"calls":{}}"#,
            r#"{"calls":{"0x1":{"core2":"0x2","core2":"0x9","core3":"0x3","core4":"0x4"}}}"#,
            r#"{"calls":{"0x+1":{"core2":"0x2","core3":"0x3","core4":"0x4"}}}"#,
            r#"{"calls":{"0x1A":{"core2":"0x2","core3":"0x3","core4":"0x4"},"0x1a":{"core2":"0x9","core3":"0x9","core4":"0x9"}}}"#,
        ] {
            assert!(KernelMap::from_json(invalid).is_err(), "{invalid}");
        }
        assert!(
            KernelMap::from_json(r#"{"calls": {"0x1": {"core2": "0x2", "core3": "0x3"}}}"#)
                .is_err()
        );
        assert!(
            KernelMap::from_json(
                r#"{"calls": {"12": {"core2": "0x2", "core3": "0x3", "core4": "0x4"}}}"#
            )
            .is_err()
        );
    }
}
