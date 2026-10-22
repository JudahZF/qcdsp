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

/// An I0..I15 index register. DM accesses in this subset require DAG1 (I0..I7).
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

/// An M0..M15 modifier register. DM accesses in this subset require M0..M7.
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
}

/// Verified VISA forms needed by a leaf callback.
///
/// Post-modify DM data-register accesses use verified 16-bit forms. Transfers and
/// other small DM accesses use 32-bit forms. Other operations use full 48-bit forms.
/// `LeafReturn` is the stock six-parcel, four-instruction return sequence, not a
/// generic RTS.
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
        };
        Ok(full_word_parcels(word))
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
}

/// A VISA stream with local, PC-relative labels measured in 16-bit parcels.
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

    /// Instructions, jumps included; labels are not instructions. A SHARC+ core issues one
    /// instruction per cycle whatever its parcel width, so this (not the parcel count) is
    /// the base of a cycle estimate.
    pub fn instruction_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| !matches!(item, Item::Label(_)))
            .count()
    }

    /// Resolves labels and returns parcels in execution order. Labels emit nothing.
    pub fn parcels(&self) -> Result<Vec<u16>, VisaError> {
        let mut labels = HashMap::new();
        let mut parcels = Vec::new();
        let mut fixups = Vec::new();
        for item in &self.items {
            match item {
                Item::Instruction(instruction) => parcels.extend(instruction.parcels()?),
                Item::Label(name) => {
                    if labels.insert(name.as_str(), parcels.len()).is_some() {
                        return Err(VisaError::DuplicateLabel(name.clone()));
                    }
                }
                Item::Jump { condition, target } => {
                    fixups.push((parcels.len(), *condition, target));
                    parcels.extend([0; 3]);
                }
            }
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
