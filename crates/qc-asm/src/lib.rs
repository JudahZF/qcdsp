//! A restricted SHARC+ ISA48/VISA encoder derived from ADI's programming reference.
//!
//! This crate encodes scalar fragments, restricted leaf-callback sequences, and the
//! frame-kernel forms (dual moves, multifunction computes, counted loops, MODE1 bits).
//! It does not track MODE1, allocate registers, schedule memory hazards, or establish an
//! ABI. Float precision, rounding, exceptions, and SIMD execution depend on the
//! caller's processor configuration. See the crate README for encoding evidence,
//! serialization, address units, and unsupported operations.

#![doc = include_str!("../README.md")]

use thiserror::Error;

mod visa;

pub use visa::{
    AluOp, Compute, Condition, DataAccess, Direction, FloatMulAlu, IndexRegister,
    LEAF_RETURN_PARCELS, LoopCount, LoopMode, MODE1_BDCST1, MODE1_BDCST9, MODE1_PEYEN,
    MemoryAccess, ModifyRegister, UniversalRegister, Update, VisaError, VisaInstruction,
    VisaProgram,
};

/// The number of serialized bytes in one normal-word ISA instruction.
pub const INSTRUCTION_BYTES: usize = 6;

/// A PEx data register. Rn and Fn are views of the same physical register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Register(u8);

impl Register {
    pub const fn new(index: u8) -> Result<Self, Error> {
        if index < 16 {
            Ok(Self(index))
        } else {
            Err(Error::InvalidRegister(index))
        }
    }

    pub const fn index(self) -> u8 {
        self.0
    }
}

/// Execution mode, not just the number of bytes occupied by an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeMode {
    Isa48,
    /// Full 48-bit ISA/VISA forms, serialized as three high-to-low 16-bit parcels.
    /// No compression or execution-mode transition is performed.
    Visa,
}

/// Container byte order: per instruction for ISA48, per 16-bit parcel for VISA.
/// Instruction bit numbering and VISA parcel order do not depend on this choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteOrder {
    LittleEndian,
    BigEndian,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Error {
    #[error("data register index {0} is outside R0..R15")]
    InvalidRegister(u8),
    /// Retained for source compatibility; both current code modes are supported.
    #[error("unsupported code mode {0:?}")]
    UnsupportedCodeMode(CodeMode),
}

/// Verified, unconditional, single-function ISA operations.
///
/// Arithmetic modifies the processor's arithmetic status. `Move` is a Type 5a
/// register transfer, not an integer or floating-point PASS computation. Immediate
/// values are raw 32-bit patterns: use `f32::to_bits()` for floating constants.
/// Direct DM addresses are processor addresses, not host byte offsets; the caller
/// must supply the correct address space and arrange load-use hazard handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Instruction {
    FloatAdd {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    FloatSub {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    FloatMul {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    FloatMin {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    FloatMax {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    FloatNeg {
        dst: Register,
        src: Register,
    },
    FloatAbs {
        dst: Register,
        src: Register,
    },
    /// Fixed-point ALU `Rn = Rx - Ry` on the 32-bit register bits.
    IntegerSub {
        dst: Register,
        lhs: Register,
        rhs: Register,
    },
    /// Floating-point ALU PASS; unlike `Move`, affects arithmetic status/precision.
    FloatPass {
        dst: Register,
        src: Register,
    },
    LoadImmediate {
        dst: Register,
        value: u32,
    },
    Move {
        dst: Register,
        src: Register,
    },
    LoadDm {
        dst: Register,
        address: u32,
    },
    StoreDm {
        src: Register,
        address: u32,
    },
}

impl Instruction {
    /// Returns the instruction word with bits 47:0 significant, before serialization.
    pub fn word(&self) -> Result<u64, Error> {
        let word = match *self {
            Self::FloatAdd { dst, lhs, rhs } => compute(0, 0x81, dst, lhs, rhs.index()),
            Self::FloatSub { dst, lhs, rhs } => compute(0, 0x82, dst, lhs, rhs.index()),
            Self::FloatMul { dst, lhs, rhs } => compute(1, 0x30, dst, lhs, rhs.index()),
            Self::FloatMin { dst, lhs, rhs } => compute(0, 0xe1, dst, lhs, rhs.index()),
            Self::FloatMax { dst, lhs, rhs } => compute(0, 0xe2, dst, lhs, rhs.index()),
            Self::FloatNeg { dst, src } => compute(0, 0xa2, dst, src, 0),
            Self::FloatAbs { dst, src } => compute(0, 0xb0, dst, src, 0),
            Self::FloatPass { dst, src } => compute(0, 0xa1, dst, src, 0),
            Self::IntegerSub { dst, lhs, rhs } => compute(0, 0x02, dst, lhs, rhs.index()),
            Self::LoadImmediate { dst, value } => {
                (0x0f_u64 << 40) | (u64::from(dst.index()) << 32) | u64::from(value)
            }
            Self::Move { dst, src } => {
                let src = u64::from(src.index());
                (0x70_u64 << 40)
                    | (0x1f_u64 << 33)
                    | ((src >> 2) << 38)
                    | ((src & 2) << 31)
                    | ((src & 1) << 31)
                    | (u64::from(dst.index()) << 23)
            }
            Self::LoadDm { dst, address } => {
                (0x10_u64 << 40) | (u64::from(dst.index()) << 32) | u64::from(address)
            }
            Self::StoreDm { src, address } => {
                (0x11_u64 << 40) | (u64::from(src.index()) << 32) | u64::from(address)
            }
        };
        Ok(word)
    }

    /// Serializes one normal-word ISA instruction in little-endian order.
    pub fn encode(&self) -> Result<[u8; INSTRUCTION_BYTES], Error> {
        self.encode_with_order(ByteOrder::LittleEndian)
    }

    /// Serializes one full-width instruction for ModuleBlob section 2 (VISA).
    ///
    /// Each of the three 16-bit parcels is little-endian, with the most significant
    /// parcel first. Only verified ISA/VISA forms are represented by `Instruction`;
    /// they need no extra escape parcel. This does not compress or switch mode.
    pub fn encode_visa(&self) -> Result<[u8; INSTRUCTION_BYTES], Error> {
        self.encode_visa_with_order(ByteOrder::LittleEndian)
    }

    /// Serializes a full-width VISA instruction with the specified parcel byte order.
    /// Big-endian parcels are useful for external decoder/ELF consumers, not QC blobs.
    pub fn encode_visa_with_order(
        &self,
        order: ByteOrder,
    ) -> Result<[u8; INSTRUCTION_BYTES], Error> {
        let mut bytes = self.encode_with_order(ByteOrder::BigEndian)?;
        if order == ByteOrder::LittleEndian {
            for parcel in bytes.chunks_exact_mut(2) {
                parcel.swap(0, 1);
            }
        }
        Ok(bytes)
    }

    /// Serializes one ISA48 word, not a little-endian VISA stream.
    pub fn encode_with_order(&self, order: ByteOrder) -> Result<[u8; INSTRUCTION_BYTES], Error> {
        let word = self.word()?;
        Ok(match order {
            ByteOrder::LittleEndian => {
                let bytes = word.to_le_bytes();
                [bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]]
            }
            ByteOrder::BigEndian => {
                let bytes = word.to_be_bytes();
                [bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]]
            }
        })
    }
}

// Type 2a: unconditional condition 11111 in bits 37:33. SINGLEFN:
// cu=21:20, opcode=19:12, rn=11:8, rx=7:4, ry=3:0; mf=22 is zero.
fn compute(unit: u64, opcode: u64, dst: Register, lhs: Register, rhs: u8) -> u64 {
    (1_u64 << 40)
        | (0x1f_u64 << 33)
        | (unit << 20)
        | (opcode << 12)
        | (u64::from(dst.index()) << 8)
        | (u64::from(lhs.index()) << 4)
        | u64::from(rhs)
}

/// An ordered, straight-line instruction list.
/// Each item occupies one ISA48 word address or three VISA parcel addresses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Program {
    instructions: Vec<Instruction>,
}

impl Program {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, instruction: Instruction) {
        self.instructions.push(instruction);
    }

    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.encode_for(CodeMode::Isa48, ByteOrder::LittleEndian)
    }

    /// Serializes full-width instructions in the selected mode and container order.
    pub fn encode_for(&self, mode: CodeMode, order: ByteOrder) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        for instruction in &self.instructions {
            bytes.extend_from_slice(&match mode {
                CodeMode::Isa48 => instruction.encode_with_order(order)?,
                CodeMode::Visa => instruction.encode_visa_with_order(order)?,
            });
        }
        Ok(bytes)
    }
}
