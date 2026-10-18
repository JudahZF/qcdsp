//! Generated VISA routines: init, process, set-parameter and fault recovery.
//!
//! Calling contract (stock Volume, see `docs/abi-status.md`):
//! - init(R4 = state): called by the entry; must leave R4 intact and finish in
//!   the stock init, which it reaches with a tail jump.
//! - process(R4 = state, R8 = io): `io[0]` points at 32 samples of channel A
//!   and `io[1]` points at 32 samples of channel B, processed in place.
//! - set_parameter(R4 = state, R8 = index, F12 = value). Catalogue entries are
//!   declared 0..1. Integer bits outside +0..=1 are rejected before arithmetic;
//!   accepted values map to the parameter's DSP range before storage.
//!
//! Every memory access is an unqualified normal-word access with word offsets,
//! so code is correct whether a pointer is byte- or normal-word-addressed.
//! Callee-preserved registers are saved in the C-runtime frame. M5 = 0, M6 = 1.

use qc_asm::{
    Condition, IndexRegister, Instruction, ModifyRegister, Register, UniversalRegister,
    VisaInstruction as V, VisaProgram,
};

use super::{
    layout::Layout,
    template::{BLOCK_SAMPLES, CHANNELS, Entries, STOCK_INIT, STOCK_PARCELS},
};
use crate::{
    Block, Result,
    codegen::{InputSource, Kernel, Step},
};

const COUNTER: u8 = 15;
const CURSOR: u8 = 14;
/// Callee-preserved data registers (CCES SHARC run-time model).
const SAVED_DATA: [u8; 10] = [3, 5, 6, 7, 9, 10, 11, 13, 14, 15];
const STATE: u8 = 0; // I0
const AUDIO: u8 = 1; // I1
const DELAY: u8 = 2; // I2, also M2
const SCRATCH_INDEX: u8 = 4; // I4, caller-saved

/// The generated routines: the parcels that follow the stock Volume code, their entries and
/// the modeled instructions of one process call (the resource estimate).
pub struct Generated {
    pub parcels: Vec<u16>,
    pub entries: Entries,
    pub cycles_per_call: u64,
}

fn r(index: u8) -> Register {
    Register::new(index).expect("register index below 16")
}
fn i(index: u8) -> IndexRegister {
    IndexRegister::new(index).expect("index register below 16")
}
fn m(index: u8) -> ModifyRegister {
    ModifyRegister::new(index).expect("modify register below 16")
}
fn ureg_r(index: u8) -> UniversalRegister {
    r(index).into()
}
fn ureg_i(index: u8) -> UniversalRegister {
    i(index).into()
}
fn ureg_m(index: u8) -> UniversalRegister {
    m(index).into()
}

fn word(offset: u32) -> i32 {
    i32::try_from(offset).expect("layout offsets fit in i32")
}

/// Data registers the sample loop leaves to the kernel: all but the loop
/// counter, and the delay cursor when the block has delay lines.
pub fn kernel_registers(block: &Block) -> u16 {
    let mut available: u16 = !(1 << COUNTER);
    if !block.delays.is_empty() {
        available &= !(1 << CURSOR);
    }
    available
}

pub fn generate(block: &Block, layout: &Layout, kernel: &Kernel) -> Result<Generated> {
    let init_start = STOCK_PARCELS;
    let init = init(block, layout, init_start)?;
    let process_start = init_start + init.len() as u32;
    let (process, cycles_per_call) = process(layout, kernel)?;
    let set_parameter_start = process_start + process.len() as u32;
    let set_parameter = set_parameter(block, layout)?;
    let recovery_start = set_parameter_start + set_parameter.len() as u32;
    let recovery = initialize(block, layout, recovery_start, false)?;
    let mut parcels = init;
    parcels.extend(process);
    parcels.extend(set_parameter);
    parcels.extend(recovery);
    Ok(Generated {
        parcels,
        entries: Entries {
            init: init_start,
            process: process_start,
            set_parameter: set_parameter_start,
            recovery: recovery_start,
        },
        cycles_per_call,
    })
}

/// Fill parameter defaults, zero the channel banks, set states and delay
/// cursors, then tail-jump to the stock init with R4 untouched.
fn init(block: &Block, layout: &Layout, start: u32) -> Result<Vec<u16>> {
    initialize(block, layout, start, true)
}

/// Recovery restores histories and cursors, keeping the current parameters.
fn initialize(block: &Block, layout: &Layout, start: u32, construction: bool) -> Result<Vec<u16>> {
    let mut code = VisaProgram::new();
    let base = i(SCRATCH_INDEX);
    code.push(V::Transfer {
        dst: ureg_i(SCRATCH_INDEX),
        src: ureg_r(4),
    });
    for (index, parameter) in block
        .parameters()
        .iter()
        .enumerate()
        .filter(|_| construction)
    {
        code.push(V::LoadImmediate {
            dst: ureg_r(0),
            value: parameter.default.to_bits(),
        });
        code.push(V::StoreDmOffset {
            src: ureg_r(0),
            index: base,
            offset: word(layout.parameters + index as u32),
        });
    }
    let bank_words = layout.banks[1] - layout.banks[0];
    if bank_words > 0 {
        // Zero both banks with a post-increment walk, then write initial values.
        code.push(V::LoadImmediate {
            dst: ureg_r(0),
            value: 0,
        });
        code.push(V::LoadImmediate {
            dst: ureg_r(1),
            value: bank_words * CHANNELS,
        });
        code.push(V::ModifyIndex {
            index: base,
            offset: word(layout.banks[0]),
        });
        code.label("zero");
        code.push(V::StoreDmPostModify {
            src: ureg_r(0),
            index: base,
            modifier: m(6),
        });
        code.push(V::IntegerSubOne {
            dst: r(1),
            src: r(1),
        });
        code.jump(Condition::Ne, "zero");
        code.push(V::Transfer {
            dst: ureg_i(SCRATCH_INDEX),
            src: ureg_r(4),
        });
        for bank in layout.banks {
            for (index, state) in block.states.iter().enumerate() {
                code.push(V::LoadImmediate {
                    dst: ureg_r(0),
                    value: state.initial.to_bits(),
                });
                code.push(V::StoreDmOffset {
                    src: ureg_r(0),
                    index: base,
                    offset: word(bank + layout.states + index as u32),
                });
            }
            for (index, &(_, length)) in layout.delays.iter().enumerate() {
                code.push(V::LoadImmediate {
                    dst: ureg_r(0),
                    value: length,
                });
                code.push(V::StoreDmOffset {
                    src: ureg_r(0),
                    index: base,
                    offset: word(bank + layout.cursors + index as u32),
                });
            }
        }
    }
    if !construction {
        code.push(V::LeafReturn);
        return Ok(code.parcels()?);
    }
    let before_jump = code.parcels()?.len() as i64;
    let offset = i64::from(STOCK_INIT) - (i64::from(start) + before_jump);
    code.push(V::JumpRelative {
        condition: Condition::Always,
        offset: i32::try_from(offset).expect("section 2 is far below 2^31 parcels"),
        delayed: false,
    });
    Ok(code.parcels()?)
}

/// Point I2 at delay `index`'s slot `cursor` in `bank`; leaves the cursor in R14.
fn delay_slot(code: &mut VisaProgram, layout: &Layout, bank: u32, index: usize) {
    let (buffer, _) = layout.delays[index];
    code.push(V::LoadDmOffset {
        dst: ureg_r(CURSOR),
        index: i(STATE),
        offset: word(bank + layout.cursors + index as u32),
    });
    code.push(V::Transfer {
        dst: ureg_m(DELAY),
        src: ureg_r(CURSOR),
    });
    code.push(V::Transfer {
        dst: ureg_i(DELAY),
        src: ureg_i(STATE),
    });
    // Cursors run L..1, so slot = buffer - 1 + cursor.
    code.push(V::ModifyIndex {
        index: i(DELAY),
        offset: word(bank + buffer) - 1,
    });
}

/// Emit one kernel step. `bank` is the channel bank (None in the control
/// phase, which touches only parameters and control slots). Returns the number
/// of delay pointer setups, each of which costs DAG load-to-use stalls.
fn step(
    code: &mut VisaProgram,
    kernel: &Kernel,
    layout: &Layout,
    bank: Option<(usize, u32)>,
    step: Step,
) -> u64 {
    let in_bank = |offset: u32| {
        let (_, bank) = bank.expect("channel steps run in the sample loop");
        word(bank + offset)
    };
    let load = |code: &mut VisaProgram, register: u8, offset: i32| {
        code.push(V::LoadDmOffset {
            dst: ureg_r(register),
            index: i(STATE),
            offset,
        })
    };
    let store = |code: &mut VisaProgram, register: u8, offset: i32| {
        code.push(V::StoreDmOffset {
            src: ureg_r(register),
            index: i(STATE),
            offset,
        })
    };
    match step {
        Step::Load { source, register } => match source {
            InputSource::Audio => code.push(V::LoadDmOffset {
                dst: ureg_r(register),
                index: i(AUDIO),
                offset: 0,
            }),
            InputSource::Parameter { index } => {
                load(code, register, word(layout.parameters + index as u32));
            }
            InputSource::Control { slot } => load(code, register, word(layout.controls + slot)),
            InputSource::State { index } => {
                load(code, register, in_bank(layout.states + index as u32));
            }
            InputSource::Delay { index } => {
                let (_, bank) = bank.expect("delay taps are per channel");
                delay_slot(code, layout, bank, index);
                code.push(V::LoadDmPreModify {
                    dst: ureg_r(register),
                    index: i(DELAY),
                    modifier: m(DELAY),
                });
                return 1;
            }
        },
        Step::Compute { instruction } => code.push(kernel.program.instructions()[instruction]),
        Step::Spill { slot, register } => store(code, register, in_bank(layout.spills + slot)),
        Step::Reload { slot, register } => load(code, register, in_bank(layout.spills + slot)),
        Step::StoreState { state, register } => {
            store(code, register, in_bank(layout.states + state as u32));
        }
        Step::StoreControl { slot, register } => {
            store(code, register, word(layout.controls + slot));
        }
        Step::StoreOutput { register } => code.push(V::StoreDmPostModify {
            src: ureg_r(register),
            index: i(AUDIO),
            modifier: m(6),
        }),
        Step::StoreDelay { delay, register } => {
            let (channel, bank) = bank.expect("delay writes are per channel");
            let wrapped = format!("wrapped{channel}_{delay}");
            delay_slot(code, layout, bank, delay);
            code.push(V::StoreDmPreModify {
                src: ureg_r(register),
                index: i(DELAY),
                modifier: m(DELAY),
            });
            code.push(V::IntegerSubOne {
                dst: r(CURSOR),
                src: r(CURSOR),
            });
            code.jump(Condition::Ne, &wrapped);
            code.push(V::LoadImmediate {
                dst: ureg_r(CURSOR),
                value: layout.delays[delay].1,
            });
            code.label(&wrapped);
            code.push(V::StoreDmOffset {
                src: ureg_r(CURSOR),
                index: i(STATE),
                offset: word(bank + layout.cursors + delay as u32),
            });
            return 1;
        }
    }
    0
}

/// Returns the code and a cycle estimate for one call (both channels).
fn process(layout: &Layout, kernel: &Kernel) -> Result<(Vec<u16>, u64)> {
    let contract = &kernel.contract;
    let saved: Vec<UniversalRegister> = SAVED_DATA
        .iter()
        .map(|&index| ureg_r(index))
        .chain([ureg_i(STATE), ureg_i(AUDIO), ureg_i(DELAY), ureg_m(DELAY)])
        .collect();
    let mut code = VisaProgram::new();
    code.push(V::ModifyIndex {
        index: i(7),
        offset: -(saved.len() as i32 + 1),
    });
    for (slot, &register) in saved.iter().enumerate() {
        // A DAG1 store may not take a DAG1 register as its source (ADI
        // UREGXDAG1), so I and M registers go through scratch R2, as stock does.
        let src = if register.code() >= 0x10 {
            code.push(V::Transfer {
                dst: ureg_r(2),
                src: register,
            });
            ureg_r(2)
        } else {
            register
        };
        code.push(V::StoreDmOffset {
            src,
            index: i(6),
            offset: -2 - slot as i32,
        });
    }
    code.push(V::Transfer {
        dst: ureg_i(STATE),
        src: ureg_r(4),
    });
    // Keep the descriptor above SP: sample code can use R8 and I2.
    let descriptor_slot = -2 - saved.len() as i32;
    code.push(V::StoreDmOffset {
        src: ureg_r(8),
        index: i(6),
        offset: descriptor_slot,
    });

    // Hoisted parameter math, once per call for both channels.
    let before = code.parcels()?.len();
    for &control in &contract.control {
        step(&mut code, kernel, layout, None, control);
    }
    let control_parcels = (code.parcels()?.len() - before) as u64;

    let mut per_sample = 0;
    for (channel, &bank) in layout.banks.iter().enumerate() {
        code.push(V::LoadDmOffset {
            dst: ureg_i(SCRATCH_INDEX),
            index: i(6),
            offset: descriptor_slot,
        });
        code.push(V::LoadDmOffset {
            dst: ureg_i(AUDIO),
            index: i(SCRATCH_INDEX),
            offset: channel as i32,
        });
        let top = format!("sample{channel}");
        code.push(V::LoadImmediate {
            dst: ureg_r(COUNTER),
            value: BLOCK_SAMPLES,
        });
        code.label(&top);
        let before = code.parcels()?.len();
        let mut delay_setups = 0;
        for &sample in &contract.sample {
            delay_setups += step(&mut code, kernel, layout, Some((channel, bank)), sample);
        }
        // Parcels over-count instructions; add the SHARC+ DAG load-to-use stalls
        // (4 cycles) for each delay pointer setup.
        let parcels = (code.parcels()?.len() - before) as u64;
        per_sample = per_sample.max(parcels + 4 * delay_setups + 2);
        code.push(V::IntegerSubOne {
            dst: r(COUNTER),
            src: r(COUNTER),
        });
        code.jump(Condition::Ne, &top);
    }

    for (slot, &register) in saved.iter().enumerate() {
        code.push(V::LoadDmOffset {
            dst: register,
            index: i(6),
            offset: -2 - slot as i32,
        });
    }
    code.push(V::LeafReturn);
    // Two channels of 32 samples, the per-sample body plus loop branch, the
    // control phase, and a fixed allowance for frame setup, saves and restores.
    let cycles = u64::from(CHANNELS * BLOCK_SAMPLES) * (per_sample + 8) + control_parcels + 120;
    Ok((code.parcels()?, cycles))
}

/// Store parameter R8 = min + F12 * (max - min). Values outside +0..=1 and indices outside the
/// block's range are ignored. Uses only scratch registers R0, R2, R12 and I4.
fn set_parameter(block: &Block, layout: &Layout) -> Result<Vec<u16>> {
    let mut code = VisaProgram::new();
    let value = r(12);
    code.push(V::Transfer {
        dst: ureg_i(SCRATCH_INDEX),
        src: ureg_r(4),
    });
    // Check integer bits first: floating MIN/MAX on NaN raises AIS.
    // Unsigned ordering accepts only +0..=1 and rejects negative zero too.
    code.push(V::LoadImmediate {
        dst: ureg_r(0),
        value: 1f32.to_bits(),
    });
    code.push(V::IntegerCompareUnsigned {
        lhs: value,
        rhs: r(0),
    });
    code.jump(Condition::UnsignedGt, "return");
    for (index, parameter) in block.parameters().iter().enumerate() {
        let next = format!("next{index}");
        if index == 0 {
            code.push(V::IntegerPass {
                dst: r(2),
                src: r(8),
            });
        } else {
            code.push(V::IntegerSubOne {
                dst: r(2),
                src: r(2),
            });
        }
        code.jump(Condition::Ne, &next);
        code.push(V::LoadImmediate {
            dst: ureg_r(0),
            value: (parameter.max - parameter.min).to_bits(),
        });
        code.push(Instruction::FloatMul {
            dst: value,
            lhs: value,
            rhs: r(0),
        });
        code.push(V::LoadImmediate {
            dst: ureg_r(0),
            value: parameter.min.to_bits(),
        });
        code.push(Instruction::FloatAdd {
            dst: value,
            lhs: value,
            rhs: r(0),
        });
        code.push(V::StoreDmOffset {
            src: ureg_r(12),
            index: i(SCRATCH_INDEX),
            offset: word(layout.parameters + index as u32),
        });
        code.jump(Condition::Always, "return");
        code.label(&next);
    }
    code.label("return");
    code.push(V::LeafReturn);
    Ok(code.parcels()?)
}
