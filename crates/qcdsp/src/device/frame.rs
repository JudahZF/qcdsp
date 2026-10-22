//! Memory plan of a network (IR 3) on the device.
//!
//! The network runs once per 32-sample call, operation by operation over the whole call.
//! Buffers are time-major: row `t` holds the buffer's channels for sample `t`, padded to an
//! even word count.
//!
//! ```text
//! DM arena (heap 0):  each buffer, one after the other
//!   no history:       32 rows (this call)
//!   history H:        a ring of H + 32 rows; this call's rows start at the buffer's cursor,
//!                     the rows before them are the past (read with circular addressing)
//! PM arena (heap 1):  the weights of each convolution, in kernel order (see `pack`)
//! state:              one cursor word per ring: the ring offset of this call's first row
//! ```
//!
//! A convolution computes two output channels per instruction in SIMD: PEx output `o`, PEy
//! output `o + 1`. Both read the same input value (I1 broadcast) and adjacent weights, so the
//! weights of an output pair are interleaved. An odd last output channel runs alone, with zero
//! weights for its missing partner.

use super::template::BLOCK_SAMPLES;
use crate::network::{Network, Op};

/// Rows of one call.
const ROWS: u32 = BLOCK_SAMPLES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameBuffer {
    pub channels: u32,
    /// Past rows a convolution reads (0: no ring).
    pub history: u32,
    /// Word offset in the DM arena.
    pub offset: u32,
}

impl FrameBuffer {
    /// Rows the buffer occupies: the call, plus the past for a ring.
    pub fn rows(&self) -> u32 {
        self.history + ROWS
    }
    /// Words of one row: the channels rounded up to even, so that a SIMD word pair never
    /// crosses a row (or a ring's end).
    pub fn row_words(&self) -> u32 {
        self.channels + self.channels % 2
    }
    pub fn words(&self) -> u32 {
        self.rows() * self.row_words()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FramePlan {
    pub buffers: Vec<FrameBuffer>,
    /// Words of the DM arena.
    pub dm_words: u32,
    /// The PM arena, in the order the kernels read it.
    pub weights: Vec<f32>,
    /// Word offset of each operation's weights in `weights` (convolutions only).
    pub op_weights: Vec<Option<u32>>,
    /// Buffers with a ring, in buffer order; each has a cursor word in the state.
    pub rings: Vec<usize>,
}

impl FramePlan {
    pub fn new(network: &Network) -> Self {
        let history = network.history();
        let live = live_ranges(network);
        let mut offset = 0;
        // Rings persist across calls; a buffer without history lives from its first to its
        // last use in one call, so buffers whose ranges do not overlap share storage.
        let mut slots: Vec<(u32, u32, usize)> = Vec::new(); // (offset, words, free after op)
        let buffers = network
            .buffers
            .iter()
            .zip(&history)
            .enumerate()
            .map(|(index, (&channels, &history))| {
                let mut buffer = FrameBuffer {
                    channels: channels as u32,
                    history: history as u32,
                    offset,
                };
                let words = buffer.words();
                let (first, last) = live[index];
                let reuse = slots
                    .iter_mut()
                    .find(|(_, size, free)| history == 0 && *size == words && *free < first);
                match reuse {
                    Some(slot) => {
                        buffer.offset = slot.0;
                        slot.2 = last;
                    }
                    None => {
                        if history == 0 {
                            slots.push((offset, words, last));
                        }
                        offset += words;
                    }
                }
                buffer
            })
            .collect();
        let mut weights = Vec::new();
        let op_weights = network
            .ops
            .iter()
            .map(|op| match op {
                Op::Conv {
                    input,
                    output,
                    kernel,
                    weights: w,
                    bias,
                    ..
                } => {
                    let at = weights.len() as u32;
                    pack(
                        &mut weights,
                        network.buffers[*input],
                        network.buffers[*output],
                        *kernel,
                        w,
                        bias.as_deref(),
                    );
                    Some(at)
                }
                _ => None,
            })
            .collect();
        Self {
            rings: (0..history.len()).filter(|&b| history[b] > 0).collect(),
            buffers,
            dm_words: offset,
            weights,
            op_weights,
        }
    }

    /// Bytes of the PM arena.
    pub fn pm_words(&self) -> u32 {
        self.weights.len() as u32
    }
}

/// The first and last operation that touches each buffer (buffer 0, the input, from the start).
fn live_ranges(network: &Network) -> Vec<(usize, usize)> {
    let mut live = vec![(usize::MAX, 0); network.buffers.len()];
    live[0].0 = 0;
    let mut touch = |buffer: usize, at: usize| {
        live[buffer].0 = live[buffer].0.min(at);
        live[buffer].1 = live[buffer].1.max(at);
    };
    for (at, op) in network.ops.iter().enumerate() {
        match op {
            Op::Gain { buffer, .. } | Op::Activation { buffer, .. } => touch(*buffer, at),
            Op::Conv { input, output, .. }
            | Op::Add { input, output }
            | Op::Copy { input, output } => {
                touch(*input, at);
                touch(*output, at);
            }
        }
    }
    // The output is read after the last operation.
    live[network.output].1 = usize::MAX;
    live
}

/// Appends one convolution's weights in kernel order: for each output pair `(o, o + 1)`, the
/// bias pair, then for each tap `k` and input `i` the pair `w[o][i][k], w[o + 1][i][k]`. A
/// missing partner (odd output count) and a missing bias are zero.
fn pack(
    out: &mut Vec<f32>,
    inputs: usize,
    outputs: usize,
    kernel: usize,
    weights: &[f32],
    bias: Option<&[f32]>,
) {
    let weight = |o: usize, i: usize, k: usize| {
        if o < outputs {
            weights[(o * inputs + i) * kernel + k]
        } else {
            0.0
        }
    };
    let bias = |o: usize| bias.filter(|_| o < outputs).map_or(0.0, |b| b[o]);
    for o in (0..outputs).step_by(2) {
        out.extend([bias(o), bias(o + 1)]);
        for k in 0..kernel {
            for i in 0..inputs {
                out.extend([weight(o, i, k), weight(o + 1, i, k)]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkBuilder;

    /// Rings hold history plus one call; a dead buffer's storage is reused; weights are
    /// interleaved by output pair with the bias pair first, and an odd output gets a zero partner.
    #[test]
    fn plan_lays_out_rings_and_interleaved_weights() {
        let mut net = NetworkBuilder::new();
        let x = net.input();
        let a = net.buffer(3);
        let b = net.buffer(1);
        // a[o] = bias + w·x; b = conv(a) with kernel 2, dilation 5.
        net.conv(
            x,
            a,
            1,
            1,
            vec![1.0, 2.0, 3.0],
            Some(vec![0.1, 0.2, 0.3]),
            false,
        );
        net.conv(a, b, 2, 5, vec![4.0, 5.0, 6.0, 7.0, 8.0, 9.0], None, false);
        let plan = FramePlan::new(&net.finish(b));

        assert_eq!(plan.rings, [1]);
        let [x, a, b] = &plan.buffers[..] else {
            panic!()
        };
        assert_eq!(
            (x.offset, x.words()),
            (0, 64),
            "rows have an even word count"
        );
        assert_eq!((a.offset, a.history, a.words()), (64, 5, 37 * 4));
        // The input is dead after the first convolution, so the output takes its storage.
        assert_eq!((b.offset, b.words()), (0, 64));
        assert_eq!(plan.dm_words, 64 + 148);

        assert_eq!(plan.op_weights, [Some(0), Some(8)]);
        #[rustfmt::skip]
        assert_eq!(plan.weights, [
            // first conv: pair (0, 1), then (2, -)
            0.1, 0.2, 1.0, 2.0,
            0.3, 0.0, 3.0, 0.0,
            // second conv: one output (0, -), bias zero; k = 0 then k = 1, inputs 0..3
            0.0, 0.0,
            4.0, 0.0, 6.0, 0.0, 8.0, 0.0,
            5.0, 0.0, 7.0, 0.0, 9.0, 0.0,
        ][..]);
    }
}
