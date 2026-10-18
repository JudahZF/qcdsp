//! Offline stock-block cloning and explicitly guarded device deployment.
//!
//! Staging does not modify the source. Device transport is opt-in; planning and
//! dry runs never connect. There is no key discovery or reboot API.
//! Each replacement is atomic for one file only; a multi-file install is not atomic.

mod builds;
mod clone;
mod device;
mod fs;
mod generated;
mod transport;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub use builds::{KNOWN_BUILDS, KernelPin, KnownBuild, known_pins};
pub use clone::{CloneSpec, Secrets, clone_stock};
pub use device::{
    DeviceFacts, DeviceManifest, DeviceOptions, DevicePlan, DeviceReceipt, DeviceStage,
    InstallOutcome, install, plan_install, plan_uninstall, preflight, uninstall,
};
pub use fs::sha256;
pub use generated::{GeneratedBackupReceipt, GeneratedBlock, PreparedGenerated, stage_generated};
pub use transport::{
    CommandRequest, CommandRunner, FakeCommandRunner, FakeTransport, RemoteOperation, SshTransport,
    SystemCommandRunner, Transport,
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Format(#[from] qc_format::FormatError),
    #[error("invalid deployment artifact: {0}")]
    Invalid(String),
    #[error("unsafe local path: {0}")]
    UnsafePath(PathBuf),
    #[error("firmware identity is unknown or does not match a pinned build: {0}")]
    Firmware(String),
    #[error("tree changed: {0}; refusing to overwrite it")]
    Changed(String),
    #[error("device writes require explicit confirm_device_write")]
    DeviceWriteNotConfirmed,
    #[error("device transport failed: {0}")]
    Transport(String),
    #[error("device publication stopped; recover with uninstall using backup {backup}: {source}")]
    DevicePublish {
        backup: PathBuf,
        #[source]
        source: Box<Error>,
    },
    #[error("local publish stopped at {path}; saved backup is at {backup}: {source}")]
    Publish {
        path: String,
        backup: PathBuf,
        #[source]
        source: Box<Error>,
    },
}

/// The on-disk SHA-256 of ZenUI and the five runtime files of one firmware build
/// on one unit. These are encrypted/on-disk hashes, not the Python toolkit's
/// plaintext hashes. [`known_pins`] makes the pins of the supported builds;
/// the current tree's identity is never accepted automatically.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwarePin {
    pub zenui_sha256: String,
    pub runtime_sha256: BTreeMap<String, String>,
}

pub const RUNTIME_FILES: [&str; 5] = [
    "ZPUKernel_Core1.ldr",
    "ZPUKernel_Core2.ldr",
    "ZPU_SC589_SHARC_Core0.blwldr",
    "SOC2_SPL_Core0.blwldr",
    "SOC2_SPL_Core0_Atma.blwldr",
];

impl FirmwarePin {
    pub fn validate(&self) -> Result<()> {
        if !fs::valid_hash(&self.zenui_sha256)
            || self.runtime_sha256.len() != RUNTIME_FILES.len()
            || RUNTIME_FILES.iter().any(|name| {
                !self
                    .runtime_sha256
                    .get(*name)
                    .is_some_and(|hash| fs::valid_hash(hash))
            })
        {
            return Err(Error::Firmware(
                "missing or malformed firmware hashes".into(),
            ));
        }
        Ok(())
    }

    /// Each pinned path under `/opt/neuraldsp` with its expected hash.
    fn entries(&self) -> impl Iterator<Item = (String, &String)> {
        std::iter::once(("ZenUI".to_owned(), &self.zenui_sha256)).chain(
            self.runtime_sha256
                .iter()
                .map(|(name, hash)| (format!("sharc_binaries/{name}"), hash)),
        )
    }

    /// The first pinned path whose hash in `found` differs.
    fn mismatch(&self, found: &BTreeMap<String, Option<String>>) -> Option<String> {
        self.entries()
            .find(|(path, expected)| found.get(path).and_then(Option::as_ref) != Some(*expected))
            .map(|(path, _)| path)
    }

    fn verify(&self, root: &Path) -> Result<()> {
        self.validate()?;
        match self.mismatch(&hash_pinned(root)?) {
            Some(path) => Err(Error::Firmware(path)),
            None => Ok(()),
        }
    }
}

/// The on-disk hash of every pinned path in `root` (`None` if it is missing).
fn hash_pinned(root: &Path) -> Result<BTreeMap<String, Option<String>>> {
    std::iter::once("ZenUI".to_owned())
        .chain(
            RUNTIME_FILES
                .iter()
                .map(|name| format!("sharc_binaries/{name}")),
        )
        .map(|path| Ok((path.clone(), fs::hash_file(root, &path)?)))
        .collect()
}

/// The pin in `pins` that every hash in `found` matches. Any other build is refused.
fn select<'a>(
    pins: &'a [FirmwarePin],
    found: &BTreeMap<String, Option<String>>,
) -> Result<&'a FirmwarePin> {
    let mut mismatches = Vec::new();
    for pin in pins {
        pin.validate()?;
        match pin.mismatch(found) {
            None => return Ok(pin),
            Some(path) => mismatches.push(path),
        }
    }
    Err(Error::Firmware(format!(
        "no pinned build matches (first difference per build: {})",
        mismatches.join(", ")
    )))
}

/// An explicitly selected local copy. Symlinks and hardlinked resource files are
/// refused. This cannot identify mount aliases; do not pass a device-mounted tree.
#[derive(Debug)]
pub struct Snapshot {
    root: PathBuf,
    pin: FirmwarePin,
}

impl Snapshot {
    /// Opens the copy if its firmware matches one of `pins` exactly.
    pub fn open(copied_neuraldsp_root: impl AsRef<Path>, pins: &[FirmwarePin]) -> Result<Self> {
        let root = fs::root(copied_neuraldsp_root.as_ref())?;
        let pin = select(pins, &hash_pinned(&root)?)?.clone();
        Ok(Self { root, pin })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The pin of the build that this copy matched.
    pub fn firmware_pin(&self) -> &FirmwarePin {
        &self.pin
    }

    pub fn verify(&self) -> Result<()> {
        self.pin.verify(&self.root)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub before_sha256: Option<String>,
    pub after_sha256: String,
    pub mode: u32,
}

/// Contains no keys. The stock clone's resource metadata is retained unchanged;
/// this manifest does not certify that arbitrary/generated code is installable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneManifest {
    pub format: u32,
    pub kind: String,
    pub firmware: FirmwarePin,
    pub clone: CloneSpec,
    pub source_sha256: BTreeMap<String, String>,
    pub changes: BTreeMap<String, Change>,
}

impl CloneManifest {
    fn validate(&self) -> Result<()> {
        self.firmware.validate()?;
        self.clone.validate()?;
        let expected = update_paths(&self.clone.blob);
        let mut source_paths = update_paths(&self.clone.source_blob);
        source_paths.sort();
        if self.format != 1
            || self.kind != "stock-clone"
            || self.changes.len() != expected.len()
            || expected.iter().any(|path| !self.changes.contains_key(path))
            || self.source_sha256.keys().collect::<Vec<_>>()
                != source_paths.iter().collect::<Vec<_>>()
            || self
                .source_sha256
                .values()
                .any(|hash| !fs::valid_hash(hash))
        {
            return Err(Error::Invalid(
                "unsupported manifest or resource inventory".into(),
            ));
        }
        let target = blob_path(&self.clone.blob);
        for (path, change) in &self.changes {
            if !fs::valid_hash(&change.after_sha256)
                || change
                    .before_sha256
                    .as_ref()
                    .is_some_and(|h| !fs::valid_hash(h))
                || (path == &target) != change.before_sha256.is_none()
                || change.mode > 0o777
                || (path != &target
                    && change.before_sha256.as_ref() != self.source_sha256.get(path))
            {
                return Err(Error::Invalid(format!("invalid change: {path}")));
            }
        }
        Ok(())
    }
}

pub(crate) fn blob_path(blob: &str) -> String {
    format!("blobs/core1/{blob}_blob.bin")
}

pub(crate) fn update_paths(blob: &str) -> Vec<String> {
    vec![
        blob_path(blob),
        "blobs/core2/diffs.blob".into(),
        "blobs/core3/diffs.blob".into(),
        "blobs/core4/diffs.blob".into(),
        "blobs/moduleReport.xml".into(),
        "ModelRepo.xml".into(),
    ]
}

/// Constructible only by validating and cloning a stock source in a pinned copy.
#[derive(Debug)]
pub struct PreparedClone {
    pub(crate) source_root: PathBuf,
    pub(crate) manifest: CloneManifest,
    pub(crate) before: BTreeMap<String, Vec<u8>>,
    pub(crate) updated: BTreeMap<String, Vec<u8>>,
}

impl PreparedClone {
    pub fn manifest(&self) -> &CloneManifest {
        &self.manifest
    }

    /// Creates a new bundle with encrypted metadata/blob and unencrypted diff
    /// files. The directory must not exist and must be outside the source tree.
    pub fn write_stage(&self, new_directory: impl AsRef<Path>) -> Result<PathBuf> {
        self.manifest.validate()?;
        fs::separate(&self.source_root, new_directory.as_ref())?;
        let root = fs::new_dir(new_directory.as_ref())?;
        for (path, bytes) in &self.updated {
            fs::write_new(&root, &format!("files/{path}"), bytes)?;
        }
        fs::write_new(
            &root,
            "manifest.json",
            &serde_json::to_vec_pretty(&self.manifest)?,
        )?;
        Ok(root)
    }

    fn verify_source(&self, snapshot: &Snapshot) -> Result<()> {
        self.manifest.validate()?;
        if snapshot.root != self.source_root || snapshot.pin != self.manifest.firmware {
            return Err(Error::Invalid(
                "clone belongs to a different snapshot".into(),
            ));
        }
        snapshot.verify()?;
        for (path, hash) in &self.manifest.source_sha256 {
            fs::expect(&snapshot.root, path, Some(hash))?;
        }
        for (path, change) in &self.manifest.changes {
            if change.before_sha256.is_some() {
                fs::expect_mode(&snapshot.root, path, change.mode)?;
            }
        }
        fs::expect(&snapshot.root, &blob_path(&self.manifest.clone.blob), None)
    }

    /// Explicit local test installation. The complete verified backup is saved
    /// before the first replacement. On failure it is retained for restore_copy.
    /// Hold exclusive access to the copied tree throughout this call.
    pub fn apply_to_copy(
        &self,
        snapshot: &Snapshot,
        new_backup_directory: impl AsRef<Path>,
    ) -> Result<BackupReceipt> {
        self.verify_source(snapshot)?;
        fs::separate(&snapshot.root, new_backup_directory.as_ref())?;
        let backup = fs::new_dir(new_backup_directory.as_ref())?;
        for (path, bytes) in &self.before {
            fs::write_new(&backup, &format!("before/{path}"), bytes)?;
        }
        let receipt = BackupReceipt {
            format: 1,
            snapshot_root: snapshot.root.clone(),
            clone: self.manifest.clone(),
        };
        fs::write_new(
            &backup,
            "receipt.json",
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        verify_backup(&backup, &receipt)?;
        // Check the entire read set again after saving the backup, before publishing.
        self.verify_source(snapshot)?;
        for path in update_paths(&self.manifest.clone.blob) {
            let change = &self.manifest.changes[&path];
            fs::replace(
                &snapshot.root,
                &path,
                &self.updated[&path],
                change.mode,
                change.before_sha256.as_deref(),
            )
            .map_err(|source| Error::Publish {
                path,
                backup: backup.clone(),
                source: Box::new(source),
            })?;
        }
        for (path, change) in &self.manifest.changes {
            fs::expect(&snapshot.root, path, Some(&change.after_sha256))?;
        }
        Ok(receipt)
    }
}

/// Rebuilds the stock clone from the pinned snapshot and supplied secrets, then
/// compares every staged byte and manifest field. A hash-valid generated blob or
/// edited resource report is not accepted as a stock clone.
pub fn validate_stage(
    snapshot: &Snapshot,
    stage_directory: impl AsRef<Path>,
    secrets: Secrets<'_>,
) -> Result<PreparedClone> {
    let stage = fs::root(stage_directory.as_ref())?;
    fs::separate(&snapshot.root, &stage)?;
    let manifest: CloneManifest = serde_json::from_slice(&fs::read(&stage, "manifest.json")?)?;
    manifest.validate()?;
    let mut inventory: Vec<_> = manifest
        .changes
        .keys()
        .map(|path| format!("files/{path}"))
        .collect();
    inventory.push("manifest.json".into());
    fs::inventory(&stage, &inventory)?;
    let rebuilt = clone_stock(snapshot, secrets, manifest.clone.clone())?;
    if rebuilt.manifest != manifest {
        return Err(Error::Invalid("stage differs from the stock clone".into()));
    }
    for (path, bytes) in &rebuilt.updated {
        if fs::read(&stage, &format!("files/{path}"))? != *bytes {
            return Err(Error::Invalid(format!("stage payload mismatch: {path}")));
        }
    }
    Ok(rebuilt)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupReceipt {
    pub format: u32,
    pub snapshot_root: PathBuf,
    pub clone: CloneManifest,
}

fn verify_backup(backup: &Path, receipt: &BackupReceipt) -> Result<BTreeMap<String, Vec<u8>>> {
    if receipt.format != 1 {
        return Err(Error::Invalid("unsupported backup format".into()));
    }
    receipt.clone.validate()?;
    let mut inventory: Vec<_> = receipt
        .clone
        .changes
        .iter()
        .filter(|(_, change)| change.before_sha256.is_some())
        .map(|(path, _)| format!("before/{path}"))
        .collect();
    inventory.push("receipt.json".into());
    fs::inventory(backup, &inventory)?;
    let mut originals = BTreeMap::new();
    for (path, change) in &receipt.clone.changes {
        if let Some(hash) = &change.before_sha256 {
            let bytes = fs::read(backup, &format!("before/{path}"))?;
            if sha256(&bytes) != *hash {
                return Err(Error::Invalid(format!("backup hash mismatch: {path}")));
            }
            originals.insert(path.clone(), bytes);
        }
    }
    Ok(originals)
}

/// Uninstall or recover a local stock clone or generated block. Verifies every
/// saved original and every current file before changing anything. A file must
/// match either its exact original or the exact installed bytes, otherwise restore is
/// refused. This permits retry after partial restore, but does not merge edits.
/// Restores exact original bytes and permissions. Operations are per-file only.
pub fn restore_copy(snapshot: &Snapshot, backup_directory: impl AsRef<Path>) -> Result<()> {
    let backup = fs::root(backup_directory.as_ref())?;
    fs::separate(&snapshot.root, &backup)?;
    let receipt_bytes = fs::read(&backup, "receipt.json")?;
    let value: serde_json::Value = serde_json::from_slice(&receipt_bytes)?;
    if value.get("generated").is_some() {
        return generated::restore_generated_copy(snapshot, &backup, &receipt_bytes);
    }
    let receipt: BackupReceipt = serde_json::from_slice(&receipt_bytes)?;
    let originals = verify_backup(&backup, &receipt)?;
    if receipt.snapshot_root != snapshot.root || receipt.clone.firmware != snapshot.pin {
        return Err(Error::Invalid(
            "backup belongs to a different snapshot".into(),
        ));
    }
    snapshot.verify()?;
    for (path, change) in &receipt.clone.changes {
        let current = fs::hash_file(&snapshot.root, path)?;
        if current != change.before_sha256 && current.as_deref() != Some(&change.after_sha256) {
            return Err(Error::Changed(path.clone()));
        }
        if current.is_some() {
            fs::expect_mode(&snapshot.root, path, change.mode)?;
        }
    }
    // Reverse publication: catalogue first, blob last.
    for path in update_paths(&receipt.clone.clone.blob).into_iter().rev() {
        let change = &receipt.clone.changes[&path];
        let current = fs::hash_file(&snapshot.root, &path)?;
        if current == change.before_sha256 {
            continue;
        }
        if current.as_deref() != Some(&change.after_sha256) {
            return Err(Error::Changed(path));
        }
        if let Some(bytes) = originals.get(&path) {
            fs::replace(
                &snapshot.root,
                &path,
                bytes,
                change.mode,
                Some(&change.after_sha256),
            )?;
        } else {
            fs::remove(&snapshot.root, &path, &change.after_sha256, change.mode)?;
        }
    }
    for (path, change) in &receipt.clone.changes {
        fs::expect(&snapshot.root, path, change.before_sha256.as_deref())?;
    }
    Ok(())
}
