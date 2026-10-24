# qcdsp

qcdsp is a Rust toolchain that makes custom DSP blocks for the Quad Cortex. You write a block with a small Rust expression language. You test the block on your computer. Then qcdsp compiles the block to SHARC+ machine code, stages it for your own unit and installs it over SSH.

qcdsp is part of [Tetrad](https://github.com/JudahZF/tetrad), an independent Rust user interface for the Quad Cortex. Tetrad includes this repository as a git submodule at `qcdsp/`.

> **Warning.** This is an unofficial research tool. An install rewrites firmware data files on your Quad Cortex. A bad block can stop audio, crash the DSP cores or make a restore from backup necessary. Make a full SD-card backup before you install a block. You use this tool at your own risk. qcdsp supports only CorOS 4.1.0 and 4.1.1 and refuses other builds.

## Crates

| Crate | Purpose |
| --- | --- |
| `qc-format` | Reads and writes Quad Cortex block files: ModuleBlob, per-core diff records, NDSP encryption and byte-preserving `ModelRepo.xml` edits |
| `qc-asm` | Encodes a verified subset of SHARC+ ISA48 and VISA instructions, with labels and PC-relative jumps |
| `qc-link` | Links code and data sections, relocations and kernel references into a ModuleBlob and its per-core diffs |
| `qcdsp` | The DSP language, IR, host reference processor, register allocator, device code generator and the `cargo-qcdsp` CLI |
| `qc-deploy` | Stages a block against a firmware pin, applies and restores it locally, and installs or uninstalls it over SSH with guards |

Each crate except `qcdsp` has its own README. `docs/abi-status.md` describes the block ABI that generated code uses.

## How Tetrad uses qcdsp

Tetrad uses these crates as path dependencies:

- **`qc-format`**: Tetrad's catalogue, block, DSP-host, media, CLI and app crates read and write ModuleBlobs, diff files and model metadata, and decrypt NDSP files.
- **`qc-asm`**: Tetrad's `tetrad-blocks` and `tetrad-sharc` tests encode VISA instructions. These encodings are an independent check of Tetrad's own SHARC+ decoder.

qcdsp does not depend on Tetrad. Tetrad's `tetrad-sharc` tests use the `qcdsp` crate and run the generated code of all nine examples on Tetrad's raw SHARC+ machine. That machine does not share code with `qc-asm`, so these tests do not check the encoder against itself.

## Status

- All nine example blocks compile to installable stages. They run correctly on Tetrad's `tetrad-sharc` machine with PRM arithmetic.
- The `Gain` example (model id 16901) loaded in a preset on a unit. It was then uninstalled to restore stock Cortex Control compatibility. Its audio output is not tested.
- The other examples have not been installed on a unit with `qc-deploy`.
- Tetrad deploys generated blocks without a file install. On 2026-10-22 six of them played on a unit and matched the host reference, including an arena echo and two NAM models (docs/abi-status.md).
- CPU figures are estimates, not measurements: one cycle per instruction plus modelled DAG stalls, doubled in the catalogue entry. On the unit, a NAM LSTM used about 1.4 times its modelled count.

`docs/abi-status.md` lists the open items.

## Write a block

```rust
use qcdsp::{BlockBuilder, Category, Parameter, Processor};

fn main() -> qcdsp::Result<()> {
    let mut dsp = BlockBuilder::new("my_gain", "My Gain", Category::Utility);
    let input = dsp.input();
    let level = dsp.parameter(Parameter::new("Level", 0.0, 2.0, 1.0)?)?;
    let output = dsp.mul(input, level)?;
    let block = dsp.finish(output)?;

    let mut reference = Processor::new(block.clone())?;
    reference.set_parameter(0, 0.5)?;
    assert_eq!(reference.process_sample(1.0), 0.5);

    std::fs::write("my_gain.qcdsp.json", block.to_json()?).unwrap();
    Ok(())
}
```

Operations: input, parameter, constant, state, delay tap, add, subtract, multiply, negate, absolute value, min and max.

IR version 2 adds:

- `div`, `recip`, `tanh` and `sigmoid`. They expand to the operations above before compilation (`Block::expanded`), so the host reference and the device compute the same thing. `recip` is a bit-pattern seed and three Newton steps: relative error about 2^-22 for normal `|x|` in about 1e-37..1e37. Outside that range the result can be infinite or NaN, which faults the block's DSP slot. `tanh` is a Padé (7, 6) approximation of the clamped input, within 1e-4 of tanh and always inside (-1, 1); `sigmoid(x)` is `0.5 + 0.5 tanh(x / 2)`.
- `sub_bits`: integer subtraction of two bit patterns (the reciprocal seed).
- `"channels": "mono"` (`BlockBuilder::mono`): the block computes channel A only and writes the result to both channels, at half the cost.
- The categories `guitar_amplifier`, `bass_amplifier`, `bass_overdrive`, `compressor`, `reverb`, `wah` and `synth`.

IR version 3 adds a **network**: vector operations on channel buffers that run before the graph, for convolutional models such as NAM WaveNets (`NetworkBuilder`, `BlockBuilder::network`; `crates/qcdsp/src/network.rs`). Buffer 0 holds the block input; the graph reads the network's one-channel output with a `network` node. Operations: `gain` by a block parameter, `conv` (a causal dilated convolution; kernel 1 is a dense layer), `activation` (`tanh`, the same Padé approximation as the node; `hardtanh`; `relu`; `leaky_relu` with a slope from 0 to 1), `add` and `copy`. A buffer is written before it is read, and a buffer whose history a convolution reads does not change after that read. A network block is mono. `examples::wavenet` builds a NAM-style WaveNet.

On the unit a network runs once per 32-sample call, kernel by kernel over the whole call, in SIMD: a convolution computes two output channels and two rows per instruction pair and loads each weight once, and fuses a following one-channel input mix (NAM's condition) and residual add; tanh runs on four values at once with its constants from a table. Buffers are rows in a DM heap arena; buffers that a convolution reads with history are circular rings (DAG B and L registers); weights are in a PM heap arena, so a weight and an input load in the same instruction from different memory blocks. The CPU figure charges the counted instructions times 1.4: on a unit the kernels took 1.22 times their instruction count. Trainer-shaped NAM models (measured on 2026-10-23, one block alone on a core): nano below 25 %, feather 48 %, lite 75 % of a core; lite matched its host reference within 1e-5. Standard needs more DM heap than one core has and is refused.

`BlockBuilder::finish` writes the oldest version that holds the block, so a block without these features stays version 1.

`cargo qcdsp export --example saturator --output saturator.qcdsp.json` writes an example's graph (or `--block <file>` re-validates one), for example for a Tetrad extension package.

`qcdsp::examples` has these blocks: gain, biquad, a mono `tanh` saturator (IR 2), feedback delay, soft clipper, sine tremolo, envelope filter, and models of the Electro-Harmonix Micro Synthesizer and Bass Micro Synthesizer (`examples::micro_synth`). `crates/qcdsp/src/examples/kit.rs` has the shared parts: one-pole filters, an envelope follower, a state-variable filter and a `2^x` approximation for exponential tapers.

Rules:

- `state(initial)` is one persistent sample value. `delay(samples)` is a delay line that starts at zero. Each one has a maximum of one update. A read gives the value from the previous sample. An update takes effect at the next sample.
- Parameters use DSP units. On the unit, each parameter shows as a 0..1 control. qcdsp maps it to `[min, max]`.
- On the unit, a block processes 32-sample blocks of two channels (a mono block computes one and copies it). Each computed channel has its own state and its own `io` descriptor pointer. The two buffers do not have to be adjacent. A state of 32 KB or less (delay lines up to approximately 4000 samples per channel) stays in the stock Volume allocation. A larger state goes into an arena that the block takes from a kernel heap at start: the DM heap (153,592 bytes) while the block's figure fits it (a stereo delay of approximately 19,000 samples per channel), else the SD heap (16 MiB of DDR per core). The catalogue reports the arena in `dm_heap` or `sd_heap`, and ZenUI refuses a layout that does not fit. That fit is the only guard: on a unit, a request larger than heap 0 was not refused by the kernel, and the core faulted. Each load or store in an SD arena costs about 40 cycles more than in L1 (measured), and the CPU figure includes it, so SD suits long delay lines with few taps. On a unit: a DM arena echo and a 512 KB SD arena delay matched the host, 100 redeploys left the heap unchanged, and SD arenas started on cores 2, 3 and 4.
- An expression of only parameters and constants (for example a taper or a filter coefficient) is calculated once per 32-sample call, not once per sample.
- The compiler loads values when they are first used. It writes each state when its new value is available. When the 14 data registers (15 without delays) are all in use, the compiler reloads or spills values to a per-channel slot in the block state. Spills count toward the per-channel state. Hoisted values are always in the 32 KB allocation.

## Build and test

You need Rust 1.95 or later.

```sh
git clone https://github.com/JudahZF/qcdsp
cd qcdsp
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
cargo install --path crates/qcdsp --locked
```

### Optional tests with your own firmware

Some tests need files that this repository does not include. These tests skip or are ignored when their variable is not set:

```sh
QC_CORPUS_DIR=path/to/corpus \
QC_BLOBDIS=path/to/blobdis \
SELACHE=path/to/selache/bin \
  cargo test --workspace -- --include-ignored
```

- `QC_CORPUS_DIR` is the corpus root, the same folder Tetrad uses: `plain/` is a decrypted copy of your own firmware tree (`blobs/core1/*.raw`, diff files and XML), `rootfs/` a copy of the root file system (diff files fall back to `rootfs/opt/neuraldsp/blobs/`), and `presets/`.
- `QC_BLOBDIS` is a blob decoder based on selache.
- `SELACHE` is the directory that holds selache's `selas` and `seldump`.

selache is GPL-3.0. qcdsp does not include or link it. The tests run it only as an external program.

With these files, the tests do a round trip of all 371 stock blobs and diff files, link the stock blobs again, reproduce 40,889 kernel patch sites, check the kernel map that a build harvests from the stock records, stage, apply and restore a real stock payload on a private copy, link each example against the real Volume template, and run each generated block against the host reference.

Without these files, `tests/dsp.rs` checks the register allocator on 300 random graphs at three register budgets against the graph interpreter. The host graph is a mathematical reference with a tolerance. It is not a bit-exact SHARC model.

## Install a block on your unit

### Inputs from your own unit

This repository does not include vendor keys, key locations, firmware, stock blobs or kernel maps. All device inputs come from your own Quad Cortex:

| Input | Flag | Source |
| --- | --- | --- |
| Firmware copy | `--snapshot` | A local copy of `/opt/neuraldsp`. Do not use the live path. |
| Serial | `--serial-file` | A copy of `/etc/qc_sn` |
| Kernel pin | `--pin` | JSON with the SHA-256 of your unit's two `ZPUKernel` files, made from a copy that you trust |
| Key file | `--keys` | A private (mode 0600) file of hex lines in any order: the repo key, the blob key (16 bytes each) and the report seed |

To find the **report seed** in your own ZenUI copy:

```sh
cargo qcdsp find-seed --snapshot path/to/neuraldsp --serial-file path/to/qc_sn --output seed.hex
cat seed.hex >> path/to/keys    # keep both files at mode 0600
```

`find-seed` tests the NUL-delimited byte runs of ZenUI, then every 57-byte window, against `moduleReport.xml`. It does not use a fixed offset.

The **repo key** and **blob key** are only in ZenUI's memory while it runs. qcdsp does not extract them. You must recover them yourself, for example with a short on-device memory scan. qcdsp checks each secret by decrypting a known file. It never prints a secret.

qcdsp knows the hashes of `ZenUI` and the three SOC2 `sharc_binaries` files of each supported build (`qc_deploy::KNOWN_BUILDS`). The CorOS installer translates the two `ZPUKernel` files per unit, so their hashes differ between units and you supply them. Both supported builds have the same kernels. The kernel pin format:

```json
{ "ZPUKernel_Core1.ldr": "<hex>", "ZPUKernel_Core2.ldr": "<hex>" }
```

Make it with `sha256sum sharc_binaries/ZPUKernel_Core{1,2}.ldr` in your copy. qcdsp checks your copy against each supported build. It refuses the copy if no build matches every file.

### Build, install and uninstall

```sh
DEV="--snapshot path/to/neuraldsp --pin pin.json --keys path/to/keys --serial-file path/to/qc_sn"

# Compile and stage. This writes only the new stage directory.
cargo qcdsp build --block my_gain.qcdsp.json $DEV --model-id 16901 --output target/my_gain.stage

# Show the install plan. This sends nothing.
cargo qcdsp install --stage target/my_gain.stage --pin pin.json \
  --serial-file path/to/qc_sn --backup target/my_gain.backup --token my-gain-1

# Install, then reboot the unit.
cargo qcdsp install ... --confirm-device-write

# Restore the saved original files, then reboot the unit.
cargo qcdsp uninstall --pin ... --serial-file ... --backup target/my_gain.backup --token my-gain-1 --confirm-device-write
```

- `--model-id` must not be in use in your `ModelRepo.xml`. Staging refuses a collision.
- Install and uninstall connect with `ssh <host>` (`--host`, default `qc`) and use your SSH config. The unit's OpenSSH 6.7 can need a proxy that sends the banner one byte at a time. Set it in your SSH config or use `--proxy-command '<command> %h %p'`.
- On the tested firmware, a changed `ModelRepo.xml` stops stock Cortex Control from connecting. The stock catalogue restores the connection.
- Generated blob names start with `qcdsp_`.
- Install backs up the original files locally and on the unit. It publishes each file with an atomic rename, but the six-file install as a whole is not transactional. It never reboots the unit.
- Uninstall refuses files that changed after the install.
- `stage-clone` stages an unchanged clone of the stock Volume block. Use it to test the install path without generated code.

Other commands: `render` (runs the host reference or the lowered interpreter on a JSON sample array or a WAV file), `export-ir`, `fragment` (shows the raw ISA48 kernel and its step list) and `inspect` (shows the decrypted ModuleBlob structure).

Listen to a block before you install it:

```sh
cargo qcdsp render --example micro-synth --input di.wav --output synth.wav --compiled \
  --param "start freq=0.05" --param "stop freq=0.9" --param resonance=0.8
```

`--param` takes a parameter index or name. `--compiled` runs the lowered steps instead of the graph.

## Licence

qcdsp is licensed under either of the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

## Trademarks

qcdsp is an independent project. It is not affiliated with, endorsed by or supported by Neural DSP Technologies. Quad Cortex, Neural DSP and CorOS are trademarks of their owners.
