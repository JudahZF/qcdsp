//! IR 3 networks: vector operations on channel buffers, for convolutional models such as NAM
//! WaveNets.
//!
//! A network runs once per sample, before the scalar graph, which reads its output through a
//! `network` node. Buffer 0 holds the block input (one channel). Each operation reads and writes
//! whole buffers; a convolution also reads a buffer's past values, so the device can run the
//! network layer by layer over a call's 32 samples and get the same result. There is no
//! feedback: a buffer's history is its value after the network for each past sample.
//!
//! The host evaluation here is the reference for the device code. Tanh uses the same Padé
//! approximation as the scalar `tanh` node, in the same operation order.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Limits for untrusted IR. They bound host memory; the device budget is checked when the block
/// is built.
pub const MAX_BUFFERS: usize = 1024;
pub const MAX_CHANNELS: usize = 64;
pub const MAX_OPS: usize = 4096;
pub const MAX_KERNEL: usize = 16;
pub const MAX_DILATION: usize = 1 << 16;
/// Weights and biases of all operations together.
pub const MAX_WEIGHTS: usize = 1 << 20;
/// Past samples of all channels of all buffers together.
pub const MAX_HISTORY: usize = 1 << 22;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Channel count of each buffer. Buffer 0 is the block input and has one channel.
    pub(crate) buffers: Vec<usize>,
    pub(crate) ops: Vec<Op>,
    /// The one-channel buffer that `network` nodes read.
    pub(crate) output: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Op {
    /// `buffer *= parameter`, all channels.
    Gain { buffer: usize, parameter: usize },
    /// A causal dilated convolution; `kernel: 1` is a dense (1×1) layer:
    /// `output[o] (+)= bias[o] + Σ_i Σ_k weights[(o·in + i)·kernel + k] · input[i](t − (kernel − 1 − k)·dilation)`.
    Conv {
        input: usize,
        output: usize,
        kernel: usize,
        dilation: usize,
        weights: Vec<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bias: Option<Vec<f32>>,
        /// Add to `output` instead of replacing it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        accumulate: bool,
    },
    /// `buffer = function(buffer)`, all channels.
    Activation { buffer: usize, function: Activation },
    /// `output += input`; both have the same channel count.
    Add { input: usize, output: usize },
    /// `output = input`; both have the same channel count.
    Copy { input: usize, output: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Activation {
    /// The Padé (7, 6) approximation of the scalar `tanh` node (within 1e-4).
    Tanh,
    /// `min(max(x, -1), 1)`.
    Hardtanh,
    /// `max(x, 0)`.
    Relu,
}

impl Network {
    pub(crate) fn validate(&self, parameters: usize) -> Result<()> {
        let invalid = |what: String| Err(Error::Invalid(format!("network: {what}")));
        if self.buffers.is_empty() || self.buffers.len() > MAX_BUFFERS || self.ops.len() > MAX_OPS {
            return invalid(format!(
                "1 to {MAX_BUFFERS} buffers and at most {MAX_OPS} operations"
            ));
        }
        if self.buffers[0] != 1 {
            return invalid("buffer 0 (the block input) has one channel".into());
        }
        if self.buffers.iter().any(|&c| c == 0 || c > MAX_CHANNELS) {
            return invalid(format!("a buffer has 1 to {MAX_CHANNELS} channels"));
        }
        if self.buffers.get(self.output) != Some(&1) {
            return invalid("the output is a one-channel buffer".into());
        }
        let buffer = |index: usize| {
            self.buffers
                .get(index)
                .copied()
                .ok_or_else(|| Error::Invalid(format!("network: no buffer {index}")))
        };
        // Every buffer is written before it is read in each run, and a buffer whose history a
        // convolution reads does not change after that read (its history is its final value).
        let mut written = vec![false; self.buffers.len()];
        written[0] = true;
        let mut frozen = vec![false; self.buffers.len()];
        let mut weights = 0usize;
        let read = |index: usize, written: &[bool]| -> Result<()> {
            buffer(index)?;
            if written[index] {
                Ok(())
            } else {
                Err(Error::Invalid(format!(
                    "network: buffer {index} is read before it is written"
                )))
            }
        };
        let write = |index: usize, written: &mut [bool], frozen: &[bool]| -> Result<()> {
            if frozen[index] {
                return Err(Error::Invalid(format!(
                    "network: buffer {index} changes after a convolution read its history"
                )));
            }
            written[index] = true;
            Ok(())
        };
        for (at, op) in self.ops.iter().enumerate() {
            match op {
                Op::Gain {
                    buffer: b,
                    parameter,
                } => {
                    if *parameter >= parameters {
                        return invalid(format!("operation {at} reads a missing parameter"));
                    }
                    read(*b, &written)?;
                    write(*b, &mut written, &frozen)?;
                }
                Op::Conv {
                    input,
                    output,
                    kernel,
                    dilation,
                    weights: w,
                    bias,
                    accumulate,
                } => {
                    let (inputs, outputs) = (buffer(*input)?, buffer(*output)?);
                    if input == output {
                        return invalid(format!("operation {at} reads and writes one buffer"));
                    }
                    if !(1..=MAX_KERNEL).contains(kernel) || !(1..=MAX_DILATION).contains(dilation)
                    {
                        return invalid(format!(
                            "operation {at}: kernel 1 to {MAX_KERNEL}, dilation 1 to {MAX_DILATION}"
                        ));
                    }
                    if w.len() != outputs * inputs * kernel
                        || bias.as_ref().is_some_and(|b| b.len() != outputs)
                    {
                        return invalid(format!("operation {at} has the wrong number of weights"));
                    }
                    if w.iter()
                        .chain(bias.iter().flatten())
                        .any(|v| !v.is_finite())
                    {
                        return invalid(format!("operation {at} has a weight that is not finite"));
                    }
                    weights += w.len() + bias.as_ref().map_or(0, Vec::len);
                    read(*input, &written)?;
                    if *accumulate {
                        read(*output, &written)?;
                    }
                    write(*output, &mut written, &frozen)?;
                    if *kernel > 1 {
                        frozen[*input] = true;
                    }
                }
                Op::Activation { buffer: b, .. } => {
                    read(*b, &written)?;
                    write(*b, &mut written, &frozen)?;
                }
                Op::Add { input, output } | Op::Copy { input, output } => {
                    if input == output || buffer(*input)? != buffer(*output)? {
                        return invalid(format!(
                            "operation {at} joins buffers of different sizes or one to itself"
                        ));
                    }
                    read(*input, &written)?;
                    if matches!(op, Op::Add { .. }) {
                        read(*output, &written)?;
                    }
                    write(*output, &mut written, &frozen)?;
                }
            }
        }
        if weights > MAX_WEIGHTS {
            return invalid(format!("more than {MAX_WEIGHTS} weights"));
        }
        if !written[self.output] {
            return invalid("the output buffer is never written".into());
        }
        let history: usize = self
            .history()
            .iter()
            .zip(&self.buffers)
            .map(|(samples, channels)| samples * channels)
            .sum();
        if history > MAX_HISTORY {
            return invalid(format!("more than {MAX_HISTORY} samples of history"));
        }
        Ok(())
    }

    /// Past samples each buffer keeps: the longest reach of a convolution that reads it.
    pub fn history(&self) -> Vec<usize> {
        let mut history = vec![0; self.buffers.len()];
        for op in &self.ops {
            if let Op::Conv {
                input,
                kernel,
                dilation,
                ..
            } = op
            {
                history[*input] = history[*input].max((kernel - 1) * dilation);
            }
        }
        history
    }

    /// Multiply-adds per sample (convolutions only).
    pub fn multiply_adds(&self) -> usize {
        self.ops
            .iter()
            .map(|op| match op {
                Op::Conv { weights, .. } => weights.len(),
                _ => 0,
            })
            .sum()
    }

    pub fn buffers(&self) -> &[usize] {
        &self.buffers
    }
}

/// Host evaluation state: the current value of every buffer and the history rings.
pub(crate) struct Evaluator {
    values: Vec<Vec<f32>>,
    /// One convolution's outputs, so processing allocates nothing.
    scratch: Vec<f32>,
    /// Per buffer: `channels` rings of `history` samples, and the slot of the next write.
    rings: Vec<Option<Ring>>,
}

struct Ring {
    samples: Vec<Vec<f32>>,
    next: usize,
}

impl Evaluator {
    pub(crate) fn new(network: &Network) -> Self {
        Self {
            values: network.buffers.iter().map(|&c| vec![0.0; c]).collect(),
            scratch: vec![0.0; network.buffers.iter().copied().max().unwrap_or(0)],
            rings: network
                .history()
                .iter()
                .zip(&network.buffers)
                .map(|(&history, &channels)| {
                    (history > 0).then(|| Ring {
                        samples: vec![vec![0.0; history]; channels],
                        next: 0,
                    })
                })
                .collect(),
        }
    }

    pub(crate) fn reset(&mut self) {
        for ring in self.rings.iter_mut().flatten() {
            ring.samples
                .iter_mut()
                .for_each(|channel| channel.fill(0.0));
            ring.next = 0;
        }
    }

    /// One sample: returns the output buffer's value.
    pub(crate) fn run(&mut self, network: &Network, parameters: &[f32], input: f32) -> f32 {
        self.values[0][0] = input;
        for op in &network.ops {
            match op {
                Op::Gain { buffer, parameter } => {
                    let gain = parameters[*parameter];
                    self.values[*buffer].iter_mut().for_each(|v| *v *= gain);
                }
                Op::Conv {
                    input,
                    output,
                    kernel,
                    dilation,
                    weights,
                    bias,
                    accumulate,
                } => {
                    let (inputs, outputs) = (network.buffers[*input], network.buffers[*output]);
                    for o in 0..outputs {
                        let mut sum = bias.as_ref().map_or(0.0, |b| b[o]);
                        for i in 0..inputs {
                            for k in 0..*kernel {
                                let back = (kernel - 1 - k) * dilation;
                                sum += weights[(o * inputs + i) * kernel + k]
                                    * self.past(*input, i, back);
                            }
                        }
                        self.scratch[o] = sum;
                    }
                    let out = &mut self.values[*output];
                    for (value, &sum) in out.iter_mut().zip(&self.scratch[..outputs]) {
                        *value = if *accumulate { *value + sum } else { sum };
                    }
                }
                Op::Activation { buffer, function } => {
                    for value in &mut self.values[*buffer] {
                        *value = activate(*function, *value);
                    }
                }
                Op::Add { input, output } => {
                    let (input, output) = (*input, *output);
                    for c in 0..network.buffers[input] {
                        self.values[output][c] += self.values[input][c];
                    }
                }
                Op::Copy { input, output } => {
                    let (from, to) = (*input, *output);
                    let (source, target) = if from < to {
                        let (low, high) = self.values.split_at_mut(to);
                        (&low[from], &mut high[0])
                    } else {
                        let (low, high) = self.values.split_at_mut(from);
                        (&high[0], &mut low[to])
                    };
                    target.copy_from_slice(source);
                }
            }
        }
        for (ring, values) in self.rings.iter_mut().zip(&self.values) {
            if let Some(ring) = ring {
                for (channel, &value) in ring.samples.iter_mut().zip(values) {
                    channel[ring.next] = value;
                }
                ring.next = (ring.next + 1) % ring.samples[0].len();
            }
        }
        self.values[network.output][0]
    }

    /// `buffer[channel]` from `back` samples ago (0 = now).
    fn past(&self, buffer: usize, channel: usize, back: usize) -> f32 {
        if back == 0 {
            return self.values[buffer][channel];
        }
        let ring = self.rings[buffer]
            .as_ref()
            .expect("a convolution with history has a ring");
        let length = ring.samples[0].len();
        ring.samples[channel][(ring.next + length - back) % length]
    }
}

pub(crate) fn activate(function: Activation, x: f32) -> f32 {
    match function {
        Activation::Tanh => tanh(x),
        Activation::Hardtanh => x.max(-1.0).min(1.0),
        Activation::Relu => x.max(0.0),
    }
}

/// The scalar `tanh` expansion (`graph::Expander::tanh`), operation for operation.
pub(crate) fn tanh(x: f32) -> f32 {
    let x = x
        .max(-crate::graph::TANH_LIMIT)
        .min(crate::graph::TANH_LIMIT);
    let x2 = x * x;
    let numerator = 378.0 + x2 * 1.0;
    let numerator = 17325.0 + x2 * numerator;
    let numerator = 135_135.0 + x2 * numerator;
    let numerator = x * numerator;
    let denominator = 3150.0 + x2 * 28.0;
    let denominator = 62370.0 + x2 * denominator;
    let denominator = 135_135.0 + x2 * denominator;
    let mut estimate =
        f32::from_bits(crate::graph::RECIP_MAGIC.wrapping_sub(denominator.to_bits()));
    for _ in 0..2 {
        estimate *= 2.0 - denominator * estimate;
    }
    numerator * estimate
}

/// Builds a [`Network`] for [`crate::BlockBuilder::network`].
pub struct NetworkBuilder {
    network: Network,
}

/// A buffer of the network being built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buffer(usize);

impl Default for NetworkBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkBuilder {
    pub fn new() -> Self {
        Self {
            network: Network {
                buffers: vec![1],
                ops: Vec::new(),
                output: 0,
            },
        }
    }

    /// The block input, one channel.
    pub fn input(&self) -> Buffer {
        Buffer(0)
    }

    pub fn buffer(&mut self, channels: usize) -> Buffer {
        self.network.buffers.push(channels);
        Buffer(self.network.buffers.len() - 1)
    }

    /// `buffer *= parameter` (the block parameter at `parameter`).
    pub fn gain(&mut self, buffer: Buffer, parameter: usize) {
        self.network.ops.push(Op::Gain {
            buffer: buffer.0,
            parameter,
        });
    }

    /// A causal dilated convolution with weights ordered `[out][in][kernel]`; see [`Op::Conv`].
    #[allow(clippy::too_many_arguments)]
    pub fn conv(
        &mut self,
        input: Buffer,
        output: Buffer,
        kernel: usize,
        dilation: usize,
        weights: Vec<f32>,
        bias: Option<Vec<f32>>,
        accumulate: bool,
    ) {
        self.network.ops.push(Op::Conv {
            input: input.0,
            output: output.0,
            kernel,
            dilation,
            weights,
            bias,
            accumulate,
        });
    }

    pub fn activation(&mut self, buffer: Buffer, function: Activation) {
        self.network.ops.push(Op::Activation {
            buffer: buffer.0,
            function,
        });
    }

    /// `output += input`.
    pub fn add(&mut self, input: Buffer, output: Buffer) {
        self.network.ops.push(Op::Add {
            input: input.0,
            output: output.0,
        });
    }

    /// `output = input`.
    pub fn copy(&mut self, input: Buffer, output: Buffer) {
        self.network.ops.push(Op::Copy {
            input: input.0,
            output: output.0,
        });
    }

    pub fn finish(mut self, output: Buffer) -> Network {
        self.network.output = output.0;
        self.network
    }
}
