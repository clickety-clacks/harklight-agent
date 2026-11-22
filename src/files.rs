use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use crate::journal::UploadSource;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileSelection {
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
}

/// Stable, path-free identity for the exact bytes supplied to one operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileManifestEntry {
    pub name: String,
    pub size_bytes: u64,
    pub sha256: String,
}

pub fn identify(paths: &[PathBuf]) -> Result<Vec<FileManifestEntry>, String> {
    inspect(paths)?
        .into_iter()
        .map(|selected| {
            let mut file = File::open(&selected.path).map_err(|error| {
                format!(
                    "could not open selected file {}: {error}",
                    selected.path.display()
                )
            })?;
            let mut size_bytes = 0u64;
            let mut digest = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer).map_err(|error| {
                    format!("could not read selected file {}: {error}", selected.name)
                })?;
                if count == 0 {
                    break;
                }
                size_bytes = size_bytes.checked_add(count as u64).ok_or_else(|| {
                    format!("file {} exceeds the supported size range", selected.name)
                })?;
                digest.update(&buffer[..count]);
            }
            Ok(FileManifestEntry {
                name: selected.name,
                size_bytes,
                sha256: hex::encode(digest.finalize()),
            })
        })
        .collect()
}

fn stage_upload(
    selected: &FileSelection,
    staged_path: &Path,
    upload_cap: Option<u64>,
) -> Result<UploadSource, String> {
    if let Some(cap) = upload_cap.filter(|cap| selected.size_bytes > *cap) {
        return Err(format!(
            "file {} is {} bytes, over the published per-file upload cap of {cap} bytes; nothing was submitted",
            selected.name, selected.size_bytes
        ));
    }
    let directory = staged_path
        .parent()
        .ok_or_else(|| "upload staging path has no parent".to_owned())?;
    create_private_directory(directory)
        .map_err(|error| format!("could not create private upload staging directory: {error}"))?;
    let temporary = directory.join(format!(".stage.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(u64, String), String> {
        let mut input = File::open(&selected.path).map_err(|error| {
            format!(
                "could not open selected file {}: {error}",
                selected.path.display()
            )
        })?;
        let mut output = create_private_new_file(&temporary)
            .map_err(|error| format!("could not create staged upload: {error}"))?;
        let mut size = 0u64;
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = input.read(&mut buffer).map_err(|error| {
                format!("could not read selected file {}: {error}", selected.name)
            })?;
            if count == 0 {
                break;
            }
            size = size.checked_add(count as u64).ok_or_else(|| {
                format!("file {} exceeds the supported size range", selected.name)
            })?;
            if upload_cap.is_some_and(|cap| size > cap) {
                return Err(format!(
                    "file {} grew beyond the published per-file upload cap of {} bytes; nothing was submitted",
                    selected.name,
                    upload_cap.unwrap_or_default()
                ));
            }
            output
                .write_all(&buffer[..count])
                .map_err(|error| format!("could not stage file {}: {error}", selected.name))?;
            digest.update(&buffer[..count]);
        }
        output
            .sync_all()
            .and_then(|()| set_private_file(&output))
            .map_err(|error| format!("could not persist staged file {}: {error}", selected.name))?;
        Ok((size, hex::encode(digest.finalize())))
    })();
    let (size_bytes, sha256) = match result {
        Ok(result) => result,
        Err(reason) => {
            let _ = fs::remove_file(&temporary);
            return Err(reason);
        }
    };
    let existing = match existing_file_matches_digest(staged_path, size_bytes, &sha256) {
        Ok(existing) => existing,
        Err(reason) => {
            let _ = fs::remove_file(&temporary);
            return Err(reason);
        }
    };
    match existing {
        Some(true) => {
            let _ = fs::remove_file(&temporary);
        }
        Some(false) => {
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "staged file {} already exists with different bytes; the original operation was not changed",
                selected.name
            ));
        }
        None => {
            if let Err(error) = fs::rename(&temporary, staged_path) {
                let _ = fs::remove_file(&temporary);
                return Err(format!(
                    "could not publish staged file {}: {error}",
                    selected.name
                ));
            }
        }
    }
    if let Ok(parent) = File::open(directory) {
        let _ = parent.sync_all();
    }
    Ok(UploadSource {
        name: selected.name.clone(),
        staged_path: Some(staged_path.to_path_buf()),
        size_bytes,
        sha256,
        upload_cap_bytes_per_file: upload_cap,
        upload_id: None,
        upload_key: None,
    })
}

pub(crate) fn stage_v2_uploads(
    paths: &[PathBuf],
    directory: &Path,
    upload_cap: Option<u64>,
) -> Result<Vec<UploadSource>, String> {
    let selections = inspect(paths)?;
    let mut uploads: Vec<UploadSource> = Vec::with_capacity(selections.len());
    for (index, selected) in selections.iter().enumerate() {
        let path = directory.join(format!("{index}.upload"));
        match stage_upload(selected, &path, upload_cap) {
            Ok(mut upload) => {
                upload.upload_key = Some(uuid::Uuid::new_v4());
                uploads.push(upload);
            }
            Err(error) => {
                cleanup_staged_uploads(&uploads);
                return Err(error);
            }
        }
    }
    Ok(uploads)
}

pub(crate) fn verified_staged_path(upload: &UploadSource) -> Result<&Path, String> {
    let path = upload
        .staged_path
        .as_deref()
        .ok_or_else(|| format!("staged bytes for {} are unavailable", upload.name))?;
    if existing_file_matches_digest(path, upload.size_bytes, &upload.sha256)? != Some(true) {
        return Err(format!(
            "staged bytes for {} do not match the saved digest",
            upload.name
        ));
    }
    Ok(path)
}

pub fn cleanup_staged_uploads(uploads: &[UploadSource]) {
    for upload in uploads {
        if let Some(path) = upload.staged_path.as_ref() {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn inspect(paths: &[PathBuf]) -> Result<Vec<FileSelection>, String> {
    let mut selections = Vec::with_capacity(paths.len());
    let mut seen_names = BTreeSet::new();
    for path in paths {
        let name = safe_basename(path)?;
        if !seen_names.insert(name.clone()) {
            return Err(format!("duplicate file name in one entry: {name}"));
        }
        let metadata = fs::metadata(path)
            .map_err(|error| format!("could not read selected file {}: {error}", path.display()))?;
        if !metadata.is_file() {
            return Err(format!(
                "selected path is not a regular file: {}",
                path.display()
            ));
        }
        selections.push(FileSelection {
            name,
            path: path.clone(),
            size_bytes: metadata.len(),
        });
    }
    Ok(selections)
}

pub fn strip_file_payloads(raw: &mut Value) {
    if let Some(files) = raw.get_mut("files").and_then(Value::as_array_mut) {
        for file in files {
            if let Some(object) = file.as_object_mut() {
                object.remove("data_base64");
            }
        }
    }
}

fn safe_basename(path: &Path) -> Result<String, String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| safe_name(name))
        .ok_or_else(|| {
            format!(
                "selected file needs a safe UTF-8 basename: {}",
                path.display()
            )
        })?;
    Ok(name.to_owned())
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|character| matches!(character, '/' | '\\' | '\0'))
}

/// Verify a streamed download before making its contents visible at the
/// requested path. Creating the destination never replaces an existing file.
pub(crate) fn persist_download(
    path: &Path,
    reader: &mut dyn Read,
    size: u64,
    sha256: &str,
) -> Result<(), String> {
    let directory = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = directory.join(format!(".harklight-download-{}.tmp", uuid::Uuid::new_v4()));
    let mut output = create_private_new_file(&temporary)
        .map_err(|error| format!("could not create temporary download: {error}"))?;
    let copied = copy_verified(reader, &mut output, size, sha256);
    drop(output);
    let result = copied.and_then(|()| match fs::hard_link(&temporary, path) {
        Ok(()) => {
            if let Ok(parent) = File::open(directory) {
                let _ = parent.sync_all();
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if existing_file_matches_digest(path, size, sha256)? == Some(true) {
                Ok(())
            } else {
                Err("a different file already exists at the requested path".into())
            }
        }
        Err(error) => Err(format!("could not publish downloaded file: {error}")),
    });
    let _ = fs::remove_file(&temporary);
    result
}

fn copy_verified(
    reader: &mut dyn Read,
    output: &mut File,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<(), String> {
    let mut size = 0u64;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("could not fetch file bytes: {error}"))?;
        if count == 0 {
            break;
        }
        size = size
            .checked_add(count as u64)
            .ok_or_else(|| "downloaded file exceeds the supported size range".to_owned())?;
        if size > expected_size {
            return Err("downloaded file exceeds the declared byte size".into());
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| format!("could not write downloaded file: {error}"))?;
        digest.update(&buffer[..count]);
    }
    if size != expected_size {
        return Err(format!(
            "downloaded file is incomplete: expected {expected_size} bytes, received {size}"
        ));
    }
    let actual_sha256 = hex::encode(digest.finalize());
    if actual_sha256 != expected_sha256 {
        return Err("downloaded file SHA-256 does not match the declared digest".into());
    }
    output
        .sync_all()
        .and_then(|()| set_private_file(output))
        .map_err(|error| format!("could not persist downloaded file: {error}"))
}

fn existing_file_matches_digest(
    path: &Path,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<Option<bool>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err("existing local file is not a regular file".into())
        }
        Ok(metadata) if metadata.len() != expected_size => Ok(Some(false)),
        Ok(_) => {
            let mut file = File::open(path)
                .map_err(|error| format!("could not read existing local file: {error}"))?;
            let mut digest = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = file
                    .read(&mut buffer)
                    .map_err(|error| format!("could not read existing local file: {error}"))?;
                if count == 0 {
                    break;
                }
                digest.update(&buffer[..count]);
            }
            Ok(Some(hex::encode(digest.finalize()) == expected_sha256))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not inspect existing local file: {error}")),
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> Result<(), std::io::Error> {
    fs::create_dir_all(path)
}

#[cfg(unix)]
fn set_private_file(file: &File) -> Result<(), std::io::Error> {
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file(_file: &File) -> Result<(), std::io::Error> {
    Ok(())
}

fn create_private_new_file(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}
