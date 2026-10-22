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
//! weights of an output pair are interleaved. An odd last output channel gets zero weights for
//! its missing partner (rows have an even word count, so its column exists).
//!
//! Some operations fuse into the convolution before them ([`Kernel::Conv`]): a one-channel
//! `accumulate` 1×1 convolution into the same output (the NAM input mixin) becomes one more
//! term, and an `add` into the output is added before the store. The sums keep the host's order.

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
    /// The PM arena: weight pairs and bias pairs of every convolution kernel.
    pub weights: Vec<f32>,
    /// The kernels in program order; together they run every operation once.
    pub kernels: Vec<Kernel>,
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
        let kernels = kernels(network, &mut weights);
        Self {
            rings: (0..history.len()).filter(|&b| history[b] > 0).collect(),
            buffers,
            dm_words: offset,
            weights,
            kernels,
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

/// What the code generator runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Kernel {
    Conv(ConvKernel),
    /// An operation that runs alone (elementwise).
    Op(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConvKernel {
    /// The convolution.
    pub op: usize,
    /// A fused one-channel `accumulate` 1×1 convolution into the same output: its input
    /// buffer. Its weights follow the convolution's in each output pair.
    pub condition: Option<usize>,
    /// Added to the sum before the store: the output itself (`accumulate`) or the input of a
    /// fused `add`.
    pub residual: Option<usize>,
    /// Word offsets in [`FramePlan::weights`] of the weight pairs and of the bias pairs.
    pub weights: u32,
    pub bias: u32,
}

impl ConvKernel {
    /// Multiply-adds of one output: the convolution's terms, plus one for the condition.
    pub fn terms(&self, network: &Network) -> usize {
        let Op::Conv { input, kernel, .. } = &network.ops[self.op] else {
            unreachable!("a convolution kernel runs a convolution")
        };
        network.buffers[*input] * kernel + usize::from(self.condition.is_some())
    }
}

/// Groups the operations into kernels and packs their weights.
fn kernels(network: &Network, packed: &mut Vec<f32>) -> Vec<Kernel> {
    let ops = &network.ops;
    let mut kernels = Vec::new();
    let mut at = 0;
    while at < ops.len() {
        let Op::Conv {
            input,
            output,
            kernel,
            weights,
            bias,
            accumulate,
            ..
        } = &ops[at]
        else {
            kernels.push(Kernel::Op(at));
            at += 1;
            continue;
        };
        let mut next = at + 1;
        let condition = match ops.get(next) {
            Some(Op::Conv {
                input: c,
                output: o,
                kernel: 1,
                weights: w,
                bias: None,
                accumulate: true,
                ..
            }) if o == output && c != output && network.buffers[*c] == 1 => {
                next += 1;
                Some((*c, w.as_slice()))
            }
            _ => None,
        };
        let residual = if *accumulate {
            Some(*output)
        } else {
            match ops.get(next) {
                Some(Op::Add {
                    input: r,
                    output: o,
                }) if o == output => {
                    next += 1;
                    Some(*r)
                }
                _ => None,
            }
        };
        let (inputs, outputs) = (network.buffers[*input], network.buffers[*output]);
        let weight_at = packed.len() as u32;
        // Per output pair: the terms in the host's order (input, then tap), then the condition.
        for o in (0..outputs).step_by(2) {
            let pair = |value: &dyn Fn(usize) -> f32| {
                [o, o + 1].map(|o| if o < outputs { value(o) } else { 0.0 })
            };
            for i in 0..inputs {
                for k in 0..*kernel {
                    packed.extend(pair(&|o| weights[(o * inputs + i) * kernel + k]));
                }
            }
            if let Some((_, w)) = condition {
                packed.extend(pair(&|o| w[o]));
            }
        }
        let bias_at = packed.len() as u32;
        for o in (0..outputs).step_by(2) {
            packed.extend([o, o + 1].map(|o| match bias {
                Some(b) if o < outputs => b[o],
                _ => 0.0,
            }));
        }
        kernels.push(Kernel::Conv(ConvKernel {
            op: at,
            condition: condition.map(|(c, _)| c),
            residual,
            weights: weight_at,
            bias: bias_at,
        }));
        at = next;
    }
    kernels
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

        let [Kernel::Conv(first), Kernel::Conv(second)] = &plan.kernels[..] else {
            panic!("two convolution kernels: {:?}", plan.kernels)
        };
        assert_eq!(
            (first.weights, first.bias, second.weights, second.bias),
            (0, 4, 8, 20)
        );
        #[rustfmt::skip]
        assert_eq!(plan.weights, [
            // first conv: pairs (0, 1) and (2, -), then their biases
            1.0, 2.0, 3.0, 0.0,
            0.1, 0.2, 0.3, 0.0,
            // second conv: one output (0, -); input 0 taps 0 and 1, input 1, input 2; no bias
            4.0, 0.0, 5.0, 0.0, 6.0, 0.0, 7.0, 0.0, 8.0, 0.0, 9.0, 0.0,
            0.0, 0.0,
        ][..]);
    }
}
