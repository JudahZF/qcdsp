use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    Change, CloneManifest, Error, FirmwarePin, Result, fs, sha256,
    transport::{ROOT, RemoteOperation, Transport, remote_stage, token_valid},
    update_paths,
};

/// Transport-only manifest for generated bundles. The parent must validate code,
/// encryption and resource metadata before staging. Hashes are not signatures or
/// a claim that arbitrary DSP code is safe to execute on hardware.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceManifest {
    pub format: u32,
    pub kind: String,
    pub firmware: FirmwarePin,
    pub blob: String,
    /// Generated resource provenance. This is not an independent resource check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources_computed_by: Option<String>,
    pub changes: BTreeMap<String, Change>,
}

impl DeviceManifest {
    pub(crate) fn validate(&self) -> Result<()> {
        self.firmware.validate()?;
        qc_format::blob::validate_blob_name(&self.blob)?;
        let paths = update_paths(&self.blob);
        if self.format != 1
            || !matches!(self.kind.as_str(), "generated-block" | "stock-clone")
            || (self.kind == "generated-block"
                && self.resources_computed_by.as_deref() != Some("qcdsp"))
            || (self.kind == "stock-clone" && self.resources_computed_by.is_some())
            || self.changes.len() != paths.len()
            || paths.iter().any(|p| !self.changes.contains_key(p))
        {
            return Err(Error::Invalid("unsupported device bundle inventory".into()));
        }
        for (path, change) in &self.changes {
            if !fs::valid_hash(&change.after_sha256)
                || change
                    .before_sha256
                    .as_ref()
                    .is_some_and(|h| !fs::valid_hash(h))
                || change.mode > 0o777
                || (path != &paths[0] && change.before_sha256.is_none())
            {
                return Err(Error::Invalid(format!("invalid device change: {path}")));
            }
        }
        Ok(())
    }
}

/// Immutable, hash-checked payloads loaded from the existing six-file layout.
/// Stock manifests retain their original validation rules. This is transport
/// validation, not the key-aware rebuilding done by `validate_stage`.
#[derive(Debug)]
pub struct DeviceStage {
    root: PathBuf,
    manifest: DeviceManifest,
    files: BTreeMap<String, Vec<u8>>,
}

impl DeviceStage {
    pub fn open(directory: impl AsRef<Path>) -> Result<Self> {
        let root = fs::root(directory.as_ref())?;
        let bytes = fs::read(&root, "manifest.json")?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let manifest =
            if value.get("kind").and_then(serde_json::Value::as_str) == Some("stock-clone") {
                let stock: CloneManifest = serde_json::from_slice(&bytes)?;
                stock.validate()?;
                DeviceManifest {
                    format: stock.format,
                    kind: stock.kind,
                    firmware: stock.firmware,
                    blob: stock.clone.blob,
                    resources_computed_by: None,
                    changes: stock.changes,
                }
            } else {
                let manifest: DeviceManifest = serde_json::from_slice(&bytes)?;
                if manifest.kind != "generated-block" {
                    return Err(Error::Invalid("unsupported device bundle kind".into()));
                }
                manifest
            };
        manifest.validate()?;
        let mut inventory: Vec<_> = manifest
            .changes
            .keys()
            .map(|p| format!("files/{p}"))
            .collect();
        inventory.push("manifest.json".into());
        fs::inventory(&root, &inventory)?;
        let mut files = BTreeMap::new();
        for (path, change) in &manifest.changes {
            let bytes = fs::read(&root, &format!("files/{path}"))?;
            if sha256(&bytes) != change.after_sha256 {
                return Err(Error::Invalid(format!("staged hash mismatch: {path}")));
            }
            files.insert(path.clone(), bytes);
        }
        Ok(Self {
            root,
            manifest,
            files,
        })
    }

    pub fn manifest(&self) -> &DeviceManifest {
        &self.manifest
    }
}

/// No write confirmation is inferred from a successful preflight or plan.
/// Use a unique caller-selected token and a new local backup directory.
#[derive(Clone, Debug)]
pub struct DeviceOptions {
    pub token: String,
    pub expected_serial: String,
    pub backup_directory: PathBuf,
    pub dry_run: bool,
    pub confirm_device_write: bool,
}

impl DeviceOptions {
    fn validate(&self) -> Result<()> {
        if !token_valid(&self.token) || !serial_valid(&self.expected_serial) {
            return Err(Error::Invalid(
                "invalid device token or expected serial".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceReceipt {
    pub format: u32,
    pub token: String,
    pub serial: String,
    pub manifest: DeviceManifest,
}

impl DeviceReceipt {
    fn validate(&self, pin: &FirmwarePin, opts: &DeviceOptions) -> Result<()> {
        self.manifest.validate()?;
        if self.format != 1 || self.token != opts.token || self.serial != opts.expected_serial {
            return Err(Error::Invalid(
                "backup token or device serial differs".into(),
            ));
        }
        verify_pin(&self.manifest, pin)
    }
}

#[derive(Clone, Debug)]
pub struct DeviceFacts {
    pub serial: String,
    pub target_listing: String,
    pub firmware: FirmwarePin,
}

/// Phases are public for approval UIs. Downloads are saved and verified locally
/// before `prepare`; all remote backups are verified before `publish`.
#[derive(Clone, Debug)]
pub struct DevicePlan {
    pub preflight: Vec<RemoteOperation>,
    pub downloads: Vec<RemoteOperation>,
    pub prepare: Vec<RemoteOperation>,
    pub publish: Vec<RemoteOperation>,
    pub verify: Vec<RemoteOperation>,
    pub receipt: DeviceReceipt,
    pub backup_directory: PathBuf,
}

impl DevicePlan {
    pub fn commands(&self, transport: &impl Transport) -> Result<Vec<String>> {
        let mut commands = Vec::new();
        for (name, operations) in [
            ("READ-ONLY PREFLIGHT", &self.preflight),
            ("DOWNLOAD LOCAL BACKUP", &self.downloads),
            ("PREPARE AND VERIFY REMOTE BACKUP", &self.prepare),
            ("PER-FILE PUBLICATION (NOT TRANSACTIONAL)", &self.publish),
            ("VERIFY", &self.verify),
        ] {
            commands.push(format!("# {name}"));
            if name == "DOWNLOAD LOCAL BACKUP" && !operations.is_empty() {
                commands.push(format!(
                    "# Verify each download; create local before/ and receipt.json in {}",
                    self.backup_directory.display()
                ));
            }
            for operation in operations {
                commands.extend(transport.describe(operation)?);
            }
        }
        Ok(commands)
    }
}

#[derive(Clone, Debug)]
pub enum InstallOutcome {
    DryRun { commands: Vec<String> },
    Installed { receipt: DeviceReceipt },
    Uninstalled { receipt: DeviceReceipt },
}

fn serial_valid(serial: &str) -> bool {
    serial.len() == 9 && serial.bytes().all(|c| c.is_ascii_alphanumeric())
}

fn device_path(path: &str) -> String {
    format!("{ROOT}/{path}")
}

fn verify_pin(manifest: &DeviceManifest, pin: &FirmwarePin) -> Result<()> {
    pin.validate()?;
    if manifest.firmware != *pin {
        return Err(Error::Firmware("stage and explicit pin differ".into()));
    }
    Ok(())
}

fn firmware_operations(pin: &FirmwarePin) -> Vec<RemoteOperation> {
    std::iter::once(("ZenUI".into(), pin.zenui_sha256.clone()))
        .chain(
            pin.runtime_sha256
                .iter()
                .map(|(p, h)| (format!("sharc_binaries/{p}"), h.clone())),
        )
        .map(|(path, hash)| RemoteOperation::Check {
            path: device_path(&path),
            hash: Some(hash),
            mode: None,
        })
        .collect()
}

fn preflight_operations(manifest: &DeviceManifest, pin: &FirmwarePin) -> Vec<RemoteOperation> {
    let mut operations = firmware_operations(pin);
    operations.push(RemoteOperation::Read {
        path: "/etc/qc_sn".into(),
    });
    operations.push(RemoteOperation::List {
        paths: update_paths(&manifest.blob)
            .iter()
            .map(|p| device_path(p))
            .collect(),
    });
    operations
}

fn run_preflight(
    transport: &mut impl Transport,
    operations: &[RemoteOperation],
    pin: &FirmwarePin,
) -> Result<DeviceFacts> {
    let mut serial = None;
    let mut listing = String::new();
    for operation in operations {
        let output = transport
            .execute(operation)
            .map_err(|error| match operation {
                RemoteOperation::Check { path, .. }
                    if path == &device_path("ZenUI")
                        || path.starts_with(&device_path("sharc_binaries/")) =>
                {
                    Error::Firmware(format!("{path}: {error}"))
                }
                _ => error,
            })?;
        match operation {
            RemoteOperation::Read { path } if path == "/etc/qc_sn" => {
                let value = std::str::from_utf8(&output)
                    .map_err(|_| Error::Invalid("non-ASCII serial".into()))?
                    .trim();
                if !serial_valid(value) {
                    return Err(Error::Invalid("invalid device serial".into()));
                }
                serial = Some(value.to_owned());
            }
            RemoteOperation::List { .. } => listing = String::from_utf8_lossy(&output).into_owned(),
            _ => {}
        }
    }
    Ok(DeviceFacts {
        serial: serial.ok_or_else(|| Error::Invalid("missing serial preflight".into()))?,
        target_listing: listing,
        firmware: pin.clone(),
    })
}

/// Explicit read-only connection: hashes ZenUI/runtimes, reads /etc/qc_sn and
/// lists the six targets. This is NOT called by planning or a dry run.
pub fn preflight(
    transport: &mut impl Transport,
    stage: &DeviceStage,
    pin: &FirmwarePin,
) -> Result<DeviceFacts> {
    verify_pin(&stage.manifest, pin)?;
    run_preflight(transport, &preflight_operations(&stage.manifest, pin), pin)
}

fn checks(manifest: &DeviceManifest, after: bool) -> Vec<RemoteOperation> {
    update_paths(&manifest.blob)
        .into_iter()
        .map(|path| {
            let change = &manifest.changes[&path];
            let hash = if after {
                Some(change.after_sha256.clone())
            } else {
                change.before_sha256.clone()
            };
            let mode = hash.as_ref().map(|_| change.mode);
            RemoteOperation::Check {
                path: device_path(&path),
                hash,
                mode,
            }
        })
        .collect()
}

/// Pure local planning. It never calls a transport or writes anything.
pub fn plan_install(
    stage: &DeviceStage,
    pin: &FirmwarePin,
    opts: &DeviceOptions,
) -> Result<DevicePlan> {
    opts.validate()?;
    verify_pin(&stage.manifest, pin)?;
    fs::separate(&stage.root, &opts.backup_directory)?;
    let receipt = DeviceReceipt {
        format: 1,
        token: opts.token.clone(),
        serial: opts.expected_serial.clone(),
        manifest: stage.manifest.clone(),
    };
    let remote = remote_stage(&opts.token);
    let mut preflight = preflight_operations(&stage.manifest, pin);
    preflight.extend(checks(&stage.manifest, false));
    let mut downloads = Vec::new();
    let mut prepare = vec![RemoteOperation::CreateStage {
        token: opts.token.clone(),
    }];
    let mut publish = Vec::new();
    for path in update_paths(&stage.manifest.blob) {
        let change = &stage.manifest.changes[&path];
        if change.before_sha256.is_some() {
            downloads.push(RemoteOperation::Read {
                path: device_path(&path),
            });
        }
        let staged = format!("{remote}/files/{path}");
        prepare.push(RemoteOperation::Upload {
            path: staged.clone(),
            bytes: stage.files[&path].clone(),
        });
        prepare.push(RemoteOperation::Check {
            path: staged.clone(),
            hash: Some(change.after_sha256.clone()),
            mode: Some(0o600),
        });
        publish.push(RemoteOperation::Publish {
            from: staged,
            to: device_path(&path),
            change: change.clone(),
        });
    }
    for path in update_paths(&stage.manifest.blob) {
        let change = &stage.manifest.changes[&path];
        if let Some(hash) = &change.before_sha256 {
            prepare.push(RemoteOperation::Backup {
                from: device_path(&path),
                to: format!("{remote}/before/{path}"),
                hash: hash.clone(),
                mode: change.mode,
            });
        }
    }
    let receipt_bytes = serde_json::to_vec_pretty(&receipt)?;
    prepare.push(RemoteOperation::Upload {
        path: format!("{remote}/receipt.json"),
        bytes: receipt_bytes.clone(),
    });
    prepare.push(RemoteOperation::Check {
        path: format!("{remote}/receipt.json"),
        hash: Some(sha256(&receipt_bytes)),
        mode: Some(0o600),
    });
    prepare.push(RemoteOperation::Sync);
    // Check the entire read set again after slow uploads and backup creation.
    prepare.extend(firmware_operations(pin));
    prepare.extend(checks(&stage.manifest, false));
    Ok(DevicePlan {
        preflight,
        downloads,
        prepare,
        publish,
        verify: checks(&stage.manifest, true),
        receipt,
        backup_directory: opts.backup_directory.clone(),
    })
}

fn dry_run(transport: &impl Transport, plan: &DevicePlan) -> Result<InstallOutcome> {
    let commands = plan.commands(transport)?;
    for command in &commands {
        println!("{command}");
    }
    Ok(InstallOutcome::DryRun { commands })
}

fn verify_local_backup(directory: &Path, receipt: &DeviceReceipt) -> Result<()> {
    let mut inventory = vec!["receipt.json".to_owned()];
    for (path, change) in &receipt.manifest.changes {
        if let Some(hash) = &change.before_sha256 {
            let relative = format!("before/{path}");
            fs::expect(directory, &relative, Some(hash))?;
            inventory.push(relative);
        }
    }
    fs::inventory(directory, &inventory)
}

fn apply_remote(transport: &mut impl Transport, plan: &DevicePlan) -> Result<()> {
    for operation in &plan.prepare {
        transport.execute(operation)?;
    }
    for operation in plan.publish.iter().chain(&plan.verify) {
        transport
            .execute(operation)
            .map_err(|source| Error::DevicePublish {
                backup: plan.backup_directory.clone(),
                source: Box::new(source),
            })?;
    }
    Ok(())
}

/// Saves a durable local recovery receipt before the first device write. No
/// automatic rollback, reboot, process kill or key discovery occurs. The caller
/// must exclude other writers; shell hash checks are not a kernel compare/swap.
pub fn install(
    transport: &mut impl Transport,
    stage: &DeviceStage,
    pin: &FirmwarePin,
    opts: &DeviceOptions,
) -> Result<InstallOutcome> {
    let plan = plan_install(stage, pin, opts)?;
    if opts.dry_run {
        return dry_run(transport, &plan);
    }
    if !opts.confirm_device_write {
        return Err(Error::DeviceWriteNotConfirmed);
    }
    let facts = run_preflight(transport, &plan.preflight, pin)?;
    if facts.serial != opts.expected_serial {
        return Err(Error::Changed("device serial".into()));
    }
    let backup = fs::new_dir(&opts.backup_directory)?;
    for operation in &plan.downloads {
        let RemoteOperation::Read { path } = operation else {
            return Err(Error::Invalid("invalid backup operation".into()));
        };
        let relative = path
            .strip_prefix(&format!("{ROOT}/"))
            .ok_or_else(|| Error::Invalid("backup path".into()))?;
        let bytes = transport.execute(operation)?;
        if Some(sha256(&bytes)) != plan.receipt.manifest.changes[relative].before_sha256 {
            return Err(Error::Changed(relative.into()));
        }
        fs::write_new(&backup, &format!("before/{relative}"), &bytes)?;
    }
    fs::write_new(
        &backup,
        "receipt.json",
        &serde_json::to_vec_pretty(&plan.receipt)?,
    )?;
    verify_local_backup(&backup, &plan.receipt)?;
    apply_remote(transport, &plan)?;
    Ok(InstallOutcome::Installed {
        receipt: plan.receipt,
    })
}

/// Reads and verifies only the LOCAL backup to construct a recovery plan. Both
/// original and installed states are accepted, so partial install/restore can be
/// retried. Unknown current contents or permissions are never overwritten.
pub fn plan_uninstall(pin: &FirmwarePin, opts: &DeviceOptions) -> Result<DevicePlan> {
    opts.validate()?;
    let backup = fs::root(&opts.backup_directory)?;
    let receipt: DeviceReceipt = serde_json::from_slice(&fs::read(&backup, "receipt.json")?)?;
    receipt.validate(pin, opts)?;
    verify_local_backup(&backup, &receipt)?;
    let remote = remote_stage(&receipt.token);
    let preflight = preflight_operations(&receipt.manifest, pin);
    let mut prepare = vec![RemoteOperation::Check {
        path: format!("{remote}/receipt.json"),
        hash: Some(sha256(&serde_json::to_vec_pretty(&receipt)?)),
        mode: Some(0o600),
    }];
    for (path, change) in &receipt.manifest.changes {
        if let Some(hash) = &change.before_sha256 {
            prepare.push(RemoteOperation::Check {
                path: format!("{remote}/before/{path}"),
                hash: Some(hash.clone()),
                mode: Some(change.mode),
            });
        }
    }
    for path in update_paths(&receipt.manifest.blob) {
        prepare.push(RemoteOperation::CheckRestorable {
            path: device_path(&path),
            change: receipt.manifest.changes[&path].clone(),
        });
    }
    let publish = update_paths(&receipt.manifest.blob)
        .into_iter()
        .rev()
        .map(|path| RemoteOperation::Restore {
            backup: format!("{remote}/before/{path}"),
            temporary: format!("{remote}/restore/{path}"),
            target: device_path(&path),
            change: receipt.manifest.changes[&path].clone(),
        })
        .collect();
    let verify = checks(&receipt.manifest, false);
    Ok(DevicePlan {
        preflight,
        downloads: Vec::new(),
        prepare,
        publish,
        verify,
        receipt,
        backup_directory: backup,
    })
}

/// Restore in reverse publication order, preserving remote and local backups.
/// A changed file, serial, firmware or backup refuses before the first restore.
pub fn uninstall(
    transport: &mut impl Transport,
    pin: &FirmwarePin,
    opts: &DeviceOptions,
) -> Result<InstallOutcome> {
    let plan = plan_uninstall(pin, opts)?;
    if opts.dry_run {
        return dry_run(transport, &plan);
    }
    if !opts.confirm_device_write {
        return Err(Error::DeviceWriteNotConfirmed);
    }
    let facts = run_preflight(transport, &plan.preflight, pin)?;
    if facts.serial != opts.expected_serial {
        return Err(Error::Changed("device serial".into()));
    }
    apply_remote(transport, &plan)?;
    Ok(InstallOutcome::Uninstalled {
        receipt: plan.receipt,
    })
}
