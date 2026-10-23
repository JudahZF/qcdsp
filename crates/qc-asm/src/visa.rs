//! Leaf-callback VISA forms and parcel-addressed local labels.

use std::collections::HashMap;

use thiserror::Error;

use crate::{Instruction, Register, compute};

/// A universal register code from ADI's R, I, M, L, B, or S register classes.
/// System-register codes are deliberately not exposed by this subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UniversalRegister(u8);

impl UniversalRegister {
    pub const fn new(code: u8) -> Result<Self, VisaError> {
        if code < 0x60 {
            Ok(Self(code))
        } else {
            Err(VisaError::InvalidUniversalRegister(code))
        }
    }

    pub const fn code(self) -> u8 {
        self.0
    }
}

impl From<Register> for UniversalRegister {
    fn from(register: Register) -> Self {
        Self(register.index())
    }
}

/// An I0..I15 index register. I0..I7 are DAG1 (DM), I8..I15 are DAG2 (PM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexRegister(Register);

impl IndexRegister {
    pub const fn new(index: u8) -> Result<Self, crate::Error> {
        match Register::new(index) {
            Ok(register) => Ok(Self(register)),
            Err(error) => Err(error),
        }
    }

    pub const fn index(self) -> u8 {
        self.0.index()
    }
}

impl From<IndexRegister> for UniversalRegister {
    fn from(register: IndexRegister) -> Self {
        Self(0x10 + register.index())
    }
}

/// An M0..M15 modifier register. M0..M7 are DAG1 (DM), M8..M15 are DAG2 (PM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModifyRegister(Register);

impl ModifyRegister {
    pub const fn new(index: u8) -> Result<Self, crate::Error> {
        match Register::new(index) {
            Ok(register) => Ok(Self(register)),
            Err(error) => Err(error),
        }
    }

    pub const fn index(self) -> u8 {
        self.0.index()
    }
}

impl From<ModifyRegister> for UniversalRegister {
    fn from(register: ModifyRegister) -> Self {
        Self(0x20 + register.index())
    }
}

/// Verified Type 8a conditions. Conditional jumps depend on both PEs in SIMD mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    Eq,
    Ne,
    Sv,
    Sz,
    NotSv,
    Always,
    /// Unsigned greater-than after COMPU (AN=0 and AZ=0).
    UnsignedGt,
}

impl Condition {
    fn code(self) -> u64 {
        match self {
            Self::Eq => 0,
            Self::Ne => 16,
            Self::Sv => 7,
            Self::Sz => 8,
            Self::NotSv => 23,
            Self::Always => 31,
            Self::UnsignedGt => 18,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum VisaError {
    #[error(transparent)]
    Encoding(#[from] crate::Error),
    #[error("universal register code {0:#x} is outside the supported R/I/M/L/B/S classes")]
    InvalidUniversalRegister(u8),
    #[error("DM index I{0} is not a DAG1 index (I0..I7)")]
    InvalidDmIndex(u8),
    #[error("DM modifier M{0} is not a DAG1 modifier (M0..M7)")]
    InvalidDmModifier(u8),
    #[error("post-modify DM accesses support only PEx R0..R15, not ureg {0:#x}")]
    UnsupportedPostModifyRegister(u8),
    #[error(
        "ureg {0:#x} is a DAG1 register; DAG1 stores need a UREGXDAG1 source (copy it to a data register first)"
    )]
    Dag1StoreSource(u8),
    #[error("PC-relative offset {0} is outside the signed 24-bit parcel range")]
    BranchOutOfRange(i64),
    #[error("CJUMP target {0:#x} does not fit 24 bits")]
    CallTargetOutOfRange(u32),
    #[error("duplicate VISA label {0:?}")]
    DuplicateLabel(String),
    #[error("undefined VISA label {0:?}")]
    UndefinedLabel(String),
    #[error("PM index I{0} is not a DAG2 index (I8..I15)")]
    InvalidPmIndex(u8),
    #[error("PM modifier M{0} is not a DAG2 modifier (M8..M15)")]
    InvalidPmModifier(u8),
    #[error("index I{index} and modifier M{modifier} belong to different DAGs")]
    MixedDag { index: u8, modifier: u8 },
    #[error("ureg {0:#x} belongs to the DAG that addresses this access (PRM UREGXDAG1/UREGXDAG2)")]
    SameDagRegister(u8),
    #[error("instruction is not a single-function compute")]
    NotACompute,
    #[error("F{register} cannot be the multifunction {operand}; it must be one of {allowed}")]
    MultifunctionOperand {
        operand: &'static str,
        register: u8,
        allowed: &'static str,
    },
    #[error("R{0} is the destination of two parallel operations")]
    DuplicateDestination(u8),
    #[error("a loop count of zero is not supported")]
    LoopCountZero,
    #[error("loop end offset {0} is outside the signed 23-bit parcel range")]
    LoopOutOfRange(i64),
    #[error("hardware loop has no body instruction")]
    EmptyLoop,
    #[error("loop_begin without a matching loop_end")]
    UnclosedLoop,
    #[error("loop_end without an open loop")]
    UnmatchedLoopEnd,
    #[error("nested loops end on the same instruction")]
    SharedLoopEnd,
    #[error(
        "the instruction at parcel {0:#x} is one of the last 11 of a loop body but is not 48-bit"
    )]
    LoopTailNotFullWidth(usize),
    #[error("a branch, call or return sits in a hardware loop body")]
    BranchInLoop,
    #[error("label {0:?} sits in a hardware loop body")]
    LabelInLoop(String),
}

/// MODE1 bit 21: PEy enable (SIMD). The mode changes one cycle after the write.
pub const MODE1_PEYEN: u32 = 1 << 21;
/// MODE1 bit 22: broadcast loads through I9 to PEx and PEy.
pub const MODE1_BDCST9: u32 = 1 << 22;
/// MODE1 bit 23: broadcast loads through I1 to PEx and PEy.
pub const MODE1_BDCST1: u32 = 1 << 23;

/// Memory transfer direction, seen from the register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `register = memory`.
    Load,
    /// `memory = register`.
    Store,
}

/// Type 3a address update: `(I, M)` post-modify or `(M, I)` pre-modify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// `(Ia, Mb)`: address Ia, then Ia += Mb.
    PostModify,
    /// `(Mb, Ia)`: address Ia + Mb; Ia is not updated.
    PreModify,
}

/// The ALU half of a float multiplier/ALU multifunction compute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AluOp {
    /// `Fa = Fxa + Fya` (opcode `011000`).
    Add,
    /// `Fa = Fxa - Fya` (opcode `011001`).
    Sub,
}

/// `Fm = Fxm * Fym, Fa = Fxa +/- Fya`: the float multiplier and ALU in one compute.
///
/// The PRM restricts the inputs to register groups: `mul_x` F0-F3, `mul_y` F4-F7,
/// `alu_x` F8-F11, `alu_y` F12-F15. The two destinations are free but must differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FloatMulAlu {
    pub product: Register,
    pub mul_x: Register,
    pub mul_y: Register,
    pub op: AluOp,
    pub alu_dst: Register,
    pub alu_x: Register,
    pub alu_y: Register,
}

/// A validated 23-bit compute field for Types 1a, 2a and 3a.
///
/// Build one from a single-function compute instruction ([`Compute::single`]) or a
/// float multiplier/ALU multifunction ([`Compute::float_mul_alu`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Compute {
    field: u32,
    destinations: [Option<Register>; 2],
}

impl Compute {
    /// The compute field of a single-function compute: `Instruction::{FloatAdd, FloatSub,
    /// FloatMul, FloatMin, FloatMax, FloatNeg, FloatAbs, FloatPass, IntegerSub}` or
    /// `VisaInstruction::{IntegerSubOne, IntegerPass, IntegerCompareUnsigned}`.
    pub fn single(instruction: impl Into<VisaInstruction>) -> Result<Self, VisaError> {
        let instruction = instruction.into();
        let destination = instruction.compute_destination()?;
        let word = instruction
            .parcels()?
            .into_iter()
            .fold(0_u64, |word, parcel| (word << 16) | u64::from(parcel));
        Ok(Self {
            field: (word & 0x7f_ffff) as u32,
            destinations: [destination, None],
        })
    }

    /// PRM "Multiplier and ALU" multifunction field: bit 22 = 1, opcode 21:16,
    /// Fm 15:12, Fa 11:8, then the 2-bit group offsets of Fxm, Fym, Fxa, Fya.
    pub fn float_mul_alu(compute: FloatMulAlu) -> Result<Self, VisaError> {
        let operand = |register: Register, base: u8, operand, allowed| {
            if register.index() & !3 == base {
                Ok(u32::from(register.index() & 3))
            } else {
                Err(VisaError::MultifunctionOperand {
                    operand,
                    register: register.index(),
                    allowed,
                })
            }
        };
        let mul_x = operand(compute.mul_x, 0, "multiplier X input", "F0-F3")?;
        let mul_y = operand(compute.mul_y, 4, "multiplier Y input", "F4-F7")?;
        let alu_x = operand(compute.alu_x, 8, "ALU X input", "F8-F11")?;
        let alu_y = operand(compute.alu_y, 12, "ALU Y input", "F12-F15")?;
        if compute.product == compute.alu_dst {
            return Err(VisaError::DuplicateDestination(compute.product.index()));
        }
        let opcode = match compute.op {
            AluOp::Add => 0x18,
            AluOp::Sub => 0x19,
        };
        Ok(Self {
            field: (1 << 22)
                | (opcode << 16)
                | (u32::from(compute.product.index()) << 12)
                | (u32::from(compute.alu_dst.index()) << 8)
                | (mul_x << 6)
                | (mul_y << 4)
                | (alu_x << 2)
                | alu_y,
            destinations: [Some(compute.product), Some(compute.alu_dst)],
        })
    }

    /// The 23-bit field placed in bits 22:0 of the instruction.
    pub const fn field(self) -> u32 {
        self.field
    }
}

/// One post-modify half of a Type 1a dual move: a PEx data register and `(I, M)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataAccess {
    pub direction: Direction,
    pub register: Register,
    pub index: IndexRegister,
    pub modifier: ModifyRegister,
}

/// A Type 3a single access. I0-I7 with M0-M7 address DM through DAG1;
/// I8-I15 with M8-M15 address PM through DAG2 (the G bit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryAccess {
    pub direction: Direction,
    pub register: UniversalRegister,
    pub index: IndexRegister,
    pub modifier: ModifyRegister,
    pub update: Update,
}

/// Type 12a loop count: `LCNTR = data16` or `LCNTR = ureg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopCount {
    Immediate(u16),
    /// The register must hold a nonzero count; the encoder cannot check it.
    Register(UniversalRegister),
}

/// Type 12a mode bit 23. F1-active ends a loop with no lost cycles; it is valid
/// because loop bodies in this crate never hold a change of flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopMode {
    E2Active,
    F1Active,
}

/// Verified VISA forms for leaf callbacks and frame kernels.
///
/// Post-modify DM data-register accesses use verified 16-bit forms. Transfers and
/// other small DM accesses use 32-bit forms. Other operations use full 48-bit forms.
/// `LeafReturn` is the stock six-parcel, four-instruction return sequence, not a
/// generic RTS. Only 48-bit forms may end a hardware loop: see the README table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisaInstruction {
    Scalar(Instruction),
    /// Type 5b universal-register transfer, not a computation or swap.
    Transfer {
        dst: UniversalRegister,
        src: UniversalRegister,
    },
    /// Type 17a raw 32-bit immediate write.
    LoadImmediate {
        dst: UniversalRegister,
        value: u32,
    },
    /// Type 19a: index = MODIFY(index, offset)(NW). Does not access memory.
    ModifyIndex {
        index: IndexRegister,
        offset: i32,
    },
    /// Type 3c: post-modify, with index update. Only PEx R0..R15 are supported.
    LoadDmPostModify {
        dst: UniversalRegister,
        index: IndexRegister,
        modifier: ModifyRegister,
    },
    /// Type 3c: post-modify, with index update. Only PEx R0..R15 are supported.
    StoreDmPostModify {
        src: UniversalRegister,
        index: IndexRegister,
        modifier: ModifyRegister,
    },
    /// Type 3b: dst = DM(modifier, index); does not update the index.
    LoadDmPreModify {
        dst: UniversalRegister,
        index: IndexRegister,
        modifier: ModifyRegister,
    },
    /// Type 3b: DM(modifier, index) = src; does not update the index.
    StoreDmPreModify {
        src: UniversalRegister,
        index: IndexRegister,
        modifier: ModifyRegister,
    },
    /// Type 15b (-64..63) or Type 15a (all other offsets). Does not update index.
    LoadDmOffset {
        dst: UniversalRegister,
        index: IndexRegister,
        offset: i32,
    },
    /// Type 15b (-64..63) or Type 15a (all other offsets). Does not update index.
    StoreDmOffset {
        src: UniversalRegister,
        index: IndexRegister,
        offset: i32,
    },
    /// Type 2a ALU decrement, including arithmetic status updates (AZ, etc.).
    IntegerSubOne {
        dst: Register,
        src: Register,
    },
    /// Type 2a integer ALU PASS, not a register transfer.
    IntegerPass {
        dst: Register,
        src: Register,
    },
    /// Integer unsigned comparison; sets AZ and AN, with no destination.
    IntegerCompareUnsigned {
        lhs: Register,
        rhs: Register,
    },
    /// Type 8a: offset is relative to the first parcel of this instruction.
    /// A delayed jump does not insert or validate its two delay-slot instructions.
    JumpRelative {
        condition: Condition,
        offset: i32,
        delayed: bool,
    },
    /// Type 25a `CJUMP addr24 (DB)`: R2 = I6, I6 = I7, then an absolute jump whose two delay
    /// slots must push R2 and the return literal (the stock C call, e.g. digital_delay
    /// `0x38c4..0x38c8`). Not a hardware CALL: nothing is pushed on the PC stack.
    CJump {
        target: u32,
    },
    /// Type 16a `DM(I7,M7) = imm32`: pushes a literal (the C call's return address − 1).
    PushImmediate {
        value: u32,
    },
    /// I12=DM(M7,I6); JUMP(M14,I12)(DB); RFRAME; NOP.
    /// Assumes the stock C-runtime frame and M7/M14 values; not an ABI validator.
    LeafReturn,
    /// Type 2a `compute;` with any [`Compute`], including multifunction. 48-bit.
    Compute(Compute),
    /// Type 1a `compute, DM(Ia,Mb) <-> Rx, PM(Ic,Md) <-> Ry;`. 48-bit.
    /// Both accesses always happen: `dm` must use I0-I7/M0-M7, `pm` I8-I15/M8-M15.
    DualMove {
        compute: Option<Compute>,
        dm: DataAccess,
        pm: DataAccess,
    },
    /// Type 3a `compute, ureg <-> DM|PM(...);`, unconditional, normal word. 48-bit.
    MemoryMove {
        compute: Option<Compute>,
        access: MemoryAccess,
    },
    /// Type 16a `DM(Ia,Mb) = imm32` (I0-I7) or `PM(Ic,Md) = imm32` (I8-I15),
    /// post-modify. 48-bit.
    StoreImmediate {
        index: IndexRegister,
        modifier: ModifyRegister,
        value: u32,
    },
    /// Type 18a `BIT SET MODE1 mask;`. 48-bit.
    BitSetMode1 {
        mask: u32,
    },
    /// Type 18a `BIT CLR MODE1 mask;`. 48-bit.
    BitClearMode1 {
        mask: u32,
    },
    /// Type 21c `NOP;`, 16 bits. Not valid in the last 11 instructions of a loop.
    Nop,
    /// Type 21a `NOP;`, the 48-bit all-zero word.
    WideNop,
}

impl From<Instruction> for VisaInstruction {
    fn from(instruction: Instruction) -> Self {
        Self::Scalar(instruction)
    }
}

/// Stock return parcels, in instruction order. Each parcel is little-endian in QC.
pub const LEAF_RETURN_PARCELS: [u16; 6] = [0x4dfe, 0x0e3f, 0x083f, 0x343f, 0x1901, 0x0001];

impl VisaInstruction {
    /// Returns parcels in execution order, without a container byte order.
    pub fn parcels(self) -> Result<Vec<u16>, VisaError> {
        let word = match self {
            Self::Scalar(instruction) => instruction.word()?,
            Self::Transfer { dst, src } => {
                // ADI Type 5b: source is split over 26:22, 16, and 15;
                // destination is 13:7; low 7 bits are the short-form marker.
                let src = u64::from(src.code());
                let word = (0x70_u64 << 24)
                    | ((src >> 2) << 22)
                    | (0x1f_u64 << 17)
                    | ((src & 2) << 15)
                    | ((src & 1) << 15)
                    | (u64::from(dst.code()) << 7)
                    | 0x3f;
                return Ok(vec![(word >> 16) as u16, word as u16]);
            }
            Self::LoadImmediate { dst, value } => {
                (0x0f_u64 << 40) | (u64::from(dst.code()) << 32) | u64::from(value)
            }
            Self::ModifyIndex { index, offset } => {
                // Type 19a: SC=01, W=1 (NW), G selects DAG2. IDIS=0
                // selects the source as destination; no cross-index modify here.
                (0x1580_u64 << 32)
                    | (u64::from(index.index() / 8) << 38)
                    | (u64::from(index.index() % 8) << 32)
                    | u64::from(offset as u32)
            }
            Self::LoadDmPostModify {
                dst,
                index,
                modifier,
            } => return dm_modify(dst, index, modifier, false, true),
            Self::StoreDmPostModify {
                src,
                index,
                modifier,
            } => return dm_modify(src, index, modifier, true, true),
            Self::LoadDmPreModify {
                dst,
                index,
                modifier,
            } => {
                return dm_modify(dst, index, modifier, false, false);
            }
            Self::StoreDmPreModify {
                src,
                index,
                modifier,
            } => {
                return dm_modify(src, index, modifier, true, false);
            }
            Self::LoadDmOffset { dst, index, offset } => {
                return dm_offset(dst, index, offset, false);
            }
            Self::StoreDmOffset { src, index, offset } => {
                return dm_offset(src, index, offset, true);
            }
            Self::IntegerSubOne { dst, src } => compute(0, 0x2a, dst, src, 0),
            Self::IntegerPass { dst, src } => compute(0, 0x21, dst, src, 0),
            Self::IntegerCompareUnsigned { lhs, rhs } => {
                compute(0, 0x0b, Register::new(0).expect("R0"), lhs, rhs.index())
            }
            Self::JumpRelative {
                condition,
                offset,
                delayed,
            } => jump_word(condition, i64::from(offset), delayed)?,
            Self::CJump { target } => {
                if target > 0xff_ffff {
                    return Err(VisaError::CallTargetOutOfRange(target));
                }
                // Type 25a: 00011000, J (DB) at bit 34, ADDR24.
                0x1804_0000_0000 | u64::from(target)
            }
            // Type 16a: 100, 1, I=7 (43:41), M=7 (40:38), G=0, DATA32.
            Self::PushImmediate { value } => 0x9fc0_0000_0000 | u64::from(value),
            Self::LeafReturn => return Ok(LEAF_RETURN_PARCELS.to_vec()),
            // Type 2a shell: bit 40, COND=11111 in 37:33.
            Self::Compute(compute) => (1 << 40) | (0x1f << 33) | u64::from(compute.field),
            Self::DualMove { compute, dm, pm } => dual_move(compute, dm, pm)?,
            Self::MemoryMove { compute, access } => memory_move(compute, access)?,
            Self::StoreImmediate {
                index,
                modifier,
                value,
            } => {
                // Type 16a: 1001, I 43:41, M 40:38, G 37, 36:34 = 000, SL=BY=0.
                let (i, m, pm) = dag_fields(index, modifier)?;
                (0x9 << 44) | (i << 41) | (m << 38) | (u64::from(pm) << 37) | u64::from(value)
            }
            // Type 18a: 00010100, BOP 39:37 (SET 000, CLR 001), SREG 35:32 (MODE1 = 2).
            Self::BitSetMode1 { mask } => 0x1402_0000_0000 | u64::from(mask),
            Self::BitClearMode1 { mask } => 0x1422_0000_0000 | u64::from(mask),
            Self::Nop => return Ok(vec![0x0001]),
            Self::WideNop => 0,
        };
        Ok(full_word_parcels(word))
    }

    /// Changes of flow, which this crate keeps out of hardware loop bodies.
    fn is_branch(self) -> bool {
        matches!(
            self,
            Self::JumpRelative { .. } | Self::CJump { .. } | Self::LeafReturn
        )
    }

    /// The destination of a single-function compute form, or `NotACompute`.
    fn compute_destination(self) -> Result<Option<Register>, VisaError> {
        use Instruction as I;
        match self {
            Self::Scalar(
                I::FloatAdd { dst, .. }
                | I::FloatSub { dst, .. }
                | I::FloatMul { dst, .. }
                | I::FloatMin { dst, .. }
                | I::FloatMax { dst, .. }
                | I::FloatNeg { dst, .. }
                | I::FloatAbs { dst, .. }
                | I::FloatPass { dst, .. }
                | I::IntegerSub { dst, .. },
            )
            | Self::IntegerSubOne { dst, .. }
            | Self::IntegerPass { dst, .. } => Ok(Some(dst)),
            Self::IntegerCompareUnsigned { .. } => Ok(None),
            _ => Err(VisaError::NotACompute),
        }
    }

    /// Emits section-2 bytes: high-to-low parcels, each parcel little-endian.
    pub fn encode(self) -> Result<Vec<u8>, VisaError> {
        Ok(self
            .parcels()?
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect())
    }
}

fn full_word_parcels(word: u64) -> Vec<u16> {
    vec![(word >> 32) as u16, (word >> 16) as u16, word as u16]
}

/// ADI's UREGXDAG1 class: a store through a DAG1 index may not take its source
/// from DAG1's own I, M, L or B registers (codes 0x10-0x17, 0x20-0x27, 0x30-0x37,
/// 0x40-0x47). Loads into those registers are legal and used by stock code.
fn check_store_source(register: UniversalRegister) -> Result<(), VisaError> {
    let code = register.code();
    if matches!(code >> 4, 1..=4) && code & 0x0f < 8 {
        return Err(VisaError::Dag1StoreSource(code));
    }
    Ok(())
}

/// The 3-bit I and M fields and whether the pair is DAG2 (PM, G=1).
fn dag_fields(
    index: IndexRegister,
    modifier: ModifyRegister,
) -> Result<(u64, u64, bool), VisaError> {
    let pm = index.index() >= 8;
    if (modifier.index() >= 8) != pm {
        return Err(VisaError::MixedDag {
            index: index.index(),
            modifier: modifier.index(),
        });
    }
    Ok((
        u64::from(index.index() & 7),
        u64::from(modifier.index() & 7),
        pm,
    ))
}

/// `true` if the ureg is an I, M, L or B register of the given DAG.
fn in_dag(register: UniversalRegister, pm: bool) -> bool {
    let code = register.code();
    matches!(code >> 4, 1..=4) && ((code & 0x0f) >= 8) == pm
}

/// PRM 05.48: parallel operations may not share a destination register.
fn check_destinations(
    compute: Option<Compute>,
    loads: impl IntoIterator<Item = u8>,
) -> Result<(), VisaError> {
    let mut written: Vec<u8> = compute
        .into_iter()
        .flat_map(|compute| compute.destinations)
        .flatten()
        .map(Register::index)
        .collect();
    for load in loads {
        if written.contains(&load) {
            return Err(VisaError::DuplicateDestination(load));
        }
        written.push(load);
    }
    Ok(())
}

/// Type 1a: 001, DMD 44, DMI 43:41, DMM 40:38, PMD 37, DMDREG 36:33,
/// PMI 32:30, PMM 29:27, PMDREG 26:23, COMPUTE 22:0.
fn dual_move(compute: Option<Compute>, dm: DataAccess, pm: DataAccess) -> Result<u64, VisaError> {
    if dm.index.index() >= 8 {
        return Err(VisaError::InvalidDmIndex(dm.index.index()));
    }
    if dm.modifier.index() >= 8 {
        return Err(VisaError::InvalidDmModifier(dm.modifier.index()));
    }
    if pm.index.index() < 8 {
        return Err(VisaError::InvalidPmIndex(pm.index.index()));
    }
    if pm.modifier.index() < 8 {
        return Err(VisaError::InvalidPmModifier(pm.modifier.index()));
    }
    let loads = [dm, pm]
        .into_iter()
        .filter(|access| access.direction == Direction::Load)
        .map(|access| access.register.index());
    check_destinations(compute, loads)?;
    let store = |access: DataAccess| u64::from(access.direction == Direction::Store);
    Ok((1 << 45)
        | (store(dm) << 44)
        | (u64::from(dm.index.index()) << 41)
        | (u64::from(dm.modifier.index()) << 38)
        | (store(pm) << 37)
        | (u64::from(dm.register.index()) << 33)
        | (u64::from(pm.index.index() & 7) << 30)
        | (u64::from(pm.modifier.index() & 7) << 27)
        | (u64::from(pm.register.index()) << 23)
        | u64::from(compute.map_or(0, Compute::field)))
}

/// Type 3a: 010, U 44, I 43:41, M 40:38, COND 37:33, G 32, D 31, L 30,
/// UREG 29:23, COMPUTE 22:0. Post-modify accesses and all stores need a ureg
/// outside the addressing DAG (UREGXDAG1 / UREGXDAG2); pre-modify loads take any ureg.
fn memory_move(compute: Option<Compute>, access: MemoryAccess) -> Result<u64, VisaError> {
    let (i, m, pm) = dag_fields(access.index, access.modifier)?;
    let post = access.update == Update::PostModify;
    let store = access.direction == Direction::Store;
    if (post || store) && in_dag(access.register, pm) {
        return Err(VisaError::SameDagRegister(access.register.code()));
    }
    let code = access.register.code();
    check_destinations(compute, (!store && code < 16).then_some(code))?;
    Ok((2 << 45)
        | (u64::from(post) << 44)
        | (i << 41)
        | (m << 38)
        | (0x1f << 33)
        | (u64::from(pm) << 32)
        | (u64::from(store) << 31)
        | (u64::from(code) << 23)
        | u64::from(compute.map_or(0, Compute::field)))
}

fn dm_index(index: IndexRegister) -> Result<u64, VisaError> {
    if index.index() < 8 {
        Ok(u64::from(index.index()))
    } else {
        Err(VisaError::InvalidDmIndex(index.index()))
    }
}

fn dm_modify(
    register: UniversalRegister,
    index: IndexRegister,
    modifier: ModifyRegister,
    store: bool,
    post_modify: bool,
) -> Result<Vec<u16>, VisaError> {
    let index = dm_index(index)?;
    if store {
        check_store_source(register)?;
    }
    if modifier.index() >= 8 {
        return Err(VisaError::InvalidDmModifier(modifier.index()));
    }
    if post_modify {
        if register.code() >= 16 {
            return Err(VisaError::UnsupportedPostModifyRegister(register.code()));
        }
        // ADI Type 3c: DM post-modify, R0..R15, fixed bit 4=1.
        let parcel = 0x9010
            | ((index as u16) << 9)
            | (u16::from(modifier.index()) << 6)
            | (u16::from(store) << 5)
            | u16::from(register.code());
        return Ok(vec![parcel]);
    }
    // Type 3b: U=0 for pre-modify; G=0 (DM),
    // L=0, X=W=1 (normal width), fixed marker in bits 5:2.
    // Relative to Type 3a, each field is shifted down by one parcel:
    // U=28, I=27:25, M=24:22, COND=21:17, G=16, D=15, UREG=13:7.
    let word = (0x40_u64 << 24)
        | (index << 25)
        | (u64::from(modifier.index()) << 22)
        | (0x1f_u64 << 17)
        | (u64::from(store) << 15)
        | (u64::from(register.code()) << 7)
        | 0x3f;
    Ok(vec![(word >> 16) as u16, word as u16])
}

fn dm_offset(
    register: UniversalRegister,
    index: IndexRegister,
    offset: i32,
    store: bool,
) -> Result<Vec<u16>, VisaError> {
    let index = dm_index(index)?;
    if store {
        check_store_source(register)?;
    }
    if (-64..=63).contains(&offset) {
        // Type 15b: signed DATA7, normal width (L=0), G=0 (DM).
        let word = (0x9008_u64 << 16)
            | (index << 25)
            | (u64::from(store) << 24)
            | (u64::from(register.code()) << 7)
            | u64::from((offset as u32) & 0x7f);
        Ok(vec![(word >> 16) as u16, word as u16])
    } else {
        // Type 15a: 32-bit modifier, no index update. No scratch register needed.
        Ok(full_word_parcels(
            (0xa0_u64 << 40)
                | (index << 41)
                | (u64::from(store) << 40)
                | (u64::from(register.code()) << 32)
                | u64::from(offset as u32),
        ))
    }
}

fn jump_word(condition: Condition, offset: i64, delayed: bool) -> Result<u64, VisaError> {
    if !(-0x80_0000..=0x7f_ffff).contains(&offset) {
        return Err(VisaError::BranchOutOfRange(offset));
    }
    // Type 8a: R=1 (PC-relative), B=A=CI=0, J selects delayed branch.
    Ok((0x07_u64 << 40)
        | (condition.code() << 33)
        | (u64::from(delayed) << 26)
        | ((offset as u64) & 0xff_ffff))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Instruction(VisaInstruction),
    Label(String),
    Jump {
        condition: Condition,
        target: String,
    },
    LoopBegin {
        count: LoopCount,
        mode: LoopMode,
    },
    LoopEnd,
}

/// The PRM (`.06.062`) requires the last 11 instructions of a loop body to be 48-bit.
const LOOP_TAIL: usize = 11;

/// A loop whose `loop_end` has not been reached yet.
struct OpenLoop {
    /// Parcel address of the Type 12a instruction.
    at: usize,
    count: LoopCount,
    mode: LoopMode,
    /// Index of the first body instruction in the instruction list.
    first: usize,
}

/// Type 12a: 0x0c DATA16 39:24 or 0x0d UREG 38:32, MODE 23, RELADDR 22:0.
fn loop_word(count: LoopCount, mode: LoopMode, offset: i64) -> Result<u64, VisaError> {
    if !(-0x40_0000..=0x3f_ffff).contains(&offset) {
        return Err(VisaError::LoopOutOfRange(offset));
    }
    let count = match count {
        LoopCount::Immediate(0) => return Err(VisaError::LoopCountZero),
        LoopCount::Immediate(count) => (0x0c << 40) | (u64::from(count) << 24),
        LoopCount::Register(register) => (0x0d << 40) | (u64::from(register.code()) << 32),
    };
    Ok(count | (u64::from(mode == LoopMode::F1Active) << 23) | ((offset as u64) & 0x7f_ffff))
}

/// A VISA stream with local, PC-relative labels and counted hardware loops,
/// measured in 16-bit parcels.
/// No instruction alignment padding, compression pass, or scheduling is performed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VisaProgram {
    items: Vec<Item>,
}

impl VisaProgram {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, instruction: impl Into<VisaInstruction>) {
        self.items.push(Item::Instruction(instruction.into()));
    }

    /// Binds a label at the current parcel position. Duplicates fail on encode.
    pub fn label(&mut self, name: impl Into<String>) {
        self.items.push(Item::Label(name.into()));
    }

    /// Emits a non-delayed Type 8a jump. Offset is target minus branch-start PC.
    pub fn jump(&mut self, condition: Condition, target: impl Into<String>) {
        self.items.push(Item::Jump {
            condition,
            target: target.into(),
        });
    }

    /// Emits a 48-bit Type 12a `LCNTR = count, DO (PC, reladdr) UNTIL LCE`. The body is
    /// every instruction pushed until the matching [`loop_end`](Self::loop_end); loops nest.
    ///
    /// `parcels()` rejects the loop if any of the last 11 body instructions (all of
    /// them for a shorter body) is not 48-bit, if the body holds a jump, call, return
    /// or label, if it is empty, or if a nested loop ends on the same instruction.
    pub fn loop_begin(&mut self, count: LoopCount, mode: LoopMode) {
        self.items.push(Item::LoopBegin { count, mode });
    }

    /// Closes the innermost open loop: the instruction pushed last is its end.
    pub fn loop_end(&mut self) {
        self.items.push(Item::LoopEnd);
    }

    /// Instructions, jumps and loop starts included (`LeafReturn` is four); labels and loop
    /// ends are not instructions. A SHARC+ core issues one instruction per cycle whatever
    /// its parcel width, so this (not the parcel count) is the base of a cycle estimate.
    pub fn instruction_count(&self) -> usize {
        self.items
            .iter()
            .map(|item| match item {
                Item::Label(_) | Item::LoopEnd => 0,
                Item::Instruction(VisaInstruction::LeafReturn) => 4,
                Item::Instruction(_) | Item::Jump { .. } | Item::LoopBegin { .. } => 1,
            })
            .sum()
    }

    /// Parcels emitted so far. Labels are not resolved (a jump or loop start is always
    /// three parcels), so this works while a forward label is still undefined.
    pub fn parcel_count(&self) -> Result<usize, VisaError> {
        self.items
            .iter()
            .map(|item| match item {
                Item::Instruction(instruction) => instruction.parcels().map(|p| p.len()),
                Item::Label(_) | Item::LoopEnd => Ok(0),
                Item::Jump { .. } | Item::LoopBegin { .. } => Ok(3),
            })
            .sum()
    }

    /// Resolves labels and loops and returns parcels in execution order. Labels emit
    /// nothing. A loop's reladdr is the first parcel of its last body instruction minus
    /// the first parcel of the DO instruction.
    pub fn parcels(&self) -> Result<Vec<u16>, VisaError> {
        let mut labels = HashMap::new();
        let mut parcels = Vec::new();
        let mut fixups = Vec::new();
        // (first parcel, parcel count) of every instruction, for the loop-tail rule.
        let mut instructions: Vec<(usize, usize)> = Vec::new();
        let mut open: Vec<OpenLoop> = Vec::new();
        let mut last_end = None;
        for item in &self.items {
            match item {
                Item::Instruction(instruction) => {
                    if !open.is_empty() && instruction.is_branch() {
                        return Err(VisaError::BranchInLoop);
                    }
                    let words = instruction.parcels()?;
                    instructions.push((parcels.len(), words.len()));
                    parcels.extend(words);
                }
                Item::Label(name) => {
                    if !open.is_empty() {
                        return Err(VisaError::LabelInLoop(name.clone()));
                    }
                    if labels.insert(name.as_str(), parcels.len()).is_some() {
                        return Err(VisaError::DuplicateLabel(name.clone()));
                    }
                }
                Item::Jump { condition, target } => {
                    if !open.is_empty() {
                        return Err(VisaError::BranchInLoop);
                    }
                    fixups.push((parcels.len(), *condition, target));
                    instructions.push((parcels.len(), 3));
                    parcels.extend([0; 3]);
                }
                &Item::LoopBegin { count, mode } => {
                    instructions.push((parcels.len(), 3));
                    open.push(OpenLoop {
                        at: parcels.len(),
                        count,
                        mode,
                        first: instructions.len(),
                    });
                    parcels.extend([0; 3]);
                }
                Item::LoopEnd => {
                    let open = open.pop().ok_or(VisaError::UnmatchedLoopEnd)?;
                    let body = &instructions[open.first..];
                    let &(end, _) = body.last().ok_or(VisaError::EmptyLoop)?;
                    if last_end.replace(end) == Some(end) {
                        return Err(VisaError::SharedLoopEnd);
                    }
                    if let Some(&(at, _)) = body.iter().rev().take(LOOP_TAIL).find(|i| i.1 != 3) {
                        return Err(VisaError::LoopTailNotFullWidth(at));
                    }
                    let offset = end as i64 - open.at as i64;
                    parcels[open.at..open.at + 3].copy_from_slice(&full_word_parcels(loop_word(
                        open.count, open.mode, offset,
                    )?));
                }
            }
        }
        if !open.is_empty() {
            return Err(VisaError::UnclosedLoop);
        }
        for (at, condition, target) in fixups {
            let &destination = labels
                .get(target.as_str())
                .ok_or_else(|| VisaError::UndefinedLabel(target.clone()))?;
            // Vec lengths cannot exceed isize::MAX, so these conversions fit i64
            // on supported 32/64-bit hosts. Range validation precedes truncation.
            let offset = destination as i64 - at as i64;
            parcels[at..at + 3]
                .copy_from_slice(&full_word_parcels(jump_word(condition, offset, false)?));
        }
        Ok(parcels)
    }

    /// Emits a complete section-2 fragment with little-endian 16-bit parcels.
    pub fn encode(&self) -> Result<Vec<u8>, VisaError> {
        Ok(self
            .parcels()?
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect())
    }
}
