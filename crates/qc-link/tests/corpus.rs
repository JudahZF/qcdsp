//! Opt-in checks against the user's own decrypted stock files.
//!
//! `QC_CORPUS_DIR` is the corpus root (as in Tetrad); `plain/` holds
//! `blobs/core1/*.raw` and `kernel_call_map.json`. Diff files are read from
//! `plain/blobs/core{2,3,4}/diffs.blob`, or from `rootfs/opt/neuraldsp/blobs/`.
//! Without the variable the tests skip.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use qc_format::blob::{ModuleBlob, Section};
use qc_format::diff::DiffFile;
use qc_link::{KernelForm, KernelMap, KernelRef, LinkError, LinkInput, link};

fn corpus() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("QC_CORPUS_DIR")?).join("plain");
    assert!(
        dir.join("blobs/core1").is_dir(),
        "QC_CORPUS_DIR has no plain/blobs/core1"
    );
    Some(dir)
}

fn stock_blobs(dir: &Path) -> Vec<(String, ModuleBlob)> {
    let mut out: Vec<_> = std::fs::read_dir(dir.join("blobs/core1"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "raw"))
        .map(|p| {
            let name = p.file_stem().unwrap().to_str().unwrap().to_string();
            let blob = ModuleBlob::parse(&std::fs::read(&p).unwrap()).unwrap();
            (name, blob)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Splits a stock blob back into linker input (no kernel references).
fn input_from(name: &str, blob: &ModuleBlob) -> LinkInput {
    let bytes = |s| blob.content(s).map(<[u8]>::to_vec).unwrap_or_default();
    let code_relocs = |s| {
        blob.relocations
            .iter()
            .find(|r| r.section == s)
            .map(|r| r.code_relocations().unwrap())
            .unwrap_or_default()
    };
    LinkInput {
        name: name.into(),
        data0: bytes(Section::Data0),
        dm: bytes(Section::Dm),
        code: bytes(Section::Code),
        isa: bytes(Section::Isa),
        dm_relocations: blob
            .relocations
            .iter()
            .find(|r| r.section == Section::Dm)
            .map(|r| r.data_relocations().unwrap())
            .unwrap_or_default(),
        code_relocations: code_relocs(Section::Code),
        isa_relocations: code_relocs(Section::Isa),
        entry_parcel: blob.entry - Section::Code.base(),
        kernel_refs: Vec::new(),
    }
}

/// Every stock blob whose relocations the linker accepts must re-link to
/// the identical bytes. The only allowed rejections are the undecoded
/// kinds 0, 5 and 9.
#[test]
fn relinks_stock_blobs_byte_for_byte() {
    let Some(dir) = corpus() else {
        return eprintln!("QC_CORPUS_DIR not set; skipped");
    };
    let (mut same, mut rejected) = (0, BTreeMap::<String, usize>::new());
    for (name, blob) in stock_blobs(&dir) {
        match link(&input_from(&name, &blob), &KernelMap::default()) {
            Ok(linked) => {
                assert!(linked.blob == blob, "{name} re-linked differently");
                assert!(
                    linked
                        .diffs
                        .iter()
                        .all(|d| d.contents.iter().all(|g| g.runs.is_empty()))
                );
                same += 1;
            }
            Err(LinkError::Relocation { reason, .. }) => {
                let key = reason.split(' ').take(2).collect::<Vec<_>>().join(" ");
                *rejected.entry(key).or_default() += 1;
            }
            Err(e) => panic!("{name}: {e}"),
        }
    }
    eprintln!(
        "{same} stock blobs re-linked identically; rejected by first relocation error: {rejected:?}"
    );
    assert!(same > 0);
    assert!(
        rejected
            .keys()
            .all(|k| ["kind 0", "kind 5", "kind 9"].contains(&k.as_str())),
        "stock relocations rejected for reasons other than undecoded kinds: {rejected:?}"
    );
}

/// For each stock CALL addr24 / Rn = imm32 site that a stock diff patches,
/// the linker's diff from the kernel map must give the stock core N bytes.
#[test]
fn kernel_map_reproduces_stock_absolute_patches() {
    let Some(dir) = corpus() else {
        return eprintln!("QC_CORPUS_DIR not set; skipped");
    };
    let Ok(text) = std::fs::read_to_string(dir.join("kernel_call_map.json")) else {
        return eprintln!("no kernel_call_map.json; skipped");
    };
    let kernel = KernelMap::from_json(&text).unwrap();
    let blobs: BTreeMap<_, _> = stock_blobs(&dir).into_iter().collect();
    let diff_files: Vec<DiffFile> = [2, 3, 4]
        .iter()
        .map(|core| {
            let local = dir.join(format!("blobs/core{core}/diffs.blob"));
            let path = if local.exists() {
                local
            } else {
                dir.parent()
                    .unwrap()
                    .join(format!("rootfs/opt/neuraldsp/blobs/core{core}/diffs.blob"))
            };
            DiffFile::parse(&std::fs::read(path).unwrap()).unwrap()
        })
        .collect();

    let mut sites = 0;
    for (name, blob) in &blobs {
        let code = blob.content(Section::Code).unwrap();
        let parcel = |p: usize| u16::from_le_bytes([code[2 * p], code[2 * p + 1]]);
        let stock: Vec<ModuleBlob> = diff_files
            .iter()
            .map(|f| f.get(name).unwrap().apply(blob).unwrap())
            .collect();

        // Candidate sites: a known opcode whose address field differs on some core.
        let mut refs = Vec::new();
        let mut p = 0;
        while p + 3 <= code.len() / 2 {
            let form = match parcel(p) {
                0x1804 => Some(KernelForm::Call24),
                op if op & 0xFFF0 == 0x0F00 => Some(KernelForm::Imm32),
                _ => None,
            };
            let field = 2 * p + 2..2 * p + 6;
            let patched = stock
                .iter()
                .any(|s| s.content(Section::Code).unwrap()[field.clone()] != code[field.clone()]);
            let address = (u32::from(parcel(p + 1)) << 16) | u32::from(parcel(p + 2));
            match form {
                Some(form) if patched && kernel.get(address).is_some() => {
                    refs.push(KernelRef {
                        parcel: p as u32,
                        form,
                        core1_address: address,
                    });
                    p += 3;
                }
                _ => p += 1,
            }
        }
        if refs.is_empty() {
            continue;
        }

        // Link just the code with these references and compare the fields.
        let input = LinkInput {
            name: name.clone(),
            code: code.to_vec(),
            kernel_refs: refs.clone(),
            ..LinkInput::default()
        };
        let linked = link(&input, &kernel).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            linked.blob.content(Section::Code).unwrap() == code,
            "{name}: core 1 code changed"
        );
        for (diff, stock_n) in linked.diffs.iter().zip(&stock) {
            let ours = diff.apply(&linked.blob).unwrap();
            let (ours, theirs) = (
                ours.content(Section::Code).unwrap(),
                stock_n.content(Section::Code).unwrap(),
            );
            for r in &refs {
                let field = r.parcel as usize * 2 + 2..r.parcel as usize * 2 + 6;
                assert_eq!(
                    ours[field.clone()],
                    theirs[field],
                    "{name} parcel {}",
                    r.parcel
                );
            }
        }
        sites += refs.len();
    }
    eprintln!("{sites} absolute kernel sites reproduced on cores 2-4");
    assert!(sites > 0);
}
