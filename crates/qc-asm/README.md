# qc-asm

A small, independently written SHARC+ **ISA48 and VISA** encoder. It builds DSP fragments and the restricted instruction sequences needed for leaf callbacks. It is not a general assembler, a callback ABI validator, or a device loader.

## Existing scalar API

```rust
use qc_asm::{ByteOrder, CodeMode, Instruction, Program, Register};

let input = Register::new(4)?;
let gain = Register::new(8)?;
let output = Register::new(0)?;
let multiply = Instruction::FloatMul { dst: output, lhs: input, rhs: gain };
let mut program = Program::new();
program.push(Instruction::LoadImmediate { dst: gain, value: 0.5_f32.to_bits() });
program.push(multiply);
let isa48 = program.encode()?; // Unchanged: section 3, little-endian 48-bit words.
let visa = program.encode_for(CodeMode::Visa, ByteOrder::LittleEndian)?; // Section 2.
let one_visa_instruction = multiply.encode_visa()?; // [u8; 6], section-2 order.
# Ok::<(), qc_asm::Error>(())
```

`Register` accepts indices 0–15. Rn and Fn are views of the same physical PEx register. `Instruction::word()` returns numeric bits 47:0. `Instruction::encode()` and `Program::encode()` retain their ISA48 behavior. `Program::instructions()` retains the typed instruction list. Both modes are now supported by `Program::encode_for()`.

Scalar operations:

- `FloatAdd`, `FloatSub`, `FloatMul`, `FloatMin`, `FloatMax`: `{ dst, lhs, rhs }`.
- `FloatNeg`, `FloatAbs`, `FloatPass`: `{ dst, src }`. PASS is an ALU computation and affects arithmetic status and precision.
- `LoadImmediate`: `{ dst, value: u32 }`. This writes a raw bit pattern; use `f32::to_bits()` for floats. The low eight bits of a 40-bit destination are cleared by hardware.
- `Move`: `{ dst, src }`. A Type 5a **register transfer**, not ALU PASS.
- `LoadDm`: `{ dst, address: u32 }`; `StoreDm`: `{ src, address: u32 }`. Direct-address Type 14a DM accesses, long-word override clear.

Every scalar `Instruction` uses its full **48-bit ISA/VISA form** in VISA mode. No compute compression or extra escape parcel is emitted. `encode_visa_with_order()` selects the byte order within each parcel. `encode_with_order()` remains the whole-word ISA48 serializer.

## Leaf-callback VISA API

```rust
use qc_asm::{
    Condition, IndexRegister, Instruction, ModifyRegister, Register,
    VisaInstruction, VisaProgram,
};

let r2 = Register::new(2)?;
let r8 = Register::new(8)?;
let i4 = IndexRegister::new(4)?;
let i5 = IndexRegister::new(5)?;
let i6 = IndexRegister::new(6)?;
let i7 = IndexRegister::new(7)?;
let mut code = VisaProgram::new();
code.push(VisaInstruction::ModifyIndex { index: i7, offset: -10 });
code.push(VisaInstruction::StoreDmOffset { src: r8.into(), index: i6, offset: -3 });
code.push(VisaInstruction::Transfer { dst: i4.into(), src: r8.into() });
code.push(VisaInstruction::IntegerPass { dst: r2, src: r8 });
code.jump(Condition::Eq, "done"); // No delay slots; label is local to this stream.
code.label("loop");
code.push(VisaInstruction::LoadDmPostModify {
    dst: r8.into(), index: i4, modifier: ModifyRegister::new(6)?,
});
code.push(Instruction::FloatNeg { dst: r8, src: r8 }); // From<Instruction>.
code.push(VisaInstruction::StoreDmOffset {
    src: r8.into(), index: i5, offset: 0x1fff, // Full-width Type 15a; no scratch I register.
});
code.push(VisaInstruction::IntegerSubOne { dst: r2, src: r2 });
code.jump(Condition::Ne, "loop");
code.label("done");
code.push(VisaInstruction::LoadDmOffset { dst: r8.into(), index: i6, offset: -3 });
code.push(VisaInstruction::LeafReturn);
let parcels = code.parcels()?; // Vec<u16>, in execution order.
let section_two_bytes = code.encode()?; // Vec<u8>, little-endian per parcel.
# Ok::<(), Box<dyn std::error::Error>>(())
```

This example illustrates encoding, **not a complete or valid callback contract**: the caller must set up buffers, initialize the counter, preserve the required registers, allocate frame slots, and handle processor hazards.

Public register types are `Register`, `IndexRegister` (I0–I15), `ModifyRegister` (M0–M15), and `UniversalRegister`. Converting any of the first three with `.into()` produces a universal register. `UniversalRegister::new(code)` accepts the physical R/I/M/L/B/S classes (`0x00..0x5f`); it does not expose system-register codes. I registers have universal codes `0x10..0x1f`, M registers `0x20..0x2f`. `Transfer`, `LoadImmediate` and `ModifyIndex` take DAG2 registers (I8–I15, M8–M15) as well. **The leaf-callback DM forms (Types 3b, 3c, 15a, 15b) are restricted to DAG1: I0–I7 and M0–M7.** The frame-kernel forms below select DM or PM from the DAG. Invalid indices, modifiers, labels, or branch offsets return `VisaError`.

`VisaInstruction` forms and their output widths:

| Form | Fields | Encoding / width |
|---|---|---|
| `Scalar` (or pass `Instruction` directly to `push`) | existing scalar instruction | Full ISA/VISA form, 48 bits / 3 parcels |
| `Transfer` | `dst, src: UniversalRegister` | Type 5b, 32 bits / 2 parcels; transfer, **not swap** |
| `LoadImmediate` | `dst: UniversalRegister, value: u32` | Type 17a, 48 bits / 3 parcels |
| `ModifyIndex` | `index: IndexRegister, offset: i32` | Type 19a, 48 bits / 3 parcels; `index=MODIFY(index,offset)(NW)` |
| `LoadDmPostModify`, `StoreDmPostModify` | `dst` / `src`, `index`, `modifier` | Type 3c, 16 bits / 1 parcel; **R0–R15 only**, index updated |
| `LoadDmPreModify`, `StoreDmPreModify` | `dst` / `src: UniversalRegister`, `index`, `modifier` | Type 3b, 32 bits / 2 parcels; `DM(modifier,index)`, index **not updated** |
| `LoadDmOffset`, `StoreDmOffset` | `dst` / `src: UniversalRegister`, `index`, `offset: i32` | Type 15b, 32 bits for −64..63; Type 15a, 48 bits otherwise; index **not updated** |
| `IntegerSubOne`, `IntegerPass` | `dst, src: Register` | Type 2a, 48 bits / 3 parcels |
| `JumpRelative` | `condition, offset: i32, delayed: bool` | Type 8a, 48 bits / 3 parcels |
| `LeafReturn` | none | Fixed four instructions, 2 + 2 + 1 + 1 parcels; 12 bytes |
| `Compute` | `Compute` | Type 2a, 48 bits; any single-function or multifunction compute |
| `DualMove` | `compute: Option<Compute>, dm, pm: DataAccess` | Type 1a, 48 bits; **both** accesses always happen, post-modify, R0–R15; `dm` I0–I7/M0–M7, `pm` I8–I15/M8–M15 |
| `MemoryMove` | `compute: Option<Compute>, access: MemoryAccess` | Type 3a, 48 bits; one access, post- or pre-modify, universal register; I0–I7 → DM (G=0), I8–I15 → PM (G=1) |
| `StoreImmediate` | `index, modifier, value: u32` | Type 16a, 48 bits; `DM(Ia,Mb)=imm32` (I0–I7) or `PM(Ic,Md)=imm32` (I8–I15), post-modify |
| `BitSetMode1`, `BitClearMode1` | `mask: u32` | Type 18a, 48 bits; `BIT SET/CLR MODE1 mask`; MODE1 only |
| `Nop` / `WideNop` | none | Type 21c, 16 bits / Type 21a, 48 bits (all zero) |

The short forms above were checked directly against ADI diagrams and stock/oracle disassembly. There is **no general VISA compression algorithm**, and no claim that arbitrary ISA words can be truncated into VISA. Non-data-register post-modify Type 3b attempts decoded with the wrong update mode in the oracle. They are not emitted or claimed verified. `UnsupportedPostModifyRegister` rejects those requests; the required PEx data-register post-modify operations use the verified Type 3c form.

**Type 3a in VISA is valid; the earlier failure was the oracle.** A VISA 32-bit form marks itself in the bits where the 48-bit form keeps `compute[22:16]`: Type 1b and 3b put `0111111`/`01111xx` there (PRM figures `.14.07`, `.14.15`), which no valid compute field produces (single-function CU `11` does not exist; multifunction sets bit 22). A no-compute Type 3a has zeros there and is 48-bit. The stock blobs hold 7759 no-compute Type 3a words, for example `537e01000000` (`R2=DM(I1,M5)`) inside the stock SIMD loop at core-1 0x5ba. selache's width table is empirical: it reads any Type 3a whose first parcel is `0x5980..0x59ff` (I4 with M6/M7/M14/M15) as 32-bit, and it reads every Type 16a except `0x9fc0` as 32-bit. The old attempt used `R2=DM(I4,M6)` (`59be01000000`), which selache splits into `59be0100` + a misaligned word. The stock disassembly shows the same bug at core-1 0x581: `59be0613 | 0f48ad02ffff | ff78018dd2e5` is really `F15=F4*F8, R12=DM(I4,M6)`; `DM(-0x88,I6)=R2`; a 32-bit Type 2b, and the stream realigns at 0x589. selache also drops G for Type 3a PM (it assembles and prints `PM(I12,M13)` as `DM(I4,M5)`; stock uses G=1, `597f00800000`) and prints a spurious `MRF+` for the float multiplier/ALU multifunction.

### Labels, offsets, status, and return assumptions

- `VisaProgram::label(name)` binds the current parcel position. `jump(condition, name)` emits a **non-delayed** Type 8a branch. Forward and backward labels are supported. Duplicate or undefined labels fail during `parcels()` / `encode()`.
- Branch displacement is **target parcel address minus the branch's first parcel address**, not the next instruction address. It is signed 24-bit (−`0x800000`..`0x7fffff`). Local PC-relative branches do not depend on where the stream is appended. No alignment padding is added.
- `Condition` supports `Eq`, `Ne`, `Sv`, `Sz`, `NotSv`, `Always`. A direct `JumpRelative` can use `delayed: true`, but it does not insert or validate its two delay-slot instructions. Label jumps are deliberately non-delayed.
- `IntegerSubOne` uses ALU opcode **`0x2a`**, `Rn = Rx - 1`, **not subtract-with-borrow**. AZ is set if the fixed-point output is zero; otherwise cleared. Saturation and overflow behavior depend on MODE1.
- `IntegerPass` uses ALU opcode **`0x21`**. It sets AZ if the fixed-point output is all zero, otherwise clears AZ. The floating extension bits are cleared. This supports `r2=pass r8; if eq ...`. `Move` / `Transfer` do not provide this flag update.
- The C runtime supplies M5=0, M6=1, M7=−1 and M14=0 for the observed calling convention. The encoder does not initialize or verify them.
- `LeafReturn` and public `LEAF_RETURN_PARCELS` emit exactly `4dfe 0e3f; 083f 343f; 1901; 0001`: `I12=DM(M7,I6); JUMP(M14,I12)(DB); RFRAME; NOP`. This assumes the stock frame and return-address layout. RFRAME and NOP are the two delayed-branch instructions. No generic RTS or automatic prologue/epilogue is implemented.

## Frame-kernel API

```rust
use qc_asm::{
    AluOp, Compute, DataAccess, Direction, FloatMulAlu, IndexRegister, LoopCount, LoopMode,
    MODE1_PEYEN, ModifyRegister, Register, VisaInstruction, VisaProgram,
};

let f = Register::new;
// F8=F0*F4, F12=F8+F12: inputs come from fixed register groups (see below).
let mac = Compute::float_mul_alu(FloatMulAlu {
    product: f(8)?, mul_x: f(0)?, mul_y: f(4)?,
    op: AluOp::Add, alu_dst: f(12)?, alu_x: f(8)?, alu_y: f(12)?,
})?;
let load = |register, index, modifier| -> Result<DataAccess, qc_asm::Error> {
    Ok(DataAccess {
        direction: Direction::Load,
        register: f(register)?,
        index: IndexRegister::new(index)?,
        modifier: ModifyRegister::new(modifier)?,
    })
};
let mut code = VisaProgram::new();
code.push(VisaInstruction::BitSetMode1 { mask: MODE1_PEYEN });
code.push(VisaInstruction::Nop); // PEYEN takes effect one cycle later.
code.loop_begin(LoopCount::Immediate(64), LoopMode::F1Active);
code.push(VisaInstruction::DualMove {
    compute: Some(mac),
    dm: load(0, 1, 6)?, // R0=DM(I1,M6)
    pm: load(4, 8, 8)?, // R4=PM(I8,M8)
});
code.loop_end(); // The DualMove is the last (and only) body instruction.
code.push(VisaInstruction::BitClearMode1 { mask: MODE1_PEYEN });
code.push(VisaInstruction::Nop);
let parcels = code.parcels()?;
# assert_eq!(parcels[4..10], [0x0c00, 0x4080, 0x0003, 0x2380, 0x0258, 0x8c00]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

`Compute` is a validated 23-bit compute field. `Compute::single` takes any single-function compute (`Instruction::{FloatAdd, FloatSub, FloatMul, FloatMin, FloatMax, FloatNeg, FloatAbs, FloatPass, IntegerSub}`, `VisaInstruction::{IntegerSubOne, IntegerPass, IntegerCompareUnsigned}`); other instructions return `NotACompute`. `Compute::float_mul_alu` builds the multifunction `Fm = Fxm * Fym, Fa = Fxa ± Fya` (opcodes `011000` add, `011001` sub). `DualMove`, `MemoryMove` and `Compute` take it; all three are 48-bit.

`VisaProgram::loop_begin(count, mode)` emits a Type 12a DO; `loop_end()` closes the innermost open loop, and the instruction pushed last is the loop end. `LoopCount::Immediate(u16)` emits `LCNTR = data16`; `LoopCount::Register(ureg)` emits `LCNTR = ureg` for counts above 65535.

### Rules for code generators

- **reladdr** is the first parcel of the last body instruction minus the first parcel of the DO instruction. Across 5284 stock loops whose body tail is all 48-bit, this convention lands on an instruction start; the alternative (the last parcel of the end instruction) fits 107. Example: stock core-1 0x5b7 `0c001080000f` (`LCNTR=0x10, DO (PC,0xf) (F)`) ends at the 48-bit instruction at 0x5c6.
- **Loop tails.** `parcels()` returns `LoopTailNotFullWidth` if any of the last 11 body instructions (all of them for a shorter body) is not 48-bit (PRM `.06.062`). 48-bit forms: `Scalar`, `LoadImmediate` (Type 17a: bit 39 = 0 keeps it apart from the 32-bit 17b), `ModifyIndex`, `Load/StoreDmOffset` outside −64..63, `IntegerSubOne`, `IntegerPass`, `IntegerCompareUnsigned`, `PushImmediate`, `Compute`, `DualMove`, `MemoryMove`, `StoreImmediate`, `BitSetMode1`, `BitClearMode1`, `WideNop`, and a nested DO. Not 48-bit: `Transfer`, `Load/StoreDmPostModify`, `Load/StoreDmPreModify`, small offsets, `Nop`. ADI-built stock code satisfies only a 10-instruction tail in about 1100 loops; this crate enforces the PRM's 11.
- **No change of flow in a loop body.** Label jumps, `JumpRelative`, `CJump`, `LeafReturn` (`BranchInLoop`) and labels (`LabelInLoop`) are rejected inside a body. F1-active mode is therefore always legal (PRM `.06.048`). A nested DO is allowed anywhere in the body: the stock kernel at 0x19ea nests an F1 `LCNTR=R15` loop as the first instruction of an F1 loop. Nested loops may not end on the same instruction (`SharedLoopEnd`, PRM `.06.065`). `Immediate(0)` is rejected; a register count must be nonzero.
- **Multifunction register groups** (PRM `.21.16`, `.20.11`–`.20.14`): `mul_x` F0–F3, `mul_y` F4–F7, `alu_x` F8–F11, `alu_y` F12–F15; `product` and `alu_dst` are free. Parallel destinations must differ (PRM `.05.48`): the two compute results, and every load in the same `DualMove` / `MemoryMove`; a store may read a register the compute writes. The float product reaches the ALU two cycles later (PRM `.05.46`), so alternate product registers, as the stock kernel does (`F8=F0*F4, F12=F8+F12` / `F9=F0*F5, F13=F9+F13`).
- **DAGs.** `DualMove`: `dm` uses I0–I7/M0–M7 and `pm` uses I8–I15/M8–M15, both post-modify, data registers only. `MemoryMove` and `StoreImmediate`: the index selects DM (I0–I7, G=0) or PM (I8–I15, G=1); the modifier must be in the same DAG (`MixedDag`). `MemoryMove` post-modify accesses and all stores need a ureg outside that DAG (PRM UREGXDAG1/UREGXDAG2, `.14.14`); pre-modify loads may target any ureg.
- **SIMD.** Enter with `BitSetMode1 { mask: MODE1_PEYEN }; Nop` and leave with `BitClearMode1 { mask: MODE1_PEYEN }; Nop` (one-cycle effect latency, PRM `.05.87` and `.04.11`). Add `MODE1_BDCST1` / `MODE1_BDCST9` to broadcast loads through I1 / I9. Stock code does **not** save MODE1: it runs 4538 `BIT SET MODE1 0x200000` and 4228 clears, with no MODE1 save in the 60 instructions before them, because the C runtime is SISD at call boundaries. selache names ureg `0x70` "mode1", but the PRM register table gives `0x70` = USTAT1 and `0x72` = MODE1; stock `9d08387d` (`DM(-3,I6)=ureg 0x70`) is a USTAT1 spill and `713eb83f` / `773e08800000` stash a pointer in USTAT1. In SIMD, PEy uses `Ia + 1` for normal-word accesses (not for broadcast loads) and the complementary S registers.
- **Weight init.** `StoreImmediate` writes one 32-bit word per instruction. Stock uses Type 16a only as `DM(I7,M7)=imm` (`9fc0…`, 76117 words); other I/M pairs rest on the PRM figure and the selas assembler. The PRM marks Type 16b with bit 34 = 1 and Type 15b with bit 35 = 1; Type 16a keeps 36:32 = 0, so its first parcel is unambiguous. If hardware disagrees, `LoadImmediate` (stock-proven for any ureg) plus a store is the fallback.

## Section layout and byte order

**ModuleBlob section 2 is VISA: 16-bit parcels, each stored little-endian.** For a full 48-bit word `w`, instruction-order parcels are `w >> 32`, `w >> 16`, `w & 0xffff` (each truncated to `u16`). The order of parcels is high-to-low; only the bytes *inside each parcel* are little-endian.

Example: word `0x0f013d4ccccd` (`R1=0x3d4ccccd`) becomes section-2 bytes **`01 0f 4c 3d cd cc`**. A section-2 code address advances by one per **parcel**, not one per typed instruction. For a complete stream, parcel count is `bytes.len() / 2` or `parcels.len()`. Host byte offsets must not be used as code addresses.

**Section 3 remains ISA48:** each 48-bit word is little-endian as a whole, and a code address advances by **one instruction word**. The same immediate word becomes **`cd cc 4c 3d 01 0f`**. Section 3's word count is `program.instructions().len()`.

The existing section-3 evidence is the stock six bytes `40 00 00 00 21 0f`, word `0x0f2100000040` (`M1=0x40`). The section-2 order was separately checked against the decrypted stock volume section, including `R1=0x3d4ccccd`, and the parcel conversion used by `blobdis`.

The external ELF oracle consumes **big-endian instruction parcels**, unlike QC section 2. `ByteOrder::BigEndian` is available for explicit external consumers. No API switches the processor's execution mode or establishes a valid entry point. Use the correct section and caller ABI; selecting a section alone does not establish either.

DM addresses and offsets are processor address units, not host byte offsets or relocated pointers. `ModifyIndex` explicitly selects `(NW)`, while DM transfers leave the long-word override clear. MODE1, memory mappings, DAG circular-buffer settings, and hazards remain the caller's responsibility.

## Encoding sources and verification

Implementation source: **Analog Devices, SHARC+ Core Programming Reference, Revision 0.1, June 2015**, installed with CCES 2.11.1:

```text
Eclipse/plugins/com.analog.crosscore.help.sharcplus.core.prm_0.0.1.201507101236.jar
```

The official XHTML tables and diagrams were inspected directly. Relevant pages under `html/`:

| Encoding / semantics | Official reference page |
|---|---|
| Type 2a compute shell | `includes_r_isr_sharc_Group_I_ConditionalComputeAndMoveOrModify_dita.14.10.html` |
| Compute field / SINGLEFN / ALU / multiplier | `includes_r_isr_sharc_ComputationReference_dita.21.02.html`, `.21.06.html`, `.21.07.html`, `.21.08.html` |
| Type 1a layout / DM and PM access | same group `.14.03.html` (figure), `.14.04.html`, `.14.05.html`; Type 1b marker `.14.07.html` |
| Type 3a full layout / access semantics | `includes_r_isr_sharc_Group_I_ConditionalComputeAndMoveOrModify_dita.14.13.html`, UREGXDAG table `.14.14.html` |
| Multiplier and ALU multifunction | `includes_r_isr_sharc_ComputationReference_dita.21.16.html`; groups `includes_r_isr_sharc_RegisterTypes_dita.20.11.html`–`.20.14.html`; destinations `c_comp_ProcessingElements_dita.05.48.html` |
| Type 12a imm / ureg counted loops | `includes_r_isr_sharc_Group_II_ConditionalProgramFlowControl_dita.15.20.html`, `.15.21.html` |
| Loop rules (VISA tail, F1/E2, ending, nesting) | `c_sequen_ProgramSequencer_dita.06.062.html`, `.06.048.html`, `.06.060.html`, `.06.065.html` |
| Type 16a / 16b marker | `includes_r_isr_sharc_Group_III_ImmediateDataMoveInstructions_dita.16.12.html`, `.16.13.html` |
| Type 18a / BOP / SREG codes | `includes_r_isr_sharc_Group_IV_Miscellaneous_dita.17.02.html`, `.17.03.html`; `includes_r_isr_sharc_RegisterTypes_dita.20.27.html` |
| MODE1 bits / SIMD latency | `SHARC-PLUS_regf_mode1.html`; `c_comp_ProcessingElements_dita.05.87.html` |
| Type 21a / 21c NOP | `includes_r_isr_sharc_Group_IV_Miscellaneous_dita.17.15.html`, `.17.16.html` |
| Type 3b layout / normal-width options | same group `.14.15.html`, `.14.16.html`, `.14.17.html`, `.14.18.html` |
| Type 3c post-modify DM | same group `.14.19.html` |
| Type 5a / 5b transfers | same group `.14.37.html`, `.14.39.html` |
| Type 14a direct DM | `includes_r_isr_sharc_Group_III_ImmediateDataMoveInstructions_dita.16.02.html` |
| Type 15a / 15b immediate-offset DM | same group `.16.10.html`, `.16.11.html` |
| Type 17a immediate load | same group `.16.14.html`, `.16.15.html` |
| Type 19a modify / `(NW)` | `includes_r_isr_sharc_Group_IV_Miscellaneous_dita.17.04.html`, `.17.05.html` |
| Type 8a PC-relative jump | `includes_r_isr_sharc_Group_II_ConditionalProgramFlowControl_dita.15.02.html`, `.15.03.html`, `.15.04.html` |
| Branch conditions | `r_isr_sharc_inst_Cond_dita.html` |
| Fixed decrement / PASS status semantics | `includes_r_isr_sharc_ALU_Fixed-Point_Computations_dita.22.12.html`, `.22.15.html` |
| Universal register codes | `includes_r_isr_sharc_RegisterTypes_dita.20.28.html` |
| Return components (semantic references) | Type 3b above; Type 9a `.15.05.html`; RFRAME `.17.21.html`; NOP `.17.16.html` |

Compute encoding: `mf=0`; `cu=21:20`; opcode `19:12`; destination `11:8`; left source `7:4`; right source `3:0`. ALU opcodes: float add `0x81`, sub `0x82`, neg `0xa2`, abs `0xb0`, pass `0xa1`, min `0xe1`, max `0xe2` (`cu=0`); float multiply `0x30` (`cu=1`). Unary unused right-source fields are zero. The full Type 2a shell is bit 40 plus unconditional condition `11111` in 37:33.

The Type 3a layout was rechecked: **U=44, I=43:41, M=40:38, COND=37:33, G=32, D=31, L=30, UREG=29:23**. In Type 3b these fields are one parcel lower. This crate's pre-modify Type 3b fixes U=0, G=L=0, X=W=1, and the bits 5:2 marker to `1111`.

Stock-identical numeric checks include:

- `707e0a3f` (`I4=R4`), `717e813f` (`R2=I5`), `713f8a3f` (`I4=I3`).
- `1587fffffff6` (`I7=MODIFY(I7,-10)(NW)`).
- Stack stores `9d0807fd`, `9d08017e`, `9d0806fb`, `9d080176`; loads `9c0807f5`, `9c080876`.
- `4b7e09bf` (`I3=DM(M5,I5)`), `9951` (`R1=DM(I4,M5)`), `997c` (`DM(I4,M5)=R12`).
- `0f013d4ccccd`, the stock Type 8a EQ/SV/SZ/NOT SV/always branch forms, and the complete return sequence.
- The compute field `00130118` in stock `687e00130118` agrees with the emitted standalone `013e00130118` multiply. The parallel memory transfer is not emitted by the scalar API.
- Type 1a: `29016258c810`, `39076059ae48`, `38ef2158df0d`, `279933930826`, `21a421000000`. Type 2a multifunction: `013e00598c70`, `013e00581280`. Type 3a: `537e01000000`, `5bbe81000000`, `597f00800000` and `5f3f86000000` (PM, G=1), `4dfe0e000000` (pre-modify), `51be02588c40`, `57be0002a220`, `553e86002ccc`.
- Type 12a: `0c001080000f`, `0c0002000012`, `0d0f00800006`. The whole stock nested MAC kernel at core-1 0x19de..0x1a0d (16 instructions, two loops) is rebuilt word for word from the typed API in `tests/kernel.rs`.
- Type 16a `9fc000f838ca`; Type 18a `140200200000`, `142200200000`, `140200600000`, `142200e00000`; Type 21a `000000000000`; DAG2 forms `0f1c3f800000` (`I12=0x3F800000`), `15c100000018` (`I9=MODIFY(I9,0x18)(NW)`), `707e0e3f`, `71ff8a3f`, `703f163f`.
- Not in stock: Type 16a other than I7/M7. `980000004b68` and `9ce000001234` are the PRM's own examples; selas assembles the same words.

Wide offset, non-stock compute variants, integer decrement/PASS, and all listed leaf-callback forms were also confirmed by **standalone selache decoder subprocesses** targeting `ADSP-SC589`. For the frame-kernel forms, the opt-in test covers only words where selache is right (see the Type 3a note above): Type 1a without multifunction, DM Type 3a outside `0x5980..0x59ff`, Type 2a single compute, Type 16a I7/M7, Type 18a, both NOPs, DAG2 transfers/immediates/modify, and an E2-active immediate loop. The opt-in tests replace assembler NOPs with **our emitted bytes** in a synthetic ELF shell, select VISA metadata where appropriate, and compare exact raw words/parcels, decoded mnemonics, instruction widths, and PC addresses. This is not an assembler round trip and not a local decoder written to agree with our encoder. No GPL implementation was copied or linked. The short RFRAME width in the fixed helper is confirmed by stock bytes and oracle decoding; the installed ADI RFRAME diagram describes the full-width semantic form, not that short representation.

```sh
CARGO_TARGET_DIR=/tmp/qc-asm-visa-target cargo test -p qc-asm --locked --offline
SELACHE=/tmp/qc-asm-selache-target/debug CARGO_TARGET_DIR=/tmp/qc-asm-visa-target \
  cargo test -p qc-asm --locked --offline -- --ignored
cargo fmt -p qc-asm --check
CARGO_TARGET_DIR=/tmp/qc-asm-visa-target cargo clippy -p qc-asm --locked --offline --all-targets -- -D warnings
```

`SELACHE` points to standalone `selas` and `seldump`; it is optional and not a runtime dependency. Ordinary tests use independent numeric vectors, field boundaries, mixed-width forward/backward labels, and explicit errors. No vendor blobs or keys are distributed by this crate.

## Limits

No text assembly parser, symbolic external relocation, calls, automatic register allocation, hazard scheduling, or arbitrary raw-parcel insertion is provided. Only local Type 8a label jumps and Type 12a counted loops are resolved. Loops are counter-based only: no arithmetic or FOREVER loops (Type 13a), no loop abort, and the nesting depth (loop stack size) is not checked. Multifunction computes are limited to the float multiplier with ALU add or subtract: no fixed-point, MRF, dual add/subtract, or DPFLP forms. Type 1a, 3a and 16a are unconditional normal-word forms: no conditions, `(LW)`, byte or short-word options, and no Type 4a immediate-modifier accesses. Only MODE1 is reachable as a system register, through Type 18a SET/CLR; nothing saves or restores it. Cross-index MODIFY is not exposed; `ModifyIndex` changes its own index. The leaf-callback post-modify DM forms are limited to R0–R15; pre-modify, immediate-offset and Type 3a forms support universal registers.

Arithmetic precision, rounding, exceptions, denormals, NaNs, saturation, status, and implicit PEy operations depend on processor configuration. Hardware `MIN`/`MAX` need not share the exceptional-value behavior of host `f32::min`/`max`. Conditional branches in SIMD mode depend on the combined PE conditions. This is static encoding and external decode verification, **not hardware execution testing, timing proof, callback ABI validation, or a claim that arbitrary fragments are safe to deploy**.

The parameter guard also exposes Type 2a `COMPU` and the Type 8a GT condition
(code 18, after COMPU). The numeric vectors and external decoder check both forms.
