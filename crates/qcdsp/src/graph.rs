use std::{
    collections::HashSet,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{Error, Result, network::Network};

static NEXT_GRAPH: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    GuitarOverdrive,
    GuitarAmplifier,
    BassAmplifier,
    BassOverdrive,
    Equalizer,
    Compressor,
    Delay,
    Modulation,
    Reverb,
    Wah,
    Utility,
    Pitch,
    Filter,
    Synth,
}

impl Category {
    /// The stock `ModelRepo.xml` category ID. Stock model IDs of a category lie in
    /// `id * 1000 ..= id * 1000 + 999`.
    pub fn id(self) -> u32 {
        match self {
            Self::GuitarOverdrive => 0,
            Self::GuitarAmplifier => 1,
            Self::BassAmplifier => 2,
            Self::BassOverdrive => 3,
            Self::Equalizer => 4,
            Self::Compressor => 5,
            Self::Delay => 6,
            Self::Modulation => 7,
            Self::Reverb => 8,
            Self::Wah => 9,
            Self::Utility => 16,
            Self::Pitch => 18,
            Self::Filter => 24,
            Self::Synth => 30,
        }
    }

    /// Categories that IR 1 readers do not know.
    fn needs_ir2(self) -> bool {
        matches!(
            self,
            Self::GuitarAmplifier
                | Self::BassAmplifier
                | Self::BassOverdrive
                | Self::Compressor
                | Self::Reverb
                | Self::Wah
                | Self::Synth
        )
    }
}

/// How many audio channels a block computes. A mono block runs once on channel A
/// and writes its result to both channels, at half the cost of a stereo block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channels {
    #[default]
    Stereo,
    Mono,
}

impl Channels {
    pub fn count(self) -> u32 {
        match self {
            Self::Stereo => 2,
            Self::Mono => 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub default: f32,
}

impl Parameter {
    pub fn new(name: impl Into<String>, min: f32, max: f32, default: f32) -> Result<Self> {
        let parameter = Self {
            name: name.into(),
            min,
            max,
            default,
        };
        parameter.validate()?;
        Ok(parameter)
    }

    fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty()
            || !self.min.is_finite()
            || !self.max.is_finite()
            || !self.default.is_finite()
            // The device maps 0..1 onto max - min: the span must be finite and not empty
            // (an empty one normalises the default to 0 / 0).
            || self.min >= self.max
            || !(self.max - self.min).is_finite()
            || !(self.min..=self.max).contains(&self.default)
        {
            return Err(Error::Invalid(format!("invalid parameter {:?}", self.name)));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signal {
    graph: u64,
    node: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct State {
    graph: u64,
    index: usize,
    signal: Signal,
}

impl State {
    pub fn signal(self) -> Signal {
        self.signal
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Delay {
    graph: u64,
    index: usize,
    signal: Signal,
}

impl Delay {
    pub fn signal(self) -> Signal {
        self.signal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BinaryOp {
    Add,
    Sub,
    Mul,
    Min,
    Max,
    /// Integer subtraction of the two bit patterns (SHARC fixed-point `Rn = Rx - Ry`).
    /// It seeds the reciprocal; it can produce any bit pattern, including NaN.
    SubBits,
    /// Expands to `lhs * recip(rhs)` (IR 2).
    Div,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Node {
    Input,
    Constant {
        value: f32,
    },
    Parameter {
        index: usize,
    },
    State {
        index: usize,
    },
    Delay {
        index: usize,
    },
    Binary {
        kind: BinaryOp,
        lhs: usize,
        rhs: usize,
    },
    Neg {
        input: usize,
    },
    Abs {
        input: usize,
    },
    /// The approximations below expand to primitive nodes before compilation
    /// ([`Block::expanded`]), so the host reference and the device compute the same thing.
    Recip {
        input: usize,
    },
    Tanh {
        input: usize,
    },
    Sigmoid {
        input: usize,
    },
    /// The output of the block's network for this sample (IR 3).
    Network,
}

impl Node {
    pub(crate) fn operands(&self) -> Vec<usize> {
        match self {
            Self::Binary { lhs, rhs, .. } => vec![*lhs, *rhs],
            Self::Neg { input }
            | Self::Abs { input }
            | Self::Recip { input }
            | Self::Tanh { input }
            | Self::Sigmoid { input } => vec![*input],
            _ => vec![],
        }
    }

    /// Nodes that only IR 2 may use.
    fn needs_ir2(&self) -> bool {
        matches!(
            self,
            Self::Binary {
                kind: BinaryOp::SubBits | BinaryOp::Div,
                ..
            } | Self::Recip { .. }
                | Self::Tanh { .. }
                | Self::Sigmoid { .. }
        )
    }

    /// True for nodes that [`Block::expanded`] replaces.
    pub(crate) fn is_macro(&self) -> bool {
        matches!(
            self,
            Self::Binary {
                kind: BinaryOp::Div,
                ..
            } | Self::Recip { .. }
                | Self::Tanh { .. }
                | Self::Sigmoid { .. }
        )
    }

    /// The number of primitive nodes that [`Block::expanded`] makes of this node.
    fn expanded_len(&self) -> usize {
        match self {
            Self::Binary {
                kind: BinaryOp::Div,
                ..
            } => RECIP_NODES + 1,
            Self::Recip { .. } => RECIP_NODES,
            Self::Tanh { .. } => TANH_NODES,
            Self::Sigmoid { .. } => TANH_NODES + 4,
            _ => 1,
        }
    }
}

/// `0x7ef311c7 - bits(x)` is within 12% of `1/x` for normal `x` of either sign
/// (the sign bit wraps); three Newton steps bring it to about 2^-22.
pub(crate) const RECIP_MAGIC: u32 = 0x7ef3_11c7;
const RECIP_NEWTON_STEPS: usize = 3;
/// Padé (7, 6) of tanh reaches 1 near here; outside it the output is clamped.
pub(crate) const TANH_LIMIT: f32 = 4.97;
/// Node counts of the expansions below; [`Block::expanded`] checks them in debug builds.
const RECIP_NODES: usize = 3 + 3 * RECIP_NEWTON_STEPS;
const TANH_NODES: usize = 36;

/// Builds the primitive expansion of macro nodes into a new node list.
struct Expander {
    nodes: Vec<Node>,
}

impl Expander {
    fn push(&mut self, node: Node) -> usize {
        self.nodes.push(node);
        self.nodes.len() - 1
    }
    fn constant(&mut self, value: f32) -> usize {
        self.push(Node::Constant { value })
    }
    fn binary(&mut self, kind: BinaryOp, lhs: usize, rhs: usize) -> usize {
        self.push(Node::Binary { kind, lhs, rhs })
    }
    fn clamp(&mut self, input: usize, limit: f32) -> usize {
        let low = self.constant(-limit);
        let high = self.constant(limit);
        let raised = self.binary(BinaryOp::Max, input, low);
        self.binary(BinaryOp::Min, raised, high)
    }
    /// `a + x * b`, the step of a Horner evaluation.
    fn mul_add(&mut self, a: f32, x: usize, b: usize) -> usize {
        let a = self.constant(a);
        let product = self.binary(BinaryOp::Mul, x, b);
        self.binary(BinaryOp::Add, a, product)
    }
    /// Valid for normal `|x|` in about 1e-37..1e37; outside it the result can be
    /// infinite or NaN, which faults the block's DSP slot.
    fn recip(&mut self, input: usize) -> usize {
        self.recip_steps(input, RECIP_NEWTON_STEPS)
    }
    fn recip_steps(&mut self, input: usize, steps: usize) -> usize {
        let magic = self.constant(f32::from_bits(RECIP_MAGIC));
        let two = self.constant(2.0);
        let mut estimate = self.binary(BinaryOp::SubBits, magic, input);
        for _ in 0..steps {
            let product = self.binary(BinaryOp::Mul, input, estimate);
            let correction = self.binary(BinaryOp::Sub, two, product);
            estimate = self.binary(BinaryOp::Mul, correction, estimate);
        }
        estimate
    }
    /// Within 1e-4 of tanh everywhere. The clamped input keeps `|result|` below 1 - 6e-6, so
    /// the result needs no clamp; two Newton steps (relative error 7e-6) suffice for it.
    fn tanh(&mut self, input: usize) -> usize {
        let x = self.clamp(input, TANH_LIMIT);
        let x2 = self.binary(BinaryOp::Mul, x, x);
        let one = self.constant(1.0);
        // x (135135 + 17325 x² + 378 x⁴ + x⁶) / (135135 + 62370 x² + 3150 x⁴ + 28 x⁶)
        let numerator = self.mul_add(378.0, x2, one);
        let numerator = self.mul_add(17325.0, x2, numerator);
        let numerator = self.mul_add(135_135.0, x2, numerator);
        let numerator = self.binary(BinaryOp::Mul, x, numerator);
        let denominator = self.constant(28.0);
        let denominator = self.mul_add(3150.0, x2, denominator);
        let denominator = self.mul_add(62370.0, x2, denominator);
        let denominator = self.mul_add(135_135.0, x2, denominator);
        let inverse = self.recip_steps(denominator, 2);
        self.binary(BinaryOp::Mul, numerator, inverse)
    }
    /// `0.5 + 0.5 tanh(x / 2)`.
    fn sigmoid(&mut self, input: usize) -> usize {
        let half = self.constant(0.5);
        let scaled = self.binary(BinaryOp::Mul, input, half);
        let tanh = self.tanh(scaled);
        let product = self.binary(BinaryOp::Mul, tanh, half);
        self.binary(BinaryOp::Add, half, product)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Memory<T> {
    pub initial: T,
    pub update: Option<usize>,
}

/// A validated, topologically ordered per-sample graph. Every state and delay
/// read sees the previous sample; updates take effect for the next sample.
/// Delay lengths are measured in samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub(crate) version: u32,
    pub(crate) blob_name: String,
    pub(crate) name: String,
    pub(crate) category: Category,
    /// IR 2; IR 1 blocks are stereo.
    #[serde(default, skip_serializing_if = "is_stereo")]
    pub(crate) channels: Channels,
    pub(crate) parameters: Vec<Parameter>,
    pub(crate) states: Vec<Memory<f32>>,
    pub(crate) delays: Vec<Memory<usize>>,
    pub(crate) nodes: Vec<Node>,
    pub(crate) output: usize,
    /// IR 3: vector operations that run before the graph; `network` nodes read the output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) network: Option<Network>,
}

fn is_stereo(channels: &Channels) -> bool {
    *channels == Channels::Stereo
}

/// Graph size limits. They bound host memory and compile time for untrusted IR;
/// the device budget (code and state size, CPU) is checked when the block is built.
/// `MAX_NODES` bounds the graph after macro expansion ([`Block::expanded`]).
pub const MAX_NODES: usize = 65_536;
pub const MAX_PARAMETERS: usize = 256;
pub const MAX_STATES: usize = 4096;

impl Block {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn blob_name(&self) -> &str {
        &self.blob_name
    }
    pub fn category(&self) -> Category {
        self.category
    }
    pub fn channels(&self) -> Channels {
        self.channels
    }
    pub fn parameters(&self) -> &[Parameter] {
        &self.parameters
    }
    pub fn version(&self) -> u32 {
        self.version
    }
    /// The number of nodes; [`Block::expanded`] gives the count that compilation sees.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The same graph under another identity. A host that installs blocks from many
    /// sources gives each one a unique blob name and its own display name. A category
    /// that IR 1 does not know raises the version to at least 2.
    pub fn renamed(
        mut self,
        blob_name: impl Into<String>,
        name: impl Into<String>,
        category: Category,
    ) -> Result<Self> {
        self.blob_name = blob_name.into();
        self.name = name.into();
        self.category = category;
        if self.needs_ir2() {
            self.version = self.version.max(2);
        }
        self.validate()?;
        Ok(self)
    }

    /// The same block with every macro node (`div`, `recip`, `tanh`, `sigmoid`)
    /// replaced by primitive nodes. Code generation and the host reference both run
    /// this form. A block without macro nodes is returned unchanged.
    pub fn expanded(&self) -> Self {
        if !self.nodes.iter().any(Node::is_macro) {
            return self.clone();
        }
        let mut expander = Expander { nodes: Vec::new() };
        let mut map = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let start = expander.nodes.len();
            let new = match *node {
                Node::Binary {
                    kind: BinaryOp::Div,
                    lhs,
                    rhs,
                } => {
                    let inverse = expander.recip(map[rhs]);
                    expander.binary(BinaryOp::Mul, map[lhs], inverse)
                }
                Node::Recip { input } => expander.recip(map[input]),
                Node::Tanh { input } => expander.tanh(map[input]),
                Node::Sigmoid { input } => expander.sigmoid(map[input]),
                Node::Binary { kind, lhs, rhs } => expander.binary(kind, map[lhs], map[rhs]),
                Node::Neg { input } => expander.push(Node::Neg { input: map[input] }),
                Node::Abs { input } => expander.push(Node::Abs { input: map[input] }),
                Node::Input
                | Node::Network
                | Node::Constant { .. }
                | Node::Parameter { .. }
                | Node::State { .. }
                | Node::Delay { .. } => expander.push(node.clone()),
            };
            debug_assert_eq!(expander.nodes.len() - start, node.expanded_len());
            map.push(new);
        }
        fn remap<T: Copy>(memories: &[Memory<T>], map: &[usize]) -> Vec<Memory<T>> {
            memories
                .iter()
                .map(|memory| Memory {
                    initial: memory.initial,
                    update: memory.update.map(|node| map[node]),
                })
                .collect()
        }
        Self {
            nodes: expander.nodes,
            output: map[self.output],
            states: remap(&self.states, &map),
            delays: remap(&self.delays, &map),
            ..self.clone()
        }
    }

    pub fn from_json(json: &str) -> Result<Self> {
        let block: Self = serde_json::from_str(json)?;
        block.validate()?;
        Ok(block)
    }

    pub fn to_json(&self) -> Result<String> {
        self.validate()?;
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn validate(&self) -> Result<()> {
        if !(1..=3).contains(&self.version) {
            return Err(Error::Invalid("unsupported IR version".into()));
        }
        let reads_network = self.nodes.contains(&Node::Network);
        match &self.network {
            Some(network) => {
                if self.version < 3 || self.channels != Channels::Mono {
                    return Err(Error::Invalid(
                        "a network needs IR version 3 and a mono block".into(),
                    ));
                }
                network.validate(self.parameters.len())?;
            }
            None if reads_network => {
                return Err(Error::Invalid("a network node needs a network".into()));
            }
            None => {}
        }
        if self.version == 1 && self.needs_ir2() {
            return Err(Error::Invalid(
                "mono blocks, sub_bits, div, recip, tanh, sigmoid and the amplifier, \
                 bass overdrive, compressor, reverb, wah and synth categories need IR version 2"
                    .into(),
            ));
        }
        if self.nodes.iter().map(Node::expanded_len).sum::<usize>() > MAX_NODES
            || self.parameters.len() > MAX_PARAMETERS
            || self.states.len() > MAX_STATES
            || self.delays.len() > MAX_STATES
        {
            return Err(Error::Invalid(format!(
                "block is too large (limits: {MAX_NODES} nodes after expansion, \
                 {MAX_PARAMETERS} parameters, {MAX_STATES} states and {MAX_STATES} delays)"
            )));
        }
        if qc_format::blob::validate_blob_name(&self.blob_name).is_err()
            || self.name.trim().is_empty()
        {
            return Err(Error::Invalid(
                "block needs a display name and an ASCII blob identifier".into(),
            ));
        }
        let mut names = HashSet::new();
        for parameter in &self.parameters {
            parameter.validate()?;
            if !names.insert(&parameter.name) {
                return Err(Error::Invalid("duplicate parameter name".into()));
            }
        }
        if self.output >= self.nodes.len() {
            return Err(Error::Invalid("missing output node".into()));
        }
        for (index, node) in self.nodes.iter().enumerate() {
            if node.operands().iter().any(|operand| *operand >= index) {
                return Err(Error::Invalid(format!(
                    "node {index} has a forward or invalid reference"
                )));
            }
            let valid = match node {
                Node::Constant { value } => value.is_finite(),
                Node::Parameter { index } => *index < self.parameters.len(),
                Node::State { index } => *index < self.states.len(),
                Node::Delay { index } => *index < self.delays.len(),
                _ => true,
            };
            if !valid {
                return Err(Error::Invalid(format!("invalid node {index}")));
            }
        }
        for state in &self.states {
            if !state.initial.is_finite()
                || state.update.is_some_and(|node| node >= self.nodes.len())
            {
                return Err(Error::Invalid("invalid state or state update".into()));
            }
        }
        let mut delay_samples = 0usize;
        for delay in &self.delays {
            // Bound host allocation as well as the eventual device resource estimate.
            delay_samples = delay_samples
                .checked_add(delay.initial)
                .ok_or_else(|| Error::Invalid("delay size overflow".into()))?;
            if delay.initial == 0
                || delay_samples > 16 * 1024 * 1024
                || delay.update.is_some_and(|node| node >= self.nodes.len())
            {
                return Err(Error::Invalid(
                    "invalid delay or delay update (limit: 16M total samples)".into(),
                ));
            }
        }
        Ok(())
    }

    fn needs_ir2(&self) -> bool {
        self.channels != Channels::Stereo
            || self.category.needs_ir2()
            || self.nodes.iter().any(Node::needs_ir2)
    }

    pub(crate) fn roots(&self) -> Vec<usize> {
        std::iter::once(self.output)
            .chain(self.states.iter().filter_map(|state| state.update))
            .chain(self.delays.iter().filter_map(|delay| delay.update))
            .collect()
    }
}

/// An explicit expression builder rather than a general-purpose Rust compiler.
/// Handles from different builders are rejected before a graph is emitted.
pub struct BlockBuilder {
    id: u64,
    block: Block,
    input: Signal,
}

impl BlockBuilder {
    pub fn new(blob_name: impl Into<String>, name: impl Into<String>, category: Category) -> Self {
        let id = NEXT_GRAPH.fetch_add(1, Ordering::Relaxed);
        Self {
            id,
            block: Block {
                version: 1,
                blob_name: blob_name.into(),
                name: name.into(),
                category,
                channels: Channels::Stereo,
                parameters: vec![],
                states: vec![],
                delays: vec![],
                nodes: vec![Node::Input],
                output: 0,
                network: None,
            },
            input: Signal { graph: id, node: 0 },
        }
    }

    pub fn input(&self) -> Signal {
        self.input
    }

    fn push(&mut self, node: Node) -> Signal {
        let signal = Signal {
            graph: self.id,
            node: self.block.nodes.len(),
        };
        self.block.nodes.push(node);
        signal
    }

    fn check(&self, signal: Signal) -> Result<usize> {
        if signal.graph != self.id {
            return Err(Error::ForeignSignal);
        }
        Ok(signal.node)
    }

    pub fn constant(&mut self, value: f32) -> Result<Signal> {
        if !value.is_finite() {
            return Err(Error::Invalid("constant must be finite".into()));
        }
        Ok(self.push(Node::Constant { value }))
    }

    pub fn parameter(&mut self, parameter: Parameter) -> Result<Signal> {
        parameter.validate()?;
        if self
            .block
            .parameters
            .iter()
            .any(|existing| existing.name == parameter.name)
        {
            return Err(Error::Invalid("duplicate parameter name".into()));
        }
        let index = self.block.parameters.len();
        self.block.parameters.push(parameter);
        Ok(self.push(Node::Parameter { index }))
    }

    pub fn state(&mut self, initial: f32) -> Result<State> {
        if !initial.is_finite() {
            return Err(Error::Invalid("state must be finite".into()));
        }
        let index = self.block.states.len();
        self.block.states.push(Memory {
            initial,
            update: None,
        });
        Ok(State {
            graph: self.id,
            index,
            signal: self.push(Node::State { index }),
        })
    }

    pub fn delay(&mut self, samples: usize) -> Result<Delay> {
        if samples == 0 || samples > 16 * 1024 * 1024 {
            return Err(Error::Invalid("delay length out of range".into()));
        }
        let index = self.block.delays.len();
        self.block.delays.push(Memory {
            initial: samples,
            update: None,
        });
        Ok(Delay {
            graph: self.id,
            index,
            signal: self.push(Node::Delay { index }),
        })
    }

    pub fn update_state(&mut self, state: State, value: Signal) -> Result<()> {
        let node = self.check(value)?;
        if state.graph != self.id {
            return Err(Error::ForeignSignal);
        }
        let memory = &mut self.block.states[state.index];
        if memory.update.is_some() {
            return Err(Error::Invalid("state already has an update".into()));
        }
        memory.update = Some(node);
        Ok(())
    }

    pub fn update_delay(&mut self, delay: Delay, value: Signal) -> Result<()> {
        let node = self.check(value)?;
        if delay.graph != self.id {
            return Err(Error::ForeignSignal);
        }
        let memory = &mut self.block.delays[delay.index];
        if memory.update.is_some() {
            return Err(Error::Invalid("delay already has an update".into()));
        }
        memory.update = Some(node);
        Ok(())
    }

    fn binary(&mut self, kind: BinaryOp, lhs: Signal, rhs: Signal) -> Result<Signal> {
        let lhs = self.check(lhs)?;
        let rhs = self.check(rhs)?;
        Ok(self.push(Node::Binary { kind, lhs, rhs }))
    }

    pub fn add(&mut self, lhs: Signal, rhs: Signal) -> Result<Signal> {
        self.binary(BinaryOp::Add, lhs, rhs)
    }
    pub fn sub(&mut self, lhs: Signal, rhs: Signal) -> Result<Signal> {
        self.binary(BinaryOp::Sub, lhs, rhs)
    }
    pub fn mul(&mut self, lhs: Signal, rhs: Signal) -> Result<Signal> {
        self.binary(BinaryOp::Mul, lhs, rhs)
    }
    pub fn min(&mut self, lhs: Signal, rhs: Signal) -> Result<Signal> {
        self.binary(BinaryOp::Min, lhs, rhs)
    }
    pub fn max(&mut self, lhs: Signal, rhs: Signal) -> Result<Signal> {
        self.binary(BinaryOp::Max, lhs, rhs)
    }

    pub fn neg(&mut self, input: Signal) -> Result<Signal> {
        let input = self.check(input)?;
        Ok(self.push(Node::Neg { input }))
    }
    pub fn abs(&mut self, input: Signal) -> Result<Signal> {
        let input = self.check(input)?;
        Ok(self.push(Node::Abs { input }))
    }

    /// `lhs / rhs` as `lhs * recip(rhs)`; see [`BlockBuilder::recip`].
    pub fn div(&mut self, lhs: Signal, rhs: Signal) -> Result<Signal> {
        self.binary(BinaryOp::Div, lhs, rhs)
    }
    /// `1 / input`, relative error about 2^-22. `|input|` must be normal and in about
    /// 1e-37..1e37: outside it the result can be infinite or NaN, which faults the
    /// block's DSP slot.
    pub fn recip(&mut self, input: Signal) -> Result<Signal> {
        let input = self.check(input)?;
        Ok(self.push(Node::Recip { input }))
    }
    /// Padé (7, 6) tanh, clamped: within 1e-4 of tanh for every finite input.
    pub fn tanh(&mut self, input: Signal) -> Result<Signal> {
        let input = self.check(input)?;
        Ok(self.push(Node::Tanh { input }))
    }
    /// `0.5 + 0.5 tanh(input / 2)`.
    pub fn sigmoid(&mut self, input: Signal) -> Result<Signal> {
        let input = self.check(input)?;
        Ok(self.push(Node::Sigmoid { input }))
    }

    /// Run `network` before the graph and return its output for this sample. A block has at
    /// most one network, and a network block is mono.
    pub fn network(&mut self, network: Network) -> Result<Signal> {
        if self.block.network.is_some() {
            return Err(Error::Invalid("a block has at most one network".into()));
        }
        network.validate(self.block.parameters.len())?;
        self.block.network = Some(network);
        self.block.channels = Channels::Mono;
        Ok(self.push(Node::Network))
    }

    /// Compute channel A only and copy it to channel B (half the cost of stereo).
    pub fn mono(&mut self) {
        self.block.channels = Channels::Mono;
    }

    pub fn finish(mut self, output: Signal) -> Result<Block> {
        self.block.output = self.check(output)?;
        // Write the oldest IR version that holds the block, so older readers accept it.
        if self.block.network.is_some() {
            self.block.version = 3;
        } else if self.block.needs_ir2() {
            self.block.version = 2;
        }
        self.block.validate()?;
        Ok(self.block)
    }
}
