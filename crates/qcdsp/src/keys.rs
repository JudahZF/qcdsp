//! Device secrets for staging, read from a private key file of hex lines:
//! the repo key, the blob key (16 bytes each) and the report seed. The report
//! secret is the seed followed by the device serial. Every secret is checked by
//! decrypting a known file from the user's snapshot; values are never printed.
//!
//! No secret or its location is stored here. [`find_seed`] searches the user's
//! own ZenUI for the seed, testing each candidate against `moduleReport.xml`.

use std::{error::Error, fs, path::Path, thread};

use qc_format::crypto::NdspCipher;
use sha2::{Digest, Sha256};

/// Length of the report seed in CorOS 4.1.0, used only by the full-scan fallback.
const SEED_LEN: usize = 57;
/// Seed candidates in the fast pass: NUL-delimited runs of these lengths.
const RUN_LENGTHS: std::ops::RangeInclusive<usize> = 16..=128;

pub struct DeviceSecrets {
    pub repo: Vec<u8>,
    pub blob: Vec<u8>,
    pub report: Vec<u8>,
}

impl DeviceSecrets {
    pub fn as_secrets(&self) -> qc_deploy::Secrets<'_> {
        qc_deploy::Secrets {
            repo: &self.repo,
            blob: &self.blob,
            report: &self.report,
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Read a file that must be private: a regular file with no group/other access.
pub fn read_private(path: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(format!("{} must be a regular file, not a symlink", path.display()).into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!("{} must have mode 0600", path.display()).into());
        }
    }
    Ok(fs::read(path)?)
}

fn starts_xml(plain: &[u8], root: &str) -> bool {
    let plain = plain.strip_prefix("\u{feff}".as_bytes()).unwrap_or(plain);
    plain.starts_with(b"<?xml") || plain.starts_with(format!("<{root}").as_bytes())
}

/// Whether `secret` decrypts `encrypted` to an XML document with this root.
fn decrypts_to(encrypted: &[u8], secret: &[u8], root: &str) -> bool {
    let cipher = NdspCipher::derive(secret);
    let head = &encrypted[..encrypted.len().min(16)];
    if !starts_xml(&cipher.crypt(head), root) {
        return false;
    }
    String::from_utf8(cipher.crypt(encrypted))
        .is_ok_and(|text| text.contains(&format!("</{root}>")))
}

fn serial(serial_file: &Path) -> Result<String, Box<dyn Error>> {
    let serial = fs::read_to_string(serial_file)?.trim().to_owned();
    if serial.len() != 9 || !serial.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("serial file must hold the 9-character device serial".into());
    }
    Ok(serial)
}

/// Search `snapshot/ZenUI` for the report seed: the bytes that, followed by the
/// serial, decrypt `blobs/moduleReport.xml`. Tries NUL-delimited byte runs first,
/// then every fixed-length window. Uses all cores; the full scan takes minutes.
pub fn find_seed(snapshot: &Path, serial_file: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let serial = serial(serial_file)?;
    let zenui = fs::read(snapshot.join("ZenUI"))?;
    let report = fs::read(snapshot.join("blobs/moduleReport.xml"))?;
    let matches = |candidate: &[u8]| {
        decrypts_to(
            &report,
            &[candidate, serial.as_bytes()].concat(),
            "ModuleReport",
        )
    };

    let mut runs = Vec::new();
    let mut start = 0;
    for (at, &byte) in zenui.iter().enumerate() {
        if byte == 0 {
            if start > 0 && RUN_LENGTHS.contains(&(at - start)) {
                runs.push(start..at);
            }
            start = at + 1;
        }
    }
    if let Some(run) = runs.iter().find(|run| matches(&zenui[(*run).clone()])) {
        return Ok(zenui[run.clone()].to_vec());
    }

    let windows = zenui.len().saturating_sub(SEED_LEN - 1);
    let threads = thread::available_parallelism().map_or(1, usize::from);
    let chunk = windows.div_ceil(threads).max(1);
    let found = thread::scope(|scope| {
        let workers: Vec<_> = (0..windows)
            .step_by(chunk)
            .map(|first| {
                let zenui = &zenui;
                let matches = &matches;
                scope.spawn(move || {
                    (first..(first + chunk).min(windows))
                        .find(|&at| matches(&zenui[at..at + SEED_LEN]))
                })
            })
            .collect();
        workers
            .into_iter()
            .find_map(|worker| worker.join().ok().flatten())
    });
    found
        .map(|at| zenui[at..at + SEED_LEN].to_vec())
        .ok_or_else(|| "no ZenUI bytes decrypt moduleReport.xml with this serial".into())
}

fn parse_hex(line: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    if !line.len().is_multiple_of(2) {
        return Err("key file line has an odd number of hex digits".into());
    }
    (0..line.len())
        .step_by(2)
        .map(|at| {
            u8::from_str_radix(&line[at..at + 2], 16)
                .map_err(|_| "key file holds non-hex text".into())
        })
        .collect()
}

/// `keys` holds hex lines in any order: two 16-byte keys and the report seed.
pub fn load(
    snapshot: &Path,
    keys: &Path,
    serial_file: &Path,
) -> Result<DeviceSecrets, Box<dyn Error>> {
    let text = String::from_utf8(read_private(keys)?)?;
    let lines: Vec<Vec<u8>> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(parse_hex)
        .collect::<Result<_, _>>()?;
    let (keys, seeds): (Vec<_>, Vec<_>) = lines.into_iter().partition(|line| line.len() == 16);
    if keys.len() != 2 {
        return Err("key file must hold the repo and blob keys as two 16-byte hex lines".into());
    }
    let [seed] = <[Vec<u8>; 1]>::try_from(seeds).map_err(|_| {
        "key file must hold one report seed line; run `cargo qcdsp find-seed` and append its output"
    })?;

    let catalogue = fs::read(snapshot.join("ModelRepo.xml"))?;
    let repo_index = (0..2)
        .find(|&index| decrypts_to(&catalogue, &keys[index], "Models"))
        .ok_or("neither key decrypts ModelRepo.xml")?;
    let repo = keys[repo_index].clone();
    let blob = keys[1 - repo_index].clone();
    let volume =
        NdspCipher::derive(&blob).crypt(&fs::read(snapshot.join("blobs/core1/volume_blob.bin"))?);
    if hex(&Sha256::digest(&volume)) != qcdsp::device::template::VOLUME_SHA256 {
        return Err("the blob key does not decrypt the supported volume blob".into());
    }

    let report = [seed.as_slice(), serial(serial_file)?.as_bytes()].concat();
    if !decrypts_to(
        &fs::read(snapshot.join("blobs/moduleReport.xml"))?,
        &report,
        "ModuleReport",
    ) {
        return Err("the report seed and serial do not decrypt moduleReport.xml".into());
    }
    Ok(DeviceSecrets { repo, blob, report })
}
