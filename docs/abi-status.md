# QC block ABI used by generated blocks

This is a summary of the block ABI recovered during Tetrad research. The full disassembly notes are private research and are not part of this repository.

## Generated block design

Generated blocks reuse the stock **Volume** framework from the user's own CorOS 4.1.0 or 4.1.1 firmware (the blob is byte-equal in both builds; pinned by plaintext SHA-256 and by the exact words at every patch point). The entry, allocation, smoother, reset and destroy code stay stock. `qcdsp::device` changes four things (five with an arena) and appends four routines (five with an arena) and an entry wrapper to section 2:

| Patch | Parcel | Change |
| --- | --- | --- |
| state allocation | 0x15b `r4=imm16` | bytes for Volume's 0x44 + generated state (≤ 0x7fff) |
| init call | 0x16c `CALL` + kind-11 reloc | generated init, which tail-jumps to stock init 0xe2 |
| ctx[2] process | 0x179 `i12=imm32` + kind-8 reloc | generated process |
| ctx[3] set-parameter | 0x17e `i12=imm32` + kind-8 reloc | generated set-parameter |
| ctx[8] destroy (arena only) | 0x174 `i12=imm32` + kind-8 reloc | generated destroy, which tail-jumps to stock destroy 0x11 |

Per-core diff records are rebuilt by applying Volume's records to the stock blob, applying the same patches (with each core's kernel addresses in the generated code), and diffing; each record must rebuild its core's blob exactly. The entry wrapper stores the generated recovery pointer in ctx[14], then tail-jumps to the stock entry. It adds one kind-8 section-2 relocation for that pointer. A block without an arena has no kernel calls and its code is unchanged by the arena work.

## Heap arena (state larger than 0x7fff bytes)

Volume allocates the state with an `r4=imm16` (0x15b), so it holds at most 0x7fff bytes. When the per-channel banks (states, delay cursors, spills, delay buffers) do not fit, `Layout` moves them to an **arena** from kernel heap 0 (the DM heap, L1 block 0, 153,592 bytes; static RE note `dsp-heap-for-generated-blocks-20261022.md` §2). Parameters and control slots stay in the state, followed by one word: the arena pointer. Heap 0 is the larger L1 heap and Volume's own state comes from it; the scalar graph never pairs a DM and a PM access, so heap 1 (PM) would gain it nothing (networks keep their weights there, see below).

| Callback | With an arena |
| --- | --- |
| init | Writes parameter defaults, then `heap_malloc(0, bytes)` (k1 `0x16adfa`, R4 = heap, R8 = bytes, result R0, 0 when full). R4 is scratch, so the state is kept in one frame word (`I7 = MODIFY(I7,-1)`, `DM(-2,I6)`), released before the tail jump. The pointer goes to the state. When it is nonzero, the arena is zeroed and the states and cursors are set. Then the tail jump to stock init 0xe2 with R4 = state, as without an arena. |
| process | Loads the arena pointer once into I3 (saved in the frame). Zero: restore and return, so the in-place buffers keep the input (dry). Bank accesses use I3; parameters and controls keep I0. |
| fault recovery (ctx[14]) | Re-initializes the arena banks when the pointer is nonzero. |
| destroy (ctx[8]) | `heap_free(0, pointer)` (k1 `0x16ad7e`) when nonzero, then the tail jump to stock destroy 0x11 (which frees the state). ctx[8] has no null check in the kernel, so it is always installed. |
| live reset (ctx[16]) | Stock; the arena is kept. |

The kernel call is the stock C call, copied from digital_delay `0x38c2..0x38c8` (heap_malloc) and `0x399d..0x39a6` (heap_free): `r4 = heap`; `CJUMP addr (DB)` (Type 25a, `1804 hi lo`); `DM(I7,M7) = R2`; `DM(I7,M7) = <return − 1>` (Type 16a, kind-7 relocation). The same shape calls `malloc` in Volume's entry (`0x15b..0x161`). The heap call from the entry's context follows digital_delay, which allocates its delay lines from its entry (note §4.1); freeing from destroy follows its destructor (`0x398d..0x3a14`).

The core 2–4 addresses of the two functions come from the user's own data: `KernelMap::add_stock_calls` on a stock blob that calls both (`template::KERNEL_CALL_SOURCE`, digital_delay) and its per-core records; `VolumeTemplate::with_kernel` takes the map. On the 4.1.x corpus this harvest gives all 188 calls of the research map, with no difference. The DM heap figure adds `(arena + 11) & !7`; a block whose figure exceeds heap 0 is refused.

Generated process first runs the control steps once (parameter-only expressions, written to control slots after the parameter values), then for each channel a 32-sample loop of the sample steps. The sample steps load values at first use, write states as soon as their old value is dead, and spill to per-channel slots when registers run out. Layout: `crates/qcdsp/src/device/layout.rs`.

## Networks (IR 3)

A network block always has a DM arena (its bank, then the frame buffers of `device/frame.rs`) and a PM arena (heap 1) for its weights, its bias pairs and the tanh constants. The state ends with both pointers and two words per ring (the ring's base address and this call's first row). Init allocates both arenas, zeroes the DM arena, points every ring at its base and writes the weights with Type 16a `DM(I4,M6) = imm32` stores; destroy frees both; recovery zeroes the DM arena and resets the rings; process plays dry when either pointer is 0.

Process runs the network before the sample loop (`device/kernels.rs`): it copies channel A into buffer 0, runs each kernel between `BIT SET MODE1` and `BIT CLR MODE1` (PEYEN, with BDCST1 for a convolution's input and BDCST9 for a fused condition), advances the ring cursors, and leaves I5 at the output buffer, which the sample loop reads through a `network` source. Every loop body is 48-bit instructions only, with no branch (PRM `.06.062`). The network phase saves and restores the preserved registers it uses (I5, I8-I11, M0-M3, M8-M11, and the B registers it sets, before I) and leaves every L register at zero.

Verification: Tetrad's machine (now with Type 1a, multifunction computes, DAG2 PM data, SIMD with broadcast, circular buffers and the VISA loop-tail check) runs a two-array WaveNet and a network with the less common paths against the host reference, with the interrupt-stack, register, write-range and L checks of the other blocks. qc-asm checks each new form against PRM figures and stock words (its README). On a unit (CorOS 4.1.1, 2026-10-23): NAM trainer nano, feather and lite run; lite (75 % of a core, 129 KB DM arena, 25 KB weights) matched the host reference within 1e-5 after its history filled; the kernels took 1.22 times their instruction count.

## Callback contract (confirmed from stock code)

- init(R4 = state): state is byte-addressed (stock uses `(bw)` byte accesses on it, which are illegal in normal-word space). Stock init only writes the first 0x44 bytes.
- process(R4 = state, R8 = io): `io[0]` and `io[1]` each address 32 samples, processed in place. The pointers can use different address aliases and need not name adjacent buffers. The descriptor is saved above I7 before sample code can reuse its argument register. 32 samples is also the smoother's block size.
- set-parameter(R4 = state, R8 = index, F12 = value). Volume's LEVEL is displayed 0–100% but used as a 0..1 gain, so ZenUI delivers normalized values. Generated parameters are declared 0..1 in `ModelRepo.xml`, so raw and normalized delivery agree; the callback uses integer COMPU and GT to reject bit patterns above +1.0 (including negative zero, negatives, infinities and NaNs) before arithmetic. Accepted values map to the parameter's DSP range.
- fault recovery (ctx[14], R4 = state): zero both channel banks, restore declared state initials and delay cursors, keep parameter values, and return through the kernel C frame. Live reset ctx[16] retains generated history and only resets the unused stock smoother.
- destroy (ctx[8], R4 = state), arena blocks only: free the arena, then tail-jump to stock destroy with the kernel's frame.
- C run-time model: R4/R8/R12 arguments, I6 frame, I7 stack, M5 = M13 = 0, M6 = M14 = 1, M7 = M15 = -1. L registers are zero except the kernel's circular stack L6/L7; generated code leaves lengths and bases unchanged. MODE1 has RND32, CBUFEN, IRPTEN and NESTM set, SIMD and broadcast clear; generated callbacks do not write it. Generated process saves every callee-preserved register it touches; init and set-parameter use only scratch registers.

A mono block (IR 2) has one channel bank. After the sample loop of channel A, process reloads `io[0]` and `io[1]` and copies the 32 results to channel B with offset loads and stores. The reciprocal seed of IR 2 uses the fixed-point ALU subtract `Rn = Rx - Ry` (compute opcode 0x02); Tetrad's `tetrad-sharc` machine executes it in the generated-block tests.

All generated memory accesses are unqualified normal-word accesses with word offsets or M modifiers, which the DAG scales for byte-addressed pointers, so the code does not depend on the numeric address space of `io[0]`.

## Verification without a device

- Tetrad's `tetrad-sharc` tests (`tests/qcdsp_blocks.rs`) execute emitted parcels (`device::generate`) with its raw Machine (PRM arithmetic), independent of qc-asm and the text decoder: all eight examples, two state address aliases, separate mixed-alias channel buffers, 40 stereo frames, parameter delivery, and fault recovery at frame 20 after poisoning all history and cursor words. Parameter endpoints and modeled process instruction bounds are also checked. Every instruction is followed by interrupt-stack canary writes; CCES preserved words, MODE1, stacks, AIS/MIS, and write bounds are checked. No firmware is required. These checks start preserved register extensions at zero, as in Tetrad's owned blocks (V2); arbitrary incoming 40-bit extensions and circular stack wrap are not covered.
- Arena blocks on the same machine (`tests/qcdsp_blocks.rs`, a 9000-sample stereo delay, 72 KB of banks, both state aliases with the arena in the other alias): the harness answers calls at `0x16adfa` and `0x16ad7e` as the kernel would. It checks the CJUMP frame (two pushed words, the caller's frame above), records R4/R8, clobbers every scratch register and 64 stack words below SP, and does the C return. Init allocates once with heap 0 and the arena size, zeroes the canary-filled arena and sets the cursors, then reaches stock init with R4 = state and the kernel's frame. Process matches the host reference over 290 frames (past the line length and its wrap). Recovery repairs a poisoned arena. Destroy frees the same pointer from heap 0, then reaches stock destroy the same way. With a null allocation, process leaves the buffers bit-equal to the input and writes only its frame, recovery writes nothing, destroy calls no heap_free and the arena keeps its canaries. Writes outside the state, arena, buffers and frame fail every run. Mutations (R4 not reloaded, banks through I0, wrong heap index, no null check, I7 not released) fail these tests.
- The firmware corpus check (also in `tests/qcdsp_blocks.rs` of Tetrad) executes each core's entry wrapper at three VISA origins, verifies its kind-8 relocation and ctx[14] store, and checks that R4 and the other ctx words remain unchanged before the stock entry. For an arena block it links against the real Volume with the map harvested from digital_delay's records: the records apply and validate on cores 2–4, each CJUMP holds that core's address from `kernel_call_map.json`, each return literal has its kind-7 relocation, and ctx[8] points at the generated destroy.
- Encodings: each form is checked against ADI's SHARC+ PRM diagrams and stock-identical words; `qc-asm` has numeric vectors and an opt-in decode by the independent selache decoder. That decoder misreads the Type 3a M field (ADI: bits 40:38); the emulator corrects for it.
- `tests/emulate.rs` executes the **independent decoder's disassembly** of each generated blob, calling init, set-parameter and process as the kernel does. All examples match the host reference on both channels over 40 blocks, preserve callee-saved registers and the stack, and write only their own state, the audio buffer and their frame. A mutation (sharing state between channels) makes it fail.
- Host f32 arithmetic is not a model of SHARC rounding modes; outputs are compared with a tolerance.

## Still open

- Hardware execution: Gain (model id 16901) previously installed, passed ZenUI's integrity check and loaded in a preset (2026-10-06); it was uninstalled on 2026-10-16 to restore stock Cortex Control compatibility. On 2026-10-22 Tetrad deployed generated blocks through the stock BlobLoad path (no file install) on one CorOS 4.1.1 unit and compared USB audio with the host reference: Gain (8e-9), a tanh saturator (2e-6), slapback (5e-9), a 9000-sample arena echo (6e-9), a NAM LSTM (1.2e-6) and a small NAM WaveNet (7e-7). The biquad, tremolo, envelope filter and Micro Synth examples are not yet tested on the device.
- The DM heap figure follows the kernel allocator: `(bytes + 11) & ~7` per allocation (k1 `0x16adfa`), with Volume's state allocation replaced by the generated one, plus the arena.
- Arena proofs on a unit (2026-10-22, through Tetrad, CorOS 4.1.1; proof IDs from the dsp-heap note):
  - P1 passed: a 9000-sample stereo echo (72 KB heap-0 arena) starts and matches the host within 6e-9.
  - P2 passed: 100 redeploys leave the four cores' DM heap figures unchanged.
  - P3 measured: eight delay lines cost the same with their banks in Volume's state or in a heap-0 arena (18 % of core 2), and 34–36 % with the banks in an SD arena: about 37 extra cycles for each of 1280 bank accesses per call. The estimate charges 40 (`SD_ACCESS_CYCLES`).
  - P4 passed on the cores that took blocks: `heap_malloc(2, 512 KB)` raised the SD heap figure by 3.1 % on cores 2, 3 and 4 (core 3 and 4 are on SOC2). The fit placed no block on core 1.
  - P5 passed: a 512 KB SD arena, zeroed in init, starts without a fault, and its delay of 131,072 samples matches the host within 6e-6 (x + 0.5·x[n − 131072]).
  - P6 failed: with init asking heap 0 for 256 KB (more than the heap), `heap_malloc` did not return 0. The DM heap figure rose to 191 % and the unit showed "Something went wrong" with no audio until a restart. The generated null check stays, but the exact heap figures and ZenUI's fit are the only guard.
  - Unit overload: an SD block that the old estimate (no SD charge) admitted took a loaded core to 100 % and faulted it. The SD charge prevents that layout now.
  - P7 (ReceiveData into a ctx[9] SD buffer) is not done; generated blocks do not use it.
  - Unproved assumptions: the SOC2 cores' heap layout, which `heap_malloc` hides (P4 shows it allocates there).
- CPU figures are estimates (instruction count + DAG stalls, ×2 safety), not measurements. A SHARC+ core issues one instruction per cycle whatever its parcel width; Tetrad's machine counts executed instructions the same way and checks that they stay within the estimate.
- Live reset (ctx[16]) preserves generated filter/delay history; fault recovery (ctx[14]) clears it. These are separate operations, as in Tetrad's owned-block ABI.
- Numerical policy: the `tetrad-sharc` PRM execution models invalid operations and the AIS/MIS stickies tested by the kernel. The older decoder-text emulator uses host f32, including Rust MIN/MAX NaN rules; it is only an encoding and mathematical cross-check. A process fault now has a recovery callback to clear generated state. The envelope-driven examples clamp their input to ±16, which handles infinities.
- Firmware updates: the Volume template and kernel layout are pinned to the builds in `qc_deploy::KNOWN_BUILDS`, CorOS 4.1.0 and 4.1.1; other builds are refused. From 4.1.0 to 4.1.1 only ZenUI and the SOC2 image `ZPU_SC589_SHARC_Core0.blwldr` changed. The SOC1 kernels (as installed, after the per-unit translation), the Volume blob and its per-core records are byte-equal. A generated block also gets records for cores 2–4, and some of these cores are probably on SOC2. The SOC2 image is encrypted, so its call contract on 4.1.1 is not checked. The report seed is found by search (`find-seed`), not by a fixed offset.
