use std::collections::BTreeMap;

use qc_format::{blob::ModuleBlob, crypto::crypt, diff::DiffFile, metadata};
use serde::{Deserialize, Serialize};

use crate::{Change, CloneManifest, Error, PreparedClone, Result, Snapshot, blob_path, fs, sha256};

/// Caller-supplied secrets. Never serialized or included in Debug output.
#[derive(Clone, Copy)]
pub struct Secrets<'a> {
    pub repo: &'a [u8],
    pub blob: &'a [u8],
    pub report: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneSpec {
    pub source_blob: String,
    pub blob: String,
    pub model_id: String,
    pub name: String,
    pub category: String,
}

impl CloneSpec {
    /// Defaults to stock `volume` in category 16 (Utility).
    pub fn volume(
        model_id: impl Into<String>,
        name: impl Into<String>,
        blob: impl Into<String>,
    ) -> Self {
        Self {
            source_blob: "volume".into(),
            blob: blob.into(),
            model_id: model_id.into(),
            name: name.into(),
            category: "16".into(),
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        qc_format::blob::validate_blob_name(&self.source_blob)?;
        qc_format::blob::validate_blob_name(&self.blob)?;
        if self.source_blob == self.blob
            || !decimal(&self.model_id)
            || !decimal(&self.category)
            || self.name.trim().is_empty()
            || self.name.chars().any(char::is_control)
            || self.name.len() > 256
        {
            return Err(Error::Invalid("invalid or unchanged clone identity".into()));
        }
        Ok(())
    }
}

fn decimal(value: &str) -> bool {
    !value.is_empty() && value.len() <= 10 && value.bytes().all(|c| c.is_ascii_digit())
}

fn xml(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| Error::Invalid("decrypted metadata is not UTF-8".into()))
}

fn required_attr(element: &str, name: &str) -> Result<String> {
    metadata::attr(element, name)?
        .ok_or_else(|| Error::Invalid(format!("missing metadata attribute: {name}")))
}

/// Validates the encrypted snapshot source, retains its complete resource/CPU
/// metadata, renames its existing core delta records, and re-encrypts updates.
/// No resource estimates or generated sample fragments can enter this API.
pub fn clone_stock(
    snapshot: &Snapshot,
    secrets: Secrets<'_>,
    spec: CloneSpec,
) -> Result<PreparedClone> {
    spec.validate()?;
    snapshot.verify()?;
    if secrets.repo.is_empty() || secrets.blob.is_empty() || secrets.report.is_empty() {
        return Err(Error::Invalid(
            "all three encryption secrets are required".into(),
        ));
    }
    let target_blob = blob_path(&spec.blob);
    fs::expect(&snapshot.root, &target_blob, None)?;
    let source_blob_path = blob_path(&spec.source_blob);
    let source_blob_encrypted = fs::read(&snapshot.root, &source_blob_path)?;
    let raw = crypt(&source_blob_encrypted, secrets.blob);
    let module = ModuleBlob::parse(&raw)?;
    module.validate()?;
    if module.to_bytes()? != raw {
        return Err(Error::Invalid(
            "stock ModuleBlob does not round-trip exactly".into(),
        ));
    }

    let mut before = BTreeMap::new();
    for path in [
        "ModelRepo.xml",
        "blobs/moduleReport.xml",
        "blobs/core2/diffs.blob",
        "blobs/core3/diffs.blob",
        "blobs/core4/diffs.blob",
    ] {
        before.insert(path.to_owned(), fs::read(&snapshot.root, path)?);
    }
    let repo = xml(crypt(&before["ModelRepo.xml"], secrets.repo))?;
    let report = xml(crypt(&before["blobs/moduleReport.xml"], secrets.report))?;
    if metadata::elements(&repo, "Models")?.len() != 1
        || metadata::elements(&report, "ModuleReport")?.len() != 1
    {
        return Err(Error::Invalid(
            "unsupported decrypted metadata roots".into(),
        ));
    }
    let source_model = metadata::find_element(&repo, "Model", "blob", &spec.source_blob)?
        .ok_or_else(|| Error::Invalid("stock source model is absent".into()))?;
    let source_id = required_attr(source_model.text, "id")?;
    let source_report = metadata::find_element(&report, "Module", "id", &source_id)?
        .ok_or_else(|| Error::Invalid("stock source resource report is absent".into()))?;
    if !decimal(&source_id)
        || required_attr(source_report.text, "blob_name")? != spec.source_blob
        || required_attr(source_report.text, "category")? != spec.category
    {
        return Err(Error::Invalid(
            "stock model/report identity or category differs".into(),
        ));
    }
    let category = metadata::find_element(&repo, "Category", "id", &spec.category)?
        .ok_or_else(|| Error::Invalid("source category is absent".into()))?;
    if source_model.range.start < category.range.start
        || source_model.range.end > category.range.end
    {
        return Err(Error::Invalid(
            "stock model is not in the requested category".into(),
        ));
    }
    // Inserting beside the original keeps the category and every unrelated byte.
    // Cross-category moves are deliberately outside this stock-clone API.
    let new_repo = metadata::clone_element(
        &repo,
        "Model",
        ("blob", &spec.source_blob),
        &[
            ("id", &spec.model_id),
            ("blob", &spec.blob),
            ("name", &spec.name),
        ],
        &["id", "blob"],
    )?;
    let new_report = metadata::clone_element(
        &report,
        "Module",
        ("id", &source_id),
        &[
            ("id", &spec.model_id),
            ("blob_name", &spec.blob),
            ("name", &spec.name),
            ("category", &spec.category),
        ],
        &["id", "blob_name"],
    )?;
    let mut updated = BTreeMap::from([
        (
            "ModelRepo.xml".to_owned(),
            crypt(new_repo.as_bytes(), secrets.repo),
        ),
        (
            "blobs/moduleReport.xml".to_owned(),
            crypt(new_report.as_bytes(), secrets.report),
        ),
        (target_blob.clone(), crypt(&raw, secrets.blob)),
    ]);
    for core in 2..=4 {
        let path = format!("blobs/core{core}/diffs.blob");
        let mut diffs = DiffFile::parse(&before[&path])?;
        if diffs.to_bytes()? != before[&path] {
            return Err(Error::Invalid(format!(
                "stock diff file does not round-trip: {path}"
            )));
        }
        if diffs.get(&spec.blob).is_some() {
            return Err(Error::Invalid(format!("clone diff already exists: {path}")));
        }
        let mut record = diffs
            .get(&spec.source_blob)
            .cloned()
            .ok_or_else(|| Error::Invalid(format!("stock diff record is absent: {path}")))?;
        let variant = record.apply(&module)?;
        variant.validate()?;
        record.name = spec.blob.clone();
        if record.apply(&module)?.to_bytes()? != variant.to_bytes()? {
            return Err(Error::Invalid("renaming changed the core variant".into()));
        }
        diffs.upsert(record);
        let bytes = diffs.to_bytes()?;
        let parsed = DiffFile::parse(&bytes)?;
        if parsed
            .get(&spec.blob)
            .ok_or_else(|| Error::Invalid("clone diff disappeared".into()))?
            .apply(&module)?
            .to_bytes()?
            != variant.to_bytes()?
        {
            return Err(Error::Invalid(
                "serialized clone delta does not reconstruct".into(),
            ));
        }
        updated.insert(path, bytes);
    }
    let mut source_sha256: BTreeMap<_, _> = before
        .iter()
        .map(|(path, bytes)| (path.clone(), sha256(bytes)))
        .collect();
    source_sha256.insert(source_blob_path.clone(), sha256(&source_blob_encrypted));
    let mut changes = BTreeMap::new();
    for (path, bytes) in &updated {
        changes.insert(
            path.clone(),
            Change {
                before_sha256: before.get(path).map(|b| sha256(b)),
                after_sha256: sha256(bytes),
                mode: fs::mode(
                    &snapshot.root,
                    if path == &target_blob {
                        &source_blob_path
                    } else {
                        path
                    },
                )?,
            },
        );
    }
    let manifest = CloneManifest {
        format: 1,
        kind: "stock-clone".into(),
        firmware: snapshot.pin.clone(),
        clone: spec,
        source_sha256,
        changes,
    };
    manifest.validate()?;
    let prepared = PreparedClone {
        source_root: snapshot.root.clone(),
        manifest,
        before,
        updated,
    };
    prepared.verify_source(snapshot)?;
    Ok(prepared)
}
