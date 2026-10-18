//! Opt-in: build every example against the user's decrypted stock Volume.
//! `QC_CORPUS_DIR` points at `research/device/plain`; no vendor bytes are stored here.

use std::{fs, path::PathBuf};

use qc_format::{
    blob::{ModuleBlob, Section},
    diff::DiffFile,
};
use qcdsp::{
    device::{self, VolumeTemplate},
    examples,
};

fn template() -> Option<VolumeTemplate> {
    let corpus = PathBuf::from(std::env::var_os("QC_CORPUS_DIR")?);
    let core1 = fs::read(corpus.join("blobs/core1/volume.raw")).unwrap();
    let diffs = [2, 3, 4].map(|core| {
        let path = format!("blobs/core{core}/diffs.blob");
        let path = if corpus.join(&path).is_file() {
            corpus.join(&path)
        } else {
            corpus.parent().unwrap().join(&path)
        };
        DiffFile::parse(&fs::read(path).unwrap())
            .unwrap()
            .get("volume")
            .unwrap()
            .clone()
    });
    Some(VolumeTemplate::new(&core1, diffs).unwrap())
}

#[test]
fn every_example_links_into_a_valid_blob_with_per_core_records() {
    let Some(template) = template() else {
        eprintln!("skipped: set QC_CORPUS_DIR");
        return;
    };
    let blocks = [
        examples::gain().unwrap(),
        examples::soft_clip().unwrap(),
        examples::delay(480).unwrap(),
        examples::biquad([0.06745527, 0.13491054, 0.06745527, -1.1429805, 0.4128016]).unwrap(),
        examples::tremolo().unwrap(),
        examples::envelope_filter().unwrap(),
        examples::micro_synth(examples::Voicing::Guitar).unwrap(),
        examples::micro_synth(examples::Voicing::Bass).unwrap(),
    ];
    for (index, block) in blocks.iter().enumerate() {
        let built =
            device::build(block, &template, 4_000_000_000 + index as u32, block.name()).unwrap();
        let bytes = built.core1.to_bytes().unwrap();
        let parsed = ModuleBlob::parse(&bytes).unwrap();
        parsed.validate().unwrap();
        for diff in &built.diffs {
            assert_eq!(diff.name, built.blob_name);
            assert!(built.blob_name.starts_with(device::BLOB_PREFIX));
            diff.apply(&parsed).unwrap().validate().unwrap();
        }
        let code = parsed.content(Section::Code).unwrap();
        assert!(code.len() > 0x27a * 2);
        assert!(built.resources.state_bytes <= 0x7fff);
        assert!(
            built
                .module_element
                .contains(&format!("sw=\"{}\"", code.len() + 256))
        );
        if let Some(out) = std::env::var_os("QCDSP_DUMP_DIR") {
            fs::write(
                PathBuf::from(out).join(format!("{}.raw", built.blob_name)),
                &bytes,
            )
            .unwrap();
        }
        eprintln!(
            "{}: {} code bytes, {} state bytes, cpu {:.3}",
            block.name(),
            code.len(),
            built.resources.state_bytes,
            built.resources.cpu()
        );
    }
}
