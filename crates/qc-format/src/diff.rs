//! Per-core patch records (`blobs/core{2,3,4}/diffs.blob`, not encrypted).
//!
//! A file is a list of named records. Each record turns the core 1
//! [`ModuleBlob`] into the blob for one other core:
//!
//! ```text
//! name\0, zero padding to a 4-byte boundary
//! 3 × group (contents = byte runs, relocations = word runs, link ranges = word runs):
//!     count, count × { section, run_count, run_count × { offset, length, values[length] } }
//! zero padding to a 4-byte boundary (may be absent at end of data)
//! ```
//!
//! Offsets and lengths are in bytes for contents and in words otherwise.
//! Stock records list every section of the blob in each group, even with
//! no runs; [`DiffRecord::between`] does the same.

use crate::blob::{ModuleBlob, Section, validate_blob_name};
use crate::{FormatError, Reader, Result, put_u32, wire_len};

/// One contiguous replacement: `values` overwrite the section from `offset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run<T> {
    pub offset: u32,
    pub values: Vec<T>,
}

/// The runs for one section, applied in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionRuns<T> {
    pub section: Section,
    pub runs: Vec<Run<T>>,
}

/// The patch that turns a core 1 blob into another core's blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRecord {
    /// Blob name; checked with [`validate_blob_name`] on parse and write.
    pub name: String,
    pub contents: Vec<SectionRuns<u8>>,
    pub relocations: Vec<SectionRuns<u32>>,
    pub ranges: Vec<SectionRuns<u32>>,
}

/// A whole `diffs.blob`. Record order is preserved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffFile {
    pub records: Vec<DiffRecord>,
}

impl DiffFile {
    /// Parses a file. Padding must be zero and only reach the next 4-byte
    /// boundary, and record names must be unique.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut r = Reader::new(data);
        let mut records: Vec<DiffRecord> = Vec::new();
        while r.remaining() > 0 {
            let record = DiffRecord::read(&mut r)?;
            if records.iter().any(|x| x.name == record.name) {
                return Err(FormatError::InvalidDiff(format!(
                    "duplicate record {:?}",
                    record.name
                )));
            }
            records.push(record);
        }
        Ok(Self { records })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for (i, record) in self.records.iter().enumerate() {
            if self.records[..i].iter().any(|x| x.name == record.name) {
                return Err(FormatError::InvalidDiff(format!(
                    "duplicate record {:?}",
                    record.name
                )));
            }
            record.write(&mut out)?;
        }
        Ok(out)
    }

    pub fn get(&self, name: &str) -> Option<&DiffRecord> {
        self.records.iter().find(|r| r.name == name)
    }

    /// Replaces the record with the same name in place, or appends it.
    pub fn upsert(&mut self, record: DiffRecord) {
        match self.records.iter_mut().find(|r| r.name == record.name) {
            Some(slot) => *slot = record,
            None => self.records.push(record),
        }
    }

    pub fn remove(&mut self, name: &str) -> Option<DiffRecord> {
        let index = self.records.iter().position(|r| r.name == name)?;
        Some(self.records.remove(index))
    }
}

impl DiffRecord {
    /// Parses a file that must hold exactly this one record.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut file = DiffFile::parse(data)?;
        match file.records.len() {
            1 => Ok(file.records.remove(0)),
            n => Err(FormatError::InvalidDiff(format!(
                "expected one record, found {n}"
            ))),
        }
    }

    /// Writes this record alone, padded like a record inside a file.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.write(&mut out)?;
        Ok(out)
    }

    /// Applies the record to a core 1 blob. Every referenced section must
    /// exist in the matching group and every run must stay inside it.
    pub fn apply(&self, core1: &ModuleBlob) -> Result<ModuleBlob> {
        let mut out = core1.clone();
        for group in &self.contents {
            let bytes = out
                .contents
                .iter_mut()
                .find(|c| c.section == group.section)
                .map(|c| &mut c.bytes)
                .ok_or_else(|| absent("contents", group.section))?;
            patch(bytes, &group.runs, group.section)?;
        }
        for group in &self.relocations {
            let words = out
                .relocations
                .iter_mut()
                .find(|r| r.section == group.section)
                .map(|r| &mut r.words)
                .ok_or_else(|| absent("relocations", group.section))?;
            patch(words, &group.runs, group.section)?;
        }
        for group in &self.ranges {
            let range = out
                .ranges
                .iter_mut()
                .find(|r| r.section == group.section)
                .ok_or_else(|| absent("link ranges", group.section))?;
            let mut words = vec![range.start, range.end];
            patch(&mut words, &group.runs, group.section)?;
            (range.start, range.end) = (words[0], words[1]);
        }
        Ok(out)
    }

    /// Builds the record that turns `core1` into `core_n` with minimal runs
    /// (only changed bytes or words). Both blobs must have the same sections
    /// in the same order, the same lengths and the same entry.
    pub fn between(name: &str, core1: &ModuleBlob, core_n: &ModuleBlob) -> Result<Self> {
        validate_blob_name(name)?;
        let mismatch = |what: &str| {
            FormatError::InvalidDiff(format!("blobs differ in {what}; a diff cannot express it"))
        };
        if core1.entry != core_n.entry {
            return Err(mismatch("entry"));
        }
        let same_shape = |a: &[(Section, usize)], b: &[(Section, usize)]| a == b;
        let shape_c = |b: &ModuleBlob| {
            b.contents
                .iter()
                .map(|c| (c.section, c.bytes.len()))
                .collect::<Vec<_>>()
        };
        let shape_r = |b: &ModuleBlob| {
            b.relocations
                .iter()
                .map(|r| (r.section, r.words.len()))
                .collect::<Vec<_>>()
        };
        let shape_l = |b: &ModuleBlob| b.ranges.iter().map(|r| (r.section, 2)).collect::<Vec<_>>();
        if !same_shape(&shape_c(core1), &shape_c(core_n)) {
            return Err(mismatch("content layout"));
        }
        if !same_shape(&shape_r(core1), &shape_r(core_n)) {
            return Err(mismatch("relocation layout"));
        }
        if !same_shape(&shape_l(core1), &shape_l(core_n)) {
            return Err(mismatch("link range layout"));
        }
        Ok(Self {
            name: name.into(),
            contents: core1
                .contents
                .iter()
                .zip(&core_n.contents)
                .map(|(a, b)| SectionRuns {
                    section: a.section,
                    runs: runs_between(&a.bytes, &b.bytes),
                })
                .collect(),
            relocations: core1
                .relocations
                .iter()
                .zip(&core_n.relocations)
                .map(|(a, b)| SectionRuns {
                    section: a.section,
                    runs: runs_between(&a.words, &b.words),
                })
                .collect(),
            ranges: core1
                .ranges
                .iter()
                .zip(&core_n.ranges)
                .map(|(a, b)| SectionRuns {
                    section: a.section,
                    runs: runs_between(&[a.start, a.end], &[b.start, b.end]),
                })
                .collect(),
        })
    }

    fn read(r: &mut Reader<'_>) -> Result<Self> {
        let start = r.pos;
        let rest = &r.data[start..];
        let len = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or(FormatError::Malformed {
                offset: start,
                reason: "record name has no terminator",
            })?;
        let name = std::str::from_utf8(&rest[..len])
            .map_err(|_| FormatError::Malformed {
                offset: start,
                reason: "record name is not UTF-8",
            })?
            .to_string();
        validate_blob_name(&name)?;
        r.bytes(len + 1)?;
        skip_padding(r, start)?;

        let contents = read_group(r, |r, len| Ok(r.bytes(len as usize)?.to_vec()))?;
        let relocations = read_group(r, |r, len| r.words(len))?;
        let ranges = read_group(r, |r, len| r.words(len))?;
        skip_end_padding(r, start)?;
        Ok(Self {
            name,
            contents,
            relocations,
            ranges,
        })
    }

    fn write(&self, out: &mut Vec<u8>) -> Result<()> {
        validate_blob_name(&self.name)?;
        let start = out.len();
        out.extend_from_slice(self.name.as_bytes());
        out.push(0);
        pad(out, start);
        write_group(out, &self.contents, |out, values| {
            out.extend_from_slice(values)
        })?;
        for group in [&self.relocations, &self.ranges] {
            write_group(out, group, |out, values| {
                values.iter().for_each(|&w| put_u32(out, w))
            })?;
        }
        pad(out, start);
        Ok(())
    }
}

fn absent(group: &str, section: Section) -> FormatError {
    FormatError::InvalidDiff(format!("{group} of {section:?} are not in the blob"))
}

fn patch<T: Copy>(target: &mut [T], runs: &[Run<T>], section: Section) -> Result<()> {
    for run in runs {
        let start = run.offset as usize;
        let slot = start
            .checked_add(run.values.len())
            .and_then(|end| target.get_mut(start..end))
            .ok_or_else(|| {
                FormatError::InvalidDiff(format!("run at {start} exceeds {section:?}"))
            })?;
        slot.copy_from_slice(&run.values);
    }
    Ok(())
}

fn runs_between<T: Copy + PartialEq>(a: &[T], b: &[T]) -> Vec<Run<T>> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < a.len() {
        if a[i] == b[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < a.len() && a[i] != b[i] {
            i += 1;
        }
        // Section lengths come from a parsed or validated blob, so they fit u32.
        runs.push(Run {
            offset: start as u32,
            values: b[start..i].to_vec(),
        });
    }
    runs
}

fn read_group<T>(
    r: &mut Reader<'_>,
    mut values: impl FnMut(&mut Reader<'_>, u32) -> Result<Vec<T>>,
) -> Result<Vec<SectionRuns<T>>> {
    let at = r.pos;
    let count = r.u32()?;
    if count > Section::ALL.len() as u32 {
        return Err(FormatError::Malformed {
            offset: at,
            reason: "more group entries than sections",
        });
    }
    let mut groups: Vec<SectionRuns<T>> = Vec::new();
    for _ in 0..count {
        let at = r.pos;
        let section = Section::from_id(r.u32()?)?;
        if groups.iter().any(|g| g.section == section) {
            return Err(FormatError::Malformed {
                offset: at,
                reason: "duplicate section in diff group",
            });
        }
        let run_count = r.u32()?;
        let mut runs = Vec::new();
        for _ in 0..run_count {
            let offset = r.u32()?;
            let len = r.u32()?;
            runs.push(Run {
                offset,
                values: values(r, len)?,
            });
        }
        groups.push(SectionRuns { section, runs });
    }
    Ok(groups)
}

fn write_group<T>(
    out: &mut Vec<u8>,
    groups: &[SectionRuns<T>],
    mut values: impl FnMut(&mut Vec<u8>, &[T]),
) -> Result<()> {
    let mut seen = Vec::new();
    put_u32(out, wire_len(groups.len(), "diff group")?);
    for group in groups {
        if seen.contains(&group.section) {
            return Err(FormatError::InvalidDiff(format!(
                "duplicate {:?} entry in a group",
                group.section
            )));
        }
        seen.push(group.section);
        put_u32(out, group.section.id());
        put_u32(out, wire_len(group.runs.len(), "run list")?);
        for run in &group.runs {
            put_u32(out, run.offset);
            put_u32(out, wire_len(run.values.len(), "run")?);
            values(out, &run.values);
        }
    }
    Ok(())
}

/// Consumes zero bytes up to the next 4-byte boundary relative to `start`.
fn skip_padding(r: &mut Reader<'_>, start: usize) -> Result<()> {
    let len = (4 - (r.pos - start) % 4) % 4;
    zeros(r, len)
}

/// Like [`skip_padding`], but data may end before the boundary: stock
/// `diffs.blob` files pad their last record, single `.record` files from
/// other tools do not. Writers always pad.
fn skip_end_padding(r: &mut Reader<'_>, start: usize) -> Result<()> {
    let len = (4 - (r.pos - start) % 4) % 4;
    zeros(r, len.min(r.remaining()))
}

fn zeros(r: &mut Reader<'_>, len: usize) -> Result<()> {
    let at = r.pos;
    if r.bytes(len)?.iter().any(|&b| b != 0) {
        return Err(FormatError::Malformed {
            offset: at,
            reason: "non-zero padding",
        });
    }
    Ok(())
}

fn pad(out: &mut Vec<u8>, start: usize) {
    let len = (4 - (out.len() - start) % 4) % 4;
    out.resize(out.len() + len, 0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::tests::sample;

    fn core_n() -> ModuleBlob {
        let mut b = sample();
        b.content_mut(Section::Code).unwrap()[5..9].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        b.content_mut(Section::Code).unwrap()[20] = 0xEE;
        b.relocations[1].words[3] = 0x1234;
        b
    }

    #[test]
    fn between_apply_and_round_trip() {
        let (a, b) = (sample(), core_n());
        let record = DiffRecord::between("vol_2", &a, &b).unwrap();
        assert_eq!(record.apply(&a).unwrap(), b);

        // Every section appears in every group, matching stock records.
        assert_eq!(record.contents.len(), 2);
        assert!(record.contents[0].runs.is_empty());
        assert_eq!(
            record.contents[1].runs,
            vec![
                Run {
                    offset: 5,
                    values: vec![0xAA, 0xBB, 0xCC, 0xDD]
                },
                Run {
                    offset: 20,
                    values: vec![0xEE]
                },
            ]
        );
        assert_eq!(record.ranges.len(), 2);

        let bytes = record.to_bytes().unwrap();
        assert_eq!(bytes.len() % 4, 0);
        assert_eq!(DiffRecord::parse(&bytes).unwrap(), record);
    }

    #[test]
    fn file_keeps_order_and_edits_one_record() {
        let (a, b) = (sample(), core_n());
        let names = ["zeta", "alpha", "mid"];
        let mut file = DiffFile {
            records: names
                .iter()
                .map(|n| DiffRecord::between(n, &a, &b).unwrap())
                .collect(),
        };
        let bytes = file.to_bytes().unwrap();
        let parsed = DiffFile::parse(&bytes).unwrap();
        assert_eq!(parsed, file);

        let empty = DiffRecord::between("alpha", &a, &a).unwrap();
        file.upsert(empty.clone());
        assert_eq!(file.records[1], empty);
        assert_eq!(file.remove("zeta").unwrap().name, "zeta");
        assert!(file.remove("zeta").is_none());
        file.upsert(DiffRecord::between("new", &a, &b).unwrap());
        let order: Vec<_> = file.records.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(order, ["alpha", "mid", "new"]);
        assert_eq!(DiffFile::parse(&[]).unwrap(), DiffFile::default());
    }

    #[test]
    fn rejects_malformed_records() {
        let record = DiffRecord::between("abc", &sample(), &core_n()).unwrap();
        let bytes = record.to_bytes().unwrap();
        // The last 3 bytes are final padding, which may be absent.
        for len in 1..bytes.len() - 3 {
            assert!(
                DiffFile::parse(&bytes[..len]).is_err(),
                "truncated to {len}"
            );
        }

        let mut bad_pad = bytes.clone();
        bad_pad[3] = 1; // "abc\0" has no padding; poison the first group count instead
        assert!(DiffFile::parse(&bad_pad).is_err());

        // A final record may end without its padding (single-record files).
        let unpadded = DiffRecord::between("abcd", &sample(), &core_n())
            .unwrap()
            .to_bytes()
            .unwrap();
        let trimmed = &unpadded[..unpadded.len() - 3];
        assert_eq!(&unpadded[unpadded.len() - 3..], &[0, 0, 0]);
        assert_eq!(
            DiffRecord::parse(trimmed).unwrap().to_bytes().unwrap(),
            unpadded
        );

        let mut extra = bytes.clone();
        extra.extend_from_slice(&[0; 4]);
        assert!(DiffFile::parse(&extra).is_err(), "extra zero padding");

        let mut twice = bytes.clone();
        twice.extend_from_slice(&bytes);
        assert!(DiffFile::parse(&twice).is_err(), "duplicate name");

        let mut file = DiffFile {
            records: vec![record.clone(), record.clone()],
        };
        assert!(file.to_bytes().is_err());
        file.records[1].name = "bad name".into();
        assert!(file.to_bytes().is_err());
    }

    #[test]
    fn apply_checks_bounds_and_sections() {
        let a = sample();
        let mut record = DiffRecord::between("x", &a, &core_n()).unwrap();
        record.contents[1].runs.push(Run {
            offset: 23,
            values: vec![1, 2],
        });
        assert!(record.apply(&a).is_err());

        let mut record = DiffRecord::between("x", &a, &a).unwrap();
        record.contents.push(SectionRuns {
            section: Section::Isa,
            runs: vec![],
        });
        assert!(record.apply(&a).is_err());

        let mut other = a.clone();
        other.entry += 1;
        assert!(DiffRecord::between("x", &a, &other).is_err());
        other = a.clone();
        other.content_mut(Section::Code).unwrap().push(0);
        assert!(DiffRecord::between("x", &a, &other).is_err());
    }
}
