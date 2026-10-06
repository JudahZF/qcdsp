use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use qc_deploy::{
    CloneSpec, Error, FirmwarePin, RUNTIME_FILES, Secrets, Snapshot, clone_stock, restore_copy,
    sha256, validate_stage,
};
use qc_format::{
    blob::{LinkRange, ModuleBlob, Section, SectionBytes},
    crypto::crypt,
    diff::{DiffFile, DiffRecord},
    metadata,
};

static TEST_ID: AtomicU64 = AtomicU64::new(0);
const REPO_SECRET: &[u8] = b"synthetic repo test secret";
const BLOB_SECRET: &[u8] = b"synthetic blob test secret";
const REPORT_SECRET: &[u8] = b"synthetic report test secret";
const REPO: &str = "<?xml version='1.0'?>\n<Models><!--preserve me-->\n<Category id='16' name='Utility'>\n<Model id='7' blob='volume' name='Synthetic Volume'><Padding cpu='0.4' dm_hp='64' sw='256'/><Parameter name='LEVEL' type='float' defaultValue='100'/></Model>\n<Model id='8' blob='other' name='Other'/>\n</Category></Models>\n";
const REPORT: &str = "<?xml version='1.0'?>\n<ModuleReport>\n<Module id='7' blob_name='volume' name='Synthetic Volume' category='16' dm_heap='64' pm_heap='0' sd_heap='0' dma='0' pm='0' dm='0' sw='6' sw_pad='0' nw='0' cpu_pad='0.4' Latency='0'><Test cpu='0.1'/></Module>\n<Module id='8' blob_name='other' name='Other' category='16'/>\n</ModuleReport>\n";

fn secrets() -> Secrets<'static> {
    Secrets {
        repo: REPO_SECRET,
        blob: BLOB_SECRET,
        report: REPORT_SECRET,
    }
}

fn spec() -> CloneSpec {
    CloneSpec::volume("900", "Local & clone", "local_volume")
}

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn module() -> ModuleBlob {
    ModuleBlob {
        contents: vec![SectionBytes {
            section: Section::Code,
            bytes: vec![0, 0, 0, 0, 0, 0],
        }],
        relocations: vec![],
        ranges: vec![LinkRange::for_contents(Section::Code, 6).unwrap()],
        entry: Section::Code.base(),
    }
}

struct Fixture {
    directory: PathBuf,
    tree: PathBuf,
    pin: FirmwarePin,
    originals: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "qc-deploy-test-{}-{}",
            std::process::id(),
            TEST_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let tree = directory.join("copied-neuraldsp");
        fs::create_dir(&tree).unwrap();
        write(&tree, "ZenUI", b"synthetic firmware executable");
        let mut runtime_sha256 = BTreeMap::new();
        for name in RUNTIME_FILES {
            let bytes = format!("synthetic runtime {name}").into_bytes();
            write(&tree, &format!("sharc_binaries/{name}"), &bytes);
            runtime_sha256.insert(name.into(), sha256(&bytes));
        }
        let pin = FirmwarePin {
            zenui_sha256: sha256(b"synthetic firmware executable"),
            runtime_sha256,
        };
        let base = module();
        let mut originals = BTreeMap::from([
            ("ModelRepo.xml".into(), crypt(REPO.as_bytes(), REPO_SECRET)),
            (
                "blobs/moduleReport.xml".into(),
                crypt(REPORT.as_bytes(), REPORT_SECRET),
            ),
            (
                "blobs/core1/volume_blob.bin".into(),
                crypt(&base.to_bytes().unwrap(), BLOB_SECRET),
            ),
        ]);
        for core in 2..=4 {
            let mut variant = base.clone();
            variant.contents[0].bytes[0] = core;
            let mut diffs = DiffFile::default();
            diffs.upsert(DiffRecord::between("volume", &base, &variant).unwrap());
            diffs.upsert(DiffRecord::between("other", &base, &base).unwrap());
            originals.insert(
                format!("blobs/core{core}/diffs.blob"),
                diffs.to_bytes().unwrap(),
            );
        }
        for (path, bytes) in &originals {
            write(&tree, path, bytes);
        }
        Self {
            directory,
            tree,
            pin,
            originals,
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot::open(&self.tree, self.pin.clone()).unwrap()
    }

    fn assert_original(&self) {
        for (path, bytes) in &self.originals {
            assert_eq!(&fs::read(self.tree.join(path)).unwrap(), bytes, "{path}");
        }
        assert!(!self.tree.join("blobs/core1/local_volume_blob.bin").exists());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).unwrap();
    }
}

#[test]
fn stage_clone_apply_uninstall_preserves_source_and_resources() {
    let fixture = Fixture::new();
    let snapshot = fixture.snapshot();
    let clone = clone_stock(&snapshot, secrets(), spec()).unwrap();
    assert_eq!(clone.manifest().kind, "stock-clone");
    let stage = fixture.directory.join("stage");
    clone.write_stage(&stage).unwrap();
    fixture.assert_original();
    assert!(clone.write_stage(&stage).is_err());
    let clone = validate_stage(&snapshot, &stage, secrets()).unwrap();
    let repo = String::from_utf8(crypt(
        &fs::read(stage.join("files/ModelRepo.xml")).unwrap(),
        REPO_SECRET,
    ))
    .unwrap();
    assert!(repo.contains("<!--preserve me-->"));
    let old = metadata::find_element(&repo, "Model", "blob", "volume")
        .unwrap()
        .unwrap();
    assert!(REPO.contains(old.text));
    let new = metadata::find_element(&repo, "Model", "blob", "local_volume")
        .unwrap()
        .unwrap();
    assert_eq!(
        metadata::attr(new.text, "name").unwrap().as_deref(),
        Some("Local & clone")
    );
    assert!(
        new.text
            .contains("<Padding cpu='0.4' dm_hp='64' sw='256'/><Parameter")
    );
    let report = String::from_utf8(crypt(
        &fs::read(stage.join("files/blobs/moduleReport.xml")).unwrap(),
        REPORT_SECRET,
    ))
    .unwrap();
    let new_report = metadata::find_element(&report, "Module", "blob_name", "local_volume")
        .unwrap()
        .unwrap();
    let old_report = metadata::find_element(REPORT, "Module", "blob_name", "volume")
        .unwrap()
        .unwrap();
    assert_eq!(
        new_report.text,
        metadata::set_attrs(
            old_report.text,
            &[
                ("id", "900"),
                ("blob_name", "local_volume"),
                ("name", "Local & clone"),
                ("category", "16")
            ]
        )
        .unwrap()
    );
    for core in 2..=4 {
        let path = format!("blobs/core{core}/diffs.blob");
        let staged = fs::read(stage.join(format!("files/{path}"))).unwrap();
        // Appending the renamed diff must keep the entire old file prefix.
        assert!(staged.starts_with(&fixture.originals[&path]));
        let diffs = DiffFile::parse(&staged).unwrap();
        assert_eq!(
            diffs.get("volume").unwrap().apply(&module()).unwrap(),
            diffs.get("local_volume").unwrap().apply(&module()).unwrap()
        );
    }
    let backup = fixture.directory.join("backup");
    clone.apply_to_copy(&snapshot, &backup).unwrap();
    assert_eq!(
        fs::read(fixture.tree.join("blobs/core1/local_volume_blob.bin")).unwrap(),
        fixture.originals["blobs/core1/volume_blob.bin"]
    );
    restore_copy(&snapshot, &backup).unwrap();
    fixture.assert_original();
    restore_copy(&snapshot, &backup).unwrap(); // Safe retry, no missing-original assumptions.
}

#[test]
fn malformed_catalogue_and_invalid_clone_characters_are_never_staged() {
    let fixture = Fixture::new();
    let snapshot = fixture.snapshot();
    let mut invalid_name = spec();
    invalid_name.name.push('\u{ffff}');
    assert!(clone_stock(&snapshot, secrets(), invalid_name).is_err());
    for invalid_repo in [format!("{REPO}<extra/>"), format!("{REPO}trailing text")] {
        write(
            &fixture.tree,
            "ModelRepo.xml",
            &crypt(invalid_repo.as_bytes(), REPO_SECRET),
        );
        assert!(clone_stock(&snapshot, secrets(), spec()).is_err());
    }
    assert!(
        !fixture
            .tree
            .join("blobs/core1/local_volume_blob.bin")
            .exists()
    );
}

#[test]
fn repeated_report_blob_names_select_the_model_id() {
    let fixture = Fixture::new();
    let report = REPORT.replace(
        "</ModuleReport>",
        "<Module id='99' blob_name='volume' name='Different model' category='16'/>\n</ModuleReport>",
    );
    write(
        &fixture.tree,
        "blobs/moduleReport.xml",
        &crypt(report.as_bytes(), REPORT_SECRET),
    );
    let snapshot = fixture.snapshot();
    let clone = clone_stock(&snapshot, secrets(), spec()).unwrap();
    let stage = fixture.directory.join("stage");
    clone.write_stage(&stage).unwrap();
    let result = String::from_utf8(crypt(
        &fs::read(stage.join("files/blobs/moduleReport.xml")).unwrap(),
        REPORT_SECRET,
    ))
    .unwrap();
    assert!(
        result
            .contains("<Module id='99' blob_name='volume' name='Different model' category='16'/>")
    );
    let copied = metadata::find_element(&result, "Module", "id", "900")
        .unwrap()
        .unwrap();
    assert!(copied.text.contains("<Test cpu='0.1'/>"));
}

#[test]
fn unknown_or_changed_firmware_fails_closed() {
    let fixture = Fixture::new();
    let mut missing = fixture.pin.clone();
    missing.runtime_sha256.clear();
    assert!(matches!(
        Snapshot::open(&fixture.tree, missing),
        Err(Error::Firmware(_))
    ));
    let snapshot = fixture.snapshot();
    write(
        &fixture.tree,
        "sharc_binaries/ZPUKernel_Core1.ldr",
        b"different build",
    );
    assert!(matches!(
        clone_stock(&snapshot, secrets(), spec()),
        Err(Error::Firmware(_))
    ));
}

#[test]
fn source_change_or_identity_collision_refuses_apply() {
    let fixture = Fixture::new();
    let snapshot = fixture.snapshot();
    let mut collision = spec();
    collision.model_id = "8".into();
    assert!(clone_stock(&snapshot, secrets(), collision).is_err());
    let clone = clone_stock(&snapshot, secrets(), spec()).unwrap();
    write(&fixture.tree, "ModelRepo.xml", b"a concurrent change");
    let backup = fixture.directory.join("backup");
    assert!(matches!(
        clone.apply_to_copy(&snapshot, &backup),
        Err(Error::Changed(_))
    ));
    assert!(!backup.exists());
    assert_eq!(
        fs::read(fixture.tree.join("ModelRepo.xml")).unwrap(),
        b"a concurrent change"
    );
}

#[test]
fn edited_payload_and_even_rehashed_generated_blob_are_not_stock_clones() {
    let fixture = Fixture::new();
    let snapshot = fixture.snapshot();
    let clone = clone_stock(&snapshot, secrets(), spec()).unwrap();
    let stage = fixture.directory.join("stage");
    clone.write_stage(&stage).unwrap();
    let path = "files/blobs/core1/local_volume_blob.bin";
    let bytes = b"generated sample fragment, not installable";
    write(&stage, path, bytes);
    assert!(validate_stage(&snapshot, &stage, secrets()).is_err());
    let mut manifest = clone.manifest().clone();
    manifest
        .changes
        .get_mut("blobs/core1/local_volume_blob.bin")
        .unwrap()
        .after_sha256 = sha256(bytes);
    write(
        &stage,
        "manifest.json",
        &serde_json::to_vec(&manifest).unwrap(),
    );
    assert!(validate_stage(&snapshot, &stage, secrets()).is_err());
}

#[test]
fn uninstall_refuses_concurrent_edits_without_touching_other_files() {
    let fixture = Fixture::new();
    let snapshot = fixture.snapshot();
    let clone = clone_stock(&snapshot, secrets(), spec()).unwrap();
    let backup = fixture.directory.join("backup");
    clone.apply_to_copy(&snapshot, &backup).unwrap();
    let installed_repo = fs::read(fixture.tree.join("ModelRepo.xml")).unwrap();
    write(
        &fixture.tree,
        "blobs/core3/diffs.blob",
        b"another writer changed this",
    );
    assert!(matches!(
        restore_copy(&snapshot, &backup),
        Err(Error::Changed(_))
    ));
    assert_eq!(
        fs::read(fixture.tree.join("ModelRepo.xml")).unwrap(),
        installed_repo
    );
    assert_eq!(
        fs::read(fixture.tree.join("blobs/core3/diffs.blob")).unwrap(),
        b"another writer changed this"
    );
}

#[test]
fn corrupt_backup_refuses_restore_and_partial_publish_can_be_recovered() {
    let fixture = Fixture::new();
    let snapshot = fixture.snapshot();
    let clone = clone_stock(&snapshot, secrets(), spec()).unwrap();
    let backup = fixture.directory.join("backup");
    clone.apply_to_copy(&snapshot, &backup).unwrap();
    let original = fs::read(backup.join("before/ModelRepo.xml")).unwrap();
    write(&backup, "before/ModelRepo.xml", b"corrupt backup");
    assert!(restore_copy(&snapshot, &backup).is_err());
    assert!(
        fixture
            .tree
            .join("blobs/core1/local_volume_blob.bin")
            .exists()
    );
    write(&backup, "before/ModelRepo.xml", &original);
    // Simulate a publish interrupted before the catalogue was replaced.
    write(
        &fixture.tree,
        "ModelRepo.xml",
        &fixture.originals["ModelRepo.xml"],
    );
    restore_copy(&snapshot, &backup).unwrap();
    fixture.assert_original();
}

#[test]
fn paths_and_unexpected_bundle_entries_are_refused() {
    let fixture = Fixture::new();
    let snapshot = fixture.snapshot();
    let mut invalid = spec();
    invalid.blob = "../../elsewhere".into();
    assert!(clone_stock(&snapshot, secrets(), invalid).is_err());
    let clone = clone_stock(&snapshot, secrets(), spec()).unwrap();
    assert!(clone.write_stage(fixture.tree.join("stage")).is_err());
    let stage = fixture.directory.join("stage");
    clone.write_stage(&stage).unwrap();
    write(&stage, "unexpected.bin", b"extra");
    assert!(validate_stage(&snapshot, &stage, secrets()).is_err());
}

#[cfg(unix)]
#[test]
fn symlinks_hardlinks_and_permission_changes_are_not_hidden() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let snapshot = fixture.snapshot();
    fs::set_permissions(
        fixture.tree.join("ModelRepo.xml"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    let clone = clone_stock(&snapshot, secrets(), spec()).unwrap();
    let backup = fixture.directory.join("backup");
    clone.apply_to_copy(&snapshot, &backup).unwrap();
    fs::set_permissions(
        fixture.tree.join("ModelRepo.xml"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(matches!(
        restore_copy(&snapshot, &backup),
        Err(Error::Changed(_))
    ));
    fs::set_permissions(
        fixture.tree.join("ModelRepo.xml"),
        fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    restore_copy(&snapshot, &backup).unwrap();
    assert_eq!(
        fs::metadata(fixture.tree.join("ModelRepo.xml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    symlink(&fixture.tree, fixture.directory.join("alias")).unwrap();
    assert!(Snapshot::open(fixture.directory.join("alias"), fixture.pin.clone()).is_err());
    fs::hard_link(
        fixture.tree.join("ModelRepo.xml"),
        fixture.directory.join("linked-resource"),
    )
    .unwrap();
    assert!(clone_stock(&snapshot, secrets(), spec()).is_err());
}
