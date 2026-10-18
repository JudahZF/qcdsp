//! Opt-in stock-payload integration, using invented secrets and synthetic firmware.
//! No vendor payloads are stored in this crate. This does not test a real device or
//! certify that the synthetic firmware pin matches any vendor firmware build.

use std::{collections::BTreeMap, fs, path::PathBuf};

use qc_deploy::{
    CloneSpec, FirmwarePin, RUNTIME_FILES, Secrets, Snapshot, clone_stock, restore_copy, sha256,
    validate_stage,
};
use qc_format::crypto::crypt;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn stock_volume_payload_stages_applies_and_restores_on_a_private_copy() {
    let Some(corpus) =
        std::env::var_os("QC_CORPUS_DIR").map(|dir| PathBuf::from(dir).join("plain"))
    else {
        eprintln!("skipped stock-payload clone: set QC_CORPUS_DIR");
        return;
    };
    let scratch_path =
        std::env::temp_dir().join(format!("qcdsp-stock-clone-{}", std::process::id()));
    fs::create_dir(&scratch_path).unwrap();
    let scratch = Scratch(scratch_path);
    let root = scratch.0.join("copied-neuraldsp");
    fs::create_dir(&root).unwrap();
    for path in [
        "blobs/core1",
        "blobs/core2",
        "blobs/core3",
        "blobs/core4",
        "sharc_binaries",
    ] {
        fs::create_dir_all(root.join(path)).unwrap();
    }
    let secret = b"synthetic-secret";
    let secrets = Secrets {
        repo: secret,
        blob: secret,
        report: secret,
    };
    let mut originals = BTreeMap::new();
    for (source, destination) in [
        ("ModelRepo.xml", "ModelRepo.xml"),
        ("moduleReport.xml", "blobs/moduleReport.xml"),
        ("blobs/core1/volume.raw", "blobs/core1/volume_blob.bin"),
    ] {
        let bytes = crypt(&fs::read(corpus.join(source)).unwrap(), secret);
        fs::write(root.join(destination), &bytes).unwrap();
        originals.insert(destination.to_owned(), bytes);
    }
    for core in 2..=4 {
        let path = format!("blobs/core{core}/diffs.blob");
        let source = if corpus.join(&path).is_file() {
            corpus.join(&path)
        } else {
            corpus
                .parent()
                .unwrap()
                .join("rootfs/opt/neuraldsp")
                .join(&path)
        };
        let bytes = fs::read(source).unwrap();
        fs::write(root.join(&path), &bytes).unwrap();
        originals.insert(path, bytes);
    }
    let zenui = b"synthetic firmware identity for payload-only integration";
    fs::write(root.join("ZenUI"), zenui).unwrap();
    let mut runtime_sha256 = BTreeMap::new();
    for name in RUNTIME_FILES {
        fs::write(root.join("sharc_binaries").join(name), name.as_bytes()).unwrap();
        runtime_sha256.insert(name.to_owned(), sha256(name.as_bytes()));
    }
    let snapshot = Snapshot::open(
        &root,
        &[FirmwarePin {
            zenui_sha256: sha256(zenui),
            runtime_sha256,
        }],
    )
    .unwrap();
    let clone = clone_stock(
        &snapshot,
        secrets,
        CloneSpec::volume(
            "4294967000",
            "Volume (qcdsp corpus test)",
            "qcdsp_corpus_volume",
        ),
    )
    .unwrap();
    let stage = scratch.0.join("stage");
    clone.write_stage(&stage).unwrap();
    validate_stage(&snapshot, &stage, secrets).unwrap();
    for (path, bytes) in &originals {
        assert_eq!(fs::read(root.join(path)).unwrap(), *bytes);
    }
    let backup = scratch.0.join("backup");
    clone.apply_to_copy(&snapshot, &backup).unwrap();
    assert_eq!(
        fs::read(root.join("blobs/core1/qcdsp_corpus_volume_blob.bin")).unwrap(),
        originals["blobs/core1/volume_blob.bin"]
    );
    restore_copy(&snapshot, &backup).unwrap();
    for (path, bytes) in originals {
        assert_eq!(fs::read(root.join(&path)).unwrap(), bytes, "{path}");
    }
    assert!(
        !root
            .join("blobs/core1/qcdsp_corpus_volume_blob.bin")
            .exists()
    );
    eprintln!(
        "real stock volume/XML/per-core diffs cloned and restored byte-for-byte with synthetic secrets and firmware identity"
    );
}
