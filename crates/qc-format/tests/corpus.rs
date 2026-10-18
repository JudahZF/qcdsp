//! Opt-in round trips over the user's own decrypted stock files.
//!
//! Set `QC_CORPUS_DIR` to the corpus root (`plain/`, `rootfs/`, `presets/`, as in Tetrad).
//! `plain/` must hold `blobs/core1/*.raw`. Diff files are read from
//! `plain/blobs/core{2,3,4}/diffs.blob`, or from `rootfs/opt/neuraldsp/blobs/` (they
//! are not encrypted, so they usually sit next to the decrypted tree).
//! Without the variable the tests skip.

use std::path::{Path, PathBuf};

use qc_format::blob::ModuleBlob;
use qc_format::diff::{DiffFile, DiffRecord};
use qc_format::metadata;

fn corpus() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("QC_CORPUS_DIR")?).join("plain");
    assert!(
        dir.join("blobs/core1").is_dir(),
        "QC_CORPUS_DIR has no plain/blobs/core1"
    );
    Some(dir)
}

fn core1_blob(dir: &Path, name: &str) -> (Vec<u8>, ModuleBlob) {
    let raw = std::fs::read(dir.join(format!("blobs/core1/{name}.raw"))).unwrap();
    let blob = ModuleBlob::parse(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
    (raw, blob)
}

fn diffs_path(dir: &Path, core: u32) -> PathBuf {
    let local = dir.join(format!("blobs/core{core}/diffs.blob"));
    if local.exists() {
        local
    } else {
        dir.parent()
            .unwrap()
            .join(format!("rootfs/opt/neuraldsp/blobs/core{core}/diffs.blob"))
    }
}

#[test]
fn stock_blobs_round_trip() {
    let Some(dir) = corpus() else {
        return eprintln!("QC_CORPUS_DIR not set; skipped");
    };
    let mut count = 0;
    for entry in std::fs::read_dir(dir.join("blobs/core1")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "raw") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap();
        qc_format::blob::validate_blob_name(name).unwrap();
        let (raw, blob) = core1_blob(&dir, name);
        blob.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
        for rel in &blob.relocations {
            match rel.section {
                qc_format::blob::Section::Dm => rel.data_relocations().map(drop),
                _ => rel.code_relocations().map(drop),
            }
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        assert!(blob.to_bytes().unwrap() == raw, "{name} did not round-trip");
        count += 1;
    }
    eprintln!("{count} stock blobs round-tripped");
    assert!(count > 0);
}

#[test]
fn stock_diffs_round_trip_and_apply() {
    let Some(dir) = corpus() else {
        return eprintln!("QC_CORPUS_DIR not set; skipped");
    };
    for core in [2, 3, 4] {
        let path = diffs_path(&dir, core);
        let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let file = DiffFile::parse(&raw).unwrap_or_else(|e| panic!("core{core}: {e}"));
        assert!(
            file.to_bytes().unwrap() == raw,
            "core{core} diffs did not round-trip"
        );

        for record in &file.records {
            assert!(DiffRecord::parse(&record.to_bytes().unwrap()).unwrap() == *record);
            let (_, core1) = core1_blob(&dir, &record.name);
            let core_n = record
                .apply(&core1)
                .unwrap_or_else(|e| panic!("core{core} {}: {e}", record.name));
            core_n.validate().unwrap();
            let rebuilt = DiffRecord::between(&record.name, &core1, &core_n).unwrap();
            assert!(
                rebuilt.apply(&core1).unwrap() == core_n,
                "core{core} {}",
                record.name
            );
        }
        eprintln!(
            "core{core}: {} records round-tripped and applied",
            file.records.len()
        );
    }
}

#[test]
fn stock_metadata_clone_and_remove_is_lossless() {
    let Some(dir) = corpus() else {
        return eprintln!("QC_CORPUS_DIR not set; skipped");
    };
    for (file, tag, blob_attr) in [
        ("ModelRepo.xml", "Model", "blob"),
        ("moduleReport.xml", "Module", "blob_name"),
    ] {
        let Ok(doc) = std::fs::read_to_string(dir.join(file)) else {
            continue;
        };
        let all = metadata::elements(&doc, tag).unwrap();
        assert!(!all.is_empty(), "{file} has no <{tag}>");
        let id = metadata::attr(all[0].text, "id").unwrap().unwrap();
        let edits = [
            ("id", "999999"),
            (blob_attr, "qcdsp_corpus_probe"),
            ("name", "Probe"),
        ];
        let cloned =
            metadata::clone_element(&doc, tag, ("id", &id), &edits, &["id", blob_attr]).unwrap();
        assert_eq!(
            metadata::elements(&cloned, tag).unwrap().len(),
            all.len() + 1
        );
        assert!(
            metadata::remove_element(&cloned, tag, ("id", "999999")).unwrap() == doc,
            "{file}"
        );
        eprintln!(
            "{file}: {} <{tag}> elements; clone/remove lossless",
            all.len()
        );
    }
}
