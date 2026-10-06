use crate::{
    Block, Error, Result,
    graph::{BinaryOp, Node},
};

struct DelayBuffer {
    samples: Vec<f32>,
    cursor: usize,
}

/// A persistent mono host reference. Processing allocates no memory; parameters
/// are already in DSP units, without vendor-specific normalization or smoothing.
pub struct Processor {
    block: Block,
    parameters: Vec<f32>,
    states: Vec<f32>,
    delays: Vec<DelayBuffer>,
    values: Vec<f32>,
    kernel: Option<crate::codegen::Kernel>,
    registers: [f32; 16],
}

impl Processor {
    pub fn new(block: Block) -> Result<Self> {
        block.validate()?;
        Ok(Self {
            parameters: block
                .parameters
                .iter()
                .map(|parameter| parameter.default)
                .collect(),
            states: block.states.iter().map(|state| state.initial).collect(),
            delays: block
                .delays
                .iter()
                .map(|delay| DelayBuffer {
                    samples: vec![0.0; delay.initial],
                    cursor: 0,
                })
                .collect(),
            values: vec![0.0; block.nodes.len()],
            block,
            kernel: None,
            registers: [0.0; 16],
        })
    }

    /// Interpret the lowered register instructions instead of the expression
    /// graph. This checks lowering/allocation, not binary encoding or hardware
    /// floating-point modes. State/delay storage is provided by this host harness.
    pub fn compiled(block: Block) -> Result<Self> {
        let kernel = crate::codegen::compile(&block)?;
        let mut processor = Self::new(block)?;
        processor.kernel = Some(kernel);
        Ok(processor)
    }

    pub fn set_parameter(&mut self, index: usize, value: f32) -> Result<()> {
        let parameter = self
            .block
            .parameters
            .get(index)
            .ok_or(Error::UnknownParameter(index))?;
        if !value.is_finite() || !(parameter.min..=parameter.max).contains(&value) {
            return Err(Error::ParameterRange {
                name: parameter.name.clone(),
                min: parameter.min,
                max: parameter.max,
            });
        }
        self.parameters[index] = value;
        Ok(())
    }

    /// Reset audio memory, not parameter values.
    pub fn reset(&mut self) {
        for (value, state) in self.states.iter_mut().zip(&self.block.states) {
            *value = state.initial;
        }
        for delay in &mut self.delays {
            delay.samples.fill(0.0);
            delay.cursor = 0;
        }
        self.values.fill(0.0);
    }

    pub fn process_sample(&mut self, input: f32) -> f32 {
        if let Some(kernel) = &self.kernel {
            use crate::codegen::InputSource;
            use qc_asm::Instruction;
            for binding in &kernel.contract.inputs {
                self.registers[binding.register as usize] = match binding.source {
                    InputSource::Audio => input,
                    InputSource::Parameter { index } => self.parameters[index],
                    InputSource::State { index } => self.states[index],
                    InputSource::Delay { index } => {
                        let delay = &self.delays[index];
                        delay.samples[delay.cursor]
                    }
                };
            }
            for instruction in kernel.program.instructions() {
                let (destination, value) = match *instruction {
                    Instruction::LoadImmediate { dst, value } => (dst, f32::from_bits(value)),
                    Instruction::Move { dst, src } => (dst, self.registers[src.index() as usize]),
                    Instruction::FloatAdd { dst, lhs, rhs } => (
                        dst,
                        self.registers[lhs.index() as usize] + self.registers[rhs.index() as usize],
                    ),
                    Instruction::FloatSub { dst, lhs, rhs } => (
                        dst,
                        self.registers[lhs.index() as usize] - self.registers[rhs.index() as usize],
                    ),
                    Instruction::FloatMul { dst, lhs, rhs } => (
                        dst,
                        self.registers[lhs.index() as usize] * self.registers[rhs.index() as usize],
                    ),
                    Instruction::FloatMin { dst, lhs, rhs } => (
                        dst,
                        self.registers[lhs.index() as usize]
                            .min(self.registers[rhs.index() as usize]),
                    ),
                    Instruction::FloatMax { dst, lhs, rhs } => (
                        dst,
                        self.registers[lhs.index() as usize]
                            .max(self.registers[rhs.index() as usize]),
                    ),
                    Instruction::FloatNeg { dst, src } => {
                        (dst, -self.registers[src.index() as usize])
                    }
                    _ => unreachable!("DSP lowering emits only scalar register arithmetic"),
                };
                self.registers[destination.index() as usize] = value;
            }
            self.values[self.block.output] = self.registers[kernel.contract.output as usize];
            for (memory, register) in self.block.states.iter().zip(&kernel.contract.state_updates) {
                if let (Some(node), Some(register)) = (memory.update, register) {
                    self.values[node] = self.registers[*register as usize];
                }
            }
            for (memory, register) in self.block.delays.iter().zip(&kernel.contract.delay_updates) {
                if let (Some(node), Some(register)) = (memory.update, register) {
                    self.values[node] = self.registers[*register as usize];
                }
            }
        } else {
            for (index, node) in self.block.nodes.iter().enumerate() {
                let value = match *node {
                    Node::Input => input,
                    Node::Constant { value } => value,
                    Node::Parameter { index } => self.parameters[index],
                    Node::State { index } => self.states[index],
                    Node::Delay { index } => {
                        let delay = &self.delays[index];
                        delay.samples[delay.cursor]
                    }
                    Node::Binary { kind, lhs, rhs } => match kind {
                        BinaryOp::Add => self.values[lhs] + self.values[rhs],
                        BinaryOp::Sub => self.values[lhs] - self.values[rhs],
                        BinaryOp::Mul => self.values[lhs] * self.values[rhs],
                        BinaryOp::Min => self.values[lhs].min(self.values[rhs]),
                        BinaryOp::Max => self.values[lhs].max(self.values[rhs]),
                    },
                    Node::Neg { input } => -self.values[input],
                };
                self.values[index] = value;
            }
        }
        // Every update reads this sample's graph values, never a just-updated state.
        for (state, memory) in self.states.iter_mut().zip(&self.block.states) {
            if let Some(update) = memory.update {
                *state = self.values[update];
            }
        }
        for (delay, memory) in self.delays.iter_mut().zip(&self.block.delays) {
            if let Some(update) = memory.update {
                delay.samples[delay.cursor] = self.values[update];
                delay.cursor = (delay.cursor + 1) % delay.samples.len();
            }
        }
        self.values[self.block.output]
    }

    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<()> {
        if input.len() != output.len() {
            return Err(Error::Invalid("input/output lengths differ".into()));
        }
        for (&sample, result) in input.iter().zip(output) {
            *result = self.process_sample(sample);
        }
        Ok(())
    }

    pub fn process_in_place(&mut self, samples: &mut [f32]) {
        for sample in samples {
            *sample = self.process_sample(*sample);
        }
    }
}
