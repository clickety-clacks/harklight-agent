use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use base64::{Engine, engine::general_purpose::STANDARD};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    api::OriginClient,
    journal::{FileTransfer, UploadSource},
};

pub const MAX_ORIGIN_REQUEST_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFileLimits {
    pub inline_decoded_bytes_per_entry: u64,
    pub upload_cap_bytes_per_file: Option<u64>,
}

impl ServerFileLimits {
    pub fn from_response(response: &Value) -> Result<Self, String> {
        let inline_decoded_bytes_per_entry = response["inline_decoded_bytes_per_entry"]
            .as_u64()
            .ok_or_else(|| "file_limits has no nonnegative inline byte limit".to_owned())?;
        let upload_cap_bytes_per_file = match &response["upload_cap_bytes_per_file"] {
            Value::String(value) if value == "none" => None,
            Value::Number(value) => {
                let cap = value.as_u64().filter(|cap| *cap > 0).ok_or_else(|| {
                    "file_limits upload cap must be a positive integer".to_owned()
                })?;
                Some(cap)
            }
            _ => {
                return Err(
                    "file_limits upload cap must be \"none\" or a positive integer".to_owned(),
                );
            }
        };

        Ok(Self {
            inline_decoded_bytes_per_entry,
            upload_cap_bytes_per_file,
        })
    }

    pub fn upload_cap_json(&self) -> Value {
        self.upload_cap_bytes_per_file
            .map(Value::from)
            .unwrap_or_else(|| Value::String("none".into()))
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedFiles {
    pub entries: Vec<Value>,
    pub uploads: Vec<UploadSource>,
    pub manifest: Vec<FileManifestEntry>,
}

struct DownloadedBody {
    reader: Box<dyn Read>,
    sha256: Option<String>,
    content_length: Option<u64>,
}

pub fn prepare_outgoing(
    paths: &[PathBuf],
    limits: &ServerFileLimits,
    staging_directory: &Path,
) -> Result<PreparedFiles, String> {
    let selections = inspect(paths)?;
    let mut entries = Vec::with_capacity(selections.len());
    let mut uploads = Vec::new();
    let mut manifest = Vec::with_capacity(selections.len());
    let mut inline_total = 0u64;
    let prepare_result = (|| -> Result<(), String> {
        for (index, selected) in selections.iter().enumerate() {
            let remaining = limits
                .inline_decoded_bytes_per_entry
                .saturating_sub(inline_total);
            let inline_bytes = if selected.size_bytes <= remaining {
                read_inline_candidate(selected, remaining)?
            } else {
                None
            };
            if let Some(bytes) = inline_bytes {
                let size_bytes = bytes.len() as u64;
                let sha256 = hex::encode(Sha256::digest(&bytes));
                inline_total = inline_total
                    .checked_add(size_bytes)
                    .ok_or_else(|| "selected file sizes exceed the supported range".to_owned())?;
                entries.push(json!({
                    "name":selected.name,
                    "data_base64":STANDARD.encode(bytes)
                }));
                manifest.push(FileManifestEntry {
                    name: selected.name.clone(),
                    size_bytes,
                    sha256,
                });
                continue;
            }

            let staged_path = staging_directory.join(format!("{index}.upload"));
            let upload = stage_upload(selected, &staged_path, limits.upload_cap_bytes_per_file)?;
            entries.push(json!({
                "name":upload.name,
                "upload_pending":true,
                "size":upload.size_bytes,
                "sha256":upload.sha256
            }));
            manifest.push(FileManifestEntry {
                name: upload.name.clone(),
                size_bytes: upload.size_bytes,
                sha256: upload.sha256.clone(),
            });
            uploads.push(upload);
        }
        Ok(())
    })();
    if let Err(reason) = prepare_result {
        for upload in &uploads {
            if let Some(path) = upload.staged_path.as_ref() {
                let _ = fs::remove_file(path);
            }
        }
        return Err(reason);
    }
    Ok(PreparedFiles {
        entries,
        uploads,
        manifest,
    })
}

/// Identify file inputs without staging or performing any network operation.
/// This lets an idempotent prepare call compare its inputs with the durable
/// operation before consulting current server limits or changing local files.
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

fn read_inline_candidate(
    selected: &FileSelection,
    remaining: u64,
) -> Result<Option<Vec<u8>>, String> {
    let mut file = File::open(&selected.path).map_err(|error| {
        format!(
            "could not open selected file {}: {error}",
            selected.path.display()
        )
    })?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(remaining.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            format!(
                "could not read selected file {}: {error}",
                selected.path.display()
            )
        })?;
    if bytes.len() as u64 > remaining {
        Ok(None)
    } else {
        Ok(Some(bytes))
    }
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
    })
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

pub fn materialize_incoming(
    raw: &mut Value,
    event_directory: &Path,
    api: &OriginClient,
) -> Result<BTreeMap<String, FileTransfer>, String> {
    materialize_incoming_with_fetch(raw, event_directory, |upload_id| {
        let downloaded = api
            .download_file(upload_id)
            .map_err(|error| error.to_string())?;
        Ok(DownloadedBody {
            reader: Box::new(downloaded.response),
            sha256: downloaded.sha256,
            content_length: downloaded.content_length,
        })
    })
}

fn materialize_incoming_with_fetch(
    raw: &mut Value,
    event_directory: &Path,
    mut fetch: impl FnMut(uuid::Uuid) -> Result<DownloadedBody, String>,
) -> Result<BTreeMap<String, FileTransfer>, String> {
    let Some(files) = raw.get_mut("files").and_then(Value::as_array_mut) else {
        return Ok(BTreeMap::new());
    };

    let mut transfers = BTreeMap::new();
    let mut seen_names = BTreeSet::new();
    for file in files {
        let object = file
            .as_object_mut()
            .ok_or_else(|| "origin feed file entry is not an object".to_owned())?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| "origin feed file has no name".to_owned())?
            .to_owned();
        if !safe_name(&name) || !seen_names.insert(name.clone()) {
            return Err(format!(
                "origin feed contains an invalid or duplicate file name: {name}"
            ));
        }

        let intended_path = event_directory.join(&name);
        let transfer = match object.get("tier").and_then(Value::as_str) {
            Some("inline") => match decode_and_validate_file(object) {
                Ok(bytes) => persist_file(event_directory, &name, &bytes),
                Err(reason) => Err(reason),
            },
            Some("upload") => materialize_uploaded_file(object, event_directory, &name, &mut fetch),
            _ => Err("origin file tier is missing or unsupported".into()),
        };
        object.remove("data_base64");
        let outcome = match transfer {
            Ok(path) => FileTransfer {
                path: Some(path),
                transferred: true,
                reason: None,
            },
            Err(reason) => FileTransfer {
                path: Some(intended_path),
                transferred: false,
                reason: Some(reason),
            },
        };
        transfers.insert(name, outcome);
    }
    Ok(transfers)
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

pub fn file_event_view(raw: &Value, transfers: &BTreeMap<String, FileTransfer>) -> Value {
    let mut view = raw.clone();
    let Some(files) = view.get_mut("files").and_then(Value::as_array_mut) else {
        return view;
    };
    for file in files {
        let Some(object) = file.as_object_mut() else {
            continue;
        };
        let Some(name) = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let transfer = transfers.get(&name);
        object.insert(
            "path".into(),
            transfer
                .and_then(|transfer| transfer.path.as_ref())
                .map(|path| Value::String(path.to_string_lossy().into_owned()))
                .unwrap_or(Value::Null),
        );
        let transferred = transfer.is_some_and(|transfer| transfer.transferred);
        object.insert(
            "transfer".into(),
            Value::String(
                if transferred {
                    "transferred"
                } else {
                    "not_transferred"
                }
                .into(),
            ),
        );
        if let Some(reason) = transfer.and_then(|transfer| transfer.reason.as_ref()) {
            object.insert("reason".into(), Value::String(reason.clone()));
        }
        let use_status = raw["file_use"][&name]["status"]
            .as_str()
            .unwrap_or("unknown");
        object.insert("use".into(), Value::String(use_status.into()));
        if let Some(server_transfer) = raw["file_transfers"][&name].as_object() {
            object.insert(
                "server_transfer".into(),
                Value::Object(server_transfer.clone()),
            );
        }
    }
    view
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

fn materialize_uploaded_file(
    file: &serde_json::Map<String, Value>,
    directory: &Path,
    name: &str,
    fetch: &mut impl FnMut(uuid::Uuid) -> Result<DownloadedBody, String>,
) -> Result<PathBuf, String> {
    let upload_id = file
        .get("upload_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "uploaded file has no server identity".to_owned())?;
    let upload_id = uuid::Uuid::parse_str(upload_id)
        .ok()
        .filter(|id| id.get_version_num() == 4)
        .ok_or_else(|| "uploaded file has an invalid server identity".to_owned())?;
    let size = file
        .get("size")
        .and_then(Value::as_u64)
        .ok_or_else(|| "uploaded file has no valid byte size".to_owned())?;
    let expected_sha256 = file
        .get("sha256")
        .and_then(Value::as_str)
        .filter(|hash| is_sha256(hash))
        .ok_or_else(|| "uploaded file has no valid SHA-256".to_owned())?;
    let existing_path = directory.join(name);
    match existing_file_matches_digest(&existing_path, size, expected_sha256)? {
        Some(true) => return Ok(existing_path),
        Some(false) => {
            return Err("a different file already exists at the session event path".into());
        }
        None => {}
    }
    let mut downloaded =
        fetch(upload_id).map_err(|error| format!("could not fetch file {name}: {error}"))?;
    let response_sha256 = downloaded
        .sha256
        .as_deref()
        .filter(|hash| is_sha256(hash))
        .ok_or_else(|| format!("file {name} fetch has no valid SHA-256 response header"))?;
    if response_sha256 != expected_sha256 {
        return Err(format!(
            "file {name} fetch SHA-256 header does not match the conversation entry"
        ));
    }
    if downloaded.content_length != Some(size) {
        return Err(format!(
            "file {name} fetch length header does not match the conversation entry"
        ));
    }
    persist_reader(
        directory,
        name,
        downloaded.reader.as_mut(),
        size,
        expected_sha256,
    )
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn decode_and_validate_file(file: &serde_json::Map<String, Value>) -> Result<Vec<u8>, String> {
    let name = file.get("name").and_then(Value::as_str).unwrap_or_default();
    if !safe_name(name) {
        return Err("origin file name is not a safe basename".into());
    }
    if file.get("tier").and_then(Value::as_str) != Some("inline") {
        return Err("origin file tier is unsupported".into());
    }
    let data = file
        .get("data_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "origin file has no inline bytes".to_owned())?;
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| "origin file has invalid base64".to_owned())?;
    if STANDARD.encode(&bytes) != data {
        return Err("origin file base64 is not canonical".into());
    }
    if file.get("size").and_then(Value::as_u64) != Some(bytes.len() as u64) {
        return Err("origin file size does not match its bytes".into());
    }
    let digest = hex::encode(Sha256::digest(&bytes));
    if file.get("sha256").and_then(Value::as_str) != Some(digest.as_str()) {
        return Err("origin file digest does not match its bytes".into());
    }
    Ok(bytes)
}

fn persist_file(directory: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    create_private_directory(directory)
        .map_err(|error| format!("could not create session file directory: {error}"))?;
    let lock_path = directory.join(".materialize.lock");
    let lock = open_private_lock(&lock_path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("could not lock session file directory: {error}"))?;
    let result = persist_file_locked(directory, name, bytes);
    let _ = FileExt::unlock(&lock);
    result
}

fn persist_reader(
    directory: &Path,
    name: &str,
    reader: &mut dyn Read,
    expected_size: u64,
    expected_sha256: &str,
) -> Result<PathBuf, String> {
    create_private_directory(directory)
        .map_err(|error| format!("could not create session file directory: {error}"))?;
    let lock_path = directory.join(".materialize.lock");
    let lock = open_private_lock(&lock_path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("could not lock session file directory: {error}"))?;

    let path = directory.join(name);
    match existing_file_matches_digest(&path, expected_size, expected_sha256)? {
        Some(true) => {
            let _ = FileExt::unlock(&lock);
            return Ok(path);
        }
        Some(false) => {
            let _ = FileExt::unlock(&lock);
            return Err("a different file already exists at the session event path".into());
        }
        None => {}
    }

    let temporary = directory.join(format!(".materialize.{}.tmp", uuid::Uuid::new_v4()));
    let mut output = match create_private_new_file(&temporary) {
        Ok(file) => file,
        Err(error) => {
            let _ = FileExt::unlock(&lock);
            return Err(format!("could not create local file: {error}"));
        }
    };
    let mut size = 0u64;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let copy_result = (|| -> Result<(), String> {
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
                return Err("downloaded file exceeds the entry's declared byte size".into());
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
            return Err("downloaded file SHA-256 does not match the entry".into());
        }
        output
            .sync_all()
            .and_then(|()| set_private_file(&output))
            .map_err(|error| format!("could not persist downloaded file: {error}"))?;
        Ok(())
    })();
    drop(output);
    if let Err(reason) = copy_result {
        let _ = fs::remove_file(&temporary);
        let _ = FileExt::unlock(&lock);
        return Err(reason);
    }

    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        let _ = FileExt::unlock(&lock);
        return Err(format!("could not publish downloaded file: {error}"));
    }
    if let Ok(parent) = File::open(directory) {
        let _ = parent.sync_all();
    }
    let _ = FileExt::unlock(&lock);
    Ok(path)
}

fn persist_file_locked(directory: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = directory.join(name);
    match existing_file_matches(&path, bytes)? {
        Some(true) => return Ok(path),
        Some(false) => {
            return Err("a different file already exists at the session event path".into());
        }
        None => {}
    }

    let temporary = directory.join(format!(".materialize.{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("could not create local file: {error}"))?;
    let result = file
        .write_all(bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| set_private_file(&file));
    drop(file);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not write local file: {error}"));
    }

    match existing_file_matches(&path, bytes) {
        Ok(Some(matches)) => {
            let _ = fs::remove_file(&temporary);
            return if matches {
                Ok(path)
            } else {
                Err("a different file already exists at the session event path".into())
            };
        }
        Ok(None) => {}
        Err(reason) => {
            let _ = fs::remove_file(&temporary);
            return Err(reason);
        }
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not publish local file: {error}"));
    }
    if let Ok(parent) = File::open(directory) {
        let _ = parent.sync_all();
    }
    Ok(path)
}

fn existing_file_matches(path: &Path, bytes: &[u8]) -> Result<Option<bool>, String> {
    existing_file_matches_digest(
        path,
        bytes.len() as u64,
        &hex::encode(Sha256::digest(bytes)),
    )
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

fn open_private_lock(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(path)
        .map_err(|error| format!("could not open session file lock: {error}"))?;
    set_private_file(&file)
        .map_err(|error| format!("could not secure session file lock: {error}"))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn scratch() -> PathBuf {
        std::env::temp_dir().join(format!("harklight-agent-files-{}", Uuid::new_v4()))
    }

    #[test]
    fn file_limits_accept_only_none_or_a_positive_upload_cap() {
        assert_eq!(
            ServerFileLimits::from_response(&json!({
                "inline_decoded_bytes_per_entry":262_144,
                "upload_cap_bytes_per_file":"none"
            }))
            .unwrap(),
            ServerFileLimits {
                inline_decoded_bytes_per_entry: 262_144,
                upload_cap_bytes_per_file: None
            }
        );
        assert_eq!(
            ServerFileLimits::from_response(&json!({
                "inline_decoded_bytes_per_entry":262_144,
                "upload_cap_bytes_per_file":1_024
            }))
            .unwrap()
            .upload_cap_bytes_per_file,
            Some(1_024)
        );
        assert!(
            ServerFileLimits::from_response(&json!({
                "inline_decoded_bytes_per_entry":262_144,
                "upload_cap_bytes_per_file":0
            }))
            .is_err()
        );
    }

    #[test]
    fn inline_file_materialization_preserves_exact_bytes_and_strips_base64_from_journal_view() {
        let root = scratch();
        let file_path = root.join("session").join("17");
        let bytes = [0, 255, 1, 128];
        let encoded = STANDARD.encode(bytes);
        let digest = hex::encode(Sha256::digest(bytes));
        let mut entry = json!({
            "files":[{
                "name":"input.bin",
                "tier":"inline",
                "size":bytes.len(),
                "sha256":digest.clone(),
                "data_base64":encoded.clone()
            }]
        });

        let transfers = materialize_incoming_with_fetch(&mut entry, &file_path, |_| {
            unreachable!("inline file must not fetch")
        })
        .unwrap();
        assert_eq!(fs::read(file_path.join("input.bin")).unwrap(), bytes);
        assert_eq!(transfers["input.bin"].transferred, true);
        assert!(entry["files"][0].get("data_base64").is_none());

        let mut replay = json!({
            "files":[{
                "name":"input.bin",
                "tier":"inline",
                "size":bytes.len(),
                "sha256":digest,
                "data_base64":encoded
            }]
        });
        let replay_transfers = materialize_incoming_with_fetch(&mut replay, &file_path, |_| {
            unreachable!("inline file must not fetch")
        })
        .unwrap();
        assert_eq!(replay_transfers["input.bin"].transferred, true);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_incoming_tier_keeps_the_intended_path_and_transfer_reason() {
        let root = scratch();
        let event_directory = root.join("session").join("18");
        let mut entry = json!({
            "files":[{"name":"large.bin","tier":"upload","size":3,"sha256":"digest"}]
        });

        let transfers = materialize_incoming_with_fetch(&mut entry, &event_directory, |_| {
            unreachable!("unsupported file tier must not fetch")
        })
        .unwrap();
        let expected_path = event_directory.join("large.bin");
        let expected_path_string = expected_path.to_string_lossy().into_owned();
        assert!(!transfers["large.bin"].transferred);
        assert_eq!(transfers["large.bin"].path.as_ref(), Some(&expected_path));
        assert!(transfers["large.bin"].reason.is_some());
        let view = file_event_view(&entry, &transfers);
        assert_eq!(view["files"][0]["transfer"], "not_transferred");
        assert_eq!(
            view["files"][0]["path"].as_str(),
            Some(expected_path_string.as_str())
        );
        fs::remove_dir_all(root).unwrap_or(());
    }

    #[test]
    fn uploaded_file_is_fetched_and_verified_before_materialization() {
        use std::io::Cursor;

        let root = scratch();
        let event_directory = root.join("session").join("18");
        let bytes = b"uploaded bytes";
        let digest = hex::encode(Sha256::digest(bytes));
        let upload_id = uuid::Uuid::new_v4();
        let mut entry = json!({
            "files":[{
                "name":"large.bin",
                "tier":"upload",
                "upload_id":upload_id,
                "size":bytes.len(),
                "sha256":digest.clone()
            }]
        });

        let transfers =
            materialize_incoming_with_fetch(&mut entry, &event_directory, |requested| {
                assert_eq!(requested, upload_id);
                Ok(DownloadedBody {
                    reader: Box::new(Cursor::new(bytes.to_vec())),
                    sha256: Some(digest.clone()),
                    content_length: Some(bytes.len() as u64),
                })
            })
            .unwrap();

        assert!(transfers["large.bin"].transferred);
        assert_eq!(fs::read(event_directory.join("large.bin")).unwrap(), bytes);
        let mut replay = json!({
            "files":[{
                "name":"large.bin",
                "tier":"upload",
                "upload_id":upload_id,
                "size":bytes.len(),
                "sha256":digest
            }]
        });
        let replay_transfers =
            materialize_incoming_with_fetch(&mut replay, &event_directory, |_| {
                unreachable!("verified local file must not fetch again")
            })
            .unwrap();
        assert!(replay_transfers["large.bin"].transferred);
        fs::remove_dir_all(root).unwrap_or(());
    }

    #[test]
    fn uploaded_file_with_wrong_digest_is_not_materialized() {
        use std::io::Cursor;

        let root = scratch();
        let event_directory = root.join("session").join("19");
        let bytes = b"wrong bytes";
        let expected = hex::encode(Sha256::digest(b"expected bytes"));
        let mut entry = json!({
            "files":[{
                "name":"large.bin",
                "tier":"upload",
                "upload_id":uuid::Uuid::new_v4(),
                "size":bytes.len(),
                "sha256":expected.clone()
            }]
        });

        let transfers = materialize_incoming_with_fetch(&mut entry, &event_directory, |_| {
            Ok(DownloadedBody {
                reader: Box::new(Cursor::new(bytes.to_vec())),
                sha256: Some(expected.clone()),
                content_length: Some(bytes.len() as u64),
            })
        })
        .unwrap();

        assert!(!transfers["large.bin"].transferred);
        assert!(transfers["large.bin"].reason.is_some());
        assert!(!event_directory.join("large.bin").exists());
        fs::remove_dir_all(root).unwrap_or(());
    }

    #[test]
    fn outgoing_file_over_server_inline_limit_is_staged_for_upload() {
        let root = scratch();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("large.bin"), [1, 2, 3, 4]).unwrap();
        let limits = ServerFileLimits {
            inline_decoded_bytes_per_entry: 3,
            upload_cap_bytes_per_file: None,
        };

        let staging = root.join("private-stage");
        let prepared = prepare_outgoing(&[root.join("large.bin")], &limits, &staging).unwrap();
        assert_eq!(prepared.entries[0]["upload_pending"], true);
        assert_eq!(prepared.uploads[0].size_bytes, 4);
        assert_eq!(
            fs::read(prepared.uploads[0].staged_path.as_ref().unwrap()).unwrap(),
            [1, 2, 3, 4]
        );
        cleanup_staged_uploads(&prepared.uploads);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_over_published_upload_cap_is_refused_without_staging_bytes() {
        let root = scratch();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("too-large.bin"), [1, 2, 3, 4]).unwrap();
        let stage = root.join("stage");
        let limits = ServerFileLimits {
            inline_decoded_bytes_per_entry: 2,
            upload_cap_bytes_per_file: Some(3),
        };

        let error = prepare_outgoing(&[root.join("too-large.bin")], &limits, &stage).unwrap_err();

        assert!(error.contains("published per-file upload cap"));
        assert!(!stage.join("0.upload").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn staging_retry_does_not_replace_bytes_for_an_existing_operation() {
        let root = scratch();
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.bin");
        let staged = root.join("stage").join("0.upload");
        fs::write(&source, b"original").unwrap();
        let first = FileSelection {
            name: "source.bin".into(),
            path: source.clone(),
            size_bytes: 8,
        };
        let original = stage_upload(&first, &staged, None).unwrap();

        fs::write(&source, b"changed!").unwrap();
        let changed = FileSelection {
            name: "source.bin".into(),
            path: source,
            size_bytes: 8,
        };
        assert!(
            stage_upload(&changed, &staged, None)
                .unwrap_err()
                .contains("original operation was not changed")
        );
        assert_eq!(fs::read(staged).unwrap(), b"original");
        cleanup_staged_uploads(&[original]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn inline_allowance_is_aggregated_per_entry_and_overflow_is_uploaded() {
        let root = scratch();
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("first.bin"), [1, 2]).unwrap();
        fs::write(root.join("second.bin"), [3, 4]).unwrap();
        let limits = ServerFileLimits {
            inline_decoded_bytes_per_entry: 3,
            upload_cap_bytes_per_file: None,
        };

        let prepared = prepare_outgoing(
            &[root.join("first.bin"), root.join("second.bin")],
            &limits,
            &root.join("stage"),
        )
        .unwrap();

        assert!(prepared.entries[0].get("data_base64").is_some());
        assert_eq!(prepared.entries[1]["upload_pending"], true);
        assert_eq!(prepared.uploads[0].size_bytes, 2);
        cleanup_staged_uploads(&prepared.uploads);
        fs::remove_dir_all(root).unwrap();
    }
}
