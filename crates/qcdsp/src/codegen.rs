//! Lowers a block to scalar ISA48 arithmetic plus an ordered list of steps that
//! says when each value is loaded, computed, spilled and stored. The caller (the
//! device emitter or the host interpreter) maps the memory steps onto its own
//! memory; nothing here assumes the QC layout.
//!
//! - Expressions of parameters and constants only are hoisted into a control
//!   phase that runs once per process call and writes control slots.
//! - Inputs are loaded at first use and can be dropped and loaded again later.
//! - A state is written as soon as its new value exists and its old value has
//!   no more reads; after that the old value is never loaded from memory again.
//! - When registers run out, the value read furthest in the future is dropped:
//!   reloaded later if memory still holds it, otherwise spilled to a slot.

use qc_asm::{Instruction, Program, Register};
use serde::Serialize;

use crate::{
    Block, Error, Result,
    graph::{BinaryOp, Node},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputSource {
    Audio,
    Parameter {
        index: usize,
    },
    State {
        index: usize,
    },
    Delay {
        index: usize,
    },
    /// A hoisted value written by the control phase.
    Control {
        slot: u32,
    },
}

/// One action of a phase, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    Load {
        source: InputSource,
        register: u8,
    },
    /// Run instruction `instruction` of the kernel program.
    Compute {
        instruction: usize,
    },
    /// Save a value to this channel's spill slot (sample phase only).
    Spill {
        slot: u32,
        register: u8,
    },
    Reload {
        slot: u32,
        register: u8,
    },
    /// Write a state's next value. Its old value is not loaded again.
    StoreState {
        state: usize,
        register: u8,
    },
    /// Write a delay line's next value and advance its cursor.
    StoreDelay {
        delay: usize,
        register: u8,
    },
    /// Write the output sample. Always the last sample step.
    StoreOutput {
        register: u8,
    },
    /// Write a control slot (control phase only, including its spills).
    StoreControl {
        slot: u32,
        register: u8,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct Contract {
    /// Runs once per process call, before any sample.
    pub control: Vec<Step>,
    /// Runs once per sample and channel.
    pub sample: Vec<Step>,
    /// Slots shared by both channels: hoisted values, then control spills.
    pub control_slots: u32,
    /// Per-channel slots for spilled sample values.
    pub spill_slots: u32,
    pub registers_used: usize,
    pub instruction_count: usize,
    pub code_bytes: usize,
}

pub struct Kernel {
    pub program: Program,
    pub contract: Contract,
}

impl Kernel {
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(self.program.encode()?)
    }
}

pub fn compile(block: &Block) -> Result<Kernel> {
    compile_with(block, u16::MAX)
}

/// Like [`compile`], but allocate only data registers whose bit is set in
/// `available` (bit n = Rn). Device callbacks reserve loop and address registers.
pub fn compile_with(block: &Block, available: u16) -> Result<Kernel> {
    block.validate()?;
    let block = &block.expanded();
    let nodes = &block.nodes;
    let mut invariant = vec![false; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        invariant[index] = match node {
            Node::Constant { .. } | Node::Parameter { .. } => true,
            Node::Input | Node::State { .. } | Node::Delay { .. } => false,
            _ => node.operands().iter().all(|&operand| invariant[operand]),
        };
    }
    let hoisted = |index: usize| invariant[index] && is_computed(&nodes[index]);
    let sample_needed = reachable(nodes, &block.roots(), |index| !hoisted(index));
    let mut control_slot = vec![None; nodes.len()];
    let mut control_roots = Vec::new();
    for index in (0..nodes.len()).filter(|&index| sample_needed[index] && hoisted(index)) {
        control_slot[index] = Some(control_roots.len() as u32);
        control_roots.push(index);
    }

    let mut program = Program::new();
    let control_needed = reachable(nodes, &control_roots, |_| true);
    let mut control_ops = Vec::new();
    for index in
        (0..nodes.len()).filter(|&index| control_needed[index] && is_computed(&nodes[index]))
    {
        control_ops.push(Op::Compute(index));
        if let Some(slot) = control_slot[index] {
            control_ops.push(Op::StoreControl { slot, value: index });
        }
    }
    let control_sources = nodes
        .iter()
        .map(|node| match *node {
            Node::Constant { value } => Some(Source::Immediate(value.to_bits())),
            Node::Parameter { index } => Some(Source::Load(InputSource::Parameter { index })),
            _ => None,
        })
        .collect();
    let control = Allocator::new(
        nodes,
        control_sources,
        &control_ops,
        available,
        Phase::Control {
            first_slot: control_roots.len() as u32,
        },
    )
    .run(&control_ops, &mut program)?;

    let sample_sources: Vec<_> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            if let Some(slot) = control_slot[index] {
                return Some(Source::Load(InputSource::Control { slot }));
            }
            match *node {
                Node::Input => Some(Source::Load(InputSource::Audio)),
                Node::Parameter { index } => Some(Source::Load(InputSource::Parameter { index })),
                Node::State { index } => Some(Source::Load(InputSource::State { index })),
                Node::Delay { index } => Some(Source::Load(InputSource::Delay { index })),
                Node::Constant { value } => Some(Source::Immediate(value.to_bits())),
                _ => None,
            }
        })
        .collect();
    let sample_ops = sample_schedule(block, &sample_needed, &sample_sources);
    let sample = Allocator::new(nodes, sample_sources, &sample_ops, available, Phase::Sample)
        .run(&sample_ops, &mut program)?;

    let code_bytes = program.encode()?.len();
    let contract = Contract {
        control: control.steps,
        sample: sample.steps,
        control_slots: control.slots,
        spill_slots: sample.slots,
        registers_used: control.high_water.max(sample.high_water),
        instruction_count: program.instructions().len(),
        code_bytes,
    };
    Ok(Kernel { program, contract })
}

fn is_computed(node: &Node) -> bool {
    matches!(
        node,
        Node::Binary { .. } | Node::Neg { .. } | Node::Abs { .. }
    )
}

/// Nodes reachable from `roots`, descending only through nodes where `descend` holds.
fn reachable(nodes: &[Node], roots: &[usize], descend: impl Fn(usize) -> bool) -> Vec<bool> {
    let mut seen = vec![false; nodes.len()];
    let mut pending = roots.to_vec();
    while let Some(node) = pending.pop() {
        if std::mem::replace(&mut seen[node], true) {
            continue;
        }
        if descend(node) {
            pending.extend(nodes[node].operands());
        }
    }
    seen
}

/// Computations in graph order, each state or delay write placed right after
/// both its new value and the last computation that reads its old value, and
/// the output write last. The placement only keeps register pressure low:
/// correctness comes from the allocator, which loads any old value still needed
/// (for example by another write, as in a swap) before a write overwrites it.
fn sample_schedule(block: &Block, needed: &[bool], sources: &[Option<Source>]) -> Vec<Op> {
    let nodes = &block.nodes;
    let computed: Vec<usize> = (0..nodes.len())
        .filter(|&index| needed[index] && sources[index].is_none())
        .collect();
    let mut position = vec![None; nodes.len()];
    let mut last_read = vec![None; nodes.len()];
    for (ordinal, &index) in computed.iter().enumerate() {
        position[index] = Some(ordinal);
        for operand in nodes[index].operands() {
            last_read[operand] = Some(ordinal);
        }
    }
    let old_reads = |home: InputSource| {
        (0..nodes.len())
            .filter(|&index| sources[index] == Some(Source::Load(home)))
            .filter_map(|index| last_read[index])
            .max()
    };
    // `None` (before every computation) sorts first.
    let mut writes = Vec::new();
    for (state, memory) in block.states.iter().enumerate() {
        if let Some(value) = memory.update {
            let after = position[value].max(old_reads(InputSource::State { index: state }));
            writes.push((after, Op::StoreState { state, value }));
        }
    }
    for (delay, memory) in block.delays.iter().enumerate() {
        if let Some(value) = memory.update {
            let after = position[value].max(old_reads(InputSource::Delay { index: delay }));
            writes.push((after, Op::StoreDelay { delay, value }));
        }
    }
    let mut ops: Vec<Op> = writes
        .iter()
        .filter(|(after, _)| after.is_none())
        .map(|&(_, op)| op)
        .collect();
    for (ordinal, &index) in computed.iter().enumerate() {
        ops.push(Op::Compute(index));
        ops.extend(
            writes
                .iter()
                .filter(|(after, _)| *after == Some(ordinal))
                .map(|&(_, op)| op),
        );
    }
    ops.push(Op::StoreOutput {
        value: block.output,
    });
    ops
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Load(InputSource),
    Immediate(u32),
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Compute(usize),
    StoreState { state: usize, value: usize },
    StoreDelay { delay: usize, value: usize },
    StoreOutput { value: usize },
    StoreControl { slot: u32, value: usize },
}

impl Op {
    /// Nodes this op reads. `canonical` maps repeated source nodes (possible in
    /// imported IR) to one node, so a value is never held twice.
    fn reads(self, nodes: &[Node], canonical: &[usize]) -> Vec<usize> {
        let reads = match self {
            Self::Compute(index) => nodes[index].operands(),
            Self::StoreState { value, .. }
            | Self::StoreDelay { value, .. }
            | Self::StoreOutput { value }
            | Self::StoreControl { value, .. } => vec![value],
        };
        reads.into_iter().map(|node| canonical[node]).collect()
    }

    /// The memory this write overwrites, if loads read it.
    fn home(self) -> Option<InputSource> {
        match self {
            Self::StoreState { state, .. } => Some(InputSource::State { index: state }),
            Self::StoreDelay { delay, .. } => Some(InputSource::Delay { index: delay }),
            Self::StoreOutput { .. } => Some(InputSource::Audio),
            Self::Compute(_) | Self::StoreControl { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Spills go to control slots numbered from `first_slot`.
    Control {
        first_slot: u32,
    },
    Sample,
}

struct Allocated {
    steps: Vec<Step>,
    /// Control slots in use (hoisted + spills) or per-channel spill slots.
    slots: u32,
    high_water: usize,
}

struct Allocator<'a> {
    nodes: &'a [Node],
    sources: Vec<Option<Source>>,
    canonical: Vec<usize>,
    phase: Phase,
    /// Ascending op positions that read each node; `next[node]` indexes the
    /// first one not yet executed.
    reads: Vec<Vec<usize>>,
    next: Vec<usize>,
    /// The register holding each node, and the node in each register.
    holder: Vec<Option<u8>>,
    owner: [Option<usize>; 16],
    usable: u16,
    /// Memory still holds this source's value, so it can be loaded again.
    fresh: Vec<bool>,
    /// Slot that holds each value: a spill slot, or a hoisted value's own
    /// control slot. Slots below `fixed_slots` are hoisted homes, never reused.
    spilled: Vec<Option<u32>>,
    free_slots: Vec<u32>,
    fixed_slots: u32,
    slots: u32,
    steps: Vec<Step>,
    high_water: usize,
}

impl<'a> Allocator<'a> {
    fn new(
        nodes: &'a [Node],
        sources: Vec<Option<Source>>,
        ops: &[Op],
        usable: u16,
        phase: Phase,
    ) -> Self {
        let mut first = std::collections::HashMap::new();
        let canonical: Vec<usize> = (0..nodes.len())
            .map(|node| match sources[node] {
                Some(Source::Load(source)) => *first.entry(source).or_insert(node),
                _ => node,
            })
            .collect();
        let mut reads = vec![Vec::new(); nodes.len()];
        for (position, op) in ops.iter().enumerate() {
            for node in op.reads(nodes, &canonical) {
                reads[node].push(position);
            }
        }
        let fixed_slots = match phase {
            Phase::Control { first_slot } => first_slot,
            Phase::Sample => 0,
        };
        Self {
            nodes,
            fresh: sources.iter().map(Option::is_some).collect(),
            sources,
            canonical,
            phase,
            reads,
            next: vec![0; nodes.len()],
            holder: vec![None; nodes.len()],
            owner: [None; 16],
            usable,
            spilled: vec![None; nodes.len()],
            free_slots: Vec::new(),
            fixed_slots,
            slots: fixed_slots,
            steps: Vec::new(),
            high_water: 0,
        }
    }

    fn run(mut self, ops: &[Op], program: &mut Program) -> Result<Allocated> {
        for (position, &op) in ops.iter().enumerate() {
            let reads = op.reads(self.nodes, &self.canonical);
            let mut locked = 0u16;
            let mut registers = Vec::with_capacity(reads.len());
            for &node in &reads {
                registers.push(self.fetch(node, &mut locked, program)?);
            }
            for &node in &reads {
                while self.reads[node]
                    .get(self.next[node])
                    .is_some_and(|&read| read <= position)
                {
                    self.next[node] += 1;
                }
            }
            match op {
                Op::Compute(index) => {
                    // Reuse the register of an operand that dies here.
                    let dying = reads
                        .iter()
                        .copied()
                        .find(|&node| self.next_read(node).is_none());
                    let dst = match dying {
                        Some(node) => {
                            let register = self.holder[node].take().expect("operand is held");
                            self.owner[register as usize] = Some(index);
                            register
                        }
                        None => self.take(index, locked)?,
                    };
                    self.holder[index] = Some(dst);
                    program.push(lower(&self.nodes[index], dst, &registers)?);
                    self.steps.push(Step::Compute {
                        instruction: program.instructions().len() - 1,
                    });
                }
                Op::StoreControl { slot, value } => {
                    self.steps.push(Step::StoreControl {
                        slot,
                        register: registers[0],
                    });
                    // Its slot now holds it, so evicting it later costs no spill.
                    self.spilled[value] = Some(slot);
                }
                _ => {
                    let home = op.home().expect("memory writes have a home");
                    self.preserve(home, &mut locked, program)?;
                    let register = registers[0];
                    self.steps.push(match op {
                        Op::StoreState { state, .. } => Step::StoreState { state, register },
                        Op::StoreDelay { delay, .. } => Step::StoreDelay { delay, register },
                        _ => Step::StoreOutput { register },
                    });
                    for node in 0..self.nodes.len() {
                        if self.sources[node] == Some(Source::Load(home)) {
                            self.fresh[node] = false;
                        }
                    }
                }
            }
            for &node in &reads {
                if self.next_read(node).is_none() {
                    self.release(node);
                }
            }
        }
        Ok(Allocated {
            steps: self.steps,
            slots: self.slots,
            high_water: self.high_water,
        })
    }

    fn next_read(&self, node: usize) -> Option<usize> {
        self.reads[node].get(self.next[node]).copied()
    }

    /// The value can be dropped from its register without saving it.
    fn reloadable(&self, node: usize) -> bool {
        self.spilled[node].is_some() || self.fresh[node]
    }

    /// Put `node` in a register (loading, rematerializing or reloading it) and
    /// lock that register for the current op.
    fn fetch(&mut self, node: usize, locked: &mut u16, program: &mut Program) -> Result<u8> {
        if let Some(register) = self.holder[node] {
            *locked |= 1 << register;
            return Ok(register);
        }
        let register = self.take(node, *locked)?;
        *locked |= 1 << register;
        if self.fresh[node] {
            match self.sources[node].expect("fresh values are sources") {
                Source::Load(source) => self.steps.push(Step::Load { source, register }),
                Source::Immediate(value) => {
                    program.push(Instruction::LoadImmediate {
                        dst: Register::new(register)?,
                        value,
                    });
                    self.steps.push(Step::Compute {
                        instruction: program.instructions().len() - 1,
                    });
                }
            }
        } else {
            let slot = self.spilled[node].expect("a dropped value was spilled");
            self.steps.push(match self.phase {
                Phase::Control { .. } => Step::Load {
                    source: InputSource::Control { slot },
                    register,
                },
                Phase::Sample => Step::Reload { slot, register },
            });
        }
        Ok(register)
    }

    /// Before a write overwrites `home`, load every value read from it that is
    /// still needed and not otherwise kept, while memory still holds it.
    fn preserve(
        &mut self,
        home: InputSource,
        locked: &mut u16,
        program: &mut Program,
    ) -> Result<()> {
        for node in 0..self.nodes.len() {
            if self.sources[node] == Some(Source::Load(home))
                && self.fresh[node]
                && self.next_read(node).is_some()
            {
                self.fetch(node, locked, program)?;
            }
        }
        Ok(())
    }

    /// A register for `node`: a free one, or one taken from the value read
    /// furthest in the future, preferring values that need no spill.
    fn take(&mut self, node: usize, locked: u16) -> Result<u8> {
        let candidates = (0..16u8).filter(|&register| self.usable & (1 << register) != 0);
        let register = match candidates
            .clone()
            .find(|&register| self.owner[register as usize].is_none())
        {
            Some(register) => register,
            None => {
                let (register, victim) = candidates
                    .filter(|&register| locked & (1 << register) == 0)
                    .filter_map(|register| Some((register, self.owner[register as usize]?)))
                    .max_by_key(|&(_, victim)| {
                        (
                            self.reloadable(victim),
                            self.next_read(victim).unwrap_or(usize::MAX),
                        )
                    })
                    .ok_or(Error::RegisterPressure)?;
                if !self.reloadable(victim) {
                    self.spill(victim, register);
                }
                self.holder[victim] = None;
                register
            }
        };
        self.owner[register as usize] = Some(node);
        self.holder[node] = Some(register);
        let held = self.owner.iter().filter(|owner| owner.is_some()).count();
        self.high_water = self.high_water.max(held);
        Ok(register)
    }

    fn spill(&mut self, node: usize, register: u8) {
        let slot = self.free_slots.pop().unwrap_or_else(|| {
            self.slots += 1;
            self.slots - 1
        });
        self.spilled[node] = Some(slot);
        self.steps.push(match self.phase {
            Phase::Control { .. } => Step::StoreControl { slot, register },
            Phase::Sample => Step::Spill { slot, register },
        });
    }

    fn release(&mut self, node: usize) {
        if let Some(register) = self.holder[node].take() {
            self.owner[register as usize] = None;
        }
        if let Some(slot) = self.spilled[node].take()
            && slot >= self.fixed_slots
        {
            self.free_slots.push(slot);
        }
    }
}

fn lower(node: &Node, dst: u8, operands: &[u8]) -> Result<Instruction> {
    let dst = Register::new(dst)?;
    let operand = |index: usize| Register::new(operands[index]);
    Ok(match *node {
        Node::Binary { kind, .. } => {
            let (lhs, rhs) = (operand(0)?, operand(1)?);
            match kind {
                BinaryOp::Add => Instruction::FloatAdd { dst, lhs, rhs },
                BinaryOp::Sub => Instruction::FloatSub { dst, lhs, rhs },
                BinaryOp::Mul => Instruction::FloatMul { dst, lhs, rhs },
                BinaryOp::Min => Instruction::FloatMin { dst, lhs, rhs },
                BinaryOp::Max => Instruction::FloatMax { dst, lhs, rhs },
                BinaryOp::SubBits => Instruction::IntegerSub { dst, lhs, rhs },
                BinaryOp::Div => unreachable!("Block::expanded replaces div"),
            }
        }
        Node::Neg { .. } => Instruction::FloatNeg {
            dst,
            src: operand(0)?,
        },
        Node::Abs { .. } => Instruction::FloatAbs {
            dst,
            src: operand(0)?,
        },
        _ => unreachable!("only computed nodes are lowered"),
    })
}
