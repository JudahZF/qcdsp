use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use qc_deploy::{
    CloneSpec, DeviceOptions, DeviceStage, Error, FakeCommandRunner, FakeTransport, FirmwarePin,
    GeneratedBlock, InstallOutcome, RUNTIME_FILES, RemoteOperation, Secrets, Snapshot, Transport,
    clone_stock, install, plan_install, plan_uninstall, preflight, restore_copy, sha256,
    stage_generated, uninstall, validate_stage,
};
use qc_format::{
    blob::{LinkRange, ModuleBlob, Section, SectionBytes},
    crypto::crypt,
    diff::{DiffFile, DiffRecord},
    metadata,
};

static ID: AtomicU64 = AtomicU64::new(0);
const SERIAL: &str = "QC0000001";
const REPO: &str = "<?xml version='1.0'?>\n<Models><!--unchanged--><Category id='16'><Model id='7' blob='volume' name='Volume'><Parameter name='LEVEL'/></Model><Model id='8' blob='other' name='Other'/></Category><Category id='17'/></Models>\n";
const REPORT: &str = "<?xml version='1.0'?>\n<ModuleReport><!--unchanged--><Module id='7' blob_name='volume' name='Volume' category='16' dm_heap='64' cpu_pad='0.4'><Test cpu='0.1'/></Module></ModuleReport>\n";

fn secrets() -> Secrets<'static> {
    Secrets {
        repo: b"test repo",
        blob: b"test blob",
        report: b"test report",
    }
}

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o640)).unwrap();
    }
}

fn module() -> ModuleBlob {
    ModuleBlob {
        contents: vec![SectionBytes {
            section: Section::Code,
            bytes: vec![0; 6],
        }],
        relocations: vec![],
        ranges: vec![LinkRange::for_contents(Section::Code, 6).unwrap()],
        entry: Section::Code.base(),
    }
}

struct Fixture {
    root: PathBuf,
    fake: PathBuf,
    tree: PathBuf,
    pin: FirmwarePin,
    originals: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "qc-device-fake-{}-{}",
            std::process::id(),
            ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fake = root.join("fake device's tree");
        let tree = fake.join("opt/neuraldsp");
        fs::create_dir_all(&tree).unwrap();
        write(&fake, "etc/qc_sn", format!("{SERIAL}\n").as_bytes());
        let mut originals = BTreeMap::from([
            ("ZenUI".into(), b"synthetic ZenUI".to_vec()),
            (
                "ModelRepo.xml".into(),
                crypt(REPO.as_bytes(), secrets().repo),
            ),
            (
                "blobs/moduleReport.xml".into(),
                crypt(REPORT.as_bytes(), secrets().report),
            ),
            (
                "blobs/core1/volume_blob.bin".into(),
                crypt(&module().to_bytes().unwrap(), secrets().blob),
            ),
        ]);
        let mut runtime_sha256 = BTreeMap::new();
        for name in RUNTIME_FILES {
            let bytes = format!("synthetic {name}").into_bytes();
            runtime_sha256.insert(name.into(), sha256(&bytes));
            originals.insert(format!("sharc_binaries/{name}"), bytes);
        }
        for core in 2..=4 {
            let mut file = DiffFile::default();
            file.upsert(DiffRecord::between("volume", &module(), &module()).unwrap());
            originals.insert(
                format!("blobs/core{core}/diffs.blob"),
                file.to_bytes().unwrap(),
            );
        }
        for (path, bytes) in &originals {
            write(&tree, path, bytes);
        }
        let pin = FirmwarePin {
            zenui_sha256: sha256(&originals["ZenUI"]),
            runtime_sha256,
        };
        Self {
            root,
            fake,
            tree,
            pin,
            originals,
        }
    }

    fn block(&self) -> GeneratedBlock {
        let mut base = module();
        base.contents[0].bytes[0] = 21;
        let diffs = std::array::from_fn(|index| {
            let mut variant = base.clone();
            variant.contents[0].bytes[1] = u8::try_from(index + 2).unwrap();
            DiffRecord::between("fake_drive", &base, &variant).unwrap()
        });
        GeneratedBlock {
            blob_name: "fake_drive".into(), model_id: "9001".into(), category: "16".into(),
            core1_plain: base.to_bytes().unwrap(), diffs,
            model_element: "<Model id='9001' blob='fake_drive' name='Drive'><Parameter name='LEVEL'/></Model>".into(),
            module_element: "<Module id='9001' blob_name='fake_drive' name='Drive' category='16' dm_heap='64' cpu_pad='0.4'><Test cpu='0.2'/></Module>".into(),
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot::open(&self.tree, self.pin.clone()).unwrap()
    }
    fn stage(&self) -> DeviceStage {
        stage_generated(
            &self.snapshot(),
            secrets(),
            &self.block(),
            &self.root.join("stage"),
        )
        .unwrap();
        DeviceStage::open(self.root.join("stage")).unwrap()
    }
    fn transport(&self) -> FakeTransport {
        FakeTransport::with_runner("fake.invalid", FakeCommandRunner::new(&self.fake).unwrap())
            .and_then(|transport| transport.with_proxy_command("python3 proxy.py %h %p"))
            .unwrap()
    }
    fn opts(&self) -> DeviceOptions {
        DeviceOptions {
            token: "synthetic_test".into(),
            expected_serial: SERIAL.into(),
            backup_directory: self.root.join("backup"),
            dry_run: false,
            confirm_device_write: true,
        }
    }
    fn assert_original(&self, blob: &str) {
        for (path, bytes) in &self.originals {
            assert_eq!(&fs::read(self.tree.join(path)).unwrap(), bytes, "{path}");
        }
        assert!(
            !self
                .tree
                .join(format!("blobs/core1/{blob}_blob.bin"))
                .exists()
        );
    }
    fn assert_installed(&self, stage: &DeviceStage) {
        for (path, change) in &stage.manifest().changes {
            assert_eq!(
                sha256(&fs::read(self.tree.join(path)).unwrap()),
                change.after_sha256,
                "{path}"
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn generated_stage_preserves_metadata_and_applies_restores_locally() {
    let f = Fixture::new();
    let snapshot = f.snapshot();
    let block = f.block();
    let stage_path = f.root.join("stage");
    let prepared = stage_generated(&snapshot, secrets(), &block, &stage_path).unwrap();
    assert_eq!(prepared.manifest().kind, "generated-block");
    assert_eq!(
        prepared.manifest().resources_computed_by.as_deref(),
        Some("qcdsp")
    );
    f.assert_original("fake_drive");
    assert!(validate_stage(&snapshot, &stage_path, secrets()).is_err());
    let repo = String::from_utf8(crypt(
        &fs::read(stage_path.join("files/ModelRepo.xml")).unwrap(),
        secrets().repo,
    ))
    .unwrap();
    assert_eq!(repo.replace(&block.model_element, ""), REPO);
    assert!(repo.contains(&format!("{}</Category>", block.model_element)));
    let report = String::from_utf8(crypt(
        &fs::read(stage_path.join("files/blobs/moduleReport.xml")).unwrap(),
        secrets().report,
    ))
    .unwrap();
    assert_eq!(report.replace(&block.module_element, ""), REPORT);
    assert_eq!(
        crypt(
            &fs::read(stage_path.join("files/blobs/core1/fake_drive_blob.bin")).unwrap(),
            secrets().blob
        ),
        block.core1_plain
    );
    for core in 2..=4 {
        let path = format!("blobs/core{core}/diffs.blob");
        let diff =
            DiffFile::parse(&fs::read(stage_path.join(format!("files/{path}"))).unwrap()).unwrap();
        let old = DiffFile::parse(&f.originals[&path]).unwrap();
        assert_eq!(diff.get("volume"), old.get("volume"));
        let base = ModuleBlob::parse(&block.core1_plain).unwrap();
        let variant = diff.get("fake_drive").unwrap().apply(&base).unwrap();
        assert_eq!(variant.contents[0].bytes[1], core);
    }
    let stage = DeviceStage::open(&stage_path).unwrap();
    prepared
        .apply_to_copy(&snapshot, f.root.join("local-backup"))
        .unwrap();
    f.assert_installed(&stage);
    restore_copy(&snapshot, f.root.join("local-backup")).unwrap();
    f.assert_original("fake_drive");
    restore_copy(&snapshot, f.root.join("local-backup")).unwrap();
}

#[test]
fn generated_inputs_refuse_identity_collisions_wrong_xml_diffs_and_category() {
    let f = Fixture::new();
    let snapshot = f.snapshot();
    for (index, mutation) in (0..7).enumerate() {
        let mut block = f.block();
        match mutation {
            0 => {
                block.model_id = "0007".into();
                block.model_element = block.model_element.replace("9001", "0007");
                block.module_element = block.module_element.replace("9001", "0007");
            }
            1 => {
                block.blob_name = "volume".into();
                block.model_element = block.model_element.replace("fake_drive", "volume");
                block.module_element = block.module_element.replace("fake_drive", "volume");
            }
            2 => {
                block.category = "99".into();
                block.module_element = block
                    .module_element
                    .replace("category='16'", "category='99'");
            }
            3 => block.model_element.push_str("<Model id='10'/>"),
            4 => block.module_element = "<Wrong/>".into(),
            5 => block.diffs[0].name = "wrong".into(),
            _ => block.core1_plain.clear(),
        }
        let stage = f.root.join(format!("bad-stage-{index}"));
        assert!(stage_generated(&snapshot, secrets(), &block, &stage).is_err());
        assert!(!stage.exists());
        f.assert_original("fake_drive");
    }
    let mut block = f.block();
    block.category = "17".into();
    block.module_element = block
        .module_element
        .replace("category='16'", "category='17'");
    stage_generated(&snapshot, secrets(), &block, &f.root.join("empty-category")).unwrap();
    let xml = String::from_utf8(crypt(
        &fs::read(f.root.join("empty-category/files/ModelRepo.xml")).unwrap(),
        secrets().repo,
    ))
    .unwrap();
    let category = metadata::find_element(&xml, "Category", "id", "17")
        .unwrap()
        .unwrap();
    assert!(category.text.contains(&block.model_element));
}

#[test]
fn fake_transport_installs_and_uninstalls_generated_and_stock_stages() {
    for stock in [false, true] {
        let f = Fixture::new();
        let stage = if stock {
            let clone = clone_stock(
                &f.snapshot(),
                secrets(),
                CloneSpec::volume("9002", "Stock copy", "stock_copy"),
            )
            .unwrap();
            clone.write_stage(f.root.join("stage")).unwrap();
            DeviceStage::open(f.root.join("stage")).unwrap()
        } else {
            f.stage()
        };
        let blob = stage.manifest().blob.clone();
        let mut transport = f.transport();
        let opts = f.opts();
        let facts = preflight(&mut transport, &stage, &f.pin).unwrap();
        assert_eq!(facts.serial, SERIAL);
        assert!(facts.target_listing.contains("ModelRepo.xml"));
        assert!(matches!(
            install(&mut transport, &stage, &f.pin, &opts).unwrap(),
            InstallOutcome::Installed { .. }
        ));
        f.assert_installed(&stage);
        let remote = f.tree.join(".qc-block-synthetic_test");
        for (path, change) in &stage.manifest().changes {
            if change.before_sha256.is_some() {
                assert_eq!(
                    fs::read(remote.join(format!("before/{path}"))).unwrap(),
                    f.originals[path]
                );
                assert_eq!(
                    fs::read(opts.backup_directory.join(format!("before/{path}"))).unwrap(),
                    f.originals[path]
                );
            }
        }
        assert!(matches!(
            uninstall(&mut transport, &f.pin, &opts).unwrap(),
            InstallOutcome::Uninstalled { .. }
        ));
        f.assert_original(&blob);
        uninstall(&mut transport, &f.pin, &opts).unwrap();
        assert!(remote.join("receipt.json").exists());
        assert!(
            transport
                .runner()
                .commands
                .iter()
                .all(|r| r.program == "ssh"
                    && r.arguments[1] == "ProxyCommand=python3 proxy.py %h %p")
        );
    }
}

#[test]
fn dry_runs_and_missing_confirmation_execute_nothing() {
    let f = Fixture::new();
    let stage = f.stage();
    let mut opts = f.opts();
    opts.confirm_device_write = false;
    let mut transport = f.transport();
    assert!(matches!(
        install(&mut transport, &stage, &f.pin, &opts),
        Err(Error::DeviceWriteNotConfirmed)
    ));
    opts.dry_run = true;
    let outcome = install(&mut transport, &stage, &f.pin, &opts).unwrap();
    let InstallOutcome::DryRun { commands } = outcome else {
        panic!("expected dry run")
    };
    assert!(
        commands
            .iter()
            .any(|c| c.contains("ProxyCommand=python3 proxy.py %h %p"))
    );
    assert!(commands.iter().any(|c| c.contains("openssl dgst -sha256")));
    assert!(commands.iter().any(|c| c.contains("stdin:")));
    assert!(!commands.iter().any(|c| c.contains("reboot")
        || c.contains("sha256sum")
        || c.contains(" dd ")
        || c.contains(" head ")));
    assert!(transport.runner().commands.is_empty());
    assert!(!opts.backup_directory.exists());
    assert!(!f.tree.join(".qc-block-synthetic_test").exists());
    f.assert_original("fake_drive");
    opts.dry_run = false;
    opts.confirm_device_write = true;
    install(&mut transport, &stage, &f.pin, &opts).unwrap();
    let count = transport.runner().commands.len();
    opts.confirm_device_write = false;
    assert!(matches!(
        uninstall(&mut transport, &f.pin, &opts),
        Err(Error::DeviceWriteNotConfirmed)
    ));
    opts.dry_run = true;
    uninstall(&mut transport, &f.pin, &opts).unwrap();
    assert_eq!(transport.runner().commands.len(), count);
    f.assert_installed(&stage);
}

#[test]
fn remote_hash_mismatch_aborts_before_first_live_rename() {
    let f = Fixture::new();
    let stage = f.stage();
    struct Corrupt<'a> {
        transport: FakeTransport,
        root: &'a Path,
        done: bool,
    }
    impl Transport for Corrupt<'_> {
        fn describe(&self, operation: &RemoteOperation) -> qc_deploy::Result<Vec<String>> {
            self.transport.describe(operation)
        }
        fn execute(&mut self, operation: &RemoteOperation) -> qc_deploy::Result<Vec<u8>> {
            let output = self.transport.execute(operation)?;
            if !self.done
                && let RemoteOperation::Upload { path, .. } = operation
            {
                let relative = path.strip_prefix("/opt/neuraldsp/").unwrap();
                fs::write(self.root.join(relative), b"corrupt upload").unwrap();
                self.done = true;
            }
            Ok(output)
        }
    }
    let mut transport = Corrupt {
        transport: f.transport(),
        root: &f.tree,
        done: false,
    };
    assert!(install(&mut transport, &stage, &f.pin, &f.opts()).is_err());
    f.assert_original("fake_drive");
    assert!(f.opts().backup_directory.join("receipt.json").exists());
}

#[test]
fn firmware_and_serial_mismatch_refuse_all_writes() {
    for runtime in [false, true] {
        let f = Fixture::new();
        let stage = f.stage();
        let path = if runtime {
            format!("sharc_binaries/{}", RUNTIME_FILES[0])
        } else {
            "ZenUI".into()
        };
        write(&f.tree, &path, b"unknown firmware");
        let mut transport = f.transport();
        assert!(matches!(
            install(&mut transport, &stage, &f.pin, &f.opts()),
            Err(Error::Firmware(_))
        ));
        assert!(!f.opts().backup_directory.exists());
        assert!(!f.tree.join(".qc-block-synthetic_test").exists());
    }
    let f = Fixture::new();
    let stage = f.stage();
    let mut opts = f.opts();
    opts.expected_serial = "QC0000002".into();
    assert!(matches!(
        install(&mut f.transport(), &stage, &f.pin, &opts),
        Err(Error::Changed(_))
    ));
    assert!(!opts.backup_directory.exists());
    let mut wrong_pin = f.pin.clone();
    wrong_pin.zenui_sha256 = sha256(b"different pin");
    assert!(matches!(
        plan_install(&stage, &wrong_pin, &f.opts()),
        Err(Error::Firmware(_))
    ));
}

fn command_count(transport: &FakeTransport, ops: &[RemoteOperation]) -> usize {
    ops.iter()
        .map(|op| transport.describe(op).unwrap().len())
        .sum()
}

#[test]
fn partial_install_and_restore_failures_recover_from_retained_backups() {
    let f = Fixture::new();
    let stage = f.stage();
    let opts = f.opts();
    let mut transport = f.transport();
    let plan = plan_install(&stage, &f.pin, &opts).unwrap();
    let before_publish = command_count(&transport, &plan.preflight)
        + command_count(&transport, &plan.downloads)
        + command_count(&transport, &plan.prepare);
    transport.runner_mut().fail_at = Some(before_publish + 2);
    assert!(matches!(
        install(&mut transport, &stage, &f.pin, &opts),
        Err(Error::DevicePublish { .. })
    ));
    assert!(f.tree.join("blobs/core1/fake_drive_blob.bin").exists());
    assert_eq!(
        fs::read(f.tree.join("ModelRepo.xml")).unwrap(),
        f.originals["ModelRepo.xml"]
    );
    let plan = plan_uninstall(&f.pin, &opts).unwrap();
    let before_restore = transport.runner().commands.len()
        + command_count(&transport, &plan.preflight)
        + command_count(&transport, &plan.prepare);
    transport.runner_mut().fail_at = Some(before_restore + 5);
    assert!(matches!(
        uninstall(&mut transport, &f.pin, &opts),
        Err(Error::DevicePublish { .. })
    ));
    uninstall(&mut transport, &f.pin, &opts).unwrap();
    f.assert_original("fake_drive");
}

#[test]
fn uninstall_refuses_concurrent_edits_permissions_and_corrupt_backups() {
    for mutation in 0..4 {
        let f = Fixture::new();
        let stage = f.stage();
        let opts = f.opts();
        let mut transport = f.transport();
        install(&mut transport, &stage, &f.pin, &opts).unwrap();
        match mutation {
            0 => {
                fs::write(f.tree.join("ModelRepo.xml"), b"concurrent edit").unwrap();
            }
            1 => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(
                        f.tree.join("ModelRepo.xml"),
                        fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                }
            }
            2 => {
                fs::write(
                    opts.backup_directory.join("before/ModelRepo.xml"),
                    b"corrupt local backup",
                )
                .unwrap();
            }
            _ => {
                fs::write(
                    f.tree.join(".qc-block-synthetic_test/before/ModelRepo.xml"),
                    b"corrupt remote backup",
                )
                .unwrap();
            }
        }
        let unchanged = fs::read(f.tree.join("blobs/moduleReport.xml")).unwrap();
        assert!(uninstall(&mut transport, &f.pin, &opts).is_err());
        assert_eq!(
            fs::read(f.tree.join("blobs/moduleReport.xml")).unwrap(),
            unchanged
        );
        assert!(f.tree.join("blobs/core1/fake_drive_blob.bin").exists());
    }
}

#[test]
fn upload_chunks_are_bounded_and_partial_restore_temporary_can_be_retried() {
    let f = Fixture::new();
    let mut transport = f.transport();
    transport
        .execute(&RemoteOperation::CreateStage {
            token: "chunks".into(),
        })
        .unwrap();
    let data = vec![39; 300 * 1024];
    let path = "/opt/neuraldsp/.qc-block-chunks/files/ModelRepo.xml".to_owned();
    transport
        .execute(&RemoteOperation::Upload {
            path: path.clone(),
            bytes: data.clone(),
        })
        .unwrap();
    assert_eq!(
        fs::read(f.tree.join(".qc-block-chunks/files/ModelRepo.xml")).unwrap(),
        data
    );
    let uploads: Vec<_> = transport
        .runner()
        .commands
        .iter()
        .filter(|c| !c.stdin.is_empty())
        .collect();
    assert_eq!(uploads.len(), 3);
    assert!(uploads.iter().all(|c| c.stdin.len() <= 128 * 1024));
    let stage = f.stage();
    let opts = f.opts();
    install(&mut transport, &stage, &f.pin, &opts).unwrap();
    write(
        &f.tree,
        ".qc-block-synthetic_test/restore/ModelRepo.xml",
        b"interrupted temporary copy",
    );
    uninstall(&mut transport, &f.pin, &opts).unwrap();
    f.assert_original("fake_drive");
}
