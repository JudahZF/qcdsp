# qc-link

Build a core-1 ModuleBlob, core-2/3/4 diff records, and exact static section sizes from caller-supplied code/data.

`link(&LinkInput, &KernelMap)` validates the section layout, explicit relocation records, and known absolute VISA kernel-reference forms. It returns `LinkedBlock { blob, diffs, resources }`. Empty optional sections are omitted. Section 2 and an entry parcel in section 2 are required because every inspected stock entry uses that mode. No callback shim, ISA/VISA transition, allocator or kernel call is generated automatically.

`KernelMap::from_json` accepts the user's `kernel_call_map.json` format (`calls`/`data` maps, hexadecimal address strings, core2/core3/core4 destinations). Conflicting mappings and duplicate JSON address keys are rejected without changing existing entries. The library embeds no firmware-specific mappings.

`StaticResources::{pm,dm,sw,nw}` are section-0/1/2/3 byte counts. Pads, dynamic heaps and measured CPU figures are separate caller responsibilities. A successfully linked block is not evidence of a correct runtime ABI or safe resource fit.

Supported code relocation kinds: 4, 7, 8 and 11, with restrictions derived from stock records. Kinds 0, 5 and 9 are explicitly rejected. Section-2 absolute kernel calls (`1804 hi lo`) and immediate address loads (`0F0n hi lo`) can generate per-core diffs. Section-3 kernel patch semantics are not established and are not guessed.

Section 2 uses little-endian sixteen-bit parcels. Section 3 uses little-endian six-byte ISA words. Code width and processor execution mode are separate.

## Validation

```sh
cargo test -p qc-link
QC_CORPUS_DIR=/path/to/research/device/plain cargo test -p qc-link --test corpus -- --nocapture
```

Corpus tests re-link the subset of stock blobs with supported relocation kinds and reproduce known absolute kernel patches for all other cores. Unsupported relocation kinds fail closed; a pass does not imply all stock blobs can be newly linked.
