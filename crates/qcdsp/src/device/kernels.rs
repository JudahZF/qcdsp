//! Network (IR 3) code: the frame kernels that process runs before its sample loop, and the
//! ring setup that init and recovery run. The memory plan is in [`super::frame`].
//!
//! Registers in the network phase (process saves the preserved ones):
//! - I0 state, I3 arena (bank and frame buffers), I4 scratch.
//! - I1 the convolution input, read with BDCST1 so both PEs get the same value; I2 the output;
//!   I5 the elementwise input; I8 the weights (DAG2); I10 an elementwise operand through DAG2.
//!   A ring buffer sets B and L of its index register, so post-modify and MODIFY wrap.
//! - I9 a fused condition (BDCST9), I11 the bias pairs.
//! - Convolutions: M0-M4 step I1 (see [`conv`]), M2 the output row; M8 = 2, M9 the weight
//!   rewind, M10 and M11 the condition and residual rows. Elementwise kernels: M3 = M8 = 2. The C
//!   runtime keeps M5 = 0, M6 = 1, M13 = 0.
//!
//! Each kernel runs in SIMD (PEx and PEy) between `BIT SET MODE1` and `BIT CLR MODE1`, each
//! followed by a NOP for the one-cycle effect latency. Every loop body is made of 48-bit
//! instructions only (VISA loop rule, PRM `.06.062`) and has no branch.

use qc_asm::{
    AluOp, Compute, DataAccess, Direction, FloatMulAlu, Instruction, LoopCount, LoopMode,
    MODE1_BDCST1, MODE1_BDCST9, MODE1_PEYEN, MemoryAccess, ModifyRegister, UniversalRegister,
    Update, VisaInstruction as V, VisaProgram,
};

use super::{
    emit::{ARENA, SCRATCH_INDEX, STATE, i, m, r, ureg_i, ureg_m, ureg_r, word},
    frame::{ConvKernel, FrameBuffer, Kernel, TANH_CONSTANTS},
    layout::{Layout, NetworkLayout},
    template::BLOCK_SAMPLES,
};
use crate::network::{Activation, Network, Op};

const X: u8 = 1;
const OUT: u8 = 2;
const SRC: u8 = 5;
const WEIGHTS: u8 = 8;
const OPERAND: u8 = 10;
/// A fused condition's input, read with BDCST9.
const COND: u8 = 9;
/// The bias pairs of a convolution.
const BIAS: u8 = 11;
/// Index registers whose B and L a kernel may set; process clears their L before it returns.
pub(super) const RING_INDICES: [u8; 6] = [X, OUT, SCRATCH_INDEX, SRC, COND, OPERAND];
/// Preserved registers the network phase changes (process saves and restores them).
pub(super) const NETWORK_SAVED: [u8; 5] = [SRC, WEIGHTS, COND, OPERAND, BIAS];
pub(super) const NETWORK_SAVED_M: [u8; 8] = [0, 1, 2, 3, 8, 9, 10, 11];

fn ureg_b(index: u8) -> UniversalRegister {
    UniversalRegister::new(0x40 + index).expect("B register")
}
fn ureg_l(index: u8) -> UniversalRegister {
    UniversalRegister::new(0x30 + index).expect("L register")
}
fn ureg_s(index: u8) -> UniversalRegister {
    UniversalRegister::new(0x50 + index).expect("S register")
}
/// B registers process saves first: writing B also sets I, so they are restored before I.
pub(super) fn saved_b() -> impl Iterator<Item = UniversalRegister> {
    RING_INDICES.into_iter().map(ureg_b)
}

fn load(register: u8, index: u8, modifier: u8) -> DataAccess {
    DataAccess {
        direction: Direction::Load,
        register: r(register),
        index: i(index),
        modifier: m(modifier),
    }
}

fn store(register: u8, index: u8, modifier: u8) -> DataAccess {
    DataAccess {
        direction: Direction::Store,
        ..load(register, index, modifier)
    }
}

/// One instruction from its parts: Type 1a with both accesses, Type 3a with one, Type 2a with
/// none. Every form is 48-bit.
fn instruction(
    compute: Option<Compute>,
    dm: Option<DataAccess>,
    pm: Option<DataAccess>,
) -> Option<V> {
    match (dm, pm) {
        (Some(dm), Some(pm)) => Some(V::DualMove { compute, dm, pm }),
        (Some(access), None) | (None, Some(access)) => Some(one_access(compute, access)),
        (None, None) => compute.map(V::Compute),
    }
}

/// Type 3a: one post-modify access, with an optional compute.
fn one_access(compute: Option<Compute>, access: DataAccess) -> V {
    V::MemoryMove {
        compute,
        access: MemoryAccess {
            direction: access.direction,
            register: access.register.into(),
            index: access.index,
            modifier: access.modifier,
            update: Update::PostModify,
        },
    }
}

fn single(instruction: Instruction) -> Compute {
    Compute::single(instruction).expect("a scalar compute")
}

fn set(code: &mut VisaProgram, register: UniversalRegister, value: i32) {
    code.push(V::LoadImmediate {
        dst: register,
        value: value as u32,
    });
}

/// Point index register `index` at this call's first row of `buffer`: a ring sets B and L from
/// the state, then I at the cursor; another buffer is the arena plus its offset.
fn point(code: &mut VisaProgram, network: &NetworkLayout, index: u8, buffer: usize) {
    let plan = &network.plan.buffers[buffer];
    match network.rings.iter().find(|ring| ring.buffer == buffer) {
        Some(ring) => {
            code.push(V::LoadDmOffset {
                dst: ureg_b(index),
                index: i(STATE),
                offset: word(ring.base),
            });
            set(code, ureg_l(index), plan.words() as i32);
            code.push(V::LoadDmOffset {
                dst: ureg_i(index),
                index: i(STATE),
                offset: word(ring.cursor),
            });
        }
        None => {
            set(code, ureg_l(index), 0);
            code.push(V::Transfer {
                dst: ureg_i(index),
                src: ureg_i(ARENA),
            });
            code.push(V::ModifyIndex {
                index: i(index),
                offset: word(network.base + plan.offset),
            });
        }
    }
}

fn simd(code: &mut VisaProgram, on: bool, mask: u32) {
    code.push(if on {
        V::BitSetMode1 { mask }
    } else {
        V::BitClearMode1 { mask }
    });
    code.push(V::Nop);
}

fn padded(buffer: &FrameBuffer) -> u32 {
    buffer.row_words()
}

/// The network phase of process for a mono block: copy channel A into buffer 0, run every
/// operation over the call, advance the ring cursors, and leave I5 at the output buffer for the
/// sample loop. `descriptor` is the frame slot of the io pointer. Returns its instruction count.
pub(super) fn network_phase(
    code: &mut VisaProgram,
    network: &Network,
    layout: &Layout,
    descriptor: i32,
) -> u64 {
    let net = layout.network.as_ref().expect("a network block");
    let before = code.instruction_count();
    let mut looped = 0;

    // Buffer 0 (column 0 of its rows) = channel A.
    code.push(V::LoadDmOffset {
        dst: ureg_i(SCRATCH_INDEX),
        index: i(6),
        offset: descriptor,
    });
    code.push(V::LoadDmOffset {
        dst: ureg_i(SRC),
        index: i(SCRATCH_INDEX),
        offset: 0,
    });
    set(code, ureg_l(SRC), 0);
    point(code, net, OUT, 0);
    set(code, ureg_m(3), 2);
    looped += loop_cost(code, BLOCK_SAMPLES, |code| {
        code.push(one_access(None, load(0, SRC, 6)));
        code.push(one_access(None, store(0, OUT, 3)));
    });

    let weights = layout
        .weights()
        .expect("a network block has weights")
        .pointer;
    for kernel in &net.plan.kernels {
        let op = match kernel {
            Kernel::Conv(conv_kernel) => {
                looped += conv(code, network, net, conv_kernel, weights);
                continue;
            }
            Kernel::Tanh { op, sum } => {
                let Op::Activation { buffer, .. } = &network.ops[*op] else {
                    unreachable!("a tanh kernel runs an activation")
                };
                let constants = net.plan.tanh_constants.expect("tanh constants are planned");
                looped += tanh(code, net, *buffer, *sum, weights, constants);
                continue;
            }
            Kernel::Op(op) => &network.ops[*op],
        };
        looped += match op {
            Op::Conv { .. } => unreachable!("convolutions are convolution kernels"),
            Op::Add { input, output } => elementwise(code, net, *input, *output, Elementwise::Add),
            Op::Copy { input, output } => {
                elementwise(code, net, *input, *output, Elementwise::Copy)
            }
            Op::Gain { buffer, parameter } => {
                code.push(V::LoadDmOffset {
                    dst: ureg_r(4),
                    index: i(STATE),
                    offset: word(layout.parameters + *parameter as u32),
                });
                code.push(V::Transfer {
                    dst: ureg_s(4),
                    src: ureg_r(4),
                });
                elementwise(code, net, *buffer, *buffer, Elementwise::Gain)
            }
            Op::Activation { buffer, function } => elementwise(
                code,
                net,
                *buffer,
                *buffer,
                Elementwise::Activation(*function),
            ),
        };
    }

    // The next call's rows follow this call's in every ring.
    for ring in &net.rings {
        let buffer = &net.plan.buffers[ring.buffer];
        code.push(V::LoadDmOffset {
            dst: ureg_b(SCRATCH_INDEX),
            index: i(STATE),
            offset: word(ring.base),
        });
        set(code, ureg_l(SCRATCH_INDEX), buffer.words() as i32);
        code.push(V::LoadDmOffset {
            dst: ureg_i(SCRATCH_INDEX),
            index: i(STATE),
            offset: word(ring.cursor),
        });
        code.push(V::ModifyIndex {
            index: i(SCRATCH_INDEX),
            offset: word(BLOCK_SAMPLES * padded(buffer)),
        });
        code.push(V::Transfer {
            dst: ureg_r(2),
            src: ureg_i(SCRATCH_INDEX),
        });
        code.push(V::StoreDmOffset {
            src: ureg_r(2),
            index: i(STATE),
            offset: word(ring.cursor),
        });
    }
    for index in RING_INDICES {
        set(code, ureg_l(index), 0);
    }
    // The sample loop reads the output (one channel, two words a row) through I5.
    point(code, net, SRC, network.output);
    (code.instruction_count() - before) as u64 + looped
}

/// Emits `LCNTR = count, DO … UNTIL LCE` around `body` (F1-active: bodies never branch) and
/// returns the instructions the extra iterations execute.
fn loop_cost(code: &mut VisaProgram, count: u32, body: impl FnOnce(&mut VisaProgram)) -> u64 {
    let count = u16::try_from(count).expect("loop counts are below 65536");
    code.loop_begin(LoopCount::Immediate(count), LoopMode::F1Active);
    let before = code.instruction_count();
    body(code);
    let length = (code.instruction_count() - before) as u64;
    code.loop_end();
    length * (u64::from(count) - 1)
}

/// A convolution kernel, two output channels and two rows at a time: PEx computes output `o`,
/// PEy output `o + 1`; F12 sums row `t`, F13 row `t + 1`. Each weight pair (F0, through I8) is
/// loaded once and multiplies the broadcast inputs of both rows (F4 and F5, through I1, which
/// alternates between the rows). A fused condition is one more term (weight F1, inputs F6 and
/// F7 through I9 with BDCST9); a residual (F10, F11 through I10) is added before the stores.
/// Each add uses a product from two instructions before.
fn conv(
    code: &mut VisaProgram,
    network: &Network,
    net: &NetworkLayout,
    kernel: &ConvKernel,
    weights_pointer: u32,
) -> u64 {
    let Op::Conv {
        input,
        output,
        kernel: taps,
        dilation,
        ..
    } = &network.ops[kernel.op]
    else {
        unreachable!("a convolution kernel runs a convolution")
    };
    let (input_buffer, output_buffer) = (&net.plan.buffers[*input], &net.plan.buffers[*output]);
    let (row_in, row_out) = (padded(input_buffer) as i32, padded(output_buffer) as i32);
    let inputs = input_buffer.channels as usize;
    let taps = *taps;
    let n = inputs * taps;
    let terms = kernel.terms(network);
    let back = (taps as i32 - 1) * *dilation as i32;

    point(code, net, X, *input);
    if back > 0 {
        code.push(V::ModifyIndex {
            index: i(X),
            offset: -back * row_in,
        });
    }
    point(code, net, OUT, *output);
    for (index, offset) in [(WEIGHTS, kernel.weights), (BIAS, kernel.bias)] {
        code.push(V::LoadDmOffset {
            dst: ureg_i(index),
            index: i(STATE),
            offset: word(weights_pointer),
        });
        if offset > 0 {
            code.push(V::ModifyIndex {
                index: i(index),
                offset: word(offset),
            });
        }
    }
    let condition = kernel.condition.map(|buffer| {
        point(code, net, COND, buffer);
        set(code, ureg_m(10), padded(&net.plan.buffers[buffer]) as i32);
        padded(&net.plan.buffers[buffer]) as i32
    });
    let residual = kernel.residual.map(|buffer| {
        point(code, net, OPERAND, buffer);
        set(code, ureg_m(11), padded(&net.plan.buffers[buffer]) as i32);
        padded(&net.plan.buffers[buffer]) as i32
    });
    // I1 alternates rows: row t to row t + 1 (M0); from row t + 1 to the next term of row t:
    // the next tap of the same input (M1), the first tap of the next input (M3), or, after the
    // last term, the first term of row t + 2 (M4).
    set(code, ureg_m(0), row_in);
    set(code, ureg_m(1), *dilation as i32 * row_in - row_in);
    set(code, ureg_m(3), 1 - back * row_in - row_in);
    set(
        code,
        ureg_m(4),
        row_in - back * row_in - (inputs as i32 - 1),
    );
    set(code, ureg_m(2), row_out);
    set(code, ureg_m(8), 2);
    set(code, ureg_m(9), -2 * (terms as i32 - 1));
    let mask = MODE1_PEYEN | MODE1_BDCST1 | if condition.is_some() { MODE1_BDCST9 } else { 0 };
    simd(code, true, mask);
    let pairs = output_buffer.channels.div_ceil(2);
    let mut inner = 0;
    let outer = loop_cost(code, pairs, |code| {
        inner = loop_cost(code, BLOCK_SAMPLES / 2, |code| {
            row_pair(code, n, taps, terms, residual.is_some())
        });
        let rows = BLOCK_SAMPLES as i32;
        let mut step = |index: u8, offset: i32| {
            code.push(V::ModifyIndex {
                index: i(index),
                offset,
            })
        };
        step(X, -rows * row_in);
        step(WEIGHTS, 2 * terms as i32);
        step(BIAS, 2);
        step(OUT, -rows * row_out + 2);
        if let Some(row) = condition {
            step(COND, -rows * row);
        }
        if let Some(row) = residual {
            step(OPERAND, -rows * row + 2);
        }
    });
    simd(code, false, mask);
    outer + inner * u64::from(pairs)
}

/// One instruction of a row pair: its compute, DM access and PM access.
#[derive(Default)]
struct Slot {
    compute: Option<Compute>,
    dm: Option<DataAccess>,
    pm: Option<DataAccess>,
}

/// Rows `t` and `t + 1` of one output pair (see [`conv`]): load the first inputs, weight and
/// biases; for each term an instruction for row `t` and one for row `t + 1`; the last adds,
/// the residuals and the stores. Loads that the steady state has no slot for (the condition's
/// weight and inputs, the residuals) take a free PM slot before they are needed, or an
/// instruction of their own.
fn row_pair(code: &mut VisaProgram, n: usize, taps: usize, terms: usize, residual: bool) {
    // The modifier of term `j`'s row t + 1 load, which moves I1 to term `j + 1` of row t.
    let after_b = |j: usize| {
        if j == n - 1 {
            4
        } else if (j + 1).is_multiple_of(taps) {
            3
        } else {
            1
        }
    };
    // Weight stream: w_0 .. w_{n-1}, then the condition's; the last load rewinds (M9).
    let w_modifier = |j: usize| if j == terms - 1 { 9 } else { 8 };
    let mac = |row: u8, j: usize| {
        let (weight, input) = if j < n { (0, 4 + row) } else { (1, 6 + row) };
        (8 + row, weight, input)
    };
    let mut slots: Vec<Slot> = Vec::new();
    slots.push(Slot {
        dm: Some(load(4, X, 0)),
        pm: Some(load(0, WEIGHTS, w_modifier(0))),
        ..Slot::default()
    });
    slots.push(Slot {
        dm: Some(load(5, X, after_b(0))),
        pm: Some(load(12, BIAS, 13)),
        ..Slot::default()
    });
    let compute = |row: u8, j: usize| {
        let (product, weight, input) = mac(row, j);
        if j == 0 {
            return single(Instruction::FloatMul {
                dst: r(product),
                lhs: r(weight),
                rhs: r(input),
            });
        }
        Compute::float_mul_alu(FloatMulAlu {
            product: r(product),
            mul_x: r(weight),
            mul_y: r(input),
            op: AluOp::Add,
            alu_dst: r(12 + row),
            alu_x: r(8 + row),
            alu_y: r(12 + row),
        })
        .expect("register groups of a multiply-add")
    };
    // Slot indices of the term instructions, for the load deadlines below.
    let first_term = slots.len();
    for j in 0..terms {
        slots.push(Slot {
            compute: Some(compute(0, j)),
            dm: (j + 1 < n).then(|| load(4, X, 0)),
            pm: (j == 0).then(|| load(13, BIAS, 13)),
        });
        slots.push(Slot {
            compute: Some(compute(1, j)),
            dm: (j + 1 < n).then(|| load(5, X, after_b(j + 1))),
            pm: (j + 1 < n).then(|| load(0, WEIGHTS, w_modifier(j + 1))),
        });
    }
    let add = |row: u8, lhs: u8| {
        Some(single(Instruction::FloatAdd {
            dst: r(12 + row),
            lhs: r(lhs),
            rhs: r(12 + row),
        }))
    };
    let tail = slots.len();
    slots.push(Slot {
        compute: add(0, 8),
        ..Slot::default()
    });
    slots.push(Slot {
        compute: add(1, 9),
        dm: (!residual).then(|| store(12, OUT, 2)),
        ..Slot::default()
    });
    if residual {
        slots.push(Slot {
            compute: add(0, 10),
            ..Slot::default()
        });
        slots.push(Slot {
            compute: add(1, 11),
            dm: Some(store(12, OUT, 2)),
            ..Slot::default()
        });
    }
    slots.push(Slot {
        dm: Some(store(13, OUT, 2)),
        ..Slot::default()
    });

    // (load, earliest slot, the slot that uses it). The condition term is term n.
    let mut extras: Vec<(DataAccess, usize, usize)> = Vec::new();
    if terms > n {
        // The condition weight follows w_{n-1} in the stream: after the slot that loads it.
        let last_weight = if n == 1 {
            0
        } else {
            first_term + 2 * (n - 2) + 1
        };
        let row_t = first_term + 2 * n;
        extras.push((load(1, WEIGHTS, w_modifier(n)), last_weight + 1, row_t));
        extras.push((load(6, COND, 10), 0, row_t));
        extras.push((load(7, COND, 10), 0, row_t + 1));
    }
    if residual {
        extras.push((load(10, OPERAND, 11), 0, tail + 2));
        extras.push((load(11, OPERAND, 11), 0, tail + 3));
    }
    // Place each in the first free PM slot in its window, after the previous load of the same
    // pointer; else in an instruction of its own just before its user.
    let mut previous: Option<(u8, usize)> = None;
    let mut inserted = 0;
    for (access, earliest, user) in extras {
        let (earliest, user) = (earliest + inserted, user + inserted);
        let after = match previous {
            Some((index, at)) if index == access.index.index() => at + 1,
            _ => 0,
        };
        let start = earliest.max(after);
        let at = match (start..user).find(|&at| slots[at].pm.is_none()) {
            Some(at) => {
                slots[at].pm = Some(access);
                at
            }
            None => {
                slots.insert(
                    user,
                    Slot {
                        pm: Some(access),
                        ..Slot::default()
                    },
                );
                inserted += 1;
                user
            }
        };
        previous = Some((access.index.index(), at));
    }
    for slot in slots {
        if let Some(instruction) = instruction(slot.compute, slot.dm, slot.pm) {
            code.push(instruction);
        }
    }
}

/// The steps of tanh for one element (see [`crate::network::tanh`]), on the registers of one
/// element group: `(op, destination, left, right)` with a register as `Ok(offset)` from the
/// group's base or a constant as `Err(index in TANH_CONSTANTS)`.
type Operand = std::result::Result<u8, usize>;
const TANH_STEPS: [(TanhOp, u8, Operand, Operand); 23] = {
    use TanhOp::{Add, Max, Min, Mul, Seed, Sub};
    const X: u8 = 0;
    const X2: u8 = 1;
    const NUM: u8 = 2;
    const DEN: u8 = 3;
    const EST: u8 = 4;
    const TMP: u8 = 5;
    [
        (Max, X, Ok(X), Err(0)),
        (Min, X, Ok(X), Err(1)),
        (Mul, X2, Ok(X), Ok(X)),
        (Mul, DEN, Ok(X2), Err(2)),
        (Add, NUM, Ok(X2), Err(3)),
        (Add, DEN, Ok(DEN), Err(4)),
        (Mul, NUM, Ok(X2), Ok(NUM)),
        (Mul, DEN, Ok(X2), Ok(DEN)),
        (Add, NUM, Ok(NUM), Err(5)),
        (Add, DEN, Ok(DEN), Err(6)),
        (Mul, NUM, Ok(X2), Ok(NUM)),
        (Mul, DEN, Ok(X2), Ok(DEN)),
        (Add, NUM, Ok(NUM), Err(7)),
        (Add, DEN, Ok(DEN), Err(7)),
        (Mul, NUM, Ok(X), Ok(NUM)),
        (Seed, EST, Err(8), Ok(DEN)),
        (Mul, TMP, Ok(DEN), Ok(EST)),
        (Sub, TMP, Err(9), Ok(TMP)),
        (Mul, EST, Ok(EST), Ok(TMP)),
        (Mul, TMP, Ok(DEN), Ok(EST)),
        (Sub, TMP, Err(9), Ok(TMP)),
        (Mul, EST, Ok(EST), Ok(TMP)),
        (Mul, X, Ok(NUM), Ok(EST)),
    ]
};

#[derive(Clone, Copy)]
enum TanhOp {
    Max,
    Min,
    Mul,
    Add,
    Sub,
    /// The reciprocal seed: an integer subtraction of the bit patterns.
    Seed,
}

/// Tanh over a buffer, two element pairs (four values) at a time: group A (F0-F5) and group B
/// (F6-F11) alternate step by step, so no step uses the result of the instruction before it.
/// The constants come from a table in the weights arena (I11), each into F12 or F13 just before
/// its first use. A fused head sum adds each result to `sum` (F1 and F7, free by then) or
/// copies it there.
fn tanh(
    code: &mut VisaProgram,
    net: &NetworkLayout,
    buffer: usize,
    sum: Option<(usize, bool)>,
    weights_pointer: u32,
    constants: u32,
) -> u64 {
    point(code, net, SRC, buffer);
    point(code, net, OUT, buffer);
    if let Some((other, copy)) = sum {
        // The sum is read through I4 (DM) and written through I10 (PM bus).
        if !copy {
            point(code, net, SCRATCH_INDEX, other);
        }
        point(code, net, OPERAND, other);
    }
    code.push(V::LoadDmOffset {
        dst: ureg_i(BIAS),
        index: i(STATE),
        offset: word(weights_pointer),
    });
    code.push(V::ModifyIndex {
        index: i(BIAS),
        offset: word(constants),
    });
    set(code, ureg_m(3), 2);
    set(code, ureg_m(8), 2);
    set(code, ureg_m(9), -2 * (TANH_CONSTANTS.len() as i32 - 1));
    simd(code, true, MODE1_PEYEN);
    let iterations = BLOCK_SAMPLES * padded(&net.plan.buffers[buffer]) / 4;
    let looped = loop_cost(code, iterations, |code| tanh_body(code, sum));
    simd(code, false, MODE1_PEYEN);
    looped
}

fn tanh_body(code: &mut VisaProgram, sum: Option<(usize, bool)>) {
    const B: u8 = 6;
    let constant = |index: usize| 12 + (index % 2) as u8;
    // Slot 0 loads; then A_k at 1 + 2k and B_k at 2 + 2k; then the sum adds, then stores.
    let mut slots: Vec<Slot> = vec![Slot::default()];
    for &(op, dst, lhs, rhs) in &TANH_STEPS {
        for base in [0, B] {
            let operand = |o: Operand| r(o.map_or_else(constant, |o| base + o));
            let (dst, lhs, rhs) = (r(base + dst), operand(lhs), operand(rhs));
            let instruction = match op {
                TanhOp::Max => Instruction::FloatMax { dst, lhs, rhs },
                TanhOp::Min => Instruction::FloatMin { dst, lhs, rhs },
                TanhOp::Mul => Instruction::FloatMul { dst, lhs, rhs },
                TanhOp::Add => Instruction::FloatAdd { dst, lhs, rhs },
                TanhOp::Sub => Instruction::FloatSub { dst, lhs, rhs },
                TanhOp::Seed => Instruction::IntegerSub { dst, lhs, rhs },
            };
            slots.push(Slot {
                compute: Some(single(instruction)),
                ..Slot::default()
            });
        }
    }
    let last = slots.len() - 1; // B_22
    let mut add_at = [0; 2];
    if let Some((_, false)) = sum {
        for (n, base) in [0, B].into_iter().enumerate() {
            add_at[n] = slots.len();
            slots.push(Slot {
                compute: Some(single(Instruction::FloatAdd {
                    dst: r(base + 1),
                    lhs: r(base + 1),
                    rhs: r(base),
                })),
                ..Slot::default()
            });
        }
    }
    // (access, PM bus?, earliest slot, first slot that must see it or the end).
    let step = |k: usize, base: u8| 1 + 2 * k + usize::from(base == B);
    let mut memory: Vec<(DataAccess, bool, usize, usize)> = vec![
        (load(0, SRC, 3), false, 0, step(0, 0)),
        (load(B, SRC, 3), false, 0, step(0, B)),
    ];
    // Each constant after the last use of its register's previous constant.
    let first_use = |index: usize| {
        TANH_STEPS
            .iter()
            .position(|&(_, _, l, r)| l == Err(index) || r == Err(index))
            .expect("every constant is used")
    };
    let last_use = |index: usize| {
        TANH_STEPS
            .iter()
            .rposition(|&(_, _, l, r)| l == Err(index) || r == Err(index))
            .expect("every constant is used")
    };
    for index in 0..TANH_CONSTANTS.len() {
        let modifier = if index == TANH_CONSTANTS.len() - 1 {
            9
        } else {
            8
        };
        let earliest = if index < 2 {
            0
        } else {
            step(last_use(index - 2), B)
        };
        memory.push((
            load(constant(index), BIAS, modifier),
            true,
            earliest,
            step(first_use(index), 0),
        ));
    }
    let end = usize::MAX;
    memory.push((store(0, OUT, 3), false, step(22, 0) + 1, end));
    memory.push((store(B, OUT, 3), false, last + 1, end));
    match sum {
        Some((_, false)) => {
            // F1 and F7 (x²) are free after step 11.
            memory.push((load(1, SCRATCH_INDEX, 3), false, step(11, 0) + 1, add_at[0]));
            memory.push((store(1, OPERAND, 8), true, add_at[0] + 1, end));
            memory.push((
                load(B + 1, SCRATCH_INDEX, 3),
                false,
                step(11, B) + 1,
                add_at[1],
            ));
            memory.push((store(B + 1, OPERAND, 8), true, add_at[1] + 1, end));
        }
        Some((_, true)) => {
            memory.push((store(0, OPERAND, 8), true, step(22, 0) + 1, end));
            memory.push((store(B, OPERAND, 8), true, last + 1, end));
        }
        None => {}
    }
    // In order, each in the first free slot of its bus in its window (accesses through one
    // pointer keep their order), else in a new slot at the end.
    let mut after: Vec<(u8, usize)> = Vec::new();
    for (access, pm, earliest, before) in memory {
        let index = access.index.index();
        let start = after
            .iter()
            .filter(|(i, _)| *i == index)
            .map(|(_, at)| at + 1)
            .fold(earliest, usize::max);
        let free = |slot: &Slot| {
            if pm {
                slot.pm.is_none()
            } else {
                slot.dm.is_none()
            }
        };
        let at = match (start..before.min(slots.len())).find(|&at| free(&slots[at])) {
            Some(at) => at,
            None => {
                assert_eq!(before, end, "a load always finds a slot before its user");
                slots.push(Slot::default());
                slots.len() - 1
            }
        };
        if pm {
            slots[at].pm = Some(access);
        } else {
            slots[at].dm = Some(access);
        }
        after.push((index, at));
    }
    for slot in slots {
        if let Some(instruction) = instruction(slot.compute, slot.dm, slot.pm) {
            code.push(instruction);
        }
    }
}

#[derive(Clone, Copy)]
enum Elementwise {
    /// `output += input`.
    Add,
    /// `output = input`.
    Copy,
    /// `buffer *= F4` (both PEs, set by the caller).
    Gain,
    Activation(Activation),
}

/// An elementwise operation over this call's rows, a word pair per iteration (pairs never
/// cross a ring's end: rows have an even word count).
fn elementwise(
    code: &mut VisaProgram,
    net: &NetworkLayout,
    input: usize,
    output: usize,
    kind: Elementwise,
) -> u64 {
    let buffer = &net.plan.buffers[output];
    point(code, net, SRC, input);
    point(code, net, OUT, output);
    if let Elementwise::Add = kind {
        point(code, net, OPERAND, output);
        set(code, ureg_m(8), 2);
    }
    set(code, ureg_m(3), 2);
    simd(code, true, MODE1_PEYEN);
    if let Elementwise::Activation(function) = kind {
        constants(code, function);
    }
    let pairs = BLOCK_SAMPLES * padded(buffer) / 2;
    let looped = loop_cost(code, pairs, |code| match kind {
        Elementwise::Copy => {
            code.push(one_access(None, load(0, SRC, 3)));
            code.push(one_access(None, store(0, OUT, 3)));
        }
        Elementwise::Add => {
            code.push(V::DualMove {
                compute: None,
                dm: load(0, SRC, 3),
                pm: load(1, OPERAND, 8),
            });
            code.push(V::Compute(single(Instruction::FloatAdd {
                dst: r(0),
                lhs: r(1),
                rhs: r(0),
            })));
            code.push(one_access(None, store(0, OUT, 3)));
        }
        Elementwise::Gain => {
            code.push(one_access(None, load(0, SRC, 3)));
            code.push(V::Compute(single(Instruction::FloatMul {
                dst: r(0),
                lhs: r(0),
                rhs: r(4),
            })));
            code.push(one_access(None, store(0, OUT, 3)));
        }
        Elementwise::Activation(function) => {
            code.push(one_access(None, load(0, SRC, 3)));
            activation(code, function);
            code.push(one_access(None, store(0, OUT, 3)));
        }
    });
    simd(code, false, MODE1_PEYEN);
    looped
}

/// Constants of an activation in both PEs (a Type 17 immediate load sets Rn and Sn in SIMD).
fn constants(code: &mut VisaProgram, function: Activation) {
    let values: &[(u8, u32)] = match function {
        Activation::Relu => &[(15, 0)],
        Activation::Hardtanh => &[(14, (-1.0_f32).to_bits()), (15, 1.0_f32.to_bits())],
        Activation::Tanh => &[
            (6, (-crate::graph::TANH_LIMIT).to_bits()),
            (7, crate::graph::TANH_LIMIT.to_bits()),
            (8, 378.0_f32.to_bits()),
            (9, 17325.0_f32.to_bits()),
            (10, 135_135.0_f32.to_bits()),
            (11, 28.0_f32.to_bits()),
            (12, 3150.0_f32.to_bits()),
            (13, 62370.0_f32.to_bits()),
            (14, crate::graph::RECIP_MAGIC),
            (15, 2.0_f32.to_bits()),
        ],
    };
    for &(register, value) in values {
        code.push(V::LoadImmediate {
            dst: ureg_r(register),
            value,
        });
    }
}

/// `F0 = function(F0)` with the constants of [`constants`]; tanh follows
/// [`crate::network::tanh`] operation for operation.
fn activation(code: &mut VisaProgram, function: Activation) {
    use Instruction::{FloatAdd, FloatMax, FloatMin, FloatMul, FloatSub, IntegerSub};
    let ops: Vec<Instruction> = match function {
        Activation::Relu => vec![FloatMax {
            dst: r(0),
            lhs: r(0),
            rhs: r(15),
        }],
        Activation::Hardtanh => vec![
            FloatMax {
                dst: r(0),
                lhs: r(0),
                rhs: r(14),
            },
            FloatMin {
                dst: r(0),
                lhs: r(0),
                rhs: r(15),
            },
        ],
        Activation::Tanh => {
            let mul = |dst, lhs, rhs| FloatMul {
                dst: r(dst),
                lhs: r(lhs),
                rhs: r(rhs),
            };
            let add = |dst, lhs, rhs| FloatAdd {
                dst: r(dst),
                lhs: r(lhs),
                rhs: r(rhs),
            };
            // The numerator and denominator Horner chains are independent, so their steps
            // alternate: each result is used two instructions later, not in the next one.
            let mut ops = vec![
                FloatMax {
                    dst: r(0),
                    lhs: r(0),
                    rhs: r(6),
                },
                FloatMin {
                    dst: r(0),
                    lhs: r(0),
                    rhs: r(7),
                },
                mul(1, 0, 0),
                mul(3, 1, 11), // 28 x²
                add(2, 1, 8),  // 378 + x²
                add(3, 3, 12), // + 3150
                mul(2, 1, 2),
                mul(3, 1, 3),
                add(2, 2, 9),  // + 17325
                add(3, 3, 13), // + 62370
                mul(2, 1, 2),
                mul(3, 1, 3),
                add(2, 2, 10), // + 135135
                add(3, 3, 10), // + 135135
                mul(2, 0, 2),  // × x
                IntegerSub {
                    dst: r(4),
                    lhs: r(14),
                    rhs: r(3),
                },
            ];
            for _ in 0..2 {
                ops.extend([
                    mul(5, 3, 4),
                    FloatSub {
                        dst: r(5),
                        lhs: r(15),
                        rhs: r(5),
                    },
                    mul(4, 4, 5),
                ]);
            }
            ops.push(mul(0, 2, 4));
            ops
        }
    };
    for op in ops {
        code.push(V::Compute(single(op)));
    }
}

/// Init and recovery: with the arena pointer in R8 and the state in R4, point every ring's
/// base and cursor at its first word. Uses R2, I4 and I12.
pub(super) fn rings(code: &mut VisaProgram, layout: &Layout) {
    let net = layout.network.as_ref().expect("a network block");
    code.push(V::Transfer {
        dst: ureg_i(SCRATCH_INDEX),
        src: ureg_r(4),
    });
    for ring in &net.rings {
        code.push(V::Transfer {
            dst: ureg_i(12),
            src: ureg_r(8),
        });
        code.push(V::ModifyIndex {
            index: i(12),
            offset: word(net.base + net.plan.buffers[ring.buffer].offset),
        });
        code.push(V::Transfer {
            dst: ureg_r(2),
            src: ureg_i(12),
        });
        for slot in [ring.base, ring.cursor] {
            code.push(V::StoreDmOffset {
                src: ureg_r(2),
                index: i(SCRATCH_INDEX),
                offset: word(slot),
            });
        }
    }
}

/// Init: write the packed weights through I4 (the weights pointer, in R0) with Type 16a
/// stores, one 48-bit instruction per word.
pub(super) fn weights(code: &mut VisaProgram, layout: &Layout) {
    let net = layout.network.as_ref().expect("a network block");
    code.push(V::Transfer {
        dst: ureg_i(SCRATCH_INDEX),
        src: ureg_r(0),
    });
    for &value in &net.plan.weights {
        code.push(V::StoreImmediate {
            index: i(SCRATCH_INDEX),
            modifier: ModifyRegister::new(6).expect("M6"),
            value: value.to_bits(),
        });
    }
}
