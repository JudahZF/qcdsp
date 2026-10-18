use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use qc_deploy::{
    FirmwarePin, GeneratedBlock, RUNTIME_FILES, Secrets, Snapshot, restore_copy, sha256,
    stage_generated,
};
use qc_format::{
    blob::{LinkRange, ModuleBlob, Section, SectionBytes},
    crypto::crypt,
    diff::{DiffFile, DiffRecord},
};

// A separate small fixture exercises generated local recovery without SSH or a
// command runner. Temporary directories are private copies, never device paths.
struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn write(root: &Path, name: &str, bytes: &[u8]) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn generated_local_recovery_refuses_edits_and_accepts_partial_original_states() {
    let root = std::env::temp_dir().join(format!("qc-generated-recovery-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let fixture = Fixture(root);
    let tree = fixture.0.join("copy");
    fs::create_dir(&tree).unwrap();
    let secrets = Secrets {
        repo: b"repo test",
        blob: b"blob test",
        report: b"report test",
    };
    let base = ModuleBlob {
        contents: vec![SectionBytes {
            section: Section::Code,
            bytes: vec![0; 6],
        }],
        relocations: vec![],
        ranges: vec![LinkRange::for_contents(Section::Code, 6).unwrap()],
        entry: Section::Code.base(),
    };
    let repo = crypt(
        b"<Models><Category id='16'><Model id='1' blob='stock'/></Category></Models>",
        secrets.repo,
    );
    let report = crypt(
        b"<ModuleReport><Module id='1' blob_name='stock'/></ModuleReport>",
        secrets.report,
    );
    write(&tree, "ModelRepo.xml", &repo);
    write(&tree, "blobs/moduleReport.xml", &report);
    write(&tree, "ZenUI", b"synthetic ZenUI");
    let mut runtime_sha256 = BTreeMap::new();
    for name in RUNTIME_FILES {
        write(&tree, &format!("sharc_binaries/{name}"), name.as_bytes());
        runtime_sha256.insert(name.into(), sha256(name.as_bytes()));
    }
    for core in 2..=4 {
        write(
            &tree,
            &format!("blobs/core{core}/diffs.blob"),
            &DiffFile::default().to_bytes().unwrap(),
        );
    }
    fs::create_dir_all(tree.join("blobs/core1")).unwrap();
    let pin = FirmwarePin {
        zenui_sha256: sha256(b"synthetic ZenUI"),
        runtime_sha256,
    };
    let snapshot = Snapshot::open(&tree, &[pin]).unwrap();
    let block = GeneratedBlock {
        blob_name: "generated".into(),
        model_id: "2".into(),
        category: "16".into(),
        core1_plain: base.to_bytes().unwrap(),
        diffs: std::array::from_fn(|_| DiffRecord::between("generated", &base, &base).unwrap()),
        model_element: "<Model id='2' blob='generated'/>".into(),
        module_element: "<Module id='2' blob_name='generated' category='16'/>".into(),
    };
    let stage = fixture.0.join("stage");
    let prepared = stage_generated(&snapshot, secrets, &block, &stage).unwrap();
    let backup = fixture.0.join("backup");
    prepared.apply_to_copy(&snapshot, &backup).unwrap();
    let installed_report = fs::read(tree.join("blobs/moduleReport.xml")).unwrap();
    write(&tree, "ModelRepo.xml", b"concurrent edit");
    assert!(restore_copy(&snapshot, &backup).is_err());
    assert_eq!(
        fs::read(tree.join("blobs/moduleReport.xml")).unwrap(),
        installed_report
    );
    assert!(tree.join("blobs/core1/generated_blob.bin").exists());
    // Simulate an already restored catalogue, but leave the other five files installed.
    write(&tree, "ModelRepo.xml", &repo);
    restore_copy(&snapshot, &backup).unwrap();
    assert_eq!(fs::read(tree.join("ModelRepo.xml")).unwrap(), repo);
    assert_eq!(
        fs::read(tree.join("blobs/moduleReport.xml")).unwrap(),
        report
    );
    assert!(!tree.join("blobs/core1/generated_blob.bin").exists());
    restore_copy(&snapshot, &backup).unwrap();
}
