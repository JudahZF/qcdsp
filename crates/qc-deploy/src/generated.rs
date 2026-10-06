use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use qc_format::{
    blob::ModuleBlob,
    crypto::crypt,
    diff::{DiffFile, DiffRecord},
    metadata,
};
use serde::{Deserialize, Serialize};

use crate::{
    Change, DeviceManifest, Error, Result, Secrets, Snapshot, blob_path, fs, sha256, update_paths,
};

/// The caller supplies code and resource metadata computed by qcdsp. This API
/// validates structure/identity, not audio behavior or resource estimates.
#[derive(Clone, Debug)]
pub struct GeneratedBlock {
    pub blob_name: String,
    pub model_id: String,
    pub category: String,
    pub core1_plain: Vec<u8>,
    /// Records for cores 2, 3 and 4, in that order.
    pub diffs: [DiffRecord; 3],
    pub model_element: String,
    pub module_element: String,
}

#[derive(Debug)]
pub struct PreparedGenerated {
    source_root: PathBuf,
    manifest: DeviceManifest,
    before: BTreeMap<String, Vec<u8>>,
    updated: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedBackupReceipt {
    pub format: u32,
    pub snapshot_root: PathBuf,
    pub generated: DeviceManifest,
}

fn decimal(value: &str) -> bool {
    !value.is_empty() && value.len() <= 10 && value.bytes().all(|c| c.is_ascii_digit())
}

fn check_element(text: &str, tag: &str, attributes: &[(&str, &str)]) -> Result<()> {
    let elements = metadata::elements(text, tag)?;
    if elements.len() != 1 || elements[0].range != (0..text.len()) {
        return Err(Error::Invalid(format!(
            "expected exactly one complete <{tag}> element"
        )));
    }
    for (name, value) in attributes {
        if metadata::attr(text, name)?.as_deref() != Some(*value) {
            return Err(Error::Invalid(format!("generated {tag} {name} differs")));
        }
    }
    Ok(())
}

fn unused(doc: &str, tag: &str, blob_attr: &str, block: &GeneratedBlock) -> Result<()> {
    for element in metadata::elements(doc, tag)? {
        let id = metadata::attr(element.text, "id")?;
        // Decimal identities are numeric, including IDs with leading zeroes.
        let same_id = id.as_deref().and_then(|id| id.parse::<u64>().ok())
            == block.model_id.parse::<u64>().ok();
        if same_id || metadata::attr(element.text, blob_attr)?.as_deref() == Some(&block.blob_name)
        {
            return Err(Error::Invalid(format!(
                "generated {tag} identity already exists"
            )));
        }
    }
    Ok(())
}

// Reuse metadata's validated element spans, then insert immediately before the
// parent's closing tag. No existing child or surrounding whitespace is rewritten.
fn append_child(
    doc: &str,
    parent: &metadata::Element<'_>,
    tag: &str,
    child: &str,
) -> Result<String> {
    let updated = if let Some(start) = parent.text.strip_suffix("/>") {
        format!("{start}>{child}</{tag}>")
    } else {
        let end = parent
            .text
            .rfind("</")
            .ok_or_else(|| Error::Invalid("missing metadata closing tag".into()))?;
        format!("{}{child}{}", &parent.text[..end], &parent.text[end..])
    };
    let result = format!(
        "{}{}{}",
        &doc[..parent.range.start],
        updated,
        &doc[parent.range.end..]
    );
    metadata::elements(&result, tag)?;
    Ok(result)
}

/// Create a new generated stage without modifying the pinned snapshot. Encrypts
/// only the blob and XML; diff files remain plaintext. Resources are marked as
/// computed by qcdsp, never passed off as an unchanged stock clone.
pub fn stage_generated(
    snapshot: &Snapshot,
    secrets: Secrets<'_>,
    block: &GeneratedBlock,
    stage_dir: &Path,
) -> Result<PreparedGenerated> {
    snapshot.verify()?;
    qc_format::blob::validate_blob_name(&block.blob_name)?;
    if !decimal(&block.model_id)
        || !decimal(&block.category)
        || secrets.repo.is_empty()
        || secrets.blob.is_empty()
        || secrets.report.is_empty()
    {
        return Err(Error::Invalid(
            "invalid generated identity or missing secrets".into(),
        ));
    }
    check_element(
        &block.model_element,
        "Model",
        &[("id", &block.model_id), ("blob", &block.blob_name)],
    )?;
    check_element(
        &block.module_element,
        "Module",
        &[
            ("id", &block.model_id),
            ("blob_name", &block.blob_name),
            ("category", &block.category),
        ],
    )?;
    let module = ModuleBlob::parse(&block.core1_plain)?;
    module.validate()?;
    if module.to_bytes()? != block.core1_plain {
        return Err(Error::Invalid("generated blob does not round-trip".into()));
    }
    let target = blob_path(&block.blob_name);
    fs::expect(&snapshot.root, &target, None)?;
    let mut before = BTreeMap::new();
    for path in update_paths(&block.blob_name).into_iter().skip(1) {
        before.insert(path.clone(), fs::read(&snapshot.root, &path)?);
    }
    let repo = String::from_utf8(crypt(&before["ModelRepo.xml"], secrets.repo))
        .map_err(|_| Error::Invalid("repo is not UTF-8".into()))?;
    let report = String::from_utf8(crypt(&before["blobs/moduleReport.xml"], secrets.report))
        .map_err(|_| Error::Invalid("report is not UTF-8".into()))?;
    let repos = metadata::elements(&repo, "Models")?;
    let reports = metadata::elements(&report, "ModuleReport")?;
    if repos.len() != 1 || reports.len() != 1 {
        return Err(Error::Invalid("unsupported metadata roots".into()));
    }
    unused(&repo, "Model", "blob", block)?;
    unused(&report, "Module", "blob_name", block)?;
    let category = metadata::find_element(&repo, "Category", "id", &block.category)?
        .ok_or_else(|| Error::Invalid("generated category is absent".into()))?;
    if category.range.start <= repos[0].range.start || category.range.end >= repos[0].range.end {
        return Err(Error::Invalid("category is outside Models".into()));
    }
    let new_repo = append_child(&repo, &category, "Category", &block.model_element)?;
    let new_report = append_child(&report, &reports[0], "ModuleReport", &block.module_element)?;
    let mut updated = BTreeMap::from([
        (target.clone(), crypt(&block.core1_plain, secrets.blob)),
        (
            "ModelRepo.xml".into(),
            crypt(new_repo.as_bytes(), secrets.repo),
        ),
        (
            "blobs/moduleReport.xml".into(),
            crypt(new_report.as_bytes(), secrets.report),
        ),
    ]);
    for (index, record) in block.diffs.iter().enumerate() {
        if record.name != block.blob_name {
            return Err(Error::Invalid("generated diff name differs".into()));
        }
        let variant = record.apply(&module)?;
        variant.validate()?;
        let path = format!("blobs/core{}/diffs.blob", index + 2);
        let mut diffs = DiffFile::parse(&before[&path])?;
        if diffs.to_bytes()? != before[&path] {
            return Err(Error::Invalid(format!(
                "source diff does not round-trip: {path}"
            )));
        }
        diffs.upsert(record.clone());
        let bytes = diffs.to_bytes()?;
        let reparsed = DiffFile::parse(&bytes)?;
        let persisted = reparsed
            .get(&block.blob_name)
            .ok_or_else(|| Error::Invalid("generated diff disappeared".into()))?;
        if persisted.apply(&module)?.to_bytes()? != variant.to_bytes()? {
            return Err(Error::Invalid("generated diff does not reconstruct".into()));
        }
        updated.insert(path, bytes);
    }
    let mut changes = BTreeMap::new();
    for (path, bytes) in &updated {
        changes.insert(
            path.clone(),
            Change {
                before_sha256: before.get(path).map(|b| sha256(b)),
                after_sha256: sha256(bytes),
                // A new data blob inherits the existing catalogue's permission bits.
                mode: fs::mode(
                    &snapshot.root,
                    if path == &target {
                        "ModelRepo.xml"
                    } else {
                        path
                    },
                )?,
            },
        );
    }
    let prepared = PreparedGenerated {
        source_root: snapshot.root.clone(),
        manifest: DeviceManifest {
            format: 1,
            kind: "generated-block".into(),
            firmware: snapshot.pin.clone(),
            blob: block.blob_name.clone(),
            resources_computed_by: Some("qcdsp".into()),
            changes,
        },
        before,
        updated,
    };
    prepared.verify_source(snapshot)?;
    prepared.write_stage(stage_dir)?;
    Ok(prepared)
}

impl PreparedGenerated {
    pub fn manifest(&self) -> &DeviceManifest {
        &self.manifest
    }

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
        if self.source_root != snapshot.root || self.manifest.firmware != snapshot.pin {
            return Err(Error::Invalid(
                "generated block belongs to another snapshot".into(),
            ));
        }
        snapshot.verify()?;
        for (path, change) in &self.manifest.changes {
            fs::expect(&snapshot.root, path, change.before_sha256.as_deref())?;
            if change.before_sha256.is_some() {
                fs::expect_mode(&snapshot.root, path, change.mode)?;
            }
        }
        Ok(())
    }

    /// Explicit local-copy test. Saves all originals and a durable receipt before
    /// publishing. `restore_copy` accepts this receipt for uninstall or recovery.
    pub fn apply_to_copy(
        &self,
        snapshot: &Snapshot,
        new_backup_directory: impl AsRef<Path>,
    ) -> Result<GeneratedBackupReceipt> {
        self.verify_source(snapshot)?;
        fs::separate(&snapshot.root, new_backup_directory.as_ref())?;
        let backup = fs::new_dir(new_backup_directory.as_ref())?;
        for (path, bytes) in &self.before {
            fs::write_new(&backup, &format!("before/{path}"), bytes)?;
        }
        let receipt = GeneratedBackupReceipt {
            format: 1,
            snapshot_root: snapshot.root.clone(),
            generated: self.manifest.clone(),
        };
        fs::write_new(
            &backup,
            "receipt.json",
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        verify_backup(&backup, &receipt)?;
        self.verify_source(snapshot)?;
        for path in update_paths(&self.manifest.blob) {
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

fn verify_backup(
    backup: &Path,
    receipt: &GeneratedBackupReceipt,
) -> Result<BTreeMap<String, Vec<u8>>> {
    receipt.generated.validate()?;
    if receipt.format != 1 || receipt.generated.kind != "generated-block" {
        return Err(Error::Invalid("unsupported generated backup".into()));
    }
    let mut inventory = vec!["receipt.json".to_owned()];
    let mut originals = BTreeMap::new();
    for (path, change) in &receipt.generated.changes {
        if let Some(hash) = &change.before_sha256 {
            let relative = format!("before/{path}");
            let bytes = fs::read(backup, &relative)?;
            if sha256(&bytes) != *hash {
                return Err(Error::Invalid(format!("backup hash mismatch: {path}")));
            }
            originals.insert(path.clone(), bytes);
            inventory.push(relative);
        }
    }
    fs::inventory(backup, &inventory)?;
    Ok(originals)
}

pub(crate) fn restore_generated_copy(
    snapshot: &Snapshot,
    backup: &Path,
    bytes: &[u8],
) -> Result<()> {
    let receipt: GeneratedBackupReceipt = serde_json::from_slice(bytes)?;
    let originals = verify_backup(backup, &receipt)?;
    if receipt.snapshot_root != snapshot.root || receipt.generated.firmware != snapshot.pin {
        return Err(Error::Invalid("backup belongs to another snapshot".into()));
    }
    snapshot.verify()?;
    for (path, change) in &receipt.generated.changes {
        let hash = fs::hash_file(&snapshot.root, path)?;
        if hash != change.before_sha256 && hash.as_deref() != Some(&change.after_sha256) {
            return Err(Error::Changed(path.clone()));
        }
        if hash.is_some() {
            fs::expect_mode(&snapshot.root, path, change.mode)?;
        }
    }
    for path in update_paths(&receipt.generated.blob).into_iter().rev() {
        let change = &receipt.generated.changes[&path];
        let hash = fs::hash_file(&snapshot.root, &path)?;
        if hash == change.before_sha256 {
            continue;
        }
        if hash.as_deref() != Some(&change.after_sha256) {
            return Err(Error::Changed(path));
        }
        if let Some(original) = originals.get(&path) {
            fs::replace(
                &snapshot.root,
                &path,
                original,
                change.mode,
                Some(&change.after_sha256),
            )?;
        } else {
            fs::remove(&snapshot.root, &path, &change.after_sha256, change.mode)?;
        }
    }
    for (path, change) in &receipt.generated.changes {
        fs::expect(&snapshot.root, path, change.before_sha256.as_deref())?;
    }
    Ok(())
}
