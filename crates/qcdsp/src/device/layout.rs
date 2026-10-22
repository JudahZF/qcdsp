//! Block state layout, in 32-bit words from a base pointer.
//!
//! ```text
//! state (Volume's allocation, at most 0x7fff bytes):
//! [0, 17)            Volume's own state (smoother etc.), initialized by stock init
//! [17, 17+P)         parameter values, written by the generated set-parameter
//! [17+P, 17+P+C)     control slots: hoisted parameter math, shared by both channels
//! bank(c)            per-channel memory: states, delay cursors, spill slots, delay buffers;
//!                    one bank per computed channel (two for stereo, one for mono)
//! ```
//! When the banks do not fit Volume's allocation, they move to an arena that the generated init
//! takes from a kernel heap ([`Heap`]), and the state ends with the arena pointer:
//! ```text
//! state: [0, 17+P+C) as above, then one word: the arena pointer (0 when the allocation failed)
//! arena: bank(c) for each computed channel, from offset 0
//! ```
//! All offsets are normal-word units. Generated code only accesses state through
//! unqualified normal-word loads/stores with these offsets, which the DAG scales
//! for byte-addressed pointers, so no pointer arithmetic is done in data registers.

use super::{
    metadata,
    template::{DM_HEAP_BYTES, MAX_STATE_BYTES, STOCK_STATE_BYTES},
};
use crate::{Block, Error, Result, codegen::Contract};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub parameters: u32,
    pub controls: u32,
    /// Word offset of each computed channel's bank: from the state pointer, or from the arena
    /// pointer when there is an arena.
    pub banks: Vec<u32>,
    pub bank_words: u32,
    /// Offsets within a bank.
    pub states: u32,
    pub cursors: u32,
    pub spills: u32,
    /// Offset within a bank and length of each delay buffer.
    pub delays: Vec<(u32, u32)>,
    /// Words of Volume's state allocation.
    pub total_words: u32,
    pub arena: Option<Arena>,
}

/// Bank memory taken with `heap_malloc(heap, bytes)` when the banks do not fit the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arena {
    /// Word offset of the arena pointer in the state.
    pub pointer: u32,
    pub words: u32,
    pub heap: Heap,
}

/// The kernel heap an arena comes from; the value is `heap_malloc`'s index (k1 heap table DM
/// `0x242348`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heap {
    /// Heap 0, L1 block 0: the heap Volume's own state comes from. An arena uses it while the
    /// block's DM figure fits the heap. Generated code never pairs a DM and a PM access, so
    /// heap 1 (PM) would gain nothing.
    Dm = 0,
    /// Heap 2, the core's 16 MiB of DDR (`0x80000000` on core 1): for arenas that do not fit
    /// heap 0. Plain DM loads and stores reach it, as stock GRU amps read their weights.
    Sd = 2,
}

impl Arena {
    pub fn bytes(&self) -> u32 {
        self.words * 4
    }
}

impl Layout {
    pub fn new(block: &Block, contract: &Contract) -> Result<Self> {
        let too_large = || Error::Invalid("state too large".into());
        let parameters = STOCK_STATE_BYTES / 4;
        let controls = parameters + block.parameters.len() as u32;
        let state_count = block.states.len() as u32;
        let delay_count = block.delays.len() as u32;
        let spills = state_count + delay_count;
        let mut bank_words = spills + contract.spill_slots;
        let mut delays = Vec::new();
        for delay in &block.delays {
            let length = u32::try_from(delay.initial)
                .map_err(|_| Error::Invalid("delay too long".into()))?;
            delays.push((bank_words, length));
            bank_words = bank_words
                .checked_add(length)
                .ok_or_else(|| Error::Invalid("delay too long".into()))?;
        }
        let first = controls + contract.control_slots;
        let channels = block.channels().count();
        let banked = bank_words.checked_mul(channels).ok_or_else(too_large)?;
        let fits = |words: u32| words.checked_mul(4).is_some_and(|b| b <= MAX_STATE_BYTES);
        let (first_bank, total_words, arena) = match first.checked_add(banked) {
            Some(total) if fits(total) => (first, total, None),
            _ => {
                // heap_malloc takes the size in a signed C int.
                if banked.checked_mul(4).is_none_or(|b| b > i32::MAX as u32) || !fits(first + 1) {
                    return Err(too_large());
                }
                let mut arena = Arena {
                    pointer: first,
                    words: banked,
                    heap: Heap::Dm,
                };
                if metadata::dm_heap((first + 1) * 4, Some(arena)) > DM_HEAP_BYTES {
                    arena.heap = Heap::Sd;
                }
                (0, first + 1, Some(arena))
            }
        };
        Ok(Self {
            parameters,
            controls,
            banks: (0..channels)
                .map(|channel| first_bank + channel * bank_words)
                .collect(),
            bank_words,
            states: 0,
            cursors: state_count,
            spills,
            delays,
            total_words,
            arena,
        })
    }

    pub fn bytes(&self) -> u32 {
        self.total_words * 4
    }
}

#[cfg(test)]
mod tests {
    use crate::{device, examples};

    fn layout(samples: usize) -> super::Layout {
        device::generate(&examples::delay(samples).unwrap())
            .unwrap()
            .0
    }

    /// The banks stay in Volume's allocation while it holds them (no kernel calls), and move
    /// to an arena, leaving one pointer word in the state, from the first word that does not fit.
    #[test]
    fn banks_move_to_an_arena_when_they_do_not_fit_the_state() {
        let one = layout(1);
        let fixed = one.total_words - 2 * one.bank_words;
        let longest = (0x7fff / 4 - fixed - 2 * (one.bank_words - 1)) / 2;
        let inside = layout(longest as usize);
        assert_eq!(inside.arena, None);
        assert!(inside.bytes() <= 0x7fff);
        assert_eq!(inside.banks, [fixed, fixed + inside.bank_words]);

        let outside = layout(longest as usize + 1);
        let arena = outside.arena.unwrap();
        assert_eq!(arena.pointer, fixed);
        assert_eq!(outside.total_words, fixed + 1);
        assert_eq!(outside.banks, [0, outside.bank_words]);
        assert_eq!(arena.words, 2 * outside.bank_words);
        assert_eq!(arena.bytes(), 8 * outside.bank_words);
        assert_eq!(arena.heap, super::Heap::Dm);
    }

    /// An arena stays in heap 0 while the block's DM figure fits it, and moves to the SD heap
    /// from the first delay sample that does not.
    #[test]
    fn arenas_move_to_the_sd_heap_when_they_do_not_fit_heap_0() {
        use super::{Arena, DM_HEAP_BYTES, Heap, metadata};
        let heap = |samples| layout(samples).arena.unwrap().heap;
        assert_eq!(heap(9000), Heap::Dm);
        assert_eq!(heap(65_536), Heap::Sd);
        let lengths: Vec<usize> = (9000..65_536).collect();
        let first_sd = lengths[lengths.partition_point(|&n| heap(n) == Heap::Dm)];
        let figure_in_dm = |samples| {
            let l = layout(samples);
            let arena = Arena {
                heap: Heap::Dm,
                ..l.arena.unwrap()
            };
            metadata::dm_heap(l.bytes(), Some(arena))
        };
        assert!(figure_in_dm(first_sd - 1) <= DM_HEAP_BYTES);
        assert!(figure_in_dm(first_sd) > DM_HEAP_BYTES);
    }
}
