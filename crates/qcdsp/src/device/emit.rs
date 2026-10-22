//! Generated VISA routines: init, process, set-parameter, fault recovery and (with an arena) destroy.
//!
//! Calling contract (stock Volume, see `docs/abi-status.md`):
//! - init(R4 = state): called by the entry; must leave R4 intact and finish in
//!   the stock init, which it reaches with a tail jump.
//! - process(R4 = state, R8 = io): `io[0]` points at 32 samples of channel A
//!   and `io[1]` points at 32 samples of channel B, processed in place.
//! - set_parameter(R4 = state, R8 = index, F12 = value). Catalogue entries are
//!   declared 0..1. Integer bits outside +0..=1 are rejected before arithmetic;
//!   accepted values map to the parameter's DSP range before storage.
//! - destroy(R4 = state), only with an arena: frees it, then tail-jumps to the stock destroy.
//!
//! With an arena ([`Layout::arena`]) init takes the bank memory from the kernel heap with the
//! stock C call ([`kernel_call`]); every callback reads the arena pointer from the state and
//! does nothing to the banks when it is 0 (the allocation failed), so process leaves the audio
//! dry. Without an arena the code has no kernel calls and no relocations.
//!
//! Every memory access is an unqualified normal-word access with word offsets,
//! so code is correct whether a pointer is byte- or normal-word-addressed.
//! Callee-preserved registers are saved in the C-runtime frame. M5 = 0, M6 = 1.

use qc_asm::{
    Condition, IndexRegister, Instruction, ModifyRegister, Register, UniversalRegister,
    VisaInstruction as V, VisaProgram,
};

use qc_format::blob::{CodeRelocation, Section};
use qc_link::{KernelForm, KernelRef};

use super::{
    layout::{Arena, Layout},
    template::{
        ARENA_HEAP, BLOCK_SAMPLES, Entries, HEAP_FREE, HEAP_MALLOC, STOCK_DESTROY, STOCK_INIT,
        STOCK_PARCELS,
    },
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
/// Process: the bank base when there is an arena (else the banks are at STATE).
const ARENA: u8 = 3; // I3
const SCRATCH_INDEX: u8 = 4; // I4, caller-saved
const STACK: u8 = 7; // I7
const FRAME: u8 = 6; // I6

/// The generated routines: the parcels that follow the stock Volume code, their entries and
/// the modeled instructions of one process call (the resource estimate).
pub struct Generated {
    pub parcels: Vec<u16>,
    pub entries: Entries,
    pub cycles_per_call: u64,
    /// The kernel calls (CJUMP sites, parcel offsets in section 2, with core 1 addresses) and
    /// the kind-7 relocations of their return literals. Both are empty without an arena.
    pub kernel_refs: Vec<KernelRef>,
    pub relocations: Vec<CodeRelocation>,
}

#[derive(Default)]
struct Links {
    kernel_refs: Vec<KernelRef>,
    relocations: Vec<CodeRelocation>,
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
    let mut links = Links::default();
    let init_start = STOCK_PARCELS;
    let init = init(block, layout, init_start, &mut links)?;
    let process_start = init_start + init.len() as u32;
    let (process, cycles_per_call) = process(layout, kernel)?;
    let set_parameter_start = process_start + process.len() as u32;
    let set_parameter = set_parameter(block, layout)?;
    let recovery_start = set_parameter_start + set_parameter.len() as u32;
    let recovery = recovery(block, layout)?;
    let destroy_start = recovery_start + recovery.len() as u32;
    let destroy = match layout.arena {
        Some(arena) => destroy(arena, destroy_start, &mut links)?,
        None => Vec::new(),
    };
    let mut parcels = init;
    parcels.extend(process);
    parcels.extend(set_parameter);
    parcels.extend(recovery);
    parcels.extend(destroy);
    Ok(Generated {
        parcels,
        entries: Entries {
            init: init_start,
            process: process_start,
            set_parameter: set_parameter_start,
            recovery: recovery_start,
            destroy: layout.arena.map(|_| destroy_start),
        },
        cycles_per_call,
        kernel_refs: links.kernel_refs,
        relocations: links.relocations,
    })
}

/// The stock C call, copied from digital_delay `0x38c4..0x38c8` (heap_malloc) and Volume
/// `0x15d..0x161` (malloc): `CJUMP addr (DB)`; `DM(I7,M7) = R2`; `DM(I7,M7) = <return − 1>`,
/// the literal with a kind-7 relocation. CJUMP sets R2 = I6, I6 = I7, so the two slots push
/// the caller's frame and return address; the callee's C return restores I6 and I7 and
/// resumes after the literal. It may clobber every scratch register (R0-R2, R4, R8, R12, I4,
/// I12, I13, M4, M12) and the stack below I7. `start` is the routine's first parcel.
fn kernel_call(code: &mut VisaProgram, start: u32, target: u32, links: &mut Links) -> Result<()> {
    let at = start + code.parcel_count()? as u32;
    code.push(V::CJump { target });
    code.push(V::StoreDmPostModify {
        src: ureg_r(2),
        index: i(STACK),
        modifier: m(7),
    });
    let literal = at + 4;
    let back = literal + 3;
    code.push(V::PushImmediate {
        value: Section::Code.base() + back - 1,
    });
    links.kernel_refs.push(KernelRef {
        parcel: at,
        form: KernelForm::Call24,
        core1_address: target,
    });
    links.relocations.push(CodeRelocation {
        kind: 7,
        offset: literal,
        target_base: Section::Code.base(),
        target_offset: back - 1,
    });
    Ok(())
}

/// `kernel_call` with R4 = `heap` and the state kept in a frame word (R4 is scratch).
/// The caller entered with a stock C call, so I7 = I6 − 2: one word is reserved below it and
/// released before the routine tail-jumps to stock code, which expects that frame.
fn heap_call(code: &mut VisaProgram, start: u32, target: u32, links: &mut Links) -> Result<()> {
    code.push(V::ModifyIndex {
        index: i(STACK),
        offset: -1,
    });
    code.push(V::StoreDmOffset {
        src: ureg_r(4),
        index: i(FRAME),
        offset: -2,
    });
    code.push(V::LoadImmediate {
        dst: ureg_r(4),
        value: ARENA_HEAP,
    });
    kernel_call(code, start, target, links)?;
    code.push(V::LoadDmOffset {
        dst: ureg_r(4),
        index: i(FRAME),
        offset: -2,
    });
    code.push(V::ModifyIndex {
        index: i(STACK),
        offset: 1,
    });
    Ok(())
}

/// Load the arena pointer into R8 and set AZ when it is 0. Uses I4 = state.
fn load_arena(code: &mut VisaProgram, arena: Arena) {
    code.push(V::Transfer {
        dst: ureg_i(SCRATCH_INDEX),
        src: ureg_r(4),
    });
    code.push(V::LoadDmOffset {
        dst: ureg_r(8),
        index: i(SCRATCH_INDEX),
        offset: word(arena.pointer),
    });
    code.push(V::IntegerPass {
        dst: r(8),
        src: r(8),
    });
}

/// A jump from `start + parcel_count` to `target`, both parcel offsets in section 2.
fn tail_jump(code: &mut VisaProgram, start: u32, target: u32) -> Result<()> {
    let offset = i64::from(target) - (i64::from(start) + code.parcel_count()? as i64);
    code.push(V::JumpRelative {
        condition: Condition::Always,
        offset: i32::try_from(offset).expect("section 2 is far below 2^31 parcels"),
        delayed: false,
    });
    Ok(())
}

/// Fill parameter defaults, set up the banks (in the state, or in an arena taken from the
/// kernel heap), then tail-jump to the stock init with R4 = state.
fn init(block: &Block, layout: &Layout, start: u32, links: &mut Links) -> Result<Vec<u16>> {
    let mut code = VisaProgram::new();
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
            index: i(SCRATCH_INDEX),
            offset: word(layout.parameters + index as u32),
        });
    }
    match layout.arena {
        None => banks(&mut code, block, layout, 4),
        Some(arena) => {
            code.push(V::LoadImmediate {
                dst: ureg_r(8),
                value: arena.bytes(),
            });
            heap_call(&mut code, start, HEAP_MALLOC, links)?;
            code.push(V::Transfer {
                dst: ureg_i(SCRATCH_INDEX),
                src: ureg_r(4),
            });
            code.push(V::StoreDmOffset {
                src: ureg_r(0),
                index: i(SCRATCH_INDEX),
                offset: word(arena.pointer),
            });
            code.push(V::IntegerPass {
                dst: r(8),
                src: r(0),
            });
            code.jump(Condition::Eq, "stock");
            code.push(V::Transfer {
                dst: ureg_i(SCRATCH_INDEX),
                src: ureg_r(8),
            });
            banks(&mut code, block, layout, 8);
            code.label("stock");
        }
    }
    tail_jump(&mut code, start, STOCK_INIT)?;
    Ok(code.parcels()?)
}

/// Fault recovery restores histories and cursors, keeping the current parameters.
fn recovery(block: &Block, layout: &Layout) -> Result<Vec<u16>> {
    let mut code = VisaProgram::new();
    match layout.arena {
        None => {
            code.push(V::Transfer {
                dst: ureg_i(SCRATCH_INDEX),
                src: ureg_r(4),
            });
            banks(&mut code, block, layout, 4);
        }
        Some(arena) => {
            load_arena(&mut code, arena);
            code.jump(Condition::Eq, "return");
            code.push(V::Transfer {
                dst: ureg_i(SCRATCH_INDEX),
                src: ureg_r(8),
            });
            banks(&mut code, block, layout, 8);
            code.label("return");
        }
    }
    code.push(V::LeafReturn);
    Ok(code.parcels()?)
}

/// ctx[8]: free the arena when there is one, then tail-jump to the stock destroy, which frees
/// the state (R4). The kernel calls destroy only after it has stopped the slot (ABI §3).
fn destroy(arena: Arena, start: u32, links: &mut Links) -> Result<Vec<u16>> {
    let mut code = VisaProgram::new();
    load_arena(&mut code, arena);
    code.jump(Condition::Eq, "stock");
    heap_call(&mut code, start, HEAP_FREE, links)?;
    code.label("stock");
    tail_jump(&mut code, start, STOCK_DESTROY)?;
    Ok(code.parcels()?)
}

/// Zero the channel banks, then write state initials and delay cursors. The bank base (the
/// state, or the arena) is in R`base` and I4; uses R0, R1 and I4.
fn banks(code: &mut VisaProgram, block: &Block, layout: &Layout, base: u8) {
    let bank_words = layout.bank_words;
    if bank_words == 0 {
        return;
    }
    let pointer = i(SCRATCH_INDEX);
    // Zero every bank with a post-increment walk, then write initial values.
    code.push(V::LoadImmediate {
        dst: ureg_r(0),
        value: 0,
    });
    code.push(V::LoadImmediate {
        dst: ureg_r(1),
        value: bank_words * layout.banks.len() as u32,
    });
    if layout.banks[0] != 0 {
        code.push(V::ModifyIndex {
            index: pointer,
            offset: word(layout.banks[0]),
        });
    }
    code.label("zero");
    code.push(V::StoreDmPostModify {
        src: ureg_r(0),
        index: pointer,
        modifier: m(6),
    });
    code.push(V::IntegerSubOne {
        dst: r(1),
        src: r(1),
    });
    code.jump(Condition::Ne, "zero");
    code.push(V::Transfer {
        dst: ureg_i(SCRATCH_INDEX),
        src: ureg_r(base),
    });
    for &bank in &layout.banks {
        for (index, state) in block.states.iter().enumerate() {
            code.push(V::LoadImmediate {
                dst: ureg_r(0),
                value: state.initial.to_bits(),
            });
            code.push(V::StoreDmOffset {
                src: ureg_r(0),
                index: pointer,
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
                index: pointer,
                offset: word(bank + layout.cursors + index as u32),
            });
        }
    }
}

/// The index register that holds the bank base in process.
fn bank_base(layout: &Layout) -> IndexRegister {
    i(if layout.arena.is_some() { ARENA } else { STATE })
}

/// Point I2 at delay `index`'s slot `cursor` in `bank`; leaves the cursor in R14.
fn delay_slot(code: &mut VisaProgram, layout: &Layout, bank: u32, index: usize) {
    let (buffer, _) = layout.delays[index];
    let base = bank_base(layout);
    code.push(V::LoadDmOffset {
        dst: ureg_r(CURSOR),
        index: base,
        offset: word(bank + layout.cursors + index as u32),
    });
    code.push(V::Transfer {
        dst: ureg_m(DELAY),
        src: ureg_r(CURSOR),
    });
    code.push(V::Transfer {
        dst: ureg_i(DELAY),
        src: base.into(),
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
    // Parameters and controls are in the state; bank words are at the bank base.
    let in_bank = |offset: u32| {
        let (_, bank) = bank.expect("channel steps run in the sample loop");
        (bank_base(layout), word(bank + offset))
    };
    let in_state = |offset: u32| (i(STATE), word(offset));
    let load = |code: &mut VisaProgram, register: u8, (index, offset): (IndexRegister, i32)| {
        code.push(V::LoadDmOffset {
            dst: ureg_r(register),
            index,
            offset,
        })
    };
    let store = |code: &mut VisaProgram, register: u8, (index, offset): (IndexRegister, i32)| {
        code.push(V::StoreDmOffset {
            src: ureg_r(register),
            index,
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
                load(code, register, in_state(layout.parameters + index as u32));
            }
            InputSource::Control { slot } => {
                load(code, register, in_state(layout.controls + slot));
            }
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
            store(code, register, in_state(layout.controls + slot));
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
                index: bank_base(layout),
                offset: word(bank + layout.cursors + delay as u32),
            });
            return 1;
        }
    }
    0
}

/// Returns the code and a cycle estimate for one call (all channels).
fn process(layout: &Layout, kernel: &Kernel) -> Result<(Vec<u16>, u64)> {
    let contract = &kernel.contract;
    let saved: Vec<UniversalRegister> = SAVED_DATA
        .iter()
        .map(|&index| ureg_r(index))
        .chain([ureg_i(STATE), ureg_i(AUDIO), ureg_i(DELAY), ureg_m(DELAY)])
        .chain(layout.arena.map(|_| ureg_i(ARENA)))
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
    if let Some(arena) = layout.arena {
        // Without an arena (the allocation failed) the buffers are left as they are: dry.
        code.push(V::LoadDmOffset {
            dst: ureg_r(0),
            index: i(STATE),
            offset: word(arena.pointer),
        });
        code.push(V::IntegerPass {
            dst: r(0),
            src: r(0),
        });
        code.jump(Condition::Eq, "dry");
        code.push(V::Transfer {
            dst: ureg_i(ARENA),
            src: ureg_r(0),
        });
    }
    // Keep the descriptor above SP: sample code can use R8 and I2.
    let descriptor_slot = -2 - saved.len() as i32;
    code.push(V::StoreDmOffset {
        src: ureg_r(8),
        index: i(6),
        offset: descriptor_slot,
    });

    // Hoisted parameter math, once per call for both channels.
    let before = code.instruction_count();
    for &control in &contract.control {
        step(&mut code, kernel, layout, None, control);
    }
    let control_instructions = (code.instruction_count() - before) as u64;

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
        let before = code.instruction_count();
        let mut delay_setups = 0;
        for &sample in &contract.sample {
            delay_setups += step(&mut code, kernel, layout, Some((channel, bank)), sample);
        }
        // One cycle per instruction, plus the SHARC+ DAG load-to-use stalls (4 cycles) of each
        // delay pointer setup. The catalogue entry doubles the result (`metadata::CPU_SAFETY`)
        // until a measurement on the unit replaces this model.
        let instructions = (code.instruction_count() - before) as u64;
        per_sample = per_sample.max(instructions + 4 * delay_setups + 2);
        code.push(V::IntegerSubOne {
            dst: r(COUNTER),
            src: r(COUNTER),
        });
        code.jump(Condition::Ne, &top);
    }
    let mut copy_cycles = 0;
    if layout.banks.len() == 1 {
        // Mono: channel B gets channel A's result. I1 moved during the loop; reload it.
        code.push(V::LoadDmOffset {
            dst: ureg_i(SCRATCH_INDEX),
            index: i(6),
            offset: descriptor_slot,
        });
        code.push(V::LoadDmOffset {
            dst: ureg_i(AUDIO),
            index: i(SCRATCH_INDEX),
            offset: 0,
        });
        code.push(V::LoadDmOffset {
            dst: ureg_i(DELAY),
            index: i(SCRATCH_INDEX),
            offset: 1,
        });
        for sample in 0..BLOCK_SAMPLES as i32 {
            code.push(V::LoadDmOffset {
                dst: ureg_r(0),
                index: i(AUDIO),
                offset: sample,
            });
            code.push(V::StoreDmOffset {
                src: ureg_r(0),
                index: i(DELAY),
                offset: sample,
            });
        }
        // Three pointer loads with DAG load-to-use stalls, then the copy.
        copy_cycles = 3 * 5 + 2 * u64::from(BLOCK_SAMPLES);
    }

    if layout.arena.is_some() {
        code.label("dry");
    }
    for (slot, &register) in saved.iter().enumerate() {
        code.push(V::LoadDmOffset {
            dst: register,
            index: i(6),
            offset: -2 - slot as i32,
        });
    }
    code.push(V::LeafReturn);
    // Each computed channel's 32 samples, the per-sample body plus loop branch, the
    // control phase, the mono copy, and a fixed allowance for frame setup, saves and restores.
    // An arena adds its pointer load, test and save/restore, with DAG stalls.
    let channels = layout.banks.len() as u64;
    let cycles = channels * u64::from(BLOCK_SAMPLES) * (per_sample + 8)
        + control_instructions
        + copy_cycles
        + 120
        + layout.arena.map_or(0, |_| 12);
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
