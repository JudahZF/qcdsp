use std::{
    collections::HashSet,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

static NEXT_GRAPH: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    GuitarOverdrive,
    Equalizer,
    Delay,
    Utility,
}

impl Category {
    pub fn id(self) -> u32 {
        match self {
            Self::GuitarOverdrive => 0,
            Self::Equalizer => 4,
            Self::Delay => 6,
            Self::Utility => 16,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
            || self.min > self.max
            // The device maps 0..1 onto max - min, so the span must be finite too.
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BinaryOp {
    Add,
    Sub,
    Mul,
    Min,
    Max,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl Node {
    pub(crate) fn operands(&self) -> Vec<usize> {
        match self {
            Self::Binary { lhs, rhs, .. } => vec![*lhs, *rhs],
            Self::Neg { input } => vec![*input],
            _ => vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Memory<T> {
    pub initial: T,
    pub update: Option<usize>,
}

/// A validated, topologically ordered per-sample graph. State updates happen
/// together after the output is evaluated. Delay lengths are measured in samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub(crate) version: u32,
    pub(crate) blob_name: String,
    pub(crate) name: String,
    pub(crate) category: Category,
    pub(crate) parameters: Vec<Parameter>,
    pub(crate) states: Vec<Memory<f32>>,
    pub(crate) delays: Vec<Memory<usize>>,
    pub(crate) nodes: Vec<Node>,
    pub(crate) output: usize,
}

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
    pub fn parameters(&self) -> &[Parameter] {
        &self.parameters
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
        if self.version != 1 {
            return Err(Error::Invalid("unsupported IR version".into()));
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
                parameters: vec![],
                states: vec![],
                delays: vec![],
                nodes: vec![Node::Input],
                output: 0,
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

    pub fn finish(mut self, output: Signal) -> Result<Block> {
        self.block.output = self.check(output)?;
        self.block.validate()?;
        Ok(self.block)
    }
}
