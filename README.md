# qcdsp

A Rust backend for a small DSP language that builds installable Quad Cortex blocks, following `plans/2026-10-05-qcdsp-custom-blocks.html`.

Write a block with the Rust expression builder, test it on the host, then compile it into an encrypted stage for your own device and install it over SSH. Generated blocks run on the SHARC+ cores as three VISA callbacks appended to the stock Volume framework from your firmware (see `docs/abi-status.md`).

**Status (2026-10-06):** the full pipeline builds, stages and installs. The generated code is verified by emulating the independent decoder's reading of it. `Gain (qcdsp)` (model id 16901) is installed on the device and loads in a preset; its audio output is not yet tested. The other examples have not been installed.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `qc-format` | ModuleBlob, per-core diffs, NDSP crypto, byte-preserving metadata edits |
| `qc-asm` | Verified SHARC+ ISA48 and VISA encoder subset, labels and PC-relative jumps |
| `qc-link` | Generic linker: sections, relocations, mapped kernel references, per-core diffs |
| `qcdsp` | DSL, IR, host reference, register allocation, device callbacks, CLI |
| `qc-deploy` | Firmware-pinned staging, local apply/restore, guarded SSH install/uninstall |

No vendor keys, key locations, firmware, stock blobs or kernel maps are included; everything device-specific is read from your own copy. `selache` is not linked; tests run it only as an external oracle.

> **Warning.** This is an unofficial research tool, not affiliated with or endorsed by Neural DSP. Installing a block rewrites firmware data files on your Quad Cortex. A faulty block can stop audio, crash the DSP cores or need a restore from backup. Make a full SD-card backup first and use it at your own risk. Only CorOS 4.1.0 is supported; other builds are refused.

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

Operations: input, parameter, constant, state, delay tap, add, subtract, multiply, negate, min, max. `examples` has gain, a biquad, a feedback delay and a soft clipper.

- `state(initial)` is one persistent sample value; `delay(samples)` is a zero-filled delay line. Each has at most one update. Reads see the previous sample; updates commit after the output.
- Parameters are in DSP units. On the device they appear as 0..1 controls and are mapped to `[min, max]`.
- On the device each block processes 32-sample blocks of two channels with independent state. Total state must fit 32 KB (delay lines of roughly 4000 samples per channel).
- More than 14 simultaneously live values (15 without delays) is a compile error; there is no spilling.

## Build, test and install

```sh
cargo test --workspace
cargo install --path crates/qcdsp --locked
```

### What a device build needs

All inputs come from your own Quad Cortex:

| Input | Flag | How to get it |
| --- | --- | --- |
| Firmware copy | `--snapshot` | A local copy of `/opt/neuraldsp` (never the live path) |
| Serial | `--serial-file` | A copy of `/etc/qc_sn` |
| Firmware pin | `--pin` | JSON with the SHA-256 of `ZenUI` and the five `sharc_binaries` runtime files, made from a copy you trust (see below) |
| Key file | `--keys` | Private (0600) file of hex lines, any order: the repo key and the blob key (16 bytes each) and the report seed |

The **report seed** is found in your own ZenUI copy:

```sh
cargo qcdsp find-seed --snapshot path/to/neuraldsp --serial-file path/to/qc_sn --output seed.hex
cat seed.hex >> path/to/keys    # both files are mode 0600
```

`find-seed` tests NUL-delimited byte runs of ZenUI, then every 57-byte window, against `moduleReport.xml`; it does not depend on a fixed offset. The **repo and blob keys** exist only in ZenUI's memory while it runs; qcdsp does not extract them. Recover them with a short-lived on-device memory scan (ZenUI kills unknown processes within seconds). qcdsp checks every secret by decrypting a known file and never prints one.

The pin format:

```json
{ "zenui_sha256": "<hex>", "runtime_sha256": {
    "ZPUKernel_Core1.ldr": "<hex>", "ZPUKernel_Core2.ldr": "<hex>",
    "ZPU_SC589_SHARC_Core0.blwldr": "<hex>", "SOC2_SPL_Core0.blwldr": "<hex>",
    "SOC2_SPL_Core0_Atma.blwldr": "<hex>" } }
```

### Build and install

```sh
DEV="--snapshot path/to/neuraldsp --pin pin.json --keys path/to/keys --serial-file path/to/qc_sn"

# Compile and stage (writes only the new stage directory)
cargo qcdsp build --block my_gain.qcdsp.json $DEV --model-id 16901 --output target/my_gain.stage

# Print the exact install plan; nothing is sent
cargo qcdsp install --stage target/my_gain.stage --pin pin.json \
  --serial-file path/to/qc_sn --backup target/my_gain.backup --token my-gain-1

# Install for real, then reboot the QC
cargo qcdsp install ... --confirm-device-write

# Restore the saved originals, then reboot
cargo qcdsp uninstall --pin ... --serial-file ... --backup target/my_gain.backup --token my-gain-1 --confirm-device-write
```

`--model-id` must be unused in your `ModelRepo.xml`; staging refuses a collision. Install and uninstall connect with `ssh <host>` (`--host`, default `qc`) using your SSH config. The QC's OpenSSH 6.7 can need a byte-at-a-time banner proxy: set it in your SSH config or pass `--proxy-command 'python3 proxy.py %h %p'`.

Generated blob names are prefixed `qcdsp_`. Install backs up originals locally and on the device, publishes each file by atomic rename (the six-file install is not transactional), and never reboots; uninstall refuses files that changed since install. `stage-clone` stages an unchanged stock Volume clone (the postplan's M1 install test).

Other commands: `render` (host reference or lowered interpreter on a JSON sample array), `export-ir`, `fragment` (raw ISA48 kernel for inspection), `inspect` (decrypted ModuleBlob structure).

## Verification

```sh
cargo clippy --workspace --all-targets -- -D warnings
QC_CORPUS_DIR=path/to/decrypted/neuraldsp \
QC_BLOBDIS=path/to/blobdis \
SELACHE=path/to/selache/bin \
  cargo test --workspace -- --include-ignored
```

The opt-in variables are optional. `QC_CORPUS_DIR` is a decrypted copy of your firmware tree (`blobs/core1/*.raw`, diff files, XML). `QC_BLOBDIS` is a selache-based blob decoder and `SELACHE` holds selache's `selas`/`seldump`; neither is included because selache is GPL-3.0.

The opt-in tests round-trip all 371 stock blobs and diff files, re-link stock blobs, reproduce 40,889 kernel patch sites, stage/apply/restore a real stock payload on a private copy, link every example against the real Volume template, and emulate each generated block against the host reference.

## Postplan milestones

| Milestone | Status |
| --- | --- |
| M1 clone test | Implemented and tested locally; skipped on device (Gain installed directly) |
| M2 ABI map | Done (summary in `docs/abi-status.md`; full notes are private) |
| M3 open gain block | Installed on the device and loads in a preset; audio test pending |
| M4 DSL v1 | Gain, biquad, delay, drive compile to installable stages; only Gain installed; CPU not measured |
| M5 emulator | Decoder-driven emulator for generated code (not a full SHARC model) |
| M6 polish | Not started (VISA compression, SIMD, hardware loops) |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
