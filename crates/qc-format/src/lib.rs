//! Quad Cortex block file formats.
//!
//! - [`blob`]: the decrypted `ModuleBlob` (`blobs/core1/<name>_blob.bin`).
//! - [`diff`]: per-core patch records (`blobs/core{2,3,4}/diffs.blob`).
//! - [`metadata`]: byte-preserving edits of `ModelRepo.xml` and `moduleReport.xml`.
//! - [`crypto`]: the NDSP AES-CTR file encryption.
//!
//! Parsers are strict: they reject truncated, trailing or out-of-range data
//! instead of guessing, and the writers reproduce stock files byte for byte.
//! This crate holds no keys and no device data; callers supply both.

pub mod blob;
pub mod crypto;
pub mod diff;
pub mod metadata;

/// Every way a parse, edit or write can fail.
#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("data ends early at byte {offset}")]
    Truncated { offset: usize },
    #[error("{count} unexpected bytes after byte {offset}")]
    Trailing { offset: usize, count: usize },
    #[error("unknown section id {0}")]
    UnknownSection(u32),
    #[error("malformed data at byte {offset}: {reason}")]
    Malformed { offset: usize, reason: &'static str },
    #[error("invalid module blob: {0}")]
    InvalidBlob(String),
    #[error("invalid diff record: {0}")]
    InvalidDiff(String),
    #[error("invalid blob name {0:?}: use 1-80 ASCII letters, digits or '_'")]
    InvalidName(String),
    #[error("XML: {0}")]
    Xml(String),
}

pub type Result<T> = std::result::Result<T, FormatError>;

/// Bounds-checked little-endian reader shared by the binary parsers.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.data.len())
            .ok_or(FormatError::Truncated { offset: self.pos })?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads `count` words; the length check happens before any allocation.
    fn words(&mut self, count: u32) -> Result<Vec<u32>> {
        let len = (count as usize)
            .checked_mul(4)
            .ok_or(FormatError::Truncated { offset: self.pos })?;
        Ok(self
            .bytes(len)?
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect())
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn finish(&self) -> Result<()> {
        match self.remaining() {
            0 => Ok(()),
            count => Err(FormatError::Trailing {
                offset: self.pos,
                count,
            }),
        }
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// Converts a length to the `u32` the wire formats store.
fn wire_len(len: usize, what: &str) -> Result<u32> {
    u32::try_from(len).map_err(|_| FormatError::InvalidBlob(format!("{what} is too long")))
}
