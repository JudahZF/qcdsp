use qc_asm::Instruction;

use crate::{
    Block, Error, Result,
    codegen::{InputSource, Kernel, Step},
    graph::{BinaryOp, Node},
    network::Evaluator,
};

struct DelayBuffer {
    samples: Vec<f32>,
    cursor: usize,
}

/// Memory and registers for running a lowered kernel step by step.
struct Lowered {
    kernel: Kernel,
    registers: [f32; 16],
    controls: Vec<f32>,
    spills: Vec<f32>,
}

/// A persistent mono host reference. Processing allocates no memory; parameters
/// are already in DSP units, without vendor-specific normalization or smoothing.
pub struct Processor {
    block: Block,
    parameters: Vec<f32>,
    states: Vec<f32>,
    delays: Vec<DelayBuffer>,
    values: Vec<f32>,
    network: Option<Evaluator>,
    lowered: Option<Lowered>,
}

impl Processor {
    pub fn new(block: Block) -> Result<Self> {
        block.validate()?;
        let block = block.expanded();
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
            network: block.network.as_ref().map(Evaluator::new),
            block,
            lowered: None,
        })
    }

    /// Interpret the lowered steps instead of the expression graph. Loads and
    /// stores hit this processor's memory in step order, so a write placed
    /// before a read of the old value shows up as a wrong output. This checks
    /// lowering and allocation, not binary encoding or SHARC float modes.
    pub fn compiled(block: Block) -> Result<Self> {
        let kernel = crate::codegen::compile(&block)?;
        Self::lowered(block, kernel)
    }

    /// Like [`Processor::compiled`], with a kernel already compiled from `block`
    /// (for example by `codegen::compile_with` with fewer registers).
    pub fn lowered(block: Block, kernel: Kernel) -> Result<Self> {
        let mut processor = Self::new(block)?;
        processor.lowered = Some(Lowered {
            controls: vec![0.0; kernel.contract.control_slots as usize],
            spills: vec![0.0; kernel.contract.spill_slots as usize],
            registers: [0.0; 16],
            kernel,
        });
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
        if let Some(network) = &mut self.network {
            network.reset();
        }
    }

    pub fn process_sample(&mut self, input: f32) -> f32 {
        let network = match (&mut self.network, &self.block.network) {
            (Some(evaluator), Some(network)) => evaluator.run(network, &self.parameters, input),
            _ => 0.0,
        };
        if let Some(mut lowered) = self.lowered.take() {
            let output = self.run_lowered(&mut lowered, input, network);
            self.lowered = Some(lowered);
            return output;
        }
        for (index, node) in self.block.nodes.iter().enumerate() {
            let value = match *node {
                Node::Input => input,
                Node::Network => network,
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
                    BinaryOp::SubBits => sub_bits(self.values[lhs], self.values[rhs]),
                    BinaryOp::Div => unreachable!("Block::expanded replaces div"),
                },
                Node::Neg { input } => -self.values[input],
                Node::Abs { input } => self.values[input].abs(),
                Node::Recip { .. } | Node::Tanh { .. } | Node::Sigmoid { .. } => {
                    unreachable!("Block::expanded replaces macro nodes")
                }
            };
            self.values[index] = value;
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

    /// The control phase runs every sample here; on the device it runs once per
    /// 32-sample call, which is equivalent while parameters are fixed.
    fn run_lowered(&mut self, lowered: &mut Lowered, input: f32, network: f32) -> f32 {
        let Lowered {
            kernel,
            registers,
            controls,
            spills,
        } = lowered;
        let mut output = f32::NAN;
        let contract = &kernel.contract;
        for &step in contract.control.iter().chain(&contract.sample) {
            match step {
                Step::Load { source, register } => {
                    registers[register as usize] = match source {
                        InputSource::Audio => input,
                        InputSource::Network => network,
                        InputSource::Parameter { index } => self.parameters[index],
                        InputSource::State { index } => self.states[index],
                        InputSource::Delay { index } => {
                            let delay = &self.delays[index];
                            delay.samples[delay.cursor]
                        }
                        InputSource::Control { slot } => controls[slot as usize],
                    }
                }
                Step::Compute { instruction } => {
                    execute(registers, kernel.program.instructions()[instruction]);
                }
                Step::Spill { slot, register } => {
                    spills[slot as usize] = registers[register as usize]
                }
                Step::Reload { slot, register } => {
                    registers[register as usize] = spills[slot as usize]
                }
                Step::StoreState { state, register } => {
                    self.states[state] = registers[register as usize];
                }
                Step::StoreDelay { delay, register } => {
                    let delay = &mut self.delays[delay];
                    delay.samples[delay.cursor] = registers[register as usize];
                    delay.cursor = (delay.cursor + 1) % delay.samples.len();
                }
                Step::StoreOutput { register } => output = registers[register as usize],
                Step::StoreControl { slot, register } => {
                    controls[slot as usize] = registers[register as usize];
                }
            }
        }
        output
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

fn execute(registers: &mut [f32; 16], instruction: Instruction) {
    let get = |register: qc_asm::Register| registers[register.index() as usize];
    let (dst, value) = match instruction {
        Instruction::LoadImmediate { dst, value } => (dst, f32::from_bits(value)),
        Instruction::Move { dst, src } => (dst, get(src)),
        Instruction::FloatAdd { dst, lhs, rhs } => (dst, get(lhs) + get(rhs)),
        Instruction::FloatSub { dst, lhs, rhs } => (dst, get(lhs) - get(rhs)),
        Instruction::FloatMul { dst, lhs, rhs } => (dst, get(lhs) * get(rhs)),
        Instruction::FloatMin { dst, lhs, rhs } => (dst, get(lhs).min(get(rhs))),
        Instruction::FloatMax { dst, lhs, rhs } => (dst, get(lhs).max(get(rhs))),
        Instruction::FloatNeg { dst, src } => (dst, -get(src)),
        Instruction::FloatAbs { dst, src } => (dst, get(src).abs()),
        Instruction::IntegerSub { dst, lhs, rhs } => (dst, sub_bits(get(lhs), get(rhs))),
        _ => unreachable!("DSP lowering emits only scalar register arithmetic"),
    };
    registers[dst.index() as usize] = value;
}

fn sub_bits(lhs: f32, rhs: f32) -> f32 {
    f32::from_bits(lhs.to_bits().wrapping_sub(rhs.to_bits()))
}
