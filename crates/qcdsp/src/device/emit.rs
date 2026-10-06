//! Generated VISA callbacks: init, process and set-parameter.
//!
//! Calling contract (stock Volume, see `docs/abi-status.md`):
//! - init(R4 = state): called by the entry; must leave R4 intact and finish in
//!   the stock init, which it reaches with a tail jump.
//! - process(R4 = state, R8 = io): `io[0]` points at 32 samples of channel A
//!   followed by 32 samples of channel B, processed in place.
//! - set_parameter(R4 = state, R8 = index, F12 = value). Catalogue entries are
//!   declared 0..1, so raw and normalized delivery agree; the value is clamped
//!   to 0..1 and mapped to the parameter's DSP range before it is stored.
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
    codegen::{self, InputSource},
};

const COUNTER: u8 = 15;
const CURSOR: u8 = 14;
/// Callee-preserved data registers (CCES SHARC run-time model).
const SAVED_DATA: [u8; 10] = [3, 5, 6, 7, 9, 10, 11, 13, 14, 15];
const STATE: u8 = 0; // I0
const AUDIO: u8 = 1; // I1
const DELAY: u8 = 2; // I2, also M2
const SCRATCH_INDEX: u8 = 4; // I4, caller-saved

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

pub fn generate(block: &Block, layout: &Layout) -> Result<Generated> {
    let init_start = STOCK_PARCELS;
    let init = init(block, layout, init_start)?;
    let process_start = init_start + init.len() as u32;
    let (process, per_sample) = process(block, layout)?;
    let set_parameter_start = process_start + process.len() as u32;
    let set_parameter = set_parameter(block, layout)?;
    let mut parcels = init;
    parcels.extend(process);
    parcels.extend(set_parameter);
    // Two channels of 32 samples, the per-sample body plus loop branch, and a
    // fixed allowance for frame setup, saves and restores.
    let cycles_per_call = u64::from(CHANNELS * BLOCK_SAMPLES) * (per_sample + 8) + 120;
    Ok(Generated {
        parcels,
        entries: Entries {
            init: init_start,
            process: process_start,
            set_parameter: set_parameter_start,
        },
        cycles_per_call,
    })
}

/// Fill parameter defaults, zero the channel banks, set states and delay
/// cursors, then tail-jump to the stock init with R4 untouched.
fn init(block: &Block, layout: &Layout, start: u32) -> Result<Vec<u16>> {
    let mut code = VisaProgram::new();
    let base = i(SCRATCH_INDEX);
    code.push(V::Transfer {
        dst: ureg_i(SCRATCH_INDEX),
        src: ureg_r(4),
    });
    for (index, parameter) in block.parameters().iter().enumerate() {
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

fn process(block: &Block, layout: &Layout) -> Result<(Vec<u16>, u64)> {
    let mut available: u16 = !(1 << COUNTER);
    if !block.delays.is_empty() {
        available &= !(1 << CURSOR);
    }
    let kernel = codegen::compile_with(block, available)?;
    let contract = &kernel.contract;

    let saved: Vec<UniversalRegister> = SAVED_DATA
        .iter()
        .map(|&index| ureg_r(index))
        .chain([ureg_i(STATE), ureg_i(AUDIO), ureg_i(DELAY), ureg_m(DELAY)])
        .collect();
    let mut code = VisaProgram::new();
    code.push(V::ModifyIndex {
        index: i(7),
        offset: -(saved.len() as i32),
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
    code.push(V::Transfer {
        dst: ureg_i(DELAY),
        src: ureg_r(8),
    });
    code.push(V::LoadDmOffset {
        dst: ureg_i(AUDIO),
        index: i(DELAY),
        offset: 0,
    });

    let mut per_sample = 0;
    for (channel, &bank) in layout.banks.iter().enumerate() {
        let top = format!("sample{channel}");
        code.push(V::LoadImmediate {
            dst: ureg_r(COUNTER),
            value: BLOCK_SAMPLES,
        });
        code.label(&top);
        let before = code.parcels()?.len();
        let body = &mut code;
        for binding in &contract.inputs {
            let dst = ureg_r(binding.register);
            match binding.source {
                InputSource::Audio => body.push(V::LoadDmOffset {
                    dst,
                    index: i(AUDIO),
                    offset: 0,
                }),
                InputSource::Parameter { index } => body.push(V::LoadDmOffset {
                    dst,
                    index: i(STATE),
                    offset: word(layout.parameters + index as u32),
                }),
                InputSource::State { index } => body.push(V::LoadDmOffset {
                    dst,
                    index: i(STATE),
                    offset: word(bank + layout.states + index as u32),
                }),
                InputSource::Delay { index } => {
                    delay_slot(body, layout, bank, index);
                    body.push(V::LoadDmPreModify {
                        dst,
                        index: i(DELAY),
                        modifier: m(DELAY),
                    });
                }
            }
        }
        for &instruction in kernel.program.instructions() {
            body.push(instruction);
        }
        body.push(V::StoreDmPostModify {
            src: ureg_r(contract.output),
            index: i(AUDIO),
            modifier: m(6),
        });
        for (index, update) in contract.state_updates.iter().enumerate() {
            if let Some(register) = update {
                body.push(V::StoreDmOffset {
                    src: ureg_r(*register),
                    index: i(STATE),
                    offset: word(bank + layout.states + index as u32),
                });
            }
        }
        for (index, update) in contract.delay_updates.iter().enumerate() {
            let Some(register) = update else { continue };
            let wrapped = format!("wrapped{channel}_{index}");
            delay_slot(body, layout, bank, index);
            body.push(V::StoreDmPreModify {
                src: ureg_r(*register),
                index: i(DELAY),
                modifier: m(DELAY),
            });
            body.push(V::IntegerSubOne {
                dst: r(CURSOR),
                src: r(CURSOR),
            });
            body.jump(Condition::Ne, &wrapped);
            body.push(V::LoadImmediate {
                dst: ureg_r(CURSOR),
                value: layout.delays[index].1,
            });
            body.label(&wrapped);
            body.push(V::StoreDmOffset {
                src: ureg_r(CURSOR),
                index: i(STATE),
                offset: word(bank + layout.cursors + index as u32),
            });
        }
        // Parcels over-count instructions; add the SHARC+ DAG load-to-use stalls
        // (4 cycles) for each delay pointer setup.
        let parcels = (code.parcels()?.len() - before) as u64;
        per_sample = per_sample.max(parcels + 8 * block.delays.len() as u64 + 2);
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
    Ok((code.parcels()?, per_sample))
}

/// Store parameter R8 = min + clamp(F12, 0, 1) * (max - min). Indices outside the
/// block's range are ignored. Uses only scratch registers R0, R2, R12 and I4.
fn set_parameter(block: &Block, layout: &Layout) -> Result<Vec<u16>> {
    let mut code = VisaProgram::new();
    let value = r(12);
    code.push(V::Transfer {
        dst: ureg_i(SCRATCH_INDEX),
        src: ureg_r(4),
    });
    if !block.parameters().is_empty() {
        code.push(V::LoadImmediate {
            dst: ureg_r(0),
            value: 0f32.to_bits(),
        });
        code.push(Instruction::FloatMax {
            dst: value,
            lhs: value,
            rhs: r(0),
        });
        code.push(V::LoadImmediate {
            dst: ureg_r(0),
            value: 1f32.to_bits(),
        });
        code.push(Instruction::FloatMin {
            dst: value,
            lhs: value,
            rhs: r(0),
        });
    }
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
