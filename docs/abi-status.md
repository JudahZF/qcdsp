# QC block ABI used by generated blocks

This is a summary of the block ABI recovered during Tetrad research. The full disassembly notes are private research and are not part of this repository.

## Generated block design

Generated blocks reuse the stock **Volume** framework from the user's own CorOS 4.1.0 or 4.1.1 firmware (the blob is byte-equal in both builds; pinned by plaintext SHA-256 and by the exact words at every patch point). The entry, allocation, smoother, reset and destroy code stay stock. `qcdsp::device` changes four things and appends four routines and an entry wrapper to section 2:

| Patch | Parcel | Change |
| --- | --- | --- |
| state allocation | 0x15b `r4=imm16` | bytes for Volume's 0x44 + generated state (≤ 0x7fff) |
| init call | 0x16c `CALL` + kind-11 reloc | generated init, which tail-jumps to stock init 0xe2 |
| ctx[2] process | 0x179 `i12=imm32` + kind-8 reloc | generated process |
| ctx[3] set-parameter | 0x17e `i12=imm32` + kind-8 reloc | generated set-parameter |

Per-core diff records are rebuilt by applying Volume's records to the stock blob, applying the same patches, and diffing. The entry wrapper stores the generated recovery pointer in ctx[14], then tail-jumps to the stock entry. It adds one kind-8 section-2 relocation for that pointer. Generated callbacks have no new kernel calls.

Generated process first runs the control steps once (parameter-only expressions, written to control slots after the parameter values), then for each channel a 32-sample loop of the sample steps. The sample steps load values at first use, write states as soon as their old value is dead, and spill to per-channel slots when registers run out. Layout: `crates/qcdsp/src/device/layout.rs`.

## Callback contract (confirmed from stock code)

- init(R4 = state): state is byte-addressed (stock uses `(bw)` byte accesses on it, which are illegal in normal-word space). Stock init only writes the first 0x44 bytes.
- process(R4 = state, R8 = io): `io[0]` and `io[1]` each address 32 samples, processed in place. The pointers can use different address aliases and need not name adjacent buffers. The descriptor is saved above I7 before sample code can reuse its argument register. 32 samples is also the smoother's block size.
- set-parameter(R4 = state, R8 = index, F12 = value). Volume's LEVEL is displayed 0–100% but used as a 0..1 gain, so ZenUI delivers normalized values. Generated parameters are declared 0..1 in `ModelRepo.xml`, so raw and normalized delivery agree; the callback uses integer COMPU and GT to reject bit patterns above +1.0 (including negative zero, negatives, infinities and NaNs) before arithmetic. Accepted values map to the parameter's DSP range.
- fault recovery (ctx[14], R4 = state): zero both channel banks, restore declared state initials and delay cursors, keep parameter values, and return through the kernel C frame. Live reset ctx[16] retains generated history and only resets the unused stock smoother.
- C run-time model: R4/R8/R12 arguments, I6 frame, I7 stack, M5 = M13 = 0, M6 = M14 = 1, M7 = M15 = -1. L registers are zero except the kernel's circular stack L6/L7; generated code leaves lengths and bases unchanged. MODE1 has RND32, CBUFEN, IRPTEN and NESTM set, SIMD and broadcast clear; generated callbacks do not write it. Generated process saves every callee-preserved register it touches; init and set-parameter use only scratch registers.

A mono block (IR 2) has one channel bank. After the sample loop of channel A, process reloads `io[0]` and `io[1]` and copies the 32 results to channel B with offset loads and stores. The reciprocal seed of IR 2 uses the fixed-point ALU subtract `Rn = Rx - Ry` (compute opcode 0x02); Tetrad's `tetrad-sharc` machine executes it in the generated-block tests.

All generated memory accesses are unqualified normal-word accesses with word offsets or M modifiers, which the DAG scales for byte-addressed pointers, so the code does not depend on the numeric address space of `io[0]`.

## Verification without a device

- Tetrad's `tetrad-sharc` tests (`tests/qcdsp_blocks.rs`) execute emitted parcels (`device::generate`) with its raw Machine (PRM arithmetic), independent of qc-asm and the text decoder: all eight examples, two state address aliases, separate mixed-alias channel buffers, 40 stereo frames, parameter delivery, and fault recovery at frame 20 after poisoning all history and cursor words. Parameter endpoints and modeled process instruction bounds are also checked. Every instruction is followed by interrupt-stack canary writes; CCES preserved words, MODE1, stacks, AIS/MIS, and write bounds are checked. No firmware is required. These checks start preserved register extensions at zero, as in Tetrad's owned blocks (V2); arbitrary incoming 40-bit extensions and circular stack wrap are not covered.
- The firmware corpus check (also in `tests/qcdsp_blocks.rs` of Tetrad) executes each core's entry wrapper at three VISA origins, verifies its kind-8 relocation and ctx[14] store, and checks that R4 and the other ctx words remain unchanged before the stock entry.
- Encodings: each form is checked against ADI's SHARC+ PRM diagrams and stock-identical words; `qc-asm` has numeric vectors and an opt-in decode by the independent selache decoder. That decoder misreads the Type 3a M field (ADI: bits 40:38); the emulator corrects for it.
- `tests/emulate.rs` executes the **independent decoder's disassembly** of each generated blob, calling init, set-parameter and process as the kernel does. All examples match the host reference on both channels over 40 blocks, preserve callee-saved registers and the stack, and write only their own state, the audio buffer and their frame. A mutation (sharing state between channels) makes it fail.
- Host f32 arithmetic is not a model of SHARC rounding modes; outputs are compared with a tolerance.

## Still open

- Hardware execution: Gain (model id 16901) previously installed, passed ZenUI's integrity check and loaded in a preset (2026-10-06); it was uninstalled on 2026-10-16 to restore stock Cortex Control compatibility. Its audio output and the stateful examples (biquad, delay, tremolo, envelope filter, Micro Synth) are not yet tested on the device.
- CPU figures are estimates (parcel count + DAG stalls, ×2 safety), not measurements. Heap is exact.
- Live reset (ctx[16]) preserves generated filter/delay history; fault recovery (ctx[14]) clears it. These are separate operations, as in Tetrad's owned-block ABI.
- Numerical policy: the `tetrad-sharc` PRM execution models invalid operations and the AIS/MIS stickies tested by the kernel. The older decoder-text emulator uses host f32, including Rust MIN/MAX NaN rules; it is only an encoding and mathematical cross-check. A process fault now has a recovery callback to clear generated state. The envelope-driven examples clamp their input to ±16, which handles infinities.
- Firmware updates: the Volume template and kernel layout are pinned to the builds in `qc_deploy::KNOWN_BUILDS`, CorOS 4.1.0 and 4.1.1; other builds are refused. From 4.1.0 to 4.1.1 only ZenUI and the SOC2 image `ZPU_SC589_SHARC_Core0.blwldr` changed. The SOC1 kernels (as installed, after the per-unit translation), the Volume blob and its per-core records are byte-equal. A generated block also gets records for cores 2–4, and some of these cores are probably on SOC2. The SOC2 image is encrypted, so its call contract on 4.1.1 is not checked. The report seed is found by search (`find-seed`), not by a fixed offset.
