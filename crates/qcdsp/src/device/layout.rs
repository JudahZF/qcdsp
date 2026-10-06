//! Block state layout, in 32-bit words from the state pointer.
//!
//! ```text
//! [0, 17)            Volume's own state (smoother etc.), initialized by stock init
//! [17, 17+P)         parameter values, written by the generated set-parameter
//! bank(c), c = 0, 1  per-channel memory: states, delay cursors, delay buffers
//! ```
//! All offsets are normal-word units. Generated code only accesses state through
//! unqualified normal-word loads/stores with these offsets, which the DAG scales
//! for byte-addressed pointers, so no pointer arithmetic is done in data registers.

use super::template::{CHANNELS, STOCK_STATE_BYTES};
use crate::{Block, Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub parameters: u32,
    /// Word offset of each channel's bank.
    pub banks: [u32; CHANNELS as usize],
    /// Offsets within a bank.
    pub states: u32,
    pub cursors: u32,
    /// Offset within a bank and length of each delay buffer.
    pub delays: Vec<(u32, u32)>,
    pub total_words: u32,
}

impl Layout {
    pub fn new(block: &Block) -> Result<Self> {
        let parameters = STOCK_STATE_BYTES / 4;
        let state_count = block.states.len() as u32;
        let delay_count = block.delays.len() as u32;
        let mut bank_words = state_count + delay_count;
        let mut delays = Vec::new();
        for delay in &block.delays {
            let length = u32::try_from(delay.initial)
                .map_err(|_| Error::Invalid("delay too long".into()))?;
            delays.push((bank_words, length));
            bank_words = bank_words
                .checked_add(length)
                .ok_or_else(|| Error::Invalid("delay too long".into()))?;
        }
        let first = parameters + block.parameters.len() as u32;
        let total_words = first
            .checked_add(
                bank_words
                    .checked_mul(CHANNELS)
                    .ok_or_else(|| Error::Invalid("state too large".into()))?,
            )
            .ok_or_else(|| Error::Invalid("state too large".into()))?;
        Ok(Self {
            parameters,
            banks: [first, first + bank_words],
            states: 0,
            cursors: state_count,
            delays,
            total_words,
        })
    }

    pub fn bytes(&self) -> u32 {
        self.total_words * 4
    }
}
