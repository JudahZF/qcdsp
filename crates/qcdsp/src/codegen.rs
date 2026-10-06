//! ISA48 scalar fragments with an explicit, project-defined register contract.
//! The caller loads inputs, parameters, state and delay taps before each sample
//! and commits the returned state/delay writes afterward. No QC callbacks,
//! memory access, hardware loops, stack frames or kernel services are implied.

use qc_asm::{Instruction, Program, Register};
use serde::Serialize;

use crate::{
    Block, Error, Result,
    graph::{BinaryOp, Node},
};

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputSource {
    Audio,
    Parameter { index: usize },
    State { index: usize },
    Delay { index: usize },
}

#[derive(Debug, Clone, Serialize)]
pub struct Binding {
    pub source: InputSource,
    pub register: u8,
}

#[derive(Debug, Clone, Serialize)]
pub struct Contract {
    pub inputs: Vec<Binding>,
    pub output: u8,
    pub state_updates: Vec<Option<u8>>,
    pub delay_updates: Vec<Option<u8>>,
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

/// Lower reachable expressions, keeping sample outputs live until the caller
/// commits them. Reuse dying operands; reject pressure rather than emit spills
/// into memory whose runtime ownership has not been established.
pub fn compile(block: &Block) -> Result<Kernel> {
    compile_with(block, u16::MAX)
}

/// Like [`compile`], but allocate only data registers whose bit is set in
/// `available` (bit n = Rn). Device callbacks reserve loop and address registers.
pub fn compile_with(block: &Block, available: u16) -> Result<Kernel> {
    block.validate()?;
    let roots = block.roots();
    let mut needed = vec![false; block.nodes.len()];
    let mut pending = roots.clone();
    while let Some(node) = pending.pop() {
        if needed[node] {
            continue;
        }
        needed[node] = true;
        pending.extend(block.nodes[node].operands());
    }
    let mut uses = vec![0usize; block.nodes.len()];
    for (index, node) in block.nodes.iter().enumerate() {
        if needed[index] {
            for operand in node.operands() {
                uses[operand] += 1;
            }
        }
    }
    for root in roots {
        uses[root] += 1;
    }

    let mut registers = vec![None; block.nodes.len()];
    let mut occupied: [bool; 16] = std::array::from_fn(|index| available & (1 << index) == 0);
    let reserved = 16 - available.count_ones() as usize;
    let mut high_water = 0;
    let mut inputs = Vec::new();
    // Inputs are all loaded before the first instruction, not lazily midway.
    for (index, node) in block.nodes.iter().enumerate() {
        if !needed[index] {
            continue;
        }
        let source = match *node {
            Node::Input => InputSource::Audio,
            Node::Parameter { index } => InputSource::Parameter { index },
            Node::State { index } => InputSource::State { index },
            Node::Delay { index } => InputSource::Delay { index },
            _ => continue,
        };
        let register = allocate(&mut occupied)?;
        high_water = high_water.max(occupied.iter().filter(|&&used| used).count() - reserved);
        registers[index] = Some(register);
        inputs.push(Binding { source, register });
    }

    let mut program = Program::new();
    for (index, node) in block.nodes.iter().enumerate() {
        if !needed[index] || registers[index].is_some() {
            continue;
        }
        let operands = node.operands();
        for &operand in &operands {
            uses[operand] -= 1;
        }
        let reused = operands.iter().copied().find(|&operand| uses[operand] == 0);
        let dst = if let Some(operand) = reused {
            registers[operand].expect("validated topological graph")
        } else {
            allocate(&mut occupied)?
        };
        high_water = high_water.max(occupied.iter().filter(|&&used| used).count() - reserved);
        let dst_reg = Register::new(dst)?;
        let instruction = match *node {
            Node::Constant { value } => Instruction::LoadImmediate {
                dst: dst_reg,
                value: value.to_bits(),
            },
            Node::Binary { kind, lhs, rhs } => {
                let lhs = Register::new(registers[lhs].expect("live lhs"))?;
                let rhs = Register::new(registers[rhs].expect("live rhs"))?;
                match kind {
                    BinaryOp::Add => Instruction::FloatAdd {
                        dst: dst_reg,
                        lhs,
                        rhs,
                    },
                    BinaryOp::Sub => Instruction::FloatSub {
                        dst: dst_reg,
                        lhs,
                        rhs,
                    },
                    BinaryOp::Mul => Instruction::FloatMul {
                        dst: dst_reg,
                        lhs,
                        rhs,
                    },
                    BinaryOp::Min => Instruction::FloatMin {
                        dst: dst_reg,
                        lhs,
                        rhs,
                    },
                    BinaryOp::Max => Instruction::FloatMax {
                        dst: dst_reg,
                        lhs,
                        rhs,
                    },
                }
            }
            Node::Neg { input } => Instruction::FloatNeg {
                dst: dst_reg,
                src: Register::new(registers[input].expect("live input"))?,
            },
            _ => unreachable!("input nodes were bound above"),
        };
        program.push(instruction);
        for operand in operands {
            if uses[operand] == 0
                && let Some(register) = registers[operand].take()
                && register != dst
            {
                occupied[register as usize] = false;
            }
        }
        registers[index] = Some(dst);
    }
    let output = registers[block.output].expect("output is live");
    let state_updates = block
        .states
        .iter()
        .map(|memory| {
            memory
                .update
                .map(|node| registers[node].expect("state update is live"))
        })
        .collect();
    let delay_updates = block
        .delays
        .iter()
        .map(|memory| {
            memory
                .update
                .map(|node| registers[node].expect("delay update is live"))
        })
        .collect();
    let code_bytes = program.encode()?.len();
    let contract = Contract {
        inputs,
        output,
        state_updates,
        delay_updates,
        registers_used: high_water,
        instruction_count: program.instructions().len(),
        code_bytes,
    };
    Ok(Kernel { program, contract })
}

fn allocate(occupied: &mut [bool; 16]) -> Result<u8> {
    let index = occupied
        .iter()
        .position(|used| !used)
        .ok_or(Error::RegisterPressure)?;
    occupied[index] = true;
    Ok(index as u8)
}
