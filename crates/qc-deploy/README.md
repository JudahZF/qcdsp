# qc-deploy

Offline stock-clone and generated-block staging, local backup/restore, and explicit,
guarded SSH device deployment. Planning and dry runs never connect to a device.

## Stock-clone API

1. Load an explicit `FirmwarePin` with the trusted SHA-256 of `ZenUI` and all
   five `RUNTIME_FILES`. These are **on-disk/encrypted hashes** from the supported
   firmware copy, not the Python installer's plaintext runtime hashes. Unknown,
   incomplete, or mismatched identity is refused. No vendor hashes are embedded.
2. Open `Snapshot::open(copied_neuraldsp_root, pin)`. Supply the root of a local
   **copy** of `opt/neuraldsp`, not the live path or a device-mounted tree.
3. Call `clone_stock(&snapshot, Secrets { repo, blob, report }, spec)`.
   `CloneSpec::volume(new_id, name, new_blob)` defaults to source `volume`, Utility
   category `16`. The complete encrypted stock snapshot is the source: XML and
   blob are decrypted with caller-supplied secrets, validated, cloned, and encrypted
   again. No key discovery or key files are built in.
4. Call `PreparedClone::write_stage(new_directory)`. This only creates a new
   bundle outside the snapshot. It does not apply anything. Inspect `manifest()`
   for the explicit firmware pin, source hashes, and before/after file hashes.
5. `validate_stage(&snapshot, stage_directory, secrets)` validates an existing
   bundle by rebuilding the clone from its pinned source and comparing every byte.
   Merely rehashing arbitrary generated code or changing resource figures is not
   accepted as a stock clone.
6. For an explicitly requested local test only, call
   `PreparedClone::apply_to_copy(&snapshot, new_backup_directory)`.
7. `restore_copy(&snapshot, backup_directory)` uninstalls or recovers a partially
   applied clone. It verifies backup hashes and current contents/permissions first.
   Modified files are refused, not overwritten or merged. Restore can be retried.

The caller must hold exclusive access to its copied tree. The offline APIs above
do not make SSH connections or write a device. No API reboots, kills processes,
starts applications, discovers keys, or edits SSH configuration.

## Generated-block API

The caller supplies `GeneratedBlock`: blob name, decimal model ID and category,
parser-valid core 1 plaintext `ModuleBlob`, three named `DiffRecord`s for cores
2/3/4, and complete `Model` and `Module` XML elements. Resource estimates must be
computed by qcdsp before calling this API; this crate does not estimate or certify
their accuracy.

```rust,ignore
let prepared = qc_deploy::stage_generated(&snapshot, secrets, &block, &stage_dir)?;
prepared.apply_to_copy(&snapshot, &local_backup_dir)?; // explicit local test only
qc_deploy::restore_copy(&snapshot, &local_backup_dir)?;
```

`stage_generated` refuses ID/blob collisions, an existing core 1 blob, missing
categories, invalid/misnamed diffs and malformed or wrong-identity XML elements.
It appends the new model as the last child of its category and the new module as
the last child of `ModuleReport`, preserving all other document bytes. An empty,
self-closing category is expanded to hold the model. Existing diff records are
retained except a matching new-blob record, which is upserted. Every generated core
variant is reconstructed and validated. Blob and XML are encrypted with the caller's
secrets; diff files stay plaintext. The new blob inherits `ModelRepo.xml`'s mode.

`PreparedGenerated::manifest()` exposes a `DeviceManifest` with `format: 1`,
`kind: "generated-block"`, `blob`, `firmware`, `resources_computed_by: "qcdsp"`, and
six `changes` containing before/after SHA-256 and permission mode. This provenance
marker is not a signature or independent resource validation. `write_stage` can
write another new stage. `validate_stage` continues to refuse generated stages.
`apply_to_copy` saves a `GeneratedBackupReceipt`; `restore_copy` dispatches to its
verified recovery path. The local-copy safeguards and per-file limits also apply.

## Device transport API

`DeviceStage::open(stage_dir)` loads either an existing stock-clone manifest or a
generated `DeviceManifest` from the same six-file layout. It verifies exact inventory,
paths, firmware fields, and every payload hash, then keeps the payloads immutable in
memory. It does **not** decrypt/rebuild stock code; use `validate_stage` first when
stock-clone semantic validation is needed. It does not validate generated resource
figures or audio behavior.

```rust,ignore
let stage = qc_deploy::DeviceStage::open(&stage_dir)?;
let mut transport = qc_deploy::SshTransport::new("root@qc")?; // no connection yet
let mut opts = qc_deploy::DeviceOptions {
    token: unique_token,                 // ASCII alphanumeric, '_' or '-', max 64
    expected_serial: trusted_serial,     // nine ASCII alphanumeric characters
    backup_directory: new_backup_dir,    // must not exist for install
    dry_run: true,
    confirm_device_write: false,
};
let plan = qc_deploy::plan_install(&stage, &pin, &opts)?; // local only
let commands = plan.commands(&transport)?;              // local only
qc_deploy::install(&mut transport, &stage, &pin, &opts)?; // prints; NO connection
// Only after separate user approval:
opts.dry_run = false;
opts.confirm_device_write = true;
qc_deploy::install(&mut transport, &stage, &pin, &opts)?;
// Use the SAME token, serial, backup directory and explicit firmware pin:
qc_deploy::plan_uninstall(&pin, &opts)?; // local verification and planning only
qc_deploy::uninstall(&mut transport, &pin, &opts)?;
```

`install` and `uninstall` refuse an unconfirmed real run before contacting the
transport. `dry_run` prints exact, shell-quoted SSH argv and one-liners and returns
`InstallOutcome::DryRun { commands }`. It performs no remote operation and creates
no local output. Upload entries identify the exact stdin chunk by length and SHA-256;
the associated bytes are held in the plan's `RemoteOperation::Upload`. Printed
commands are an approval report, not a standalone replay script. `Installed` and
`Uninstalled` outcomes return the durable receipt.

An explicitly requested `preflight(transport, stage, pin)` is read-only but **does
connect**. It checks ZenUI and all five encrypted/on-disk runtime SHA-256 values
against the explicit pin, reads `/etc/qc_sn`, and lists the six target paths. Unknown
or mismatched firmware is refused. No automatic pin discovery or acceptance occurs.
Install and uninstall also bind the receipt to the caller's explicit serial.

### Steps that touch the device

1. **Read only:** run `openssl dgst -sha256`, `stat`, `ls`, and `cat` to check the
   firmware, serial, target hashes, permissions, and path/link safety. Download all
   existing target bytes with `cat`, verify them locally, and durably save `before/`
   and `receipt.json` before the first remote write.
2. **Private remote writes:** create `/opt/neuraldsp/.qc-block-<token>` with mode
   restricted by `umask 077`. Upload the six payloads under `files/`, one bounded
   128 KiB stdin chunk per short SSH command. Verify each on-device SHA-256. Copy
   originals with `cp -p` into `before/`, verify hashes/modes, upload the recovery
   receipt, and sync. Recheck the firmware and entire live read set after uploads.
3. **Live install writes:** for each file, check the staged hash, set its mode,
   recheck the original/absence, then `mv` it into place, verify hash/mode, and sync.
   Order: core 1 blob, core 2/3/4 diffs, module report, model catalogue. Each rename
   is atomic; the whole install is **not transactional**.
4. **Uninstall/recovery:** verify local and remote receipts/backups and ALL live
   files before restoring anything. A live file must be exactly original or exactly
   installed, with its recorded mode. Copy each remote original into `restore/`,
   recheck its hash and current installed hash, then rename in reverse install
   order. Remove only a newly installed blob whose exact hash/mode still match.
   A partially copied private restore temporary can be overwritten on retry; links
   are refused. Keep both backups after restore. Unknown concurrent edits are refused.
5. **Reboot:** never performed here. Request and approve reboot as a separate action.

There is no automatic rollback. A failure after publication begins returns
`Error::DevicePublish` with the local recovery directory. Run the explicit guarded
`uninstall` to recover after fixing connectivity. A prepare failure leaves local
backups and possibly a partial private stage but does not replace live files. Use a
new token and backup directory for another install attempt; no stage is auto-deleted.
Recovery requires the verified remote backup as well as the local receipt/backup.

`SshTransport` uses the caller's SSH config for the host, or an explicit
`-o ProxyCommand=...` from `with_proxy_command` (the QC's OpenSSH 6.7 can need a
byte-at-a-time banner proxy). It retains normal host-key checking. No SCP, long-running helper, `head`, `dd`, device
Python, or `sha256sum` is used. SSH command uploads are chunked because unknown
processes can be killed within 1–9 seconds. Hash/copy/sync steps are separate short
shell one-liners. An original download interrupted by the device process killer
fails before remote writes. No claim of protection against that killer or power loss
is made.

`Transport` is the typed operation boundary. `SshTransport<R>` uses a replaceable
`CommandRunner`; `SystemCommandRunner` is the real process runner. `FakeTransport`
uses `FakeCommandRunner`, a private local `fake_root/opt/neuraldsp` tree and
`fake_root/etc/qc_sn`. The fake runner executes rewritten shell commands locally,
never starts SSH/SCP, records commands, and supports one-shot failure injection.
It tests the actual rendered shell; it is not a sandbox for arbitrary supplied scripts.

## Stock-clone preserved data

- Only model ID, display name, blob name, and the matching module-report identity
  change. All parameters, padding, heap, CPU tests, and resource figures are copied
  verbatim. Cross-category moves are refused; the requested category must already
  contain the stock source.
- The core 1 ModuleBlob must round-trip exactly. It is not edited.
- Stock per-core diff files must round-trip exactly. The source record is validated
  against the ModuleBlob, renamed, and appended. Every existing record is retained.
- Existing file permission bits are retained. Ownership, timestamps, ACLs, and
  extended attributes are not backed up or restored.

A staged directory holds `manifest.json` and six files under `files/`: the encrypted
core 1 clone, three plaintext `diffs.blob` files, encrypted `moduleReport.xml`, and
`ModelRepo.xml`. It is a qcdsp stock-clone format, **not** a compatible reader/writer
for Tim's version-2 ZIP packages. A backup holds `receipt.json` and the five original
files under `before/`. The new clone must not previously exist, so uninstall removes
it rather than overwriting another blob. Neither artifact holds encryption keys.

## Safety limits

- Literal/canonical `/opt/neuraldsp`, symlinked resource paths, hardlinked files,
  path traversal, overlapping snapshot/bundle directories, duplicate identities,
  unexpected bundle entries, and pre-existing output directories are refused.
- Bind-mount aliases cannot be identified by these portable path checks. The caller
  must ensure the selected path is a genuine private copy and exclude other writers.
  Hash/permission checks catch observed concurrent changes; they are not an OS-level
  compare-and-swap against a hostile writer racing the final rename.
- Files and backup directory entries are synced before publication. Each file uses
  a same-directory temporary and rename. **The six-file publication and restore are
  not atomic.** Failure or power loss can leave a partial tree. No automatic rollback
  is claimed; the retained verified backup is used for explicit recovery.
- Failed staging or backup creation leaves its private partial directory for
  inspection. The next call must use a new directory. No existing directory is deleted.
- SHA-256 manifests detect corruption and bind this clone to this snapshot; they are
  not signatures. Backups and trusted pins must be protected from untrusted changes.
- A stock clone is not proof of audio behavior on hardware. Device writes still need
  separate, later user approval.

## Tests

`cargo test -p qc-deploy` uses synthetic XML, a synthetic ModuleBlob, synthetic
firmware bytes, and obviously synthetic secrets. It checks staging without source
changes, byte-preserving metadata/resources, all three core variants, package edits,
firmware refusal, verified uninstall, corrupt backups, concurrent data/mode changes,
partial-publication recovery, permission restoration, and path/link guards.

The generated-stage tests also check metadata preservation, category insertion,
identity collisions, generated core reconstruction, stock-validation refusal, and
local partial recovery. Fake-device tests execute the rendered shell against a
private temporary tree with synthetic firmware and serial bytes. They cover both
bundle kinds, dry-run/confirmation guards, pinned firmware refusal, corrupted
uploads, interrupted publication/restore, concurrent content/mode edits, verified
local and remote backups, and bounded upload chunks. They never invoke SSH/SCP.

The opt-in corpus test exercises a real stock volume payload, both full stock XML documents, and all three stock diff files on a new private temporary copy:

```sh
QC_CORPUS_DIR=/path/to/decrypted/firmware cargo test -p qc-deploy --test corpus -- --nocapture
```

It re-encrypts the stock plaintext with invented secrets and uses synthetic firmware identity files. Staging, validation, local apply and restore must preserve the original files byte-for-byte. This tests real payload structure, not compatibility with an actual pinned firmware tree or audio behavior.

`cargo clippy -p qc-deploy --all-targets --no-deps -- -D warnings` checks this crate.
No vendor keys, blobs, firmware, or kernel maps are in this crate or its fixtures.

## Format references

The field layouts were read from the local research specification and Python toolkit
`install.py`, `package.py`, and `diffs.py`. This crate uses the independently implemented
`qc-format` for XML, ModuleBlob, diff, and cryptographic operations. It does not link
GPL disassembler code, copy prepared proprietary payloads, or ship runtime maps.
