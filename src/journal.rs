#[cfg(test)]
use std::process;

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

mod progress_files;
mod v2_feed;
pub(crate) use progress_files::is_progress;
pub(crate) use v2_feed::{DeliveryAttempt, DeliveryPages};

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("journal database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("journal database uses an unsupported schema; Agent requires schema 2")]
    UnsupportedSchema,
    #[error("journal database cursor is invalid")]
    InvalidCursor,
    #[error("operation key {0} was already used with different content")]
    OperationConflict(Uuid),
    #[error("local message reference already has a different server item ID")]
    ItemBindingConflict,
    #[error("a revision is unresolved for this item; retry operation {0}")]
    PendingRevision(Uuid),
    #[error("journal has no operation with key {0}")]
    MissingOperation(Uuid),
    #[error("operation {op_id} is {state}; only an unresolved operation can be abandoned")]
    NotUnresolved { op_id: Uuid, state: &'static str },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationRecord {
    pub op_id: Uuid,
    pub session_handle: Uuid,
    pub kind: String,
    pub request: Value,
    pub state: OperationState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub response: Option<Value>,
    #[serde(default)]
    pub uploads: Vec<UploadSource>,
    #[serde(default)]
    pub prepared_send: Option<PreparedSend>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared_request: Option<PreparedRequest>,
    // Earlier records can remain in an Agent 0.2 journal. The missing marker
    // defaults to false so they cannot be retried against the current API.
    #[serde(default)]
    pub protocol_v2: bool,
}

/// The finalized v2 HTTP request. The enclosing operation ID is its
/// idempotency key. Store the JSON text intact so a retry sends the same bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedRequest {
    pub method: RequestMethod,
    pub path: Vec<String>,
    pub if_match: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestMethod {
    Post,
    Put,
    Delete,
}

/// The caller-visible immutable portion of a two-phase send. `request` is the
/// exact v1 request returned by `send --prepare`; the operation's live request
/// may later replace upload placeholders with the server-assigned identities
/// before the first send POST, then remains stable for retries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreparedSend {
    pub inputs: Value,
    pub request: Value,
    pub files: Vec<crate::files::FileManifestEntry>,
}

/// Private local bytes and the server identity for one upload associated with
/// an operation. The staged source is retained until the entry operation is
/// known committed or refused, so retries resend the exact same bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadSource {
    pub name: String,
    pub staged_path: Option<PathBuf>,
    pub size_bytes: u64,
    pub sha256: String,
    pub upload_cap_bytes_per_file: Option<u64>,
    pub upload_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_key: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Prepared,
    Unknown,
    Committed,
    Rejected,
}

pub struct OperationLock {
    op_id: Uuid,
    _file: File,
}

#[cfg(test)]
thread_local! {
    static EXIT_AFTER_NEXT_COMMIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub struct JournalStore {
    journal_path: PathBuf,
    journal_lock_path: PathBuf,
}

impl JournalStore {
    pub fn open(
        state_root: &Path,
        device_id: &str,
        session_handle: Uuid,
    ) -> Result<Self, JournalError> {
        let identity_scope = hex::encode(Sha256::digest(device_id.as_bytes()));
        let directory = state_root.join(identity_scope);
        create_private_directory(&directory)?;
        // Keep the original database and lock paired for existing 0.2 state.
        // An older process may still use both names during an upgrade.
        let existing_path = directory.join("origin-feed.sqlite3");
        let store = if existing_path.try_exists()? {
            Self {
                journal_path: existing_path,
                journal_lock_path: directory.join("origin-feed.lock"),
            }
        } else {
            Self {
                journal_path: directory.join("journal.sqlite3"),
                journal_lock_path: directory.join("journal.lock"),
            }
        };
        store.initialize()?;
        store.maintain_progress_material(session_handle)?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), JournalError> {
        let _guard = open_lock_file(&self.journal_lock_path)?;
        _guard.lock_exclusive()?;
        let connection = self.open_connection()?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS operations (
                op_id TEXT PRIMARY KEY NOT NULL, session_handle TEXT NOT NULL,
                kind TEXT NOT NULL, state TEXT NOT NULL, message_id TEXT,
                request_revision INTEGER, in_reply_to TEXT, payload TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS v2_feed_cursors (
                scope TEXT PRIMARY KEY NOT NULL, position TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS v2_feed_resources (
                scope TEXT NOT NULL, resource TEXT NOT NULL, rev TEXT NOT NULL,
                payload TEXT NOT NULL, PRIMARY KEY(scope, resource)
             );
             CREATE TABLE IF NOT EXISTS v2_item_ids (
                sender TEXT NOT NULL, local_id TEXT NOT NULL, item_id TEXT NOT NULL,
                PRIMARY KEY(sender, local_id)
             );
             CREATE INDEX IF NOT EXISTS operations_by_session_message
                ON operations(session_handle, message_id, op_id);
             CREATE INDEX IF NOT EXISTS operations_pending_by_session
                ON operations(session_handle, state)
                WHERE state IN ('prepared', 'unknown');",
        )?;

        let schema: Option<String> = connection
            .query_row("SELECT value FROM meta WHERE key='schema'", [], |row| {
                row.get(0)
            })
            .optional()?;
        match schema.as_deref() {
            Some("2") => {}
            None => {
                let has_origin_cursor = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM meta WHERE key='cursor')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?;
                if has_origin_cursor {
                    return Err(JournalError::UnsupportedSchema);
                }
                connection.execute("INSERT INTO meta(key,value) VALUES('schema','2')", [])?;
                if let Some(parent) = self.journal_path.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            Some(_) => return Err(JournalError::UnsupportedSchema),
        }
        Ok(())
    }

    fn open_connection(&self) -> Result<Connection, JournalError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&self.journal_path)?;
        set_private_file(&file)?;
        drop(file);
        let connection = Connection::open(&self.journal_path)?;
        // Every Agent process opens the journal only while holding its
        // exclusive lock file, so SQLite reports busy only while some other
        // program holds the database. Wait for that holder to finish instead
        // of failing a journal step that may record a Server answer.
        connection.busy_handler(Some(wait_for_other_database_holder))?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        Ok(connection)
    }

    fn with_read<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, JournalError>,
    ) -> Result<T, JournalError> {
        let _guard = open_lock_file(&self.journal_lock_path)?;
        _guard.lock_exclusive()?;
        let connection = self.open_connection()?;
        read(&connection)
    }

    fn with_transaction<T>(
        &self,
        update: impl FnOnce(&Transaction<'_>) -> Result<T, JournalError>,
    ) -> Result<T, JournalError> {
        let _guard = open_lock_file(&self.journal_lock_path)?;
        _guard.lock_exclusive()?;
        let mut connection = self.open_connection()?;
        let tx = connection.transaction()?;
        let result = update(&tx)?;
        tx.commit()?;
        #[cfg(test)]
        if EXIT_AFTER_NEXT_COMMIT.get() {
            process::exit(73);
        }
        Ok(result)
    }

    /// Identifies this journal instance for callers recovering an intent.
    pub fn journal_id(&self) -> Result<String, JournalError> {
        self.with_transaction(|tx| {
            let current: Option<String> = tx
                .query_row("SELECT value FROM meta WHERE key='journal_id'", [], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(current) = current {
                return Ok(current);
            }
            let id = Uuid::new_v4().to_string();
            tx.execute("INSERT INTO meta(key,value) VALUES('journal_id',?1)", [&id])?;
            Ok(id)
        })
    }

    pub fn operation(&self, op_id: Uuid) -> Result<Option<OperationRecord>, JournalError> {
        self.with_read(|connection| read_operation(connection, op_id))
    }

    pub fn upload_staging_directory(&self, session_handle: Uuid, op_id: Uuid) -> PathBuf {
        self.journal_path
            .parent()
            .expect("journal path has a parent")
            .join("upload-staging")
            .join(session_handle.to_string())
            .join(op_id.to_string())
    }

    pub fn prepare_v2_operation(
        &mut self,
        op_id: Uuid,
        session_handle: Uuid,
        kind: String,
        request: Value,
        uploads: Vec<UploadSource>,
    ) -> Result<OperationRecord, JournalError> {
        self.with_transaction(|tx| {
            if let Some(existing) = read_operation(tx, op_id)? {
                if existing.session_handle != session_handle
                    || existing.kind != kind
                    || !existing.protocol_v2
                    || !operation_requests_equivalent(&existing.request, &request)
                {
                    return Err(JournalError::OperationConflict(op_id));
                }
                return Ok(existing);
            }
            if let Some(item_id) = request["revision_intent"].as_str() {
                let pending: Option<String> = tx
                    .query_row(
                        "SELECT op_id FROM operations
                     WHERE state IN ('prepared','unknown')
                       AND json_extract(payload,'$.request.revision_intent')=?1
                     LIMIT 1",
                        [item_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(pending) = pending {
                    let existing_key = Uuid::parse_str(&pending)
                        .map_err(|_| JournalError::OperationConflict(op_id))?;
                    return Err(JournalError::PendingRevision(existing_key));
                }
            }
            let now = now_ms();
            let operation = OperationRecord {
                op_id,
                session_handle,
                kind,
                request,
                state: OperationState::Prepared,
                created_at_ms: now,
                updated_at_ms: now,
                response: None,
                uploads,
                prepared_send: None,
                prepared_request: None,
                protocol_v2: true,
            };
            write_operation(tx, &operation)?;
            Ok(operation)
        })
    }

    /// Reserve the session and normalized send inputs under the supplied
    /// operation key. An identical lookup returns the original prepared send.
    pub fn prepare_v2_send(
        &mut self,
        lock: &OperationLock,
        session: Uuid,
        prepared: PreparedSend,
        uploads: Vec<UploadSource>,
    ) -> Result<OperationRecord, JournalError> {
        let op_id = lock.op_id;
        self.with_transaction(|tx| {
            if let Some(existing) = read_operation(tx, op_id)? {
                if existing.protocol_v2
                    && existing.kind == "send"
                    && existing
                        .prepared_send
                        .as_ref()
                        .is_some_and(|saved| saved.inputs == prepared.inputs)
                {
                    return Ok(existing);
                }
                return Err(JournalError::OperationConflict(op_id));
            }
            let now = now_ms();
            let operation = OperationRecord {
                op_id,
                session_handle: session,
                kind: "send".into(),
                request: prepared.request.clone(),
                state: OperationState::Prepared,
                created_at_ms: now,
                updated_at_ms: now,
                response: None,
                uploads,
                prepared_send: Some(prepared),
                prepared_request: None,
                protocol_v2: true,
            };
            write_operation(tx, &operation)?;
            Ok(operation)
        })
    }

    pub fn v2_item_id(&self, sender: &str, local_id: &str) -> Result<Option<String>, JournalError> {
        self.with_read(|connection| read_v2_item_id(connection, sender, local_id))
    }

    pub(crate) fn record_v2_supersedes(
        &self,
        op_id: Uuid,
        resolved: Value,
    ) -> Result<(), JournalError> {
        self.update_operation(op_id, |operation| {
            if operation.prepared_request.is_some() {
                return Err(JournalError::OperationConflict(op_id));
            }
            operation.request["_resolved_supersedes"] = resolved;
            Ok(true)
        })
    }

    pub(crate) fn record_v2_upload(
        &self,
        op_id: Uuid,
        upload_key: Uuid,
        file_id: String,
    ) -> Result<(), JournalError> {
        self.update_operation(op_id, |operation| {
            let upload = operation
                .uploads
                .iter_mut()
                .find(|upload| upload.upload_key == Some(upload_key))
                .ok_or(JournalError::OperationConflict(op_id))?;
            if let Some(previous) = &upload.upload_id {
                return if previous == &file_id {
                    Ok(false)
                } else {
                    Err(JournalError::OperationConflict(op_id))
                };
            }
            upload.upload_id = Some(file_id);
            Ok(true)
        })
    }

    fn update_operation(
        &self,
        op_id: Uuid,
        update: impl FnOnce(&mut OperationRecord) -> Result<bool, JournalError>,
    ) -> Result<(), JournalError> {
        self.with_transaction(|tx| {
            let mut operation =
                read_operation(tx, op_id)?.ok_or(JournalError::MissingOperation(op_id))?;
            if update(&mut operation)? {
                write_operation(tx, &operation)?;
            }
            Ok(())
        })
    }

    /// Serialize delivery and settlement for one local operation across
    /// processes. Other operation keys can advance while this request runs.
    pub fn lock_operation(&self, op_id: Uuid) -> Result<OperationLock, JournalError> {
        let path = self
            .journal_path
            .with_file_name(format!("operation-{op_id}.lock"));
        let file = open_lock_file(&path)?;
        file.lock_exclusive()?;
        Ok(OperationLock { op_id, _file: file })
    }

    /// Freeze the request and record that it may be sent in one commit.
    pub fn begin_v2_write(
        &mut self,
        lock: &OperationLock,
        request: PreparedRequest,
    ) -> Result<OperationRecord, JournalError> {
        let op_id = lock.op_id;
        self.with_transaction(|tx| {
            let mut operation =
                read_operation(tx, op_id)?.ok_or(JournalError::MissingOperation(op_id))?;
            if !operation.protocol_v2 {
                return Err(JournalError::OperationConflict(op_id));
            }
            if !matches!(
                operation.state,
                OperationState::Prepared | OperationState::Unknown
            ) {
                return Err(JournalError::NotUnresolved {
                    op_id,
                    state: operation_state_name(operation.state),
                });
            }
            if let Some(saved) = &operation.prepared_request {
                return if saved == &request {
                    Ok(operation)
                } else {
                    Err(JournalError::OperationConflict(op_id))
                };
            }
            operation.prepared_request = Some(request);
            operation.state = OperationState::Unknown;
            operation.updated_at_ms = now_ms();
            write_operation(tx, &operation)?;
            Ok(operation)
        })
    }

    pub fn mark_operation_unknown(&mut self, op_id: Uuid) -> Result<(), JournalError> {
        self.update_operation(op_id, |operation| {
            operation.state = OperationState::Unknown;
            operation.updated_at_ms = now_ms();
            Ok(true)
        })
    }

    pub fn settle_operation(&mut self, op_id: Uuid, response: Value) -> Result<(), JournalError> {
        let result = self.with_transaction(|tx| {
            let mut operation =
                read_operation(tx, op_id)?.ok_or(JournalError::MissingOperation(op_id))?;
            if operation.protocol_v2 && operation.kind == "send" {
                let sender = operation.request["sender"]
                    .as_str()
                    .ok_or(JournalError::OperationConflict(op_id))?;
                let item = &response["message"];
                let item_id = item["item_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .ok_or(JournalError::OperationConflict(op_id))?;
                let session = (!operation.session_handle.is_nil())
                    .then(|| operation.session_handle.to_string());
                if item["sender"] != sender || item["session"].as_str() != session.as_deref() {
                    return Err(JournalError::OperationConflict(op_id));
                }
                write_v2_item_id(tx, sender, &op_id.to_string(), item_id)?;
            }
            let session = operation.session_handle;
            operation.state = OperationState::Committed;
            operation.updated_at_ms = now_ms();
            if !is_progress(&operation.request["message"]) {
                operation.request = redact_file_payloads(&operation.request);
                redact_prepared_send(&mut operation);
                for upload in &mut operation.uploads {
                    upload.staged_path = None;
                }
            }
            operation.response = Some(redact_file_payloads(&response));
            write_operation(tx, &operation)?;
            Ok(())
        });
        if result.is_ok() {
            self.maintain_operation_progress(op_id);
        }
        result
    }

    pub fn reject_operation(&mut self, op_id: Uuid, response: Value) -> Result<(), JournalError> {
        let result = self.update_operation(op_id, |operation| {
            mark_rejected(operation, &response, false);
            Ok(true)
        });
        if result.is_ok() {
            self.maintain_operation_progress(op_id);
        }
        result
    }

    /// Give up an operation whose outcome was never resolved. It stops
    /// blocking new operations for its request and can no longer be retried.
    /// Only an unresolved (prepared or unknown) operation can be abandoned;
    /// abandoning one already abandoned is a no-op. The Server may or may not
    /// have applied it: that is the caller's explicit choice.
    pub fn abandon_operation(&mut self, op_id: Uuid) -> Result<OperationRecord, JournalError> {
        let result = self.update_operation(op_id, |operation| {
            match operation.state {
                OperationState::Prepared | OperationState::Unknown => {}
                OperationState::Rejected if operation_is_abandoned(operation) => {
                    return Ok(false);
                }
                state => {
                    return Err(JournalError::NotUnresolved {
                        op_id,
                        state: operation_state_name(state),
                    });
                }
            }
            mark_rejected(
                operation,
                &serde_json::json!({"kind":"abandoned","reason":"abandoned by explicit command"}),
                true,
            );
            Ok(true)
        });
        result?;
        self.maintain_operation_progress(op_id);
        self.operation(op_id)?
            .ok_or(JournalError::MissingOperation(op_id))
    }

    fn maintain_operation_progress(&self, op_id: Uuid) {
        if let Ok(Some(op)) = self.operation(op_id) {
            if let Some(id) = op.request["message"]["message_id"]
                .as_str()
                .or_else(|| op.request["message_id"].as_str())
            {
                let _ = self.prune_progress_request(op.session_handle, id);
            }
        }
    }
}

fn encode_sequence(sequence: u64) -> String {
    format!("{sequence:020}")
}

pub(crate) fn operation_request_revision(operation: &OperationRecord) -> Option<i64> {
    let revision = match operation.kind.as_str() {
        "send" => Some(1),
        "revise" => operation
            .response
            .as_ref()
            .and_then(|response| response["revision"].as_u64())
            .or_else(|| {
                operation.request["base_revision"]
                    .as_u64()
                    .and_then(|revision| revision.checked_add(1))
            }),
        _ => None,
    }?;
    i64::try_from(revision).ok()
}

fn mark_rejected(operation: &mut OperationRecord, response: &Value, remove_staged: bool) {
    operation.state = OperationState::Rejected;
    operation.updated_at_ms = now_ms();
    operation.request = redact_file_payloads(&operation.request);
    redact_prepared_send(operation);
    operation.response = Some(redact_file_payloads(response));
    if !is_progress(&operation.request["message"]) {
        for upload in &mut operation.uploads {
            if remove_staged {
                if let Some(path) = upload.staged_path.as_ref() {
                    let _ = fs::remove_file(path);
                }
            }
            upload.staged_path = None;
        }
    }
}

pub fn operation_is_abandoned(operation: &OperationRecord) -> bool {
    operation.state == OperationState::Rejected
        && operation
            .response
            .as_ref()
            .is_some_and(|response| response["kind"] == "abandoned")
}

fn operation_state_name(state: OperationState) -> &'static str {
    match state {
        OperationState::Prepared => "prepared",
        OperationState::Unknown => "unknown",
        OperationState::Committed => "committed",
        OperationState::Rejected => "rejected",
    }
}

fn write_operation(tx: &Transaction<'_>, operation: &OperationRecord) -> Result<(), JournalError> {
    let payload = serde_json::to_string(operation)?;
    let message_id = operation.request["message"]["message_id"]
        .as_str()
        .or_else(|| operation.request["message_id"].as_str());
    let request_revision = operation_request_revision(operation);
    let in_reply_to = operation.request["in_reply_to"].as_str();
    tx.execute(
        "INSERT INTO operations(
            op_id,session_handle,kind,state,message_id,request_revision,in_reply_to,payload
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(op_id) DO UPDATE SET session_handle=excluded.session_handle,
            kind=excluded.kind,state=excluded.state,message_id=excluded.message_id,
            request_revision=excluded.request_revision,in_reply_to=excluded.in_reply_to,
            payload=excluded.payload",
        params![
            operation.op_id.to_string(),
            operation.session_handle.to_string(),
            operation.kind,
            operation_state_name(operation.state),
            message_id,
            request_revision,
            in_reply_to,
            payload
        ],
    )?;
    Ok(())
}

fn read_v2_item_id(
    connection: &Connection,
    sender: &str,
    local_id: &str,
) -> Result<Option<String>, JournalError> {
    connection
        .query_row(
            "SELECT item_id FROM v2_item_ids WHERE sender=?1 AND local_id=?2",
            params![sender, local_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(JournalError::from)
}

fn write_v2_item_id(
    tx: &Transaction<'_>,
    sender: &str,
    local_id: &str,
    item_id: &str,
) -> Result<(), JournalError> {
    if let Some(saved) = read_v2_item_id(tx, sender, local_id)? {
        if saved != item_id {
            return Err(JournalError::ItemBindingConflict);
        }
        return Ok(());
    }
    tx.execute(
        "INSERT INTO v2_item_ids(sender,local_id,item_id) VALUES(?1,?2,?3)",
        params![sender, local_id, item_id],
    )?;
    Ok(())
}

fn read_operation(
    connection: &Connection,
    op_id: Uuid,
) -> Result<Option<OperationRecord>, JournalError> {
    let payload: Option<String> = connection
        .query_row(
            "SELECT payload FROM operations WHERE op_id=?1",
            [op_id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    payload
        .map(|payload| serde_json::from_str(&payload).map_err(JournalError::from))
        .transpose()
}

fn strip_file_payloads(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Array(files)) = object.get_mut("files") {
                for file in files {
                    if let Some(file) = file.as_object_mut() {
                        if let Some(data) = file.remove("data_base64") {
                            if let Some(encoded) = data.as_str() {
                                if let Ok(bytes) = STANDARD.decode(encoded) {
                                    file.entry("size")
                                        .or_insert(Value::from(bytes.len() as u64));
                                    file.entry("sha256").or_insert_with(|| {
                                        Value::String(hex::encode(Sha256::digest(bytes)))
                                    });
                                }
                            }
                        }
                    }
                }
            }
            for child in object.values_mut() {
                strip_file_payloads(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                strip_file_payloads(child);
            }
        }
        _ => {}
    }
}

fn redact_file_payloads(value: &Value) -> Value {
    let mut redacted = value.clone();
    strip_file_payloads(&mut redacted);
    redacted
}

fn redact_prepared_send(operation: &mut OperationRecord) {
    if let Some(prepared_send) = operation.prepared_send.as_mut() {
        prepared_send.request = redact_file_payloads(&prepared_send.request);
    }
}

fn operation_requests_equivalent(left: &Value, right: &Value) -> bool {
    redact_file_payloads(left) == redact_file_payloads(right)
}

fn wait_for_other_database_holder(_attempts: i32) -> bool {
    thread::sleep(OTHER_HOLDER_RECHECK);
    true
}

fn open_lock_file(path: &Path) -> Result<File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    set_private_file(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
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
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file(_file: &File) -> Result<(), std::io::Error> {
    Ok(())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("harklight-agent-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn journal_waits_for_another_program_holding_the_database_past_the_old_limit() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let store = JournalStore::open(&scratch.0, "device", session).unwrap();

        // Another program (for example an inspecting sqlite3 shell) holds the
        // database longer than the journal used to tolerate.
        let holder = Connection::open(&store.journal_path).unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        let hold = Duration::from_secs(8);
        // Start the clock before the holder's countdown so the release can
        // never happen sooner than `hold` after it, however threads are
        // scheduled.
        let started = std::time::Instant::now();
        let release = thread::spawn(move || {
            thread::sleep(hold);
            holder.execute_batch("COMMIT;").unwrap();
        });

        let journal_id = store.journal_id().unwrap();
        assert!(started.elapsed() >= hold);
        release.join().unwrap();
        assert_eq!(store.journal_id().unwrap(), journal_id);
    }

    fn open(root: &Scratch, session: Uuid) -> JournalStore {
        JournalStore::open(&root.0, "enrolled-device", session).unwrap()
    }

    #[test]
    fn agent_02_state_reopens_with_its_journal_and_lock() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let store = open(&root, session);
        assert_eq!(store.journal_path.file_name().unwrap(), "journal.sqlite3");
        assert_eq!(store.journal_lock_path.file_name().unwrap(), "journal.lock");

        let legacy_tables = |journal: &JournalStore| {
            journal
                .with_read(|db| {
                    Ok(db.query_row(
                        "SELECT count(*) FROM sqlite_schema
                     WHERE type='table' AND name IN ('entries','advisory_cursors','advisories')",
                        [],
                        |row| row.get::<_, u32>(0),
                    )?)
                })
                .unwrap()
        };
        assert_eq!(legacy_tables(&store), 0);

        // Reproduce the 0.2 paths and the metadata present in older journals.
        let existing = JournalStore {
            journal_path: store.journal_path.with_file_name("origin-feed.sqlite3"),
            journal_lock_path: store.journal_lock_path.with_file_name("origin-feed.lock"),
        };
        fs::rename(&store.journal_path, &existing.journal_path).unwrap();
        fs::rename(&store.journal_lock_path, &existing.journal_lock_path).unwrap();
        existing
            .with_transaction(|tx| {
                tx.execute_batch(
                    "INSERT INTO meta(key,value) VALUES('cursor','00000000000000000008');
                 CREATE TABLE entries (payload TEXT);
                 INSERT INTO entries VALUES ('held-entry');
                 CREATE TABLE advisories (payload TEXT);
                 INSERT INTO advisories VALUES ('held-advisory');",
                )?;
                Ok(())
            })
            .unwrap();
        existing
            .record_v2_feed("upgrade", None, 1, &[], false)
            .unwrap();
        let journal_id = existing.journal_id().unwrap();

        let reopened = open(&root, session);
        assert_eq!(reopened.journal_path, existing.journal_path);
        assert_eq!(reopened.journal_lock_path, existing.journal_lock_path);
        assert_eq!(reopened.journal_id().unwrap(), journal_id);
        assert_eq!(reopened.v2_feed_cursor("upgrade").unwrap(), Some(1));
        reopened
            .record_v2_feed("upgrade", Some(1), 2, &[], false)
            .unwrap();
        assert_eq!(existing.v2_feed_cursor("upgrade").unwrap(), Some(2));
        assert_eq!(legacy_tables(&reopened), 2);
        reopened
            .with_read(|db| {
                for sql in [
                    "SELECT payload FROM entries WHERE payload='held-entry'",
                    "SELECT payload FROM advisories WHERE payload='held-advisory'",
                    "SELECT value FROM meta WHERE key='cursor' AND value='00000000000000000008'",
                ] {
                    db.query_row(sql, [], |row| row.get::<_, String>(0))?;
                }
                Ok(())
            })
            .unwrap();
        assert!(!store.journal_path.exists());
    }

    #[test]
    fn v2_send_lookup_and_server_item_binding_survive_a_reopened_journal() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let key = Uuid::new_v4();
        let sender = "app:example";
        let prepared = PreparedSend {
            inputs: json!({"session":"new","sender":sender,"message":{"title":"Example"}}),
            request: json!({"session":session,"sender":sender,"message":{"title":"Example"}}),
            files: Vec::new(),
        };
        {
            let mut store = open(&root, session);
            let lock = store.lock_operation(key).unwrap();
            let operation = store
                .prepare_v2_send(&lock, session, prepared.clone(), Vec::new())
                .unwrap();
            assert_eq!(operation.session_handle, session);
            assert_eq!(store.v2_item_id(sender, &key.to_string()).unwrap(), None);
            let recovered = store
                .prepare_v2_send(&lock, Uuid::new_v4(), prepared.clone(), Vec::new())
                .unwrap();
            assert_eq!(recovered.session_handle, session);
            let mut changed = prepared.clone();
            changed.inputs["sender"] = "app:changed".into();
            assert!(matches!(
                store.prepare_v2_send(&lock, session, changed, Vec::new()),
                Err(JournalError::OperationConflict(id)) if id == key
            ));
            let admitted = json!({"rev":2,"message":{"sender":sender,"session":session,"item_id":"server/opaque"}});
            let wrong_sender = json!({"rev":2,"message":{"sender":"app:other","session":session,"item_id":"server/opaque"}});
            assert!(store.settle_operation(key, wrong_sender).is_err());
            assert_eq!(store.v2_item_id(sender, &key.to_string()).unwrap(), None);
            store.settle_operation(key, admitted).unwrap();
        }
        let store = open(&root, session);
        let saved = store.operation(key).unwrap().unwrap();
        assert_eq!(saved.state, OperationState::Committed);
        assert_eq!(
            store
                .v2_item_id(sender, &key.to_string())
                .unwrap()
                .as_deref(),
            Some("server/opaque")
        );
        assert_eq!(
            store.v2_item_id("app:other", &key.to_string()).unwrap(),
            None
        );
    }

    fn sample_wire_request() -> PreparedRequest {
        PreparedRequest {
            method: RequestMethod::Put,
            path: vec!["agent".into(), "messages".into(), "opaque/id".into()],
            if_match: Some(r#""1""#.into()),
            body: r#"{ "message" : {"title":"test"} }"#.into(),
        }
    }

    #[test]
    fn v2_write_freezes_wire_request_and_key_before_delivery() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let key = Uuid::new_v4();
        let request = sample_wire_request();
        {
            let mut store = open(&root, session);
            store
                .prepare_v2_operation(key, session, "revise".into(), json!({}), Vec::new())
                .unwrap();
            let lock = store.lock_operation(key).unwrap();
            let operation = store.begin_v2_write(&lock, request.clone()).unwrap();
            assert_eq!(operation.state, OperationState::Unknown);
        }

        let mut store = open(&root, session);
        let operation = store.operation(key).unwrap().unwrap();
        assert_eq!(operation.op_id, key);
        assert_eq!(operation.state, OperationState::Unknown);
        assert_eq!(operation.prepared_request, Some(request.clone()));
        let mut changed = request.clone();
        changed.body = changed.body.replace(" : ", ":");
        let lock = store.lock_operation(key).unwrap();
        assert!(matches!(
            store.begin_v2_write(&lock, changed),
            Err(JournalError::OperationConflict(id)) if id == key
        ));
        assert_eq!(
            store
                .begin_v2_write(&lock, request.clone())
                .unwrap()
                .prepared_request,
            Some(request),
        );
    }

    #[test]
    fn v2_write_does_not_retarget_an_earlier_protocol_operation() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let key = Uuid::new_v4();
        let mut store = open(&root, session);
        // Restore a persisted operation as it was written before the v2 marker.
        let operation: OperationRecord = serde_json::from_value(json!({
            "op_id":key,"session_handle":session,"kind":"send","request":{"v":1},
            "state":"prepared","created_at_ms":0,"updated_at_ms":0
        }))
        .unwrap();
        store
            .with_transaction(|tx| write_operation(tx, &operation))
            .unwrap();

        let lock = store.lock_operation(key).unwrap();
        for state in [OperationState::Prepared, OperationState::Unknown] {
            assert_eq!(store.operation(key).unwrap().unwrap().state, state);
            assert!(matches!(
                store.begin_v2_write(&lock, sample_wire_request()),
                Err(JournalError::OperationConflict(id)) if id == key
            ));
            assert!(
                store
                    .operation(key)
                    .unwrap()
                    .unwrap()
                    .prepared_request
                    .is_none()
            );
            store.mark_operation_unknown(key).unwrap();
        }
    }

    #[test]
    fn v2_write_admission_refuses_settled_operations() {
        let root = Scratch::new();
        let mut store = open(&root, Uuid::nil());
        for terminal in ["committed", "refused", "abandoned"] {
            let key = Uuid::new_v4();
            let wire = sample_wire_request();
            store
                .prepare_v2_operation(key, Uuid::nil(), "revise".into(), json!({}), Vec::new())
                .unwrap();
            let lock = store.lock_operation(key).unwrap();
            store.begin_v2_write(&lock, wire.clone()).unwrap();
            match terminal {
                "committed" => store.settle_operation(key, json!({})).unwrap(),
                "refused" => store.reject_operation(key, json!({})).unwrap(),
                "abandoned" => {
                    store.abandon_operation(key).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(matches!(
                store.begin_v2_write(&lock, wire),
                Err(JournalError::NotUnresolved { op_id, .. }) if op_id == key
            ));
        }
    }

    #[test]
    fn v2_freeze_survives_process_exit_at_its_first_commit() {
        const TEST_ROOT: &str = "HARKLIGHT_TEST_FREEZE_ROOT";
        const TEST_KEY: &str = "HARKLIGHT_TEST_FREEZE_KEY";
        if let (Some(root), Ok(key)) = (std::env::var_os(TEST_ROOT), std::env::var(TEST_KEY)) {
            let mut store = JournalStore::open(Path::new(&root), "device", Uuid::nil()).unwrap();
            let lock = store.lock_operation(key.parse().unwrap()).unwrap();
            EXIT_AFTER_NEXT_COMMIT.set(true);
            store.begin_v2_write(&lock, sample_wire_request()).unwrap();
            panic!("child should exit at the first commit");
        }

        let root = Scratch::new();
        let key = Uuid::new_v4();
        let mut store = JournalStore::open(&root.0, "device", Uuid::nil()).unwrap();
        store
            .prepare_v2_operation(key, Uuid::nil(), "revise".into(), json!({}), Vec::new())
            .unwrap();
        let output = process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "journal::tests::v2_freeze_survives_process_exit_at_its_first_commit",
            ])
            .env(TEST_ROOT, &root.0)
            .env(TEST_KEY, key.to_string())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(73), "{output:?}");

        let reopened = JournalStore::open(&root.0, "device", Uuid::nil()).unwrap();
        let recorded = reopened.operation(key).unwrap().unwrap();
        assert_eq!(recorded.state, OperationState::Unknown);
        assert_eq!(recorded.prepared_request, Some(sample_wire_request()));
    }

    #[test]
    fn operation_key_is_durable_before_retry_and_conflicts_on_changed_payload() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        {
            let mut store = open(&root, session);
            store
                .prepare_v2_operation(
                    op_id,
                    session,
                    "reply".into(),
                    json!({"body": "one"}),
                    Vec::new(),
                )
                .unwrap();
            store.mark_operation_unknown(op_id).unwrap();
        }

        let mut reopened = open(&root, session);
        assert_eq!(
            reopened.operation(op_id).unwrap().unwrap().state,
            OperationState::Unknown
        );
        assert_eq!(
            reopened
                .prepare_v2_operation(
                    op_id,
                    session,
                    "reply".into(),
                    json!({"body": "one"}),
                    Vec::new()
                )
                .unwrap()
                .state,
            OperationState::Unknown
        );
        assert!(matches!(
            reopened.prepare_v2_operation(op_id, session, "reply".into(), json!({"body": "two"}), Vec::new()),
            Err(JournalError::OperationConflict(id)) if id == op_id
        ));
    }

    #[test]
    fn a_reused_operation_key_returns_the_one_recorded_operation() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let request = json!({"entry_id":"1"});
        let mut first = open(&root, session);
        let mut second = open(&root, session);
        let recorded = first
            .prepare_v2_operation(
                op_id,
                session,
                "receipt".into(),
                request.clone(),
                Vec::new(),
            )
            .unwrap();
        let again = second
            .prepare_v2_operation(op_id, session, "receipt".into(), request, Vec::new())
            .unwrap();
        assert_eq!(recorded.created_at_ms, again.created_at_ms);
        let operation_count = first
            .with_read(|db| {
                Ok(db.query_row("SELECT count(*) FROM operations", [], |row| {
                    row.get::<_, u32>(0)
                })?)
            })
            .unwrap();
        assert_eq!(operation_count, 1);
        assert!(
            second
                .prepare_v2_operation(
                    op_id,
                    session,
                    "receipt".into(),
                    json!({"entry_id":"2"}),
                    Vec::new(),
                )
                .is_err()
        );
    }
}
