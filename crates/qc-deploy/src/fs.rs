use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use sha2::{Digest, Sha256};

use crate::{Error, Result};

const MAX_RESOURCE_BYTES: u64 = 32 * 1024 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

pub(crate) fn root(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::UnsafePath(path.to_owned()));
    }
    let canonical = path.canonicalize()?;
    deny_production(&canonical)?;
    Ok(canonical)
}

fn deny_production(path: &Path) -> Result<()> {
    let production = Path::new("/opt/neuraldsp");
    if path.starts_with(production) || production.canonicalize().is_ok_and(|p| path.starts_with(p))
    {
        return Err(Error::UnsafePath(path.to_owned()));
    }
    Ok(())
}

pub(crate) fn checked_path(root: &Path, relative: &str) -> Result<PathBuf> {
    if self::root(root)? != root {
        return Err(Error::UnsafePath(root.to_owned()));
    }
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(Error::UnsafePath(relative.to_owned()));
    }
    let mut path = root.to_owned();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(m) => {
                if m.file_type().is_symlink() || (!m.is_dir() && !m.is_file()) {
                    return Err(Error::UnsafePath(path));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if m.is_file() && m.nlink() != 1 {
                        return Err(Error::UnsafePath(path));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

pub(crate) fn read(root: &Path, relative: &str) -> Result<Vec<u8>> {
    let path = checked_path(root, relative)?;
    let file = File::open(&path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > MAX_RESOURCE_BYTES {
        return Err(Error::Invalid(format!(
            "not a bounded resource file: {relative}"
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_RESOURCE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RESOURCE_BYTES {
        return Err(Error::Invalid(format!("oversized resource: {relative}")));
    }
    Ok(bytes)
}

pub(crate) fn hash_file(root: &Path, relative: &str) -> Result<Option<String>> {
    let path = checked_path(root, relative)?;
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(Error::UnsafePath(path));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(Some(
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    ))
}

pub(crate) fn expect(root: &Path, relative: &str, expected: Option<&str>) -> Result<()> {
    if hash_file(root, relative)?.as_deref() != expected {
        return Err(Error::Changed(relative.to_owned()));
    }
    Ok(())
}

pub(crate) fn mode(root: &Path, relative: &str) -> Result<u32> {
    let metadata = fs::metadata(checked_path(root, relative)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(0o600)
    }
}

pub(crate) fn expect_mode(root: &Path, relative: &str, expected: u32) -> Result<()> {
    if mode(root, relative)? != expected {
        return Err(Error::Changed(format!("{relative} permissions")));
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

pub(crate) fn new_dir(path: &Path) -> Result<PathBuf> {
    let parent = root(
        path.parent()
            .map(|parent| {
                if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                }
            })
            .ok_or_else(|| Error::UnsafePath(path.to_owned()))?,
    )?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::UnsafePath(path.to_owned()))?;
    let path = parent.join(name);
    deny_production(&path)?;
    fs::create_dir(&path)?;
    set_mode(&path, 0o700)?;
    File::open(parent)?.sync_all()?;
    Ok(path)
}

fn parents(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = checked_path(root, relative)?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::UnsafePath(path.clone()))?;
    fs::create_dir_all(parent)?;
    checked_path(root, relative)?;
    Ok(path)
}

pub(crate) fn write_new(root: &Path, relative: &str, bytes: &[u8]) -> Result<()> {
    let path = parents(root, relative)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    set_mode(&path, 0o600)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let mut directory = path.parent();
    while let Some(parent) = directory.filter(|parent| parent.starts_with(root)) {
        File::open(parent)?.sync_all()?;
        directory = parent.parent();
    }
    expect(root, relative, Some(&sha256(bytes)))
}

/// Atomic for this one file, not the entire clone. The caller must exclude other writers.
pub(crate) fn replace(
    root: &Path,
    relative: &str,
    bytes: &[u8],
    mode: u32,
    expected: Option<&str>,
) -> Result<()> {
    let path = checked_path(root, relative)?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::UnsafePath(path.clone()))?;
    let temporary = parent.join(format!(
        ".qc-deploy-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        set_mode(&temporary, mode)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        expect(root, relative, expected)?;
        if expected.is_some() {
            expect_mode(root, relative, mode)?;
        }
        fs::rename(&temporary, &path)?;
        File::open(parent)?.sync_all()?;
        expect(root, relative, Some(&sha256(bytes)))
    })();
    if result.is_err() {
        // This is only our create_new temporary, never a pre-existing path.
        let _ = fs::remove_file(temporary);
    }
    result
}

pub(crate) fn remove(
    root: &Path,
    relative: &str,
    expected: &str,
    expected_mode: u32,
) -> Result<()> {
    let path = checked_path(root, relative)?;
    expect(root, relative, Some(expected))?;
    expect_mode(root, relative, expected_mode)?;
    fs::remove_file(&path)?;
    File::open(
        path.parent()
            .ok_or_else(|| Error::UnsafePath(path.clone()))?,
    )?
    .sync_all()?;
    expect(root, relative, None)
}

pub(crate) fn inventory(root: &Path, expected: &[String]) -> Result<()> {
    let mut pending = vec![root.to_owned()];
    let mut found = Vec::new();
    let mut total = 0;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|_| Error::UnsafePath(path.clone()))?
                .to_str()
                .ok_or_else(|| Error::UnsafePath(path.clone()))?
                .to_owned();
            checked_path(root, &relative)?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                if !expected
                    .iter()
                    .any(|name| name.starts_with(&format!("{relative}/")))
                {
                    return Err(Error::Invalid(format!(
                        "unexpected bundle directory: {relative}"
                    )));
                }
                pending.push(path);
            } else {
                total += metadata.len();
                if total > MAX_RESOURCE_BYTES || !expected.contains(&relative) {
                    return Err(Error::Invalid(format!(
                        "oversized or unexpected bundle entry: {relative}"
                    )));
                }
                found.push(relative);
            }
        }
    }
    found.sort();
    let mut expected = expected.to_vec();
    expected.sort();
    if found != expected {
        return Err(Error::Invalid("incomplete bundle inventory".into()));
    }
    Ok(())
}

pub(crate) fn separate(snapshot: &Path, output: &Path) -> Result<()> {
    let parent = root(
        output
            .parent()
            .map(|parent| {
                if parent.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    parent
                }
            })
            .ok_or_else(|| Error::UnsafePath(output.to_owned()))?,
    )?;
    let output = parent.join(
        output
            .file_name()
            .ok_or_else(|| Error::UnsafePath(output.to_owned()))?,
    );
    if output.starts_with(snapshot) || snapshot.starts_with(&output) {
        return Err(Error::UnsafePath(output));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_relative_bundle_path_uses_the_current_directory() {
        let name = format!(".qc-deploy-relative-test-{}", std::process::id());
        let output = Path::new(&name);
        let snapshot = std::env::temp_dir().join("qc-deploy-relative-snapshot");
        separate(&snapshot, output).unwrap();
        let created = new_dir(output).unwrap();
        assert_eq!(created, std::env::current_dir().unwrap().join(&name));
        fs::remove_dir(created).unwrap();
    }
}
