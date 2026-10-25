use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process, thread,
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

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("journal database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("journal schema {0} is not supported")]
    UnsupportedSchema(u32),
    #[error("journal database is missing its cursor")]
    MissingCursor,
    #[error("journal database cursor is invalid")]
    InvalidCursor,
    #[error("feed sequence {sequence} does not follow cursor {cursor}")]
    FeedGap { cursor: u64, sequence: u64 },
    #[error("feed sequence {sequence} was replayed with different content")]
    FeedConflict { sequence: u64 },
    #[error("operation key {0} was already used with different content")]
    OperationConflict(Uuid),
    #[error("journal has no operation with key {0}")]
    MissingOperation(Uuid),
    #[error("listener already active (pid {0})")]
    ListenerActive(u32),
    #[error("listener lock is held but its process id is not available")]
    ListenerActiveUnknown,
    #[error("journal has no entry for event {0}")]
    MissingEvent(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    schema: u32,
    pub cursor: u64,
    pub entries: BTreeMap<u64, JournalEntry>,
    pub operations: BTreeMap<Uuid, OperationRecord>,
}

impl Journal {
    fn empty() -> Self {
        Self {
            schema: 1,
            cursor: 0,
            entries: BTreeMap::new(),
            operations: BTreeMap::new(),
        }
    }

    pub fn pending_for(&self, session_handle: Uuid) -> Vec<&JournalEntry> {
        let closed_requests: std::collections::BTreeSet<_> = self
            .entries
            .values()
            .filter(|entry| entry.kind == "terminal")
            .filter_map(JournalEntry::request_key)
            .collect();
        let answered_requests: std::collections::BTreeSet<_> = self
            .entries
            .values()
            .filter(|entry| entry.kind == "terminal" && entry.raw["terminal_state"] == "answered")
            .filter_map(JournalEntry::request_key)
            .collect();

        self.entries
            .values()
            .filter(|entry| entry.session_handle == session_handle)
            .filter(|entry| entry.actionable && entry.acted_at_ms.is_none())
            .filter(|entry| match entry.kind.as_str() {
                "discussion" => entry.request_key().is_none_or(|key| {
                    !closed_requests.contains(&key)
                        || (entry.raw["channel"] == "chat" && answered_requests.contains(&key))
                }),
                "terminal" if entry.raw["terminal_state"] != "answered" => {
                    entry.delivered_at_ms.is_none()
                }
                "terminal" => true,
                _ => false,
            })
            .collect()
    }

    pub fn entry_by_event(&self, event_id: &str) -> Option<&JournalEntry> {
        self.entries
            .values()
            .find(|entry| entry.event_id == event_id)
    }

    pub fn unknown_operations(&self) -> impl Iterator<Item = &OperationRecord> {
        self.operations.values().filter(|operation| {
            matches!(
                operation.state,
                OperationState::Prepared | OperationState::Unknown
            )
        })
    }

    pub fn unknown_operations_for(
        &self,
        session_handle: Uuid,
    ) -> impl Iterator<Item = &OperationRecord> {
        self.unknown_operations()
            .filter(move |operation| operation.session_handle == session_handle)
    }

    pub fn pending_acknowledgements_for(
        &self,
        session_handle: Uuid,
    ) -> impl Iterator<Item = &JournalEntry> {
        self.entries.values().filter(move |entry| {
            entry.session_handle == session_handle
                && entry.ack_pending_at_ms.is_some()
                && entry.delivered_at_ms.is_none()
        })
    }

    pub fn latest_request(
        &self,
        session_handle: Uuid,
        sender_id: &str,
        message_id: &str,
    ) -> Option<&JournalEntry> {
        self.entries.values().rev().find(|entry| {
            entry.session_handle == session_handle
                && entry.sender_id.as_deref() == Some(sender_id)
                && entry.request_id.as_deref() == Some(message_id)
                && entry.kind == "revision"
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub feed_seq: u64,
    pub event_id: String,
    pub session_handle: Uuid,
    pub kind: String,
    pub author: String,
    pub actionable: bool,
    pub sender_id: Option<String>,
    pub request_id: Option<String>,
    pub request_revision: Option<u64>,
    pub raw: Value,
    pub journaled_at_ms: u64,
    pub emitted_at_ms: Option<u64>,
    pub ack_pending_at_ms: Option<u64>,
    pub delivered_at_ms: Option<u64>,
    pub acted_at_ms: Option<u64>,
    pub file_transfers: BTreeMap<String, FileTransfer>,
}

impl JournalEntry {
    pub fn fetched(
        feed_seq: u64,
        event_id: String,
        session_handle: Uuid,
        kind: String,
        author: String,
        actionable: bool,
        sender_id: Option<String>,
        request_id: Option<String>,
        request_revision: Option<u64>,
        raw: Value,
    ) -> Self {
        Self {
            feed_seq,
            event_id,
            session_handle,
            kind,
            actionable: actionable && author != "origin",
            author,
            sender_id,
            request_id,
            request_revision,
            raw,
            journaled_at_ms: now_ms(),
            emitted_at_ms: None,
            ack_pending_at_ms: None,
            delivered_at_ms: None,
            acted_at_ms: None,
            file_transfers: BTreeMap::new(),
        }
    }

    pub fn own_entry(&self) -> bool {
        self.author == "origin"
    }

    fn request_key(&self) -> Option<(Uuid, String, String)> {
        Some((
            self.session_handle,
            self.sender_id.clone()?,
            self.request_id.clone()?,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileTransfer {
    pub path: Option<PathBuf>,
    pub transferred: bool,
    pub reason: Option<String>,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Prepared,
    Unknown,
    Committed,
    Rejected,
}

pub struct JournalStore {
    journal_path: PathBuf,
    legacy_journal_path: PathBuf,
    journal_lock_path: PathBuf,
    listener_lock_path: PathBuf,
    listener_lock: Option<File>,
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
        let store = Self {
            journal_path: directory.join("origin-feed.sqlite3"),
            legacy_journal_path: directory.join("origin-feed.json"),
            journal_lock_path: directory.join("origin-feed.lock"),
            listener_lock_path: directory.join(format!("{session_handle}.listener.lock")),
            listener_lock: None,
        };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<(), JournalError> {
        let _guard = open_lock_file(&self.journal_lock_path)?;
        _guard.lock_exclusive()?;
        let mut connection = self.open_connection()?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS entries (
                feed_seq TEXT PRIMARY KEY NOT NULL, event_id TEXT NOT NULL UNIQUE,
                session_handle TEXT NOT NULL, kind TEXT NOT NULL, author TEXT NOT NULL,
                actionable INTEGER NOT NULL, sender_id TEXT, request_id TEXT,
                terminal_state TEXT, in_reply_to TEXT, result_status TEXT,
                pending_active INTEGER NOT NULL,
                emitted_at_ms INTEGER, ack_pending_at_ms INTEGER, delivered_at_ms INTEGER,
                acted_at_ms INTEGER, payload TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS entries_by_session_request
                ON entries(session_handle, sender_id, request_id, feed_seq);
             CREATE INDEX IF NOT EXISTS entries_by_session_message
                ON entries(session_handle, request_id, feed_seq);
             CREATE INDEX IF NOT EXISTS entries_pending_by_session
                ON entries(session_handle, feed_seq) WHERE pending_active = 1;
             CREATE INDEX IF NOT EXISTS entries_ack_pending_by_session
                ON entries(session_handle, feed_seq)
                WHERE ack_pending_at_ms IS NOT NULL AND delivered_at_ms IS NULL;
             CREATE TABLE IF NOT EXISTS operations (
                op_id TEXT PRIMARY KEY NOT NULL, session_handle TEXT NOT NULL,
                kind TEXT NOT NULL, state TEXT NOT NULL, message_id TEXT,
                request_revision INTEGER, in_reply_to TEXT, payload TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS operations_by_session_message
                ON operations(session_handle, message_id, op_id);
             CREATE INDEX IF NOT EXISTS operations_pending_by_session
                ON operations(session_handle, state)
                WHERE state IN ('prepared', 'unknown');",
        )?;

        ensure_column(&connection, "entries", "in_reply_to", "TEXT")?;
        ensure_column(&connection, "entries", "result_status", "TEXT")?;
        ensure_column(&connection, "operations", "request_revision", "INTEGER")?;
        ensure_column(&connection, "operations", "in_reply_to", "TEXT")?;

        let initialized: Option<String> = connection
            .query_row("SELECT value FROM meta WHERE key='cursor'", [], |row| {
                row.get(0)
            })
            .optional()?;
        if initialized.is_none() {
            let legacy = if self.legacy_journal_path.exists() {
                let bytes = fs::read(&self.legacy_journal_path)?;
                let journal: Journal = serde_json::from_slice(&bytes)?;
                if journal.schema != 1 {
                    return Err(JournalError::UnsupportedSchema(journal.schema));
                }
                journal
            } else {
                Journal::empty()
            };
            let tx = connection.transaction()?;
            for entry in legacy.entries.values() {
                write_entry(&tx, entry)?;
                if entry.kind == "terminal" {
                    if let Some((sender_id, request_id)) =
                        entry.sender_id.as_deref().zip(entry.request_id.as_deref())
                    {
                        close_pending_discussions(
                            &tx,
                            entry.session_handle,
                            sender_id,
                            request_id,
                        )?;
                    }
                }
            }
            for operation in legacy.operations.values() {
                write_operation(&tx, operation)?;
            }
            tx.execute(
                "INSERT INTO meta(key,value) VALUES('schema','2')
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [],
            )?;
            write_cursor(&tx, legacy.cursor)?;
            tx.commit()?;
            if let Some(parent) = self.journal_path.parent() {
                File::open(parent)?.sync_all()?;
            }
        } else {
            let schema: Option<String> = connection
                .query_row("SELECT value FROM meta WHERE key='schema'", [], |row| {
                    row.get(0)
                })
                .optional()?;
            if schema
                .as_deref()
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(1)
                < 2
            {
                migrate_request_indexes(&mut connection)?;
            }
        }

        connection.execute_batch(
            "CREATE INDEX IF NOT EXISTS entries_by_session_request_kind
                ON entries(session_handle, request_id, kind, feed_seq DESC);
             CREATE INDEX IF NOT EXISTS entries_result_by_target
                ON entries(session_handle, request_id, in_reply_to, result_status, feed_seq DESC)
                WHERE kind='result';
             CREATE INDEX IF NOT EXISTS operations_pending_by_request
                ON operations(session_handle, message_id, op_id)
                WHERE state IN ('prepared', 'unknown');
             CREATE INDEX IF NOT EXISTS operations_committed_revision_by_request
                ON operations(session_handle, message_id, request_revision DESC)
                WHERE state='committed' AND kind IN ('send', 'revise');",
        )?;
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
        connection.busy_timeout(Duration::from_secs(5))?;
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
        Ok(result)
    }

    pub fn cursor(&self) -> Result<u64, JournalError> {
        self.with_read(read_cursor)
    }

    pub fn snapshot(&self) -> Result<Journal, JournalError> {
        self.with_read(|connection| {
            let mut journal = Journal {
                schema: 1,
                cursor: read_cursor(connection)?,
                entries: BTreeMap::new(),
                operations: BTreeMap::new(),
            };
            let mut statement =
                connection.prepare("SELECT payload FROM entries ORDER BY feed_seq")?;
            let payloads = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for payload in payloads {
                let entry: JournalEntry = serde_json::from_str(&payload)?;
                journal.entries.insert(entry.feed_seq, entry);
            }
            let mut statement = connection.prepare("SELECT payload FROM operations")?;
            let payloads = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for payload in payloads {
                let operation: OperationRecord = serde_json::from_str(&payload)?;
                journal.operations.insert(operation.op_id, operation);
            }
            Ok(journal)
        })
    }

    pub fn request_snapshot(
        &self,
        session_handle: Uuid,
        message_id: &str,
        target_event_id: Option<&str>,
    ) -> Result<Journal, JournalError> {
        self.with_read(|connection| {
            let mut journal = Journal {
                schema: 1,
                cursor: read_cursor(connection)?,
                entries: BTreeMap::new(),
                operations: BTreeMap::new(),
            };
            let session = session_handle.to_string();

            for kind in ["revision", "terminal"] {
                let payload: Option<String> = connection
                    .query_row(
                        "SELECT payload FROM entries
                         WHERE session_handle=?1 AND request_id=?2 AND kind=?3
                         ORDER BY feed_seq DESC LIMIT 1",
                        params![session, message_id, kind],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(payload) = payload {
                    let entry: JournalEntry = serde_json::from_str(&payload)?;
                    journal.entries.insert(entry.feed_seq, entry);
                }
            }

            if let Some(event_id) = target_event_id {
                if let Some(entry) = read_entry_by_event(connection, event_id)? {
                    journal.entries.insert(entry.feed_seq, entry);
                }

                let payload: Option<String> = connection
                    .query_row(
                        "SELECT payload FROM entries
                         WHERE session_handle=?1 AND request_id=?2 AND kind='result'
                           AND in_reply_to=?3 AND result_status='received'
                         ORDER BY feed_seq DESC LIMIT 1",
                        params![session, message_id, event_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(payload) = payload {
                    let entry: JournalEntry = serde_json::from_str(&payload)?;
                    journal.entries.insert(entry.feed_seq, entry);
                }
            }

            let current_operation: Option<String> = connection
                .query_row(
                    "SELECT payload FROM operations
                     WHERE session_handle=?1 AND message_id=?2 AND state='committed'
                       AND kind IN ('send', 'revise') AND request_revision IS NOT NULL
                     ORDER BY request_revision DESC LIMIT 1",
                    params![session, message_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(payload) = current_operation {
                let operation: OperationRecord = serde_json::from_str(&payload)?;
                journal.operations.insert(operation.op_id, operation);
            }

            let pending_operation: Option<String> = connection
                .query_row(
                    "SELECT payload FROM operations
                     WHERE session_handle=?1 AND message_id=?2
                       AND state IN ('prepared', 'unknown')
                     ORDER BY op_id LIMIT 1",
                    params![session, message_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(payload) = pending_operation {
                let operation: OperationRecord = serde_json::from_str(&payload)?;
                journal.operations.insert(operation.op_id, operation);
            }

            Ok(journal)
        })
    }

    pub fn entry_by_event(&self, event_id: &str) -> Result<Option<JournalEntry>, JournalError> {
        self.with_read(|connection| read_entry_by_event(connection, event_id))
    }

    pub fn operation(&self, op_id: Uuid) -> Result<Option<OperationRecord>, JournalError> {
        self.with_read(|connection| read_operation(connection, op_id))
    }

    /// Return operations for one origin session in the order they were first
    /// journaled. This lets multi-step commands resume the exact pending step
    /// before creating a later operation.
    pub fn operations_for_session(
        &self,
        session_handle: Uuid,
    ) -> Result<Vec<OperationRecord>, JournalError> {
        self.with_read(|connection| {
            let mut statement = connection
                .prepare("SELECT payload FROM operations WHERE session_handle=?1 ORDER BY rowid")?;
            let payloads = statement
                .query_map([session_handle.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            payloads
                .into_iter()
                .map(|payload| serde_json::from_str(&payload).map_err(JournalError::from))
                .collect()
        })
    }

    pub fn pending_for(&self, session_handle: Uuid) -> Result<Vec<JournalEntry>, JournalError> {
        self.with_read(|connection| read_entry_list(
            connection,
            "SELECT payload FROM entries WHERE session_handle=?1 AND pending_active=1 ORDER BY feed_seq",
            &session_handle.to_string(),
        ))
    }

    pub fn pending_acknowledgements_for(
        &self,
        session_handle: Uuid,
    ) -> Result<Vec<JournalEntry>, JournalError> {
        self.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT payload FROM entries WHERE session_handle=?1 AND ack_pending_at_ms IS NOT NULL AND delivered_at_ms IS NULL ORDER BY feed_seq",
            )?;
            let payloads = statement.query_map([session_handle.to_string()], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            payloads.into_iter().map(|payload| serde_json::from_str(&payload).map_err(JournalError::from)).collect()
        })
    }

    pub fn unknown_operations_for(
        &self,
        session_handle: Uuid,
    ) -> Result<Vec<OperationRecord>, JournalError> {
        self.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT payload FROM operations WHERE session_handle=?1 AND state IN ('prepared','unknown') ORDER BY op_id",
            )?;
            let payloads = statement.query_map([session_handle.to_string()], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            payloads.into_iter().map(|payload| serde_json::from_str(&payload).map_err(JournalError::from)).collect()
        })
    }

    pub fn event_file_directory(&self, session_handle: Uuid, event_id: &str) -> PathBuf {
        self.journal_path
            .parent()
            .expect("journal path has a parent")
            .join("files")
            .join(session_handle.to_string())
            .join(event_id)
    }

    pub fn upload_staging_directory(&self, session_handle: Uuid, op_id: Uuid) -> PathBuf {
        self.journal_path
            .parent()
            .expect("journal path has a parent")
            .join("upload-staging")
            .join(session_handle.to_string())
            .join(op_id.to_string())
    }

    pub fn acquire_listener(&mut self) -> Result<(), JournalError> {
        if self.listener_lock.is_some() {
            return Ok(());
        }
        let lock_file = open_lock_file(&self.listener_lock_path)?;
        if let Err(error) = lock_file.try_lock_exclusive() {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(match read_lock_pid(&self.listener_lock_path) {
                    Some(pid) => JournalError::ListenerActive(pid),
                    None => JournalError::ListenerActiveUnknown,
                });
            }
            return Err(error.into());
        }
        let mut lock_file = lock_file;
        lock_file.set_len(0)?;
        writeln!(&mut lock_file, "{}", process::id())?;
        lock_file.sync_all()?;
        self.listener_lock = Some(lock_file);
        Ok(())
    }

    pub fn record_entry(&mut self, entry: JournalEntry) -> Result<bool, JournalError> {
        self.with_transaction(|tx| {
            let cursor = read_cursor(tx)?;
            if entry.feed_seq <= cursor {
                let mut existing =
                    read_entry_by_seq(tx, entry.feed_seq)?.ok_or(JournalError::FeedConflict {
                        sequence: entry.feed_seq,
                    })?;
                if existing.event_id != entry.event_id
                    || existing.session_handle != entry.session_handle
                    || existing.kind != entry.kind
                    || existing.author != entry.author
                    || immutable_entry(&existing.raw) != immutable_entry(&entry.raw)
                {
                    return Err(JournalError::FeedConflict {
                        sequence: entry.feed_seq,
                    });
                }
                let changed = merge_server_facts(&mut existing, &entry)
                    | merge_local_file_transfers(&mut existing, &entry);
                let reconciled = reconcile_own_entry(tx, &entry)?;
                if changed {
                    write_entry(tx, &existing)?;
                }
                return Ok(changed || reconciled);
            }
            if cursor.checked_add(1) != Some(entry.feed_seq) {
                return Err(JournalError::FeedGap {
                    cursor,
                    sequence: entry.feed_seq,
                });
            }
            let mut entry = entry;
            merge_server_facts_into_new(&mut entry);
            write_entry(tx, &entry)?;
            if entry.kind == "terminal" {
                if let Some((sender, request)) =
                    entry.sender_id.as_deref().zip(entry.request_id.as_deref())
                {
                    close_pending_discussions(tx, entry.session_handle, sender, request)?;
                }
            }
            write_cursor(tx, entry.feed_seq)?;
            reconcile_own_entry(tx, &entry)?;
            Ok(true)
        })
    }

    pub fn mark_emitted(&mut self, event_id: &str) -> Result<(), JournalError> {
        self.update_entry(event_id, |entry| {
            entry.emitted_at_ms.get_or_insert_with(now_ms);
        })
    }

    pub fn mark_delivered(&mut self, event_id: &str) -> Result<(), JournalError> {
        self.update_entry(event_id, |entry| {
            entry.ack_pending_at_ms = None;
            entry.delivered_at_ms.get_or_insert_with(now_ms);
            if let Some(raw) = entry.raw.as_object_mut() {
                raw.insert("delivered".into(), Value::Bool(true));
            }
        })
    }

    pub fn mark_ack_pending(&mut self, event_id: &str) -> Result<(), JournalError> {
        self.update_entry(event_id, |entry| {
            if entry.ack_pending_at_ms.is_none() && entry.delivered_at_ms.is_none() {
                entry.ack_pending_at_ms = Some(now_ms());
            }
        })
    }

    pub fn mark_acted(&mut self, event_id: &str) -> Result<(), JournalError> {
        self.update_entry(event_id, |entry| {
            entry.acted_at_ms.get_or_insert_with(now_ms);
            if let Some(raw) = entry.raw.as_object_mut() {
                raw.insert("acted".into(), Value::Bool(true));
            }
        })
    }

    pub fn record_file_transfer(
        &mut self,
        event_id: &str,
        file_name: String,
        transfer: FileTransfer,
    ) -> Result<(), JournalError> {
        self.update_entry(event_id, move |entry| {
            entry.file_transfers.insert(file_name, transfer);
        })
    }

    pub fn record_server_file_transfers(
        &mut self,
        event_id: &str,
        transfers: Value,
    ) -> Result<(), JournalError> {
        self.update_entry(event_id, move |entry| {
            if let Some(raw) = entry.raw.as_object_mut() {
                raw.insert("file_transfers".into(), transfers);
            }
        })
    }

    pub fn record_server_file_use(
        &mut self,
        event_id: &str,
        file_name: &str,
        outcome: Value,
    ) -> Result<(), JournalError> {
        let file_name = file_name.to_owned();
        self.update_entry(event_id, move |entry| {
            if let Some(raw) = entry.raw.as_object_mut() {
                let uses = raw
                    .entry("file_use")
                    .or_insert_with(|| Value::Object(serde_json::Map::new()));
                if let Some(uses) = uses.as_object_mut() {
                    uses.insert(file_name, outcome);
                }
            }
        })
    }

    fn update_entry(
        &self,
        event_id: &str,
        update: impl FnOnce(&mut JournalEntry),
    ) -> Result<(), JournalError> {
        self.with_transaction(|tx| {
            let mut entry = read_entry_by_event(tx, event_id)?
                .ok_or_else(|| JournalError::MissingEvent(event_id.to_owned()))?;
            update(&mut entry);
            write_entry(tx, &entry)
        })
    }

    pub fn prepare_operation(
        &mut self,
        op_id: Uuid,
        session_handle: Uuid,
        kind: String,
        request: Value,
    ) -> Result<OperationRecord, JournalError> {
        self.prepare_operation_with_uploads(op_id, session_handle, kind, request, Vec::new())
    }

    pub fn prepare_operation_with_uploads(
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
                    || !operation_requests_equivalent(&existing.request, &request)
                {
                    return Err(JournalError::OperationConflict(op_id));
                }
                return Ok(existing);
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
            };
            write_operation(tx, &operation)?;
            Ok(operation)
        })
    }

    pub fn prepare_send_operation(
        &mut self,
        op_id: Uuid,
        session_handle: Uuid,
        request: Value,
        uploads: Vec<UploadSource>,
        prepared_send: PreparedSend,
    ) -> Result<OperationRecord, JournalError> {
        self.with_transaction(|tx| {
            if let Some(existing) = read_operation(tx, op_id)? {
                let conflicts = existing.kind != "send"
                    || existing.prepared_send.as_ref().is_none_or(|saved| {
                        let same_inputs = saved.inputs == prepared_send.inputs
                            && saved.files == prepared_send.files;
                        let same_session = saved.inputs["session"] == "new"
                            || existing.session_handle == session_handle;
                        !same_inputs || !same_session
                    });
                if conflicts {
                    return Err(JournalError::OperationConflict(op_id));
                }
                return Ok(existing);
            }

            if prepared_send.request != request
                || request["op_id"].as_str() != Some(op_id.to_string().as_str())
                || request["session_handle"].as_str() != Some(session_handle.to_string().as_str())
            {
                return Err(JournalError::OperationConflict(op_id));
            }

            let now = now_ms();
            let operation = OperationRecord {
                op_id,
                session_handle,
                kind: "send".into(),
                request,
                state: OperationState::Prepared,
                created_at_ms: now,
                updated_at_ms: now,
                response: None,
                uploads,
                prepared_send: Some(prepared_send),
            };
            write_operation(tx, &operation)?;
            Ok(operation)
        })
    }

    pub fn record_operation_upload(
        &mut self,
        op_id: Uuid,
        file_name: &str,
        upload_id: String,
    ) -> Result<(), JournalError> {
        let file_name = file_name.to_owned();
        self.update_operation(op_id, move |operation| {
            let source = operation
                .uploads
                .iter()
                .find(|source| source.name == file_name)
                .ok_or(JournalError::OperationConflict(op_id))?;
            if let Some(existing) = source.upload_id.as_deref() {
                if existing != upload_id {
                    return Err(JournalError::OperationConflict(op_id));
                }
                return Ok(false);
            }
            let (size, sha256, name) = (
                source.size_bytes,
                source.sha256.clone(),
                source.name.clone(),
            );
            operation
                .uploads
                .iter_mut()
                .find(|source| source.name == file_name)
                .ok_or(JournalError::OperationConflict(op_id))?
                .upload_id = Some(upload_id.clone());
            let files = operation.request["files"]
                .as_array_mut()
                .ok_or(JournalError::OperationConflict(op_id))?;
            let slot = files
                .iter_mut()
                .find(|file| {
                    file["name"].as_str() == Some(file_name.as_str())
                        && file["upload_pending"] == true
                })
                .ok_or(JournalError::OperationConflict(op_id))?;
            *slot =
                serde_json::json!({"name":name,"upload_id":upload_id,"size":size,"sha256":sha256});
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

    pub fn mark_operation_unknown(&mut self, op_id: Uuid) -> Result<(), JournalError> {
        self.update_operation(op_id, |operation| {
            operation.state = OperationState::Unknown;
            operation.updated_at_ms = now_ms();
            Ok(true)
        })
    }

    pub fn settle_operation(
        &mut self,
        op_id: Uuid,
        response: Value,
        acted_event_id: Option<&str>,
    ) -> Result<(), JournalError> {
        let acted_event_id = acted_event_id.map(str::to_owned);
        self.with_transaction(|tx| {
            let mut operation =
                read_operation(tx, op_id)?.ok_or(JournalError::MissingOperation(op_id))?;
            let session = operation.session_handle;
            operation.state = OperationState::Committed;
            operation.updated_at_ms = now_ms();
            operation.request = redact_file_payloads(&operation.request);
            redact_prepared_send(&mut operation);
            operation.response = Some(redact_file_payloads(&response));
            for upload in &mut operation.uploads {
                upload.staged_path = None;
            }
            write_operation(tx, &operation)?;
            if let Some(event_id) = acted_event_id {
                let mut entry = read_entry_by_event(tx, &event_id)?
                    .ok_or_else(|| JournalError::MissingEvent(event_id.clone()))?;
                if entry.session_handle != session || !entry.actionable {
                    return Err(JournalError::MissingEvent(event_id));
                }
                entry.acted_at_ms.get_or_insert_with(now_ms);
                if let Some(raw) = entry.raw.as_object_mut() {
                    raw.insert("acted".into(), Value::Bool(true));
                }
                write_entry(tx, &entry)?;
            }
            Ok(())
        })
    }

    pub fn reject_operation(&mut self, op_id: Uuid, response: Value) -> Result<(), JournalError> {
        self.update_operation(op_id, |operation| {
            operation.state = OperationState::Rejected;
            operation.updated_at_ms = now_ms();
            operation.request = redact_file_payloads(&operation.request);
            redact_prepared_send(operation);
            operation.response = Some(redact_file_payloads(&response));
            for upload in &mut operation.uploads {
                upload.staged_path = None;
            }
            Ok(true)
        })
    }
}

impl Drop for JournalStore {
    fn drop(&mut self) {
        if let Some(lock_file) = self.listener_lock.as_ref() {
            let _ = lock_file.unlock();
        }
    }
}

fn encode_sequence(sequence: u64) -> String {
    format!("{sequence:020}")
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<(), JournalError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|existing| existing == column) {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {declaration}"
        ))?;
    }
    Ok(())
}

/// Populate the columns used by targeted request queries when opening a
/// journal created before schema 2. This full scan happens once per database;
/// ordinary request operations use the resulting indexes and bounded reads.
fn migrate_request_indexes(connection: &mut Connection) -> Result<(), JournalError> {
    let tx = connection.transaction()?;
    let entries = {
        let mut statement = tx.prepare("SELECT feed_seq, payload FROM entries")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (sequence, payload) in entries {
        let entry: JournalEntry = serde_json::from_str(&payload)?;
        tx.execute(
            "UPDATE entries SET in_reply_to=?1, result_status=?2 WHERE feed_seq=?3",
            params![
                entry.raw["in_reply_to"].as_str(),
                entry.raw["status"].as_str(),
                sequence
            ],
        )?;
    }

    let operations = {
        let mut statement = tx.prepare("SELECT op_id, payload FROM operations")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    for (op_id, payload) in operations {
        let operation: OperationRecord = serde_json::from_str(&payload)?;
        tx.execute(
            "UPDATE operations SET request_revision=?1, in_reply_to=?2 WHERE op_id=?3",
            params![
                operation_request_revision(&operation),
                operation.request["in_reply_to"].as_str(),
                op_id
            ],
        )?;
    }

    tx.execute(
        "INSERT INTO meta(key,value) VALUES('schema','2')
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

fn operation_request_revision(operation: &OperationRecord) -> Option<i64> {
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

fn operation_state_name(state: OperationState) -> &'static str {
    match state {
        OperationState::Prepared => "prepared",
        OperationState::Unknown => "unknown",
        OperationState::Committed => "committed",
        OperationState::Rejected => "rejected",
    }
}

fn read_cursor(connection: &Connection) -> Result<u64, JournalError> {
    let value: Option<String> = connection
        .query_row("SELECT value FROM meta WHERE key='cursor'", [], |row| {
            row.get(0)
        })
        .optional()?;
    value
        .ok_or(JournalError::MissingCursor)?
        .parse()
        .map_err(|_| JournalError::InvalidCursor)
}

fn write_cursor(tx: &Transaction<'_>, cursor: u64) -> Result<(), JournalError> {
    tx.execute(
        "INSERT INTO meta(key,value) VALUES('cursor',?1)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [cursor.to_string()],
    )?;
    Ok(())
}

fn entry_pending_active(tx: &Transaction<'_>, entry: &JournalEntry) -> Result<bool, JournalError> {
    if !entry.actionable || entry.acted_at_ms.is_some() {
        return Ok(false);
    }
    match entry.kind.as_str() {
        "discussion" => {
            let Some((sender_id, request_id)) =
                entry.sender_id.as_deref().zip(entry.request_id.as_deref())
            else {
                return Ok(true);
            };
            let terminal_state: Option<String> = tx
                .query_row(
                    "SELECT terminal_state FROM entries
                     WHERE session_handle=?1 AND sender_id=?2 AND request_id=?3 AND kind='terminal'
                     ORDER BY feed_seq DESC LIMIT 1",
                    params![entry.session_handle.to_string(), sender_id, request_id],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(match terminal_state.as_deref() {
                None => true,
                Some("answered") => entry.raw["channel"] == "chat",
                Some(_) => false,
            })
        }
        "terminal" if entry.raw["terminal_state"] == "answered" => Ok(true),
        "terminal" => Ok(entry.delivered_at_ms.is_none()),
        _ => Ok(false),
    }
}

fn write_entry(tx: &Transaction<'_>, entry: &JournalEntry) -> Result<(), JournalError> {
    let payload = serde_json::to_string(entry)?;
    let pending_active = entry_pending_active(tx, entry)?;
    let terminal_state = entry.raw["terminal_state"].as_str();
    tx.execute(
        "INSERT INTO entries(
            feed_seq,event_id,session_handle,kind,author,actionable,sender_id,request_id,
            terminal_state,in_reply_to,result_status,pending_active,emitted_at_ms,
            ack_pending_at_ms,delivered_at_ms,acted_at_ms,payload
         ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
         ON CONFLICT(feed_seq) DO UPDATE SET
            event_id=excluded.event_id,session_handle=excluded.session_handle,kind=excluded.kind,
            author=excluded.author,actionable=excluded.actionable,sender_id=excluded.sender_id,
            request_id=excluded.request_id,terminal_state=excluded.terminal_state,
            in_reply_to=excluded.in_reply_to,result_status=excluded.result_status,
            pending_active=excluded.pending_active,emitted_at_ms=excluded.emitted_at_ms,
            ack_pending_at_ms=excluded.ack_pending_at_ms,delivered_at_ms=excluded.delivered_at_ms,
            acted_at_ms=excluded.acted_at_ms,payload=excluded.payload",
        params![
            encode_sequence(entry.feed_seq),
            entry.event_id,
            entry.session_handle.to_string(),
            entry.kind,
            entry.author,
            if entry.actionable { 1i64 } else { 0i64 },
            entry.sender_id,
            entry.request_id,
            terminal_state,
            entry.raw["in_reply_to"].as_str(),
            entry.raw["status"].as_str(),
            if pending_active { 1i64 } else { 0i64 },
            entry.emitted_at_ms.map(|value| value as i64),
            entry.ack_pending_at_ms.map(|value| value as i64),
            entry.delivered_at_ms.map(|value| value as i64),
            entry.acted_at_ms.map(|value| value as i64),
            payload
        ],
    )?;
    Ok(())
}

fn close_pending_discussions(
    tx: &Transaction<'_>,
    session_handle: Uuid,
    sender_id: &str,
    request_id: &str,
) -> Result<(), JournalError> {
    tx.execute(
        "UPDATE entries SET pending_active=0
         WHERE session_handle=?1 AND sender_id=?2 AND request_id=?3 AND kind='discussion'",
        params![session_handle.to_string(), sender_id, request_id],
    )?;
    Ok(())
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

fn read_entry_by_seq(
    connection: &Connection,
    sequence: u64,
) -> Result<Option<JournalEntry>, JournalError> {
    let payload: Option<String> = connection
        .query_row(
            "SELECT payload FROM entries WHERE feed_seq=?1",
            [encode_sequence(sequence)],
            |row| row.get(0),
        )
        .optional()?;
    payload
        .map(|payload| serde_json::from_str(&payload).map_err(JournalError::from))
        .transpose()
}

fn read_entry_by_event(
    connection: &Connection,
    event_id: &str,
) -> Result<Option<JournalEntry>, JournalError> {
    let payload: Option<String> = connection
        .query_row(
            "SELECT payload FROM entries WHERE event_id=?1",
            [event_id],
            |row| row.get(0),
        )
        .optional()?;
    payload
        .map(|payload| serde_json::from_str(&payload).map_err(JournalError::from))
        .transpose()
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

fn read_entry_list(
    connection: &Connection,
    query: &str,
    session: &str,
) -> Result<Vec<JournalEntry>, JournalError> {
    let mut statement = connection.prepare(query)?;
    let payloads = statement
        .query_map([session], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    payloads
        .into_iter()
        .map(|payload| serde_json::from_str(&payload).map_err(JournalError::from))
        .collect()
}

fn immutable_entry(value: &Value) -> Value {
    let mut value = value.clone();
    strip_file_payloads(&mut value);
    if let Some(object) = value.as_object_mut() {
        object.remove("delivered");
        object.remove("acted");
        object.remove("file_transfers");
        object.remove("file_use");
    }
    value
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

fn reconcile_own_entry(tx: &Transaction<'_>, entry: &JournalEntry) -> Result<bool, JournalError> {
    if !entry.own_entry() {
        return Ok(false);
    }

    let mut changed = false;
    if let Some(source_event_id) = entry.raw["in_reply_to"].as_str() {
        if let Some(mut source) = read_entry_by_event(tx, source_event_id)? {
            if source.session_handle == entry.session_handle
                && source.actionable
                && source.acted_at_ms.is_none()
            {
                source.acted_at_ms = Some(now_ms());
                if let Some(raw) = source.raw.as_object_mut() {
                    raw.insert("acted".into(), Value::Bool(true));
                }
                write_entry(tx, &source)?;
                changed = true;
            }
        }
    }

    let mut matching = Vec::new();
    if let Some(entry_op_id) = entry.raw["op_id"].as_str() {
        if let Ok(op_id) = Uuid::parse_str(entry_op_id) {
            if let Some(operation) = read_operation(tx, op_id)? {
                if operation.state == OperationState::Unknown
                    && operation_matches_entry(&operation, entry)
                {
                    matching.push(operation);
                }
            }
        }
    } else if let Some(message_id) = entry.request_id.as_deref() {
        let mut statement = tx.prepare(
            "SELECT payload FROM operations
             WHERE session_handle=?1 AND message_id=?2 AND state='unknown'",
        )?;
        let payloads = statement
            .query_map(
                params![entry.session_handle.to_string(), message_id],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        for payload in payloads {
            let operation: OperationRecord = serde_json::from_str(&payload)?;
            if operation_matches_entry(&operation, entry) {
                matching.push(operation);
            }
        }
    }

    if matching.len() == 1 {
        let mut operation = matching.remove(0);
        operation.state = OperationState::Committed;
        operation.updated_at_ms = now_ms();
        operation.request = redact_file_payloads(&operation.request);
        redact_prepared_send(&mut operation);
        operation.response = Some(serde_json::json!({
            "v":1,
            "type":"ok",
            "recovered_from_feed":true,
            "entry":redact_file_payloads(&entry.raw)
        }));
        write_operation(tx, &operation)?;
        changed = true;
    }
    Ok(changed)
}

fn operation_matches_entry(operation: &OperationRecord, entry: &JournalEntry) -> bool {
    if operation.session_handle != entry.session_handle || entry.author != "origin" {
        return false;
    }

    if let Some(entry_op_id) = entry.raw["op_id"].as_str() {
        if entry_op_id != operation.op_id.to_string() {
            return false;
        }
        return match operation.kind.as_str() {
            "send" | "revise" => entry.kind == "revision",
            "reply" => entry.kind == "reply",
            "withdraw" => entry.kind == "terminal" && entry.raw["terminal_state"] == "retracted",
            "result" => entry.kind == "result",
            _ => false,
        };
    }

    let request = &operation.request;
    let sender_id = if operation.kind == "send" {
        request["message"]["sender_id"].as_str()
    } else {
        request["sender_id"].as_str()
    };
    let message_id = if operation.kind == "send" {
        request["message"]["message_id"].as_str()
    } else {
        request["message_id"].as_str()
    };
    if sender_id != entry.sender_id.as_deref() || message_id != entry.request_id.as_deref() {
        return false;
    }

    match operation.kind.as_str() {
        "send" => {
            entry.kind == "revision"
                && entry.request_revision == Some(1)
                && entry.raw["message"] == request["message"]
        }
        "reply" => {
            entry.kind == "reply"
                && entry.raw["in_reply_to"] == request["in_reply_to"]
                && entry.request_revision == request["request_revision"].as_u64()
                && entry.raw["text"] == request["text"]
        }
        "revise" => {
            let next_revision = request["base_revision"]
                .as_u64()
                .and_then(|revision| revision.checked_add(1));
            entry.kind == "revision"
                && entry.request_revision == next_revision
                && entry.raw["message"] == request["message"]
        }
        "withdraw" => entry.kind == "terminal" && entry.raw["terminal_state"] == "retracted",
        "result" => {
            entry.kind == "result"
                && entry.raw["in_reply_to"] == request["in_reply_to"]
                && entry.raw["status"] == request["status"]
        }
        _ => false,
    }
}

fn merge_server_facts(existing: &mut JournalEntry, incoming: &JournalEntry) -> bool {
    let mut changed = false;
    if incoming.raw["delivered"] == true {
        if existing.ack_pending_at_ms.is_some() {
            existing.ack_pending_at_ms = None;
            changed = true;
        }
        if existing.delivered_at_ms.is_none() {
            existing.delivered_at_ms = Some(now_ms());
            changed = true;
        }
        if existing.raw["delivered"] != true {
            if let Some(raw) = existing.raw.as_object_mut() {
                raw.insert("delivered".into(), Value::Bool(true));
            }
            changed = true;
        }
    }
    if incoming.raw["acted"] == true && existing.acted_at_ms.is_none() {
        existing.acted_at_ms = Some(now_ms());
        if let Some(raw) = existing.raw.as_object_mut() {
            raw.insert("acted".into(), Value::Bool(true));
        }
        changed = true;
    }
    for field in ["file_transfers", "file_use"] {
        if let Some(value) = incoming.raw.get(field) {
            if existing.raw.get(field) != Some(value) {
                if let Some(raw) = existing.raw.as_object_mut() {
                    raw.insert(field.to_owned(), value.clone());
                }
                changed = true;
            }
        }
    }
    changed
}

fn merge_local_file_transfers(existing: &mut JournalEntry, incoming: &JournalEntry) -> bool {
    if existing.ack_pending_at_ms.is_some() || existing.delivered_at_ms.is_some() {
        return false;
    }
    let mut changed = false;
    for (name, incoming_transfer) in &incoming.file_transfers {
        if existing.file_transfers.get(name) != Some(incoming_transfer) {
            existing
                .file_transfers
                .insert(name.clone(), incoming_transfer.clone());
            changed = true;
        }
    }
    changed
}

fn merge_server_facts_into_new(entry: &mut JournalEntry) {
    if entry.raw["delivered"] == true {
        entry.ack_pending_at_ms = None;
        entry.delivered_at_ms = Some(now_ms());
    }
    if entry.raw["acted"] == true {
        entry.acted_at_ms = Some(now_ms());
    }
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

fn read_lock_pid(path: &Path) -> Option<u32> {
    // A second listener can arrive in the short interval after the lock is
    // acquired and before the owner publishes its PID. Only local lock metadata
    // is retried here; this does not drive server requests or feed polling.
    for _ in 0..32 {
        let mut content = String::new();
        if File::open(path)
            .and_then(|mut file| file.read_to_string(&mut content))
            .is_ok()
        {
            if let Ok(pid) = content.trim().parse() {
                return Some(pid);
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    None
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

fn now_ms() -> u64 {
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

    fn entry(session: Uuid, seq: u64, body: &str) -> JournalEntry {
        JournalEntry::fetched(
            seq,
            seq.to_string(),
            session,
            "discussion".into(),
            "person".into(),
            true,
            Some("app:test".into()),
            Some("request-1".into()),
            Some(1),
            json!({
                "event_id": seq.to_string(),
                "session_handle": session.to_string(),
                "kind": "discussion",
                "author": "person",
                "sender_id": "app:test",
                "message_id": "request-1",
                "request_revision": 1,
                "text": body,
                "delivered": false,
                "acted": false
            }),
        )
    }

    fn terminal(session: Uuid, seq: u64, state: &str, delivered: bool) -> JournalEntry {
        JournalEntry::fetched(
            seq,
            seq.to_string(),
            session,
            "terminal".into(),
            "person".into(),
            true,
            Some("app:test".into()),
            Some("request-1".into()),
            Some(1),
            json!({
                "event_id": seq.to_string(),
                "session_handle": session.to_string(),
                "kind": "terminal",
                "author": "person",
                "sender_id": "app:test",
                "message_id": "request-1",
                "request_revision": 1,
                "terminal_state": state,
                "delivered": delivered,
                "acted": false
            }),
        )
    }

    fn origin_terminal(session: Uuid, seq: u64, state: &str, op_id: Option<Uuid>) -> JournalEntry {
        let mut raw = json!({
            "event_id": seq.to_string(),
            "session_handle": session.to_string(),
            "kind": "terminal",
            "author": "origin",
            "sender_id": "app:test",
            "message_id": "request-1",
            "request_revision": 1,
            "terminal_state": state,
            "delivered": false,
            "acted": false
        });
        if let Some(op_id) = op_id {
            raw["op_id"] = json!(op_id.to_string());
        }
        JournalEntry::fetched(
            seq,
            seq.to_string(),
            session,
            "terminal".into(),
            "origin".into(),
            false,
            Some("app:test".into()),
            Some("request-1".into()),
            Some(1),
            raw,
        )
    }

    fn open(root: &Scratch, session: Uuid) -> JournalStore {
        JournalStore::open(&root.0, "enrolled-device", session).unwrap()
    }

    #[test]
    fn request_snapshot_stays_bounded_with_long_entry_and_operation_history() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = open(&root, session);
        let initial = JournalEntry::fetched(
            1,
            "1".into(),
            session,
            "revision".into(),
            "person".into(),
            false,
            Some("app:test".into()),
            Some("request-1".into()),
            Some(1),
            json!({
                "event_id":"1",
                "session_handle":session.to_string(),
                "kind":"revision",
                "author":"person",
                "sender_id":"app:test",
                "message_id":"request-1",
                "request_revision":1,
                "message":{"sender_id":"app:test","message_id":"request-1","title":"initial"}
            }),
        );
        store.record_entry(initial).unwrap();

        for sequence in 2..=201 {
            store
                .record_entry(entry(session, sequence, &format!("turn {sequence}")))
                .unwrap();
        }

        let send_id = Uuid::new_v4();
        store
            .prepare_operation(
                send_id,
                session,
                "send".into(),
                json!({
                    "v":1,
                    "type":"send",
                    "op_id":send_id.to_string(),
                    "session_handle":session.to_string(),
                    "message":{"sender_id":"app:test","message_id":"request-1","title":"initial"}
                }),
            )
            .unwrap();
        store
            .settle_operation(send_id, json!({"type":"ok","revision":1}), None)
            .unwrap();

        for sequence in 0..128 {
            let op_id = Uuid::new_v4();
            store
                .prepare_operation(
                    op_id,
                    session,
                    "reply".into(),
                    json!({
                        "v":1,
                        "type":"reply",
                        "op_id":op_id.to_string(),
                        "sender_id":"app:test",
                        "message_id":"request-1",
                        "in_reply_to":sequence.to_string(),
                        "text":format!("committed action {sequence}")
                    }),
                )
                .unwrap();
            store
                .settle_operation(op_id, json!({"type":"ok","status":"replied"}), None)
                .unwrap();
        }

        let unresolved_id = Uuid::new_v4();
        store
            .prepare_operation(
                unresolved_id,
                session,
                "result".into(),
                json!({
                    "v":1,
                    "type":"result",
                    "op_id":unresolved_id.to_string(),
                    "sender_id":"app:test",
                    "message_id":"request-1",
                    "in_reply_to":"201",
                    "status":"received"
                }),
            )
            .unwrap();
        store.mark_operation_unknown(unresolved_id).unwrap();

        let snapshot = store
            .request_snapshot(session, "request-1", Some("201"))
            .unwrap();
        assert_eq!(snapshot.entries.len(), 2);
        assert!(
            snapshot
                .entries
                .values()
                .any(|entry| entry.kind == "revision")
        );
        assert!(
            snapshot
                .entries
                .values()
                .any(|entry| entry.event_id == "201")
        );
        assert_eq!(snapshot.operations.len(), 2);
        assert!(snapshot.operations.contains_key(&send_id));
        assert!(snapshot.operations.contains_key(&unresolved_id));
        assert!(store.snapshot().unwrap().entries.len() >= 201);
        assert_eq!(store.snapshot().unwrap().operations.len(), 130);

        store
            .with_read(|connection| {
                let mut statement = connection.prepare(
                    "EXPLAIN QUERY PLAN SELECT payload FROM entries
                     WHERE session_handle=?1 AND request_id=?2 AND kind='revision'
                     ORDER BY feed_seq DESC LIMIT 1",
                )?;
                let plan = statement
                    .query_map(params![session.to_string(), "request-1"], |row| {
                        row.get::<_, String>(3)
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                    .join(" ");
                assert!(plan.contains("entries_by_session_request_kind"), "{plan}");

                let mut statement = connection.prepare(
                    "EXPLAIN QUERY PLAN SELECT payload FROM operations
                     WHERE session_handle=?1 AND message_id=?2 AND state='committed'
                       AND kind IN ('send','revise') AND request_revision IS NOT NULL
                     ORDER BY request_revision DESC LIMIT 1",
                )?;
                let plan = statement
                    .query_map(params![session.to_string(), "request-1"], |row| {
                        row.get::<_, String>(3)
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                    .join(" ");
                assert!(
                    plan.contains("operations_committed_revision_by_request"),
                    "{plan}"
                );

                let mut statement = connection.prepare(
                    "EXPLAIN QUERY PLAN SELECT payload FROM operations
                     WHERE session_handle=?1 AND message_id=?2
                       AND state IN ('prepared','unknown') ORDER BY op_id LIMIT 1",
                )?;
                let plan = statement
                    .query_map(params![session.to_string(), "request-1"], |row| {
                        row.get::<_, String>(3)
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                    .join(" ");
                assert!(plan.contains("operations_pending_by_request"), "{plan}");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn cursor_and_entry_commit_together_and_survive_reopen() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        {
            let mut store = open(&root, session);
            assert_eq!(store.snapshot().unwrap().cursor, 0);
            assert!(store.record_entry(entry(session, 1, "hello")).unwrap());
            assert_eq!(store.snapshot().unwrap().cursor, 1);
        }

        let mut reopened = open(&root, session);
        let snapshot = reopened.snapshot().unwrap();
        assert_eq!(snapshot.cursor, 1);
        assert_eq!(snapshot.entries.get(&1).unwrap().event_id, "1");
        assert!(!reopened.record_entry(entry(session, 1, "hello")).unwrap());
        assert!(matches!(
            reopened.record_entry(entry(session, 3, "gap")),
            Err(JournalError::FeedGap {
                cursor: 1,
                sequence: 3
            })
        ));
        assert!(matches!(
            reopened.record_entry(entry(session, 1, "changed")),
            Err(JournalError::FeedConflict { sequence: 1 })
        ));
    }

    #[test]
    fn emitted_delivered_and_acted_remain_separate_durable_facts() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = open(&root, session);
        store.record_entry(entry(session, 1, "hello")).unwrap();
        assert_eq!(store.snapshot().unwrap().pending_for(session).len(), 1);

        store.mark_emitted("1").unwrap();
        let after_emit = store.snapshot().unwrap();
        let emitted = after_emit.entry_by_event("1").unwrap();
        assert!(emitted.emitted_at_ms.is_some());
        assert!(emitted.delivered_at_ms.is_none());
        assert!(emitted.acted_at_ms.is_none());

        store.mark_delivered("1").unwrap();
        let after_delivery = store.snapshot().unwrap();
        let delivered = after_delivery.entry_by_event("1").unwrap();
        assert!(delivered.delivered_at_ms.is_some());
        assert!(delivered.acted_at_ms.is_none());
        assert_eq!(after_delivery.pending_for(session).len(), 1);

        store.mark_acted("1").unwrap();
        let after_action = store.snapshot().unwrap();
        assert!(
            after_action
                .entry_by_event("1")
                .unwrap()
                .acted_at_ms
                .is_some()
        );
        assert_eq!(after_action.pending_for(session).len(), 0);
    }

    #[test]
    fn own_entries_are_journaled_but_never_actionable() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = open(&root, session);
        let own = JournalEntry::fetched(
            1,
            "1".into(),
            session,
            "reply".into(),
            "origin".into(),
            true,
            Some("app:test".into()),
            Some("request-1".into()),
            Some(1),
            json!({
                "event_id": "1",
                "session_handle": session.to_string(),
                "kind": "reply",
                "author": "origin",
                "sender_id": "app:test",
                "message_id": "request-1",
                "request_revision": 1,
                "text": "acknowledged",
                "in_reply_to": "0"
            }),
        );
        assert!(own.own_entry());
        assert!(!own.actionable);
        store.record_entry(own).unwrap();
        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.cursor, 1);
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.pending_for(session).len(), 0);
    }

    #[test]
    fn operation_key_is_durable_before_retry_and_conflicts_on_changed_payload() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        {
            let mut store = open(&root, session);
            store
                .prepare_operation(op_id, session, "reply".into(), json!({"body": "one"}))
                .unwrap();
            store.mark_operation_unknown(op_id).unwrap();
        }

        let mut reopened = open(&root, session);
        let snapshot = reopened.snapshot().unwrap();
        assert_eq!(snapshot.unknown_operations_for(session).count(), 1);
        assert_eq!(
            reopened
                .prepare_operation(op_id, session, "reply".into(), json!({"body": "one"}))
                .unwrap()
                .state,
            OperationState::Unknown
        );
        assert!(matches!(
            reopened.prepare_operation(op_id, session, "reply".into(), json!({"body": "two"})),
            Err(JournalError::OperationConflict(id)) if id == op_id
        ));
    }

    #[test]
    fn own_reply_replay_marks_source_acted_and_settles_one_unknown_operation() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let mut store = open(&root, session);
        store
            .record_entry(entry(session, 1, "please check"))
            .unwrap();
        store.mark_delivered("1").unwrap();
        store
            .prepare_operation(
                op_id,
                session,
                "reply".into(),
                json!({
                    "v":1,
                    "type":"reply",
                    "op_id":op_id,
                    "sender_id":"app:test",
                    "message_id":"request-1",
                    "request_revision":1,
                    "in_reply_to":"1",
                    "text":"done"
                }),
            )
            .unwrap();
        store.mark_operation_unknown(op_id).unwrap();

        store
            .record_entry(JournalEntry::fetched(
                2,
                "2".into(),
                session,
                "reply".into(),
                "origin".into(),
                false,
                Some("app:test".into()),
                Some("request-1".into()),
                Some(1),
                json!({
                    "event_id":"2",
                    "session_handle":session.to_string(),
                    "kind":"reply",
                    "author":"origin",
                    "sender_id":"app:test",
                    "message_id":"request-1",
                    "request_revision":1,
                    "in_reply_to":"1",
                    "text":"done"
                }),
            ))
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        assert!(snapshot.entry_by_event("1").unwrap().acted_at_ms.is_some());
        let operation = snapshot.operations.get(&op_id).unwrap();
        assert_eq!(operation.state, OperationState::Committed);
        assert_eq!(
            operation.response.as_ref().unwrap()["recovered_from_feed"],
            true
        );
        assert!(snapshot.pending_for(session).is_empty());
    }

    #[test]
    fn listener_lease_blocks_only_another_listener() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut listener = open(&root, session);
        listener.acquire_listener().unwrap();

        let mut command = open(&root, session);
        assert!(matches!(
            command.acquire_listener(),
            Err(JournalError::ListenerActive(pid)) if pid == process::id()
        ));
        command.record_entry(entry(session, 1, "hello")).unwrap();
        command.mark_delivered("1").unwrap();
        assert!(
            command
                .snapshot()
                .unwrap()
                .entry_by_event("1")
                .unwrap()
                .delivered_at_ms
                .is_some()
        );

        drop(listener);
        command.acquire_listener().unwrap();
    }

    #[test]
    fn operation_commit_and_acted_fact_share_one_snapshot() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let mut store = open(&root, session);
        store.record_entry(entry(session, 1, "hello")).unwrap();
        store
            .prepare_operation(op_id, session, "reply".into(), json!({"body": "done"}))
            .unwrap();
        store
            .settle_operation(op_id, json!({"event_id": "2"}), Some("1"))
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        assert_eq!(
            snapshot.operations.get(&op_id).unwrap().state,
            OperationState::Committed
        );
        assert!(snapshot.entry_by_event("1").unwrap().acted_at_ms.is_some());
        assert_eq!(snapshot.pending_for(session).len(), 0);
    }

    #[test]
    fn feed_cursor_is_origin_wide_while_pending_and_listener_leases_are_per_session() {
        let root = Scratch::new();
        let first_session = Uuid::new_v4();
        let second_session = Uuid::new_v4();
        let mut first = open(&root, first_session);
        let mut second = open(&root, second_session);
        first.acquire_listener().unwrap();
        second.acquire_listener().unwrap();

        first
            .record_entry(entry(second_session, 1, "for second"))
            .unwrap();
        second
            .record_entry(entry(first_session, 2, "for first"))
            .unwrap();

        let snapshot = first.snapshot().unwrap();
        assert_eq!(snapshot.cursor, 2);
        assert_eq!(snapshot.pending_for(first_session).len(), 1);
        assert_eq!(snapshot.pending_for(second_session).len(), 1);
    }

    #[test]
    fn replay_refreshes_server_receipts_without_changing_immutable_entry_content() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = open(&root, session);
        store.record_entry(entry(session, 1, "hello")).unwrap();

        let mut replay = entry(session, 1, "hello");
        replay.raw["delivered"] = Value::Bool(true);
        replay.raw["acted"] = Value::Bool(true);
        assert!(store.record_entry(replay).unwrap());

        let snapshot = store.snapshot().unwrap();
        let saved = snapshot.entry_by_event("1").unwrap();
        assert!(saved.delivered_at_ms.is_some());
        assert!(saved.acted_at_ms.is_some());
        assert_eq!(snapshot.pending_for(session).len(), 0);

        assert!(!store.record_entry(entry(session, 1, "hello")).unwrap());
        assert!(
            store
                .snapshot()
                .unwrap()
                .entry_by_event("1")
                .unwrap()
                .acted_at_ms
                .is_some()
        );
    }

    #[test]
    fn ack_intent_is_durable_without_claiming_delivery_and_clears_on_receipt() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        {
            let mut store = open(&root, session);
            store.record_entry(entry(session, 1, "hello")).unwrap();
            store.mark_ack_pending("1").unwrap();
        }

        let mut reopened = open(&root, session);
        let snapshot = reopened.snapshot().unwrap();
        let saved = snapshot.entry_by_event("1").unwrap();
        assert!(saved.ack_pending_at_ms.is_some());
        assert!(saved.delivered_at_ms.is_none());
        assert_eq!(snapshot.pending_acknowledgements_for(session).count(), 1);

        reopened.mark_delivered("1").unwrap();
        let delivered = reopened.snapshot().unwrap();
        assert!(
            delivered
                .entry_by_event("1")
                .unwrap()
                .ack_pending_at_ms
                .is_none()
        );
        assert!(
            delivered
                .entry_by_event("1")
                .unwrap()
                .delivered_at_ms
                .is_some()
        );
        assert_eq!(delivered.pending_acknowledgements_for(session).count(), 0);
    }

    #[test]
    fn closed_discussion_is_settled_by_terminal_and_answered_terminal_stays_pending() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = open(&root, session);
        let mut discussion = entry(session, 1, "hello");
        discussion.raw["delivered"] = Value::Bool(true);
        store.record_entry(discussion).unwrap();
        store
            .record_entry(terminal(session, 2, "answered", true))
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        let pending = snapshot.pending_for(session);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].event_id, "2");

        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = open(&root, session);
        store
            .record_entry(terminal(session, 1, "expired", true))
            .unwrap();
        assert!(store.snapshot().unwrap().pending_for(session).is_empty());
    }

    #[test]
    fn post_answer_chat_stays_pending_and_non_answer_terminals_still_close_chat() {
        for terminal_kind in [None, Some("no_comment")] {
            let root = Scratch::new();
            let session = Uuid::new_v4();
            let mut store = open(&root, session);
            let mut answered = terminal(session, 1, "answered", true);
            answered.raw["channel"] = json!("reply");
            if let Some(terminal_kind) = terminal_kind {
                answered.raw["terminal_kind"] = json!(terminal_kind);
            }
            store.record_entry(answered).unwrap();

            let mut chat = entry(session, 2, "chat after answer");
            chat.raw["channel"] = json!("chat");
            store.record_entry(chat).unwrap();
            store.mark_acted("1").unwrap();

            let snapshot = store.snapshot().unwrap();
            let pending = snapshot.pending_for(session);
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].event_id, "2");
            assert_eq!(pending[0].raw["channel"], "chat");
        }

        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = open(&root, session);
        store
            .record_entry(terminal(session, 1, "expired", true))
            .unwrap();
        let mut chat = entry(session, 2, "chat after expiry");
        chat.raw["channel"] = json!("chat");
        store.record_entry(chat).unwrap();
        assert!(store.snapshot().unwrap().pending_for(session).is_empty());
    }

    #[test]
    fn origin_retraction_is_journaled_as_nonactionable_and_closes_pending_discussion() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = open(&root, session);
        store
            .record_entry(entry(session, 1, "please review"))
            .unwrap();
        store
            .record_entry(origin_terminal(
                session,
                2,
                "retracted",
                Some(Uuid::new_v4()),
            ))
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        let retraction = snapshot.entry_by_event("2").unwrap();
        assert!(retraction.own_entry());
        assert!(!retraction.actionable);
        assert!(retraction.emitted_at_ms.is_none());
        assert!(snapshot.pending_for(session).is_empty());
    }

    #[test]
    fn withdrawal_recovery_matches_retracted_but_not_ordinary_cleared_terminal() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let mut store = open(&root, session);
        store
            .prepare_operation(
                op_id,
                session,
                "withdraw".into(),
                json!({
                    "v":1,
                    "type":"withdraw",
                    "op_id":op_id,
                    "sender_id":"app:test",
                    "message_id":"request-1"
                }),
            )
            .unwrap();
        let snapshot = store.snapshot().unwrap();
        let operation = snapshot.operations.get(&op_id).unwrap();

        assert!(operation_matches_entry(
            operation,
            &origin_terminal(session, 1, "retracted", Some(op_id))
        ));
        assert!(operation_matches_entry(
            operation,
            &origin_terminal(session, 1, "retracted", None)
        ));
        assert!(!operation_matches_entry(
            operation,
            &origin_terminal(session, 1, "cleared", Some(op_id))
        ));
    }

    #[test]
    fn committed_file_operation_keeps_fingerprint_but_drops_inline_bytes() {
        let root = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let data = STANDARD.encode([1, 2, 3, 4]);
        let request = serde_json::json!({
            "v":1,
            "type":"send",
            "op_id":op_id,
            "session_handle":session,
            "message":{"sender_id":"app:test","message_id":"request-1","title":"file"},
            "files":[{"name":"input.bin","data_base64":data}]
        });
        let mut store = open(&root, session);
        store
            .prepare_operation(op_id, session, "send".into(), request.clone())
            .unwrap();
        store.mark_operation_unknown(op_id).unwrap();
        store
            .settle_operation(op_id, serde_json::json!({"type":"ok"}), None)
            .unwrap();

        let saved = store.snapshot().unwrap();
        let saved_request = &saved.operations.get(&op_id).unwrap().request;
        assert!(saved_request["files"][0].get("data_base64").is_none());
        assert_eq!(saved_request["files"][0]["size"], 4);
        assert_eq!(
            saved_request["files"][0]["sha256"],
            hex::encode(Sha256::digest([1, 2, 3, 4]))
        );
        assert!(
            store
                .prepare_operation(op_id, session, "send".into(), request.clone())
                .is_ok()
        );

        let changed = serde_json::json!({
            "v":1,
            "type":"send",
            "op_id":op_id,
            "session_handle":session,
            "message":{"sender_id":"app:test","message_id":"request-1","title":"file"},
            "files":[{"name":"input.bin","data_base64":STANDARD.encode([9, 8, 7, 6])}]
        });
        assert!(matches!(
            store.prepare_operation(op_id, session, "send".into(), changed),
            Err(JournalError::OperationConflict(_))
        ));
    }

    #[test]
    fn server_file_receipts_change_without_changing_immutable_feed_content() {
        let bytes = STANDARD.encode([7, 8]);
        let mut first = serde_json::json!({
            "event_id":"1",
            "delivered":false,
            "acted":false,
            "files":[{"name":"input.bin","tier":"inline","size":2,"sha256":"hash","data_base64":bytes.clone()}],
            "file_transfers":{},
            "file_use":{}
        });
        let second = serde_json::json!({
            "event_id":"1",
            "delivered":true,
            "acted":true,
            "files":[{"name":"input.bin","tier":"inline","size":2,"sha256":"hash","data_base64":bytes}],
            "file_transfers":{"input.bin":{"status":"transferred"}},
            "file_use":{"input.bin":{"status":"used"}}
        });
        strip_file_payloads(&mut first);
        assert_eq!(immutable_entry(&first), immutable_entry(&second));
        assert!(first["files"][0].get("data_base64").is_none());
    }
}
