# qc-format

Strict parsers and byte-preserving writers for QC ModuleBlob, per-core diffs, XML metadata, and NDSP encryption. All proprietary input and encryption secrets come from the caller; none are shipped.

## API

- `blob::ModuleBlob::parse(bytes)` / `to_bytes()` preserve section order and raw relocation words. `validate()` adds semantic checks. `Section::{Data0,Dm,Code,Isa}` define placeholder bases and address units. Link ranges have inclusive ends.
- `diff::DiffFile` reads/writes an ordered named record collection. `DiffRecord::apply()` patches byte and relocation-word runs; `between()` creates an equivalent delta. Truncated records, invalid sections, overlapping runs, invalid padding and out-of-bounds patches are rejected.
- `metadata` makes targeted element/attribute edits while preserving unrelated text bytes. `find_element()` rejects ambiguous matches. XML text is validated before edits. Clone/remove helpers support local stock-clone preparation.
- `crypto::crypt(data, secret)` implements the format's repeated SHA-1 derivation and AES-128 CTR operation. This encryption is unauthenticated; successful decryption alone is not integrity verification. Callers must parse and verify the expected plaintext and compare trusted hashes. No key discovery is performed.

## Tests

Synthetic tests cover malformed inputs, exact round trips, bounds, metadata collisions and independently generated crypto vectors with invented secrets.

```sh
QC_CORPUS_DIR=/path/to/corpus cargo test -p qc-format --test corpus -- --nocapture
```

The opt-in corpus tests verify all `.raw` blobs under `plain/blobs/core1`, per-core diff files under `plain/blobs/` or `rootfs/opt/neuraldsp/blobs/`, and lossless metadata clone/remove. Without the environment variable these tests print a skip message and do not validate a real corpus.

No LDR parser is implemented yet; the current backend does not modify boot streams.
