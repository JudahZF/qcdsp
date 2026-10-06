# QC block ABI used by generated blocks

Date: 2026-10-05. This is a summary; the full disassembly notes are private research and not part of this repository.

## Generated block design

Generated blocks reuse the stock **Volume** framework from the user's own CorOS 4.1.0 firmware (pinned by plaintext SHA-256 and by the exact words at every patch point). The entry, allocation, smoother, reset and destroy code stay stock. `qcdsp::device` changes four things and appends three functions to section 2:

| Patch | Parcel | Change |
| --- | --- | --- |
| state allocation | 0x15b `r4=imm16` | bytes for Volume's 0x44 + generated state (≤ 0x7fff) |
| init call | 0x16c `CALL` + kind-11 reloc | generated init, which tail-jumps to stock init 0xe2 |
| ctx[2] process | 0x179 `i12=imm32` + kind-8 reloc | generated process |
| ctx[3] set-parameter | 0x17e `i12=imm32` + kind-8 reloc | generated set-parameter |

Per-core diff records are rebuilt by applying Volume's records to the stock blob, applying the same patches, and diffing. Generated code has no kernel calls or new relocations.

## Callback contract (confirmed from stock code)

- init(R4 = state): state is byte-addressed (stock uses `(bw)` byte accesses on it, which are illegal in normal-word space). Stock init only writes the first 0x44 bytes.
- process(R4 = state, R8 = io): `io[0]` addresses 32 samples of channel A then 32 of channel B, processed in place. Stock touches exactly these 64 normal words; generated code touches the same footprint. 32 samples is also the smoother's block size.
- set-parameter(R4 = state, R8 = index, F12 = value). Volume's LEVEL is displayed 0–100% but used as a 0..1 gain, so ZenUI delivers normalized values. Generated parameters are declared 0..1 in `ModelRepo.xml`, so raw and normalized delivery agree; the callback clamps to 0..1 and maps to the parameter's DSP range.
- C run-time model: R4/R8/R12 arguments, I6 frame, I7 stack, M5 = 0, M6 = 1, M7 = -1, L registers 0. Generated process saves every callee-preserved register it touches; init and set-parameter use only scratch registers.

All generated memory accesses are unqualified normal-word accesses with word offsets or M modifiers, which the DAG scales for byte-addressed pointers, so the code does not depend on the numeric address space of `io[0]`.

## Verification without a device

- Encodings: each form is checked against ADI's SHARC+ PRM diagrams and stock-identical words; `qc-asm` has numeric vectors and an opt-in decode by the independent selache decoder. That decoder misreads the Type 3a M field (ADI: bits 40:38); the emulator corrects for it.
- `tests/emulate.rs` executes the **independent decoder's disassembly** of each generated blob, calling init, set-parameter and process as the kernel does. All examples match the host reference on both channels over 40 blocks, preserve callee-saved registers and the stack, and write only their own state, the audio buffer and their frame. A mutation (sharing state between channels) makes it fail.
- Host f32 arithmetic is not a model of SHARC rounding modes; outputs are compared with a tolerance.

## Still open

- Hardware execution: Gain (model id 16901) installs, passes ZenUI's blob integrity check and loads in a preset (2026-10-06). Its audio output and the stateful examples are not yet tested on the device.
- CPU figures are estimates (parcel count + DAG stalls, ×2 safety), not measurements. Heap is exact.
- Reset (ctx[16]) still resets only Volume's smoother; generated filter and delay state are not cleared on reset.
- Firmware updates: the Volume template and kernel layout are pinned to CorOS 4.1.0; other builds are refused. The report seed is found by search (`find-seed`), not by a fixed offset.
