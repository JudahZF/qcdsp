//! The stock `volume` block used as a framework for generated blocks.
//!
//! Generated blocks keep Volume's allocation, smoother, reset and destroy code.
//! They patch the allocation size, init call, process and parameter pointers, and the destroy
//! pointer when the block has an arena. An entry wrapper also installs the generated
//! fault-recovery callback. The bytes come from the user's own decrypted firmware and are pinned
//! by SHA-256 and by the exact words at every patch point, so a different build is refused, not
//! patched.

use qc_asm::{Condition, IndexRegister, Register, VisaInstruction as V, VisaProgram};

use qc_format::{
    blob::{ModuleBlob, Section},
    diff::DiffRecord,
};
use qc_link::{KernelForm, KernelMap};
use sha2::{Digest, Sha256};

use super::Generated;
use crate::{Error, Result};

/// SHA-256 of the decrypted `volume` ModuleBlob. The blob and its per-core records
/// are byte-equal in every build of `qc_deploy::KNOWN_BUILDS` (CorOS 4.1.0 and
/// 4.1.1). Only the hash is recorded here; the blob is read from the user's device data.
pub const VOLUME_SHA256: &str = "2895ac789b87f1ce46b1d5551acc7ba081372a58e66c1ea6f3abc96fdf06f5ae";

/// Stock code length in 16-bit parcels. Generated functions are appended after it.
pub const STOCK_PARCELS: u32 = 0x27a;
/// Bytes Volume allocates for its own state; generated fields follow it.
pub const STOCK_STATE_BYTES: u32 = 0x44;
/// The largest state the allocation immediate at parcel 0x15b holds.
pub const MAX_STATE_BYTES: u32 = 0x7fff;
/// Stock init function; the generated init tail-jumps here.
pub const STOCK_INIT: u32 = 0xe2;
/// Stock destroy (`ctx[8]`): frees the state with `free`. A generated destroy tail-jumps here.
pub const STOCK_DESTROY: u32 = 0x11;
/// Samples per channel per process call, and channel pointers in the descriptor.
pub const BLOCK_SAMPLES: u32 = 32;
pub const CHANNELS: u32 = 2;
/// Volume's report figures, which the generated block extends.
pub const STOCK_DM_HEAP: u32 = 208;
pub const STOCK_CPU: f64 = 0.72;

/// Core 1 kernel `heap_malloc(R4 = heap, R8 = bytes) -> R0` (k1 `0x16adfa`; 0 when the heap is
/// full) and `heap_free(R4 = heap, R8 = pointer)` (k1 `0x16ad7e`). Their core 2-4 addresses
/// come from the [`KernelMap`] given to [`VolumeTemplate::with_kernel`].
pub const HEAP_MALLOC: u32 = 0x16_adfa;
pub const HEAP_FREE: u32 = 0x16_ad7e;
/// Length of heap 0 in bytes (`0x257f8`; the core 2 kernel has the same heaps 0 and 1). A block
/// whose DM heap figure exceeds it can never fit.
pub const DM_HEAP_BYTES: u32 = 153_592;
/// Length of heap 2, the SD heap, in bytes (`0xfffef8`, base `0x80000000` on core 1 and
/// `0x81000000` on core 2).
pub const SD_HEAP_BYTES: u32 = 0xff_fef8;
/// A stock blob that calls both heap functions (digital_delay: `heap_malloc` at `0x38c4`,
/// `heap_free` at `0x39a2`), so its per-core records give their core 2-4 addresses
/// ([`KernelMap::add_stock_calls`]).
pub const KERNEL_CALL_SOURCE: &str = "digital_delay";

/// A patch point: parcel offset, expected stock parcels, and the relocation
/// record (kind, target offset) that must describe it.
struct PatchPoint {
    parcel: u32,
    stock: &'static [u16],
    relocation: Option<(u32, u32)>,
}

const ALLOC: PatchPoint = PatchPoint {
    parcel: 0x15b,
    stock: &[0x0f84, 0x0044],
    relocation: None,
};
const INIT_CALL: PatchPoint = PatchPoint {
    parcel: 0x16c,
    stock: &[0x1804, 0x00f8, 0x00e2],
    relocation: Some((11, 0xe2)),
};
const PROCESS_PTR: PatchPoint = PatchPoint {
    parcel: 0x179,
    stock: &[0x0f1c, 0x00f8, 0x00aa],
    relocation: Some((8, 0xaa)),
};
const SETPARAM_PTR: PatchPoint = PatchPoint {
    parcel: 0x17e,
    stock: &[0x0f1c, 0x00f8, 0x026d],
    relocation: Some((8, 0x26d)),
};
/// ctx[8] destroy: `i12 = 0xf80011`, stored at `0x177` after the `DM(I5, M6)` post-modify.
const DESTROY_PTR: PatchPoint = PatchPoint {
    parcel: 0x174,
    stock: &[0x0f1c, 0x00f8, 0x0011],
    relocation: Some((8, STOCK_DESTROY)),
};

/// Entry points of the generated functions, as parcel offsets in section 2.
#[derive(Debug, Clone, Copy)]
pub struct Entries {
    pub init: u32,
    pub process: u32,
    pub set_parameter: u32,
    pub recovery: u32,
    /// Frees the arena, then runs the stock destroy. Only blocks with an arena have one.
    pub destroy: Option<u32>,
}

pub struct VolumeTemplate {
    blob: ModuleBlob,
    diffs: [DiffRecord; 3],
    kernel: KernelMap,
}

impl VolumeTemplate {
    /// `core1` is the decrypted `volume` blob; `diffs` are Volume's records from
    /// `blobs/core{2,3,4}/diffs.blob`.
    pub fn new(core1: &[u8], diffs: [DiffRecord; 3]) -> Result<Self> {
        let hash: String = Sha256::digest(core1)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        if hash != VOLUME_SHA256 {
            return Err(Error::Template(format!(
                "volume blob sha256 {hash} is not the Volume blob of a supported CorOS build"
            )));
        }
        let blob = ModuleBlob::parse(core1)?;
        blob.validate()?;
        let template = Self {
            blob,
            diffs,
            kernel: KernelMap::default(),
        };
        template.check(&template.blob)?;
        for diff in &template.diffs {
            template.check(&diff.apply(&template.blob)?)?;
        }
        Ok(template)
    }

    fn check(&self, blob: &ModuleBlob) -> Result<()> {
        let code = blob
            .content(Section::Code)
            .ok_or_else(|| Error::Template("no code".into()))?;
        if code.len() != STOCK_PARCELS as usize * 2 {
            return Err(Error::Template("unexpected volume code length".into()));
        }
        let relocations = blob
            .relocations
            .iter()
            .find(|group| group.section == Section::Code)
            .ok_or_else(|| Error::Template("no code relocations".into()))?
            .code_relocations()?;
        for point in [ALLOC, INIT_CALL, PROCESS_PTR, SETPARAM_PTR, DESTROY_PTR] {
            let actual: Vec<u16> = (0..point.stock.len())
                .map(|index| parcel(code, point.parcel + index as u32))
                .collect();
            if actual != point.stock {
                return Err(Error::Template(format!(
                    "patch point {:#x} holds {actual:04x?}",
                    point.parcel
                )));
            }
            if let Some((kind, target)) = point.relocation
                && !relocations.iter().any(|r| {
                    r.offset == point.parcel
                        && r.kind == kind
                        && r.target_base == Section::Code.base()
                        && r.target_offset == target
                })
            {
                return Err(Error::Template(format!(
                    "relocation at {:#x} is not kind {kind} -> {target:#x}",
                    point.parcel
                )));
            }
        }
        Ok(())
    }

    /// Core 2-4 addresses of kernel functions, for blocks with an arena (they call
    /// [`HEAP_MALLOC`] and [`HEAP_FREE`]). Build it from the user's own data, for example with
    /// [`KernelMap::add_stock_calls`] on [`KERNEL_CALL_SOURCE`].
    pub fn with_kernel(mut self, kernel: KernelMap) -> Self {
        self.kernel = kernel;
        self
    }

    /// Core 1 blob plus core 2-4 diff records for a block whose generated functions are
    /// `generated` (little-endian parcels appended after the stock code).
    pub fn link(
        &self,
        name: &str,
        generated: &Generated,
        state_bytes: u32,
    ) -> Result<(ModuleBlob, [DiffRecord; 3])> {
        if state_bytes > MAX_STATE_BYTES {
            return Err(Error::Invalid(format!(
                "block state needs {state_bytes} bytes; the allocation immediate holds 0x7fff"
            )));
        }
        let patched = patch(&self.blob, &generated.parcels, generated, state_bytes)?;
        let mut diffs = Vec::with_capacity(3);
        for (core, diff) in self.diffs.iter().enumerate() {
            let code = self.kernel_code(generated, core)?;
            let core_n = patch(&diff.apply(&self.blob)?, &code, generated, state_bytes)?;
            let record = DiffRecord::between(name, &patched, &core_n)?;
            // Every per-core record must turn the new core 1 blob into the patched stock core
            // blob, with each kernel call at that core's address.
            let applied = record.apply(&patched)?;
            applied.validate()?;
            if applied != core_n {
                return Err(Error::Template(format!(
                    "the core {} record does not rebuild its blob",
                    core + 2
                )));
            }
            diffs.push(record);
        }
        Ok((patched, diffs.try_into().expect("three cores")))
    }

    /// The generated parcels with each kernel call at the address of core `core + 2`.
    fn kernel_code(&self, generated: &Generated, core: usize) -> Result<Vec<u16>> {
        let mut code = generated.parcels.clone();
        for call in &generated.kernel_refs {
            let address = self.kernel.get(call.core1_address).ok_or_else(|| {
                Error::Template(format!(
                    "kernel call {:#x} is not in the kernel map (VolumeTemplate::with_kernel)",
                    call.core1_address
                ))
            })?[core];
            let at = (call.parcel - STOCK_PARCELS) as usize;
            let current = (u32::from(code[at + 1]) << 16) | u32::from(code[at + 2]);
            if call.form != KernelForm::Call24
                || code[at] != 0x1804
                || current != call.core1_address
                || address > 0xff_ffff
            {
                return Err(Error::Template(format!(
                    "kernel call at parcel {:#x} is not CJUMP {:#x}",
                    call.parcel, call.core1_address
                )));
            }
            code[at + 1] = (address >> 16) as u16;
            code[at + 2] = address as u16;
        }
        Ok(code)
    }
}

fn parcel(code: &[u8], index: u32) -> u16 {
    let at = index as usize * 2;
    u16::from_le_bytes([code[at], code[at + 1]])
}

fn set_parcel(code: &mut [u8], index: u32, value: u16) {
    let at = index as usize * 2;
    code[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

/// Point a three-parcel absolute address field (`CALL addr24` or `ureg = imm32`)
/// and its relocation record at `target` (a parcel offset in section 2).
fn retarget(blob: &mut ModuleBlob, point: &PatchPoint, target: u32) -> Result<()> {
    let address = Section::Code.base() + target;
    let code = blob.content_mut(Section::Code).expect("checked");
    set_parcel(code, point.parcel + 1, (address >> 16) as u16);
    set_parcel(code, point.parcel + 2, address as u16);
    let group = blob
        .relocations
        .iter_mut()
        .find(|group| group.section == Section::Code)
        .expect("checked");
    let (kind, _) = point
        .relocation
        .expect("retargeted points have relocations");
    let record = group
        .words
        .chunks_exact_mut(4)
        .find(|record| record[0] == kind && record[1] == point.parcel)
        .expect("checked");
    record[3] = target;
    Ok(())
}

fn patch(
    stock: &ModuleBlob,
    code: &[u16],
    generated: &Generated,
    state_bytes: u32,
) -> Result<ModuleBlob> {
    let entries = generated.entries;
    let mut blob = stock.clone();
    let section = blob.content_mut(Section::Code).expect("checked");
    set_parcel(section, ALLOC.parcel + 1, state_bytes as u16);
    section.extend(code.iter().flat_map(|parcel| parcel.to_le_bytes()));
    // Install ctx[14] before tail-jumping to Volume's allocating entry.
    // Volume installs its other callbacks without changing this slot.
    let entry = section.len() as u32 / 2;
    let mut wrapper = VisaProgram::new();
    wrapper.push(V::Transfer {
        dst: IndexRegister::new(4)?.into(),
        src: Register::new(4)?.into(),
    });
    wrapper.push(V::LoadImmediate {
        dst: Register::new(0)?.into(),
        value: Section::Code.base() + entries.recovery,
    });
    wrapper.push(V::StoreDmOffset {
        src: Register::new(0)?.into(),
        index: IndexRegister::new(4)?,
        offset: 14,
    });
    let at = entry + wrapper.parcels()?.len() as u32;
    wrapper.push(V::JumpRelative {
        condition: Condition::Always,
        offset: 0x150 - at as i32,
        delayed: false,
    });
    section.extend(wrapper.parcels()?.into_iter().flat_map(u16::to_le_bytes));
    let length = section.len();
    blob.entry = Section::Code.base() + entry;
    // Stock records are sorted by offset: the generated routines, then the wrapper.
    let records = &mut blob
        .relocations
        .iter_mut()
        .find(|group| group.section == Section::Code)
        .expect("checked")
        .words;
    records.extend(generated.relocations.iter().flat_map(|r| r.words()));
    records.extend([8, entry + 2, Section::Code.base(), entries.recovery]);
    retarget(&mut blob, &INIT_CALL, entries.init)?;
    retarget(&mut blob, &PROCESS_PTR, entries.process)?;
    retarget(&mut blob, &SETPARAM_PTR, entries.set_parameter)?;
    if let Some(destroy) = entries.destroy {
        retarget(&mut blob, &DESTROY_PTR, destroy)?;
    }
    let range = blob
        .ranges
        .iter_mut()
        .find(|range| range.section == Section::Code)
        .expect("checked");
    *range = qc_format::blob::LinkRange::for_contents(Section::Code, length)?;
    blob.validate()?;
    Ok(blob)
}
