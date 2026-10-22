//! Network (IR 3) code: the frame kernels that process runs before its sample loop, and the
//! ring setup that init and recovery run. The memory plan is in [`super::frame`].
//!
//! Registers in the network phase (process saves the preserved ones):
//! - I0 state, I3 arena (bank and frame buffers), I4 scratch.
//! - I1 the convolution input, read with BDCST1 so both PEs get the same value; I2 the output;
//!   I5 the elementwise input; I8 the weights (DAG2); I10 an elementwise operand through DAG2.
//!   A ring buffer sets B and L of its index register, so post-modify and MODIFY wrap.
//! - M0 tap jump, M1 next row, M2 output row stride, M3 = 2; M8 = 2, M9 weight rewind. The C
//!   runtime keeps M5 = 0, M6 = 1.
//!
//! Each kernel runs in SIMD (PEx and PEy) between `BIT SET MODE1` and `BIT CLR MODE1`, each
//! followed by a NOP for the one-cycle effect latency. Every loop body is made of 48-bit
//! instructions only (VISA loop rule, PRM `.06.062`) and has no branch.

use qc_asm::{
    AluOp, Compute, DataAccess, Direction, FloatMulAlu, Instruction, LoopCount, LoopMode,
    MODE1_BDCST1, MODE1_PEYEN, MemoryAccess, ModifyRegister, UniversalRegister, Update,
    VisaInstruction as V, VisaProgram,
};

use super::{
    emit::{ARENA, SCRATCH_INDEX, STATE, i, m, r, ureg_i, ureg_m, ureg_r, word},
    frame::FrameBuffer,
    layout::{Layout, NetworkLayout},
    template::BLOCK_SAMPLES,
};
use crate::network::{Activation, Network, Op};

const X: u8 = 1;
const OUT: u8 = 2;
const SRC: u8 = 5;
const WEIGHTS: u8 = 8;
const OPERAND: u8 = 10;
/// Index registers whose B and L a kernel may set; process clears their L before it returns.
pub(super) const RING_INDICES: [u8; 5] = [X, OUT, SCRATCH_INDEX, SRC, OPERAND];
/// Preserved registers the network phase changes (process saves and restores them).
pub(super) const NETWORK_SAVED: [u8; 3] = [SRC, WEIGHTS, OPERAND];
pub(super) const NETWORK_SAVED_M: [u8; 6] = [0, 1, 2, 3, 8, 9];

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

fn simd(code: &mut VisaProgram, on: bool, broadcast: bool) {
    let mask = MODE1_PEYEN | if broadcast { MODE1_BDCST1 } else { 0 };
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

    for (at, op) in network.ops.iter().enumerate() {
        looped += match op {
            Op::Conv {
                input,
                output,
                kernel,
                dilation,
                accumulate,
                ..
            } => conv(
                code,
                net,
                Conv {
                    input: *input,
                    output: *output,
                    kernel: *kernel as u32,
                    dilation: *dilation as u32,
                    weights: net.plan.op_weights[at].expect("a convolution has weights"),
                    accumulate: *accumulate,
                },
                layout
                    .weights()
                    .expect("a network block has weights")
                    .pointer,
            ),
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

struct Conv {
    input: usize,
    output: usize,
    kernel: u32,
    dilation: u32,
    /// Word offset of the packed weights in the weights arena.
    weights: u32,
    accumulate: bool,
}

/// A convolution, two output channels at a time (PEx `o`, PEy `o + 1`). For each pair and each
/// row: the bias pair, then `N = kernel × inputs` multiply-adds into two accumulators (F12 for
/// even terms, F13 for odd terms, so an add uses a product from two instructions before), the
/// sum, and a store of the pair.
fn conv(code: &mut VisaProgram, net: &NetworkLayout, op: Conv, weights_pointer: u32) -> u64 {
    let input = &net.plan.buffers[op.input];
    let output = &net.plan.buffers[op.output];
    let (row_in, row_out) = (padded(input) as i32, padded(output) as i32);
    let inputs = input.channels as i32;
    let (kernel, dilation) = (op.kernel as i32, op.dilation as i32);
    let n = (kernel * inputs) as usize;

    point(code, net, X, op.input);
    let back = (kernel - 1) * dilation;
    if back > 0 {
        code.push(V::ModifyIndex {
            index: i(X),
            offset: -back * row_in,
        });
    }
    point(code, net, OUT, op.output);
    code.push(V::LoadDmOffset {
        dst: ureg_i(WEIGHTS),
        index: i(STATE),
        offset: word(weights_pointer),
    });
    if op.weights > 0 {
        code.push(V::ModifyIndex {
            index: i(WEIGHTS),
            offset: word(op.weights),
        });
    }
    // From the last input of a tap to the first input of the next tap (dilation rows on), and
    // from the last input of the last tap to the first tap of the next row.
    set(code, ureg_m(0), dilation * row_in - (inputs - 1));
    set(
        code,
        ureg_m(1),
        row_in - (kernel - 1) * dilation * row_in - (inputs - 1),
    );
    set(code, ureg_m(2), row_out);
    set(code, ureg_m(8), 2);
    set(code, ureg_m(9), -2 * n as i32);
    simd(code, true, true);
    let pairs = output.channels.div_ceil(2);
    let mut inner = 0;
    let outer = loop_cost(code, pairs, |code| {
        inner = loop_cost(code, BLOCK_SAMPLES, |code| {
            mac_row(code, n, inputs as usize, kernel as usize, op.accumulate)
        });
        code.push(V::ModifyIndex {
            index: i(X),
            offset: -(BLOCK_SAMPLES as i32) * row_in,
        });
        code.push(V::ModifyIndex {
            index: i(WEIGHTS),
            offset: 2 * n as i32 + 2,
        });
        code.push(V::ModifyIndex {
            index: i(OUT),
            offset: -(BLOCK_SAMPLES as i32) * row_out + 2,
        });
    });
    simd(code, false, true);
    outer + inner * u64::from(pairs)
}

/// One row of a convolution pair (see [`conv`]). Term `j` multiplies the weight pair `w_j`
/// (F0/F1, loaded one instruction after its input) by the broadcast input `x_j` (F4/F5) into
/// F8/F9; the add of term `j` runs two instructions after its product.
fn mac_row(code: &mut VisaProgram, n: usize, inputs: usize, kernel: usize, accumulate: bool) {
    // Input modifiers: +1 within a tap, M0 to the next tap, M1 to the next row.
    let x_modifier = |j: usize| {
        if j == n - 1 {
            1
        } else if (j + 1) % inputs == 0 {
            0
        } else {
            6
        }
    };
    debug_assert_eq!(n, kernel * inputs);
    for step in 0..n + 4 {
        let dm = if step < n {
            Some(load(4 + (step % 2) as u8, X, x_modifier(step)))
        } else if accumulate && step == n {
            Some(load(14, OUT, 5))
        } else {
            None
        };
        let pm = match step {
            0 => Some(load(12, WEIGHTS, 8)),
            s if s <= n => {
                let j = s - 1;
                Some(load((j % 2) as u8, WEIGHTS, if j == n - 1 { 9 } else { 8 }))
            }
            _ => None,
        };
        let product = (2..n + 2).contains(&step).then(|| step - 2);
        let sum = (4..n + 4).contains(&step).then(|| step - 4);
        let compute = match (product, sum) {
            (Some(j), Some(k)) => Some(
                Compute::float_mul_alu(FloatMulAlu {
                    product: r(8 + (j % 2) as u8),
                    mul_x: r((j % 2) as u8),
                    mul_y: r(4 + (j % 2) as u8),
                    op: AluOp::Add,
                    alu_dst: r(12 + (k % 2) as u8),
                    alu_x: r(8 + (k % 2) as u8),
                    alu_y: r(12 + (k % 2) as u8),
                })
                .expect("register groups of a multiply-add"),
            ),
            (Some(j), None) => Some(single(Instruction::FloatMul {
                dst: r(8 + (j % 2) as u8),
                lhs: r((j % 2) as u8),
                rhs: r(4 + (j % 2) as u8),
            })),
            (None, Some(k)) => Some(single(Instruction::FloatAdd {
                dst: r(12 + (k % 2) as u8),
                lhs: r(8 + (k % 2) as u8),
                rhs: r(12 + (k % 2) as u8),
            })),
            (None, None) if step == 0 => Some(single(Instruction::IntegerSub {
                dst: r(13),
                lhs: r(13),
                rhs: r(13),
            })),
            (None, None) => None,
        };
        if let Some(instruction) = instruction(compute, dm, pm) {
            code.push(instruction);
        }
    }
    code.push(V::Compute(single(Instruction::FloatAdd {
        dst: r(12),
        lhs: r(12),
        rhs: r(13),
    })));
    if accumulate {
        code.push(V::Compute(single(Instruction::FloatAdd {
            dst: r(12),
            lhs: r(12),
            rhs: r(14),
        })));
    }
    code.push(one_access(None, store(12, OUT, 2)));
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
    simd(code, true, false);
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
    simd(code, false, false);
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
                // numerator: 378 + x², then two Horner steps, then × x.
                add(2, 1, 8),
                mul(2, 1, 2),
                add(2, 2, 9),
                mul(2, 1, 2),
                add(2, 2, 10),
                mul(2, 0, 2),
                // denominator: 3150 + 28 x², then two Horner steps.
                mul(3, 1, 11),
                add(3, 3, 12),
                mul(3, 1, 3),
                add(3, 3, 13),
                mul(3, 1, 3),
                add(3, 3, 10),
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
