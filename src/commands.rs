use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    io::{self, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};
use thiserror::Error;
use uuid::{Uuid, Version};

use crate::{
    api::{ApiError, OriginClient},
    config::{EnrolledIdentity, default_state_root},
    files::{self, PreparedFiles, ServerFileLimits},
    journal::{
        Journal, JournalEntry, JournalError, JournalStore, OperationRecord, OperationState,
        PreparedSend,
    },
};

pub const EXIT_REFUSED: i32 = 2;
pub const EXIT_UNSUPPORTED: i32 = 5;
pub const EXIT_LISTENER_ACTIVE: i32 = 6;
pub const EXIT_OUTCOME_UNKNOWN: i32 = 7;

#[derive(Debug, Error)]
#[error("{message}")]
pub struct CommandError {
    pub message: String,
    pub exit_code: i32,
}

impl CommandError {
    fn new(message: impl Into<String>, exit_code: i32) -> Self {
        Self {
            message: message.into(),
            exit_code,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginHarness {
    ClaudeCode,
    Codex,
    Other,
}

impl OriginHarness {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Other => "other",
        }
    }

    fn unsupported_reason(self, controller: bool) -> Value {
        match self {
            Self::ClaudeCode => Value::Null,
            Self::Codex => Value::String("Codex listening is not supported yet".into()),
            Self::Other if controller => Value::Null,
            Self::Other => Value::String("no controller declared for the other route".into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionContext {
    Interactive,
    Bare,
    Print,
    Sdk,
}

pub struct CommandContext {
    pub api: OriginClient,
    pub state_root: PathBuf,
    pub device_id: String,
}

impl CommandContext {
    pub fn load(
        identity_dir: Option<&Path>,
        ca_file: Option<&Path>,
        origin_port: Option<u16>,
        state_dir: Option<&Path>,
    ) -> Result<Self, String> {
        let identity_dir = identity_dir
            .map(Path::to_path_buf)
            .or_else(|| env_path("HARKLIGHT_IDENTITY_DIR"))
            .ok_or_else(|| "provide --identity-dir or HARKLIGHT_IDENTITY_DIR".to_owned())?;
        let ca_file = ca_file
            .map(Path::to_path_buf)
            .or_else(|| env_path("HARKLIGHT_CA_FILE"))
            .ok_or_else(|| {
                "provide --ca-file or HARKLIGHT_CA_FILE for the enrolled server CA".to_owned()
            })?;
        let state_root = state_dir
            .map(Path::to_path_buf)
            .or_else(|| env_path("HARKLIGHT_AGENT_STATE_DIR"))
            .map(Ok)
            .unwrap_or_else(default_state_root)
            .map_err(|error| error.to_string())?;
        let identity =
            EnrolledIdentity::load(&identity_dir, &ca_file).map_err(|error| error.to_string())?;
        let device_id = identity.device_id.clone();
        let api = OriginClient::new(identity, origin_port).map_err(|error| error.to_string())?;
        Ok(Self {
            api,
            state_root,
            device_id,
        })
    }

    fn open_store(&self, session: Uuid) -> Result<JournalStore, CommandError> {
        JournalStore::open(&self.state_root, &self.device_id, session)
            .map_err(|error| journal_error(error, 1))
    }

    fn open_global_store(&self) -> Result<JournalStore, CommandError> {
        self.open_store(Uuid::nil())
    }
}

pub fn unsupported(operation: &str, reason: &str) -> Result<(), CommandError> {
    emit(&json!({"kind":"unsupported", "operation":operation, "reason":reason}))?;
    Err(CommandError::new(reason, EXIT_UNSUPPORTED))
}

fn prepare_outgoing_files(
    context: &CommandContext,
    store: &JournalStore,
    session: Uuid,
    op_id: Uuid,
    paths: &[PathBuf],
    operation: &str,
    draft: Value,
) -> Result<PreparedFiles, CommandError> {
    prepare_outgoing_files_with_lookup(store, session, op_id, paths, operation, draft, || {
        context.api.post(&json!({"v":1,"type":"file_limits"}))
    })
}

fn prepare_outgoing_files_with_lookup(
    store: &JournalStore,
    session: Uuid,
    op_id: Uuid,
    paths: &[PathBuf],
    operation: &str,
    draft: Value,
    lookup_limits: impl FnOnce() -> Result<Value, ApiError>,
) -> Result<PreparedFiles, CommandError> {
    if paths.is_empty() {
        return Ok(PreparedFiles {
            entries: Vec::new(),
            uploads: Vec::new(),
            manifest: Vec::new(),
        });
    }
    let selections = files::inspect(paths).map_err(|reason| CommandError::new(reason, 1))?;
    let selection_view: Vec<Value> = selections
        .iter()
        .map(|file| {
            json!({
                "name":file.name,
                "path":file.path,
                "size_bytes":file.size_bytes
            })
        })
        .collect();
    let limits_response = match lookup_limits() {
        Ok(response) if response["type"] == "ok" => response,
        Ok(response) => {
            let reason = response["reason"]
                .as_str()
                .unwrap_or("origin did not provide file limits")
                .to_owned();
            emit(&json!({
                "kind":"file_limits_unavailable",
                "operation":operation,
                "sent":false,
                "reason":reason,
                "draft":draft,
                "files":selection_view
            }))?;
            return Err(CommandError::new(
                "origin file limits are unavailable; nothing was submitted",
                EXIT_UNSUPPORTED,
            ));
        }
        Err(error) => {
            let reason = error.to_string();
            emit(&json!({
                "kind":"file_limits_unavailable",
                "operation":operation,
                "sent":false,
                "reason":reason,
                "draft":draft,
                "files":selection_view
            }))?;
            return Err(CommandError::new(
                "could not read origin file limits; nothing was submitted",
                1,
            ));
        }
    };
    let limits = match ServerFileLimits::from_response(&limits_response) {
        Ok(limits) => limits,
        Err(reason) => {
            emit(&json!({
                "kind":"file_limits_unavailable",
                "operation":operation,
                "sent":false,
                "reason":reason,
                "draft":draft,
                "files":selection_view
            }))?;
            return Err(CommandError::new(
                "origin returned unusable file limits; nothing was submitted",
                EXIT_UNSUPPORTED,
            ));
        }
    };
    let staging_directory = store.upload_staging_directory(session, op_id);
    match files::prepare_outgoing(paths, &limits, &staging_directory) {
        Ok(prepared) => Ok(prepared),
        Err(reason) => {
            emit(&json!({
                "kind":"file_preparation_refused",
                "operation":operation,
                "sent":false,
                "reason":reason,
                "inline_decoded_bytes_per_entry":limits.inline_decoded_bytes_per_entry,
                "upload_cap_bytes_per_file":limits.upload_cap_json(),
                "draft":draft,
                "files":selection_view
            }))?;
            Err(CommandError::new(reason, EXIT_REFUSED))
        }
    }
}

fn validate_request_size(request: &Value) -> Result<(), CommandError> {
    let body = serde_json::to_vec(request)
        .map_err(|error| CommandError::new(format!("could not encode request: {error}"), 1))?;
    if body.len() as u64 > files::MAX_ORIGIN_REQUEST_BYTES {
        return Err(CommandError::new(
            "origin request exceeds its existing 2 MiB body bound; nothing was submitted",
            EXIT_UNSUPPORTED,
        ));
    }
    Ok(())
}

fn validate_request_pre_upload(request: &Value) -> Result<(), CommandError> {
    let mut projected = request.clone();
    if let Some(files) = projected.get_mut("files").and_then(Value::as_array_mut) {
        for file in files {
            let Some(object) = file.as_object_mut() else {
                continue;
            };
            if object.remove("upload_pending") == Some(Value::Bool(true)) {
                object.insert(
                    "upload_id".into(),
                    Value::String("00000000-0000-4000-8000-000000000000".into()),
                );
            }
        }
    }
    validate_request_size(&projected)
}

fn validate_send_route(
    session_arg: &str,
    route: Option<OriginHarness>,
    controller: bool,
) -> Result<Option<OriginHarness>, CommandError> {
    if controller && (session_arg != "new" || route != Some(OriginHarness::Other)) {
        return Err(CommandError::new(
            "--controller requires --session new --route other",
            EXIT_REFUSED,
        ));
    }
    if session_arg == "new" && route.is_none() {
        return Err(CommandError::new(
            "--route is required when minting a new session handle",
            EXIT_REFUSED,
        ));
    }
    if session_arg != "new" && route.is_some() {
        return Err(CommandError::new(
            "--route is only accepted when minting a new session handle",
            EXIT_REFUSED,
        ));
    }
    Ok(route)
}

/// The client's existing answer descriptor schema. Choices are written as one
/// `value` action each; the server stores the message as supplied and the
/// client renders the descriptor with its existing controls.
const ANSWER_UI_SCHEMA: &str = "attention-client.actions/1";

fn choices_answer_ui(choices: &[String]) -> Result<Value, CommandError> {
    let mut seen = BTreeSet::new();
    let mut actions = Vec::with_capacity(choices.len());
    for (index, choice) in choices.iter().enumerate() {
        if choice.trim().is_empty() {
            return Err(CommandError::new(
                format!("--choice {} is empty", index + 1),
                EXIT_REFUSED,
            ));
        }
        if !seen.insert(choice.as_str()) {
            return Err(CommandError::new(
                format!("--choice {choice:?} is given more than once"),
                EXIT_REFUSED,
            ));
        }
        actions.push(json!({
            "id": format!("choice-{}", index + 1),
            "label": choice,
            "kind": "value",
            "value": choice
        }));
    }
    Ok(json!({ "schema": ANSWER_UI_SCHEMA, "actions": actions }))
}

/// Builds the revised message from the current one: `--choice` replaces the
/// answer descriptor, `--clear-choices` removes it, neither keeps it. A legacy
/// `choices` member written by an earlier build is never carried forward.
fn revised_message(
    current: &Value,
    title: &str,
    body: Option<&str>,
    choices: &[String],
    clear_choices: bool,
) -> Result<Value, CommandError> {
    let answer_ui = if choices.is_empty() {
        None
    } else {
        Some(choices_answer_ui(choices)?)
    };
    let mut message = current.clone();
    message["title"] = Value::String(title.to_owned());
    message["created_at"] = Value::from(now_ms());
    if let Some(body) = body {
        message["body"] = Value::String(body.to_owned());
    }
    if let Some(object) = message.as_object_mut() {
        object.remove("choices");
        if clear_choices {
            object.remove("answer_ui");
        }
    }
    if let Some(answer_ui) = answer_ui {
        message["answer_ui"] = answer_ui;
    }
    Ok(message)
}

pub fn send_command(
    context: &CommandContext,
    session: Option<&str>,
    sender_id: Option<&str>,
    title: Option<&str>,
    body: Option<&str>,
    choices: &[String],
    vendor_session_id: Option<&str>,
    route: Option<OriginHarness>,
    controller: bool,
    files: &[PathBuf],
    prepare: bool,
    execute: bool,
    op_id: Option<Uuid>,
) -> Result<(), CommandError> {
    if prepare && execute {
        return Err(CommandError::new(
            "choose only one of --prepare or --execute",
            EXIT_REFUSED,
        ));
    }
    if execute {
        let op_id =
            op_id.ok_or_else(|| CommandError::new("--execute requires --op-id", EXIT_REFUSED))?;
        if session.is_some()
            || sender_id.is_some()
            || title.is_some()
            || body.is_some()
            || !choices.is_empty()
            || vendor_session_id.is_some()
            || route.is_some()
            || controller
            || !files.is_empty()
        {
            return Err(CommandError::new(
                "--execute accepts only --op-id; it uses the saved send inputs",
                EXIT_REFUSED,
            ));
        }
        return execute_send(context, op_id);
    }
    if op_id.is_some() && !prepare {
        return Err(CommandError::new(
            "--op-id requires --prepare or --execute",
            EXIT_REFUSED,
        ));
    }
    let session =
        session.ok_or_else(|| CommandError::new("send requires --session", EXIT_REFUSED))?;
    let sender_id =
        sender_id.ok_or_else(|| CommandError::new("send requires --sender-id", EXIT_REFUSED))?;
    let title = title.ok_or_else(|| CommandError::new("send requires --title", EXIT_REFUSED))?;
    if prepare {
        let op_id =
            op_id.ok_or_else(|| CommandError::new("--prepare requires --op-id", EXIT_REFUSED))?;
        return prepare_send(
            context,
            session,
            sender_id,
            title,
            body,
            choices,
            vendor_session_id,
            route,
            controller,
            files,
            op_id,
        );
    }
    send(
        context,
        session,
        sender_id,
        title,
        body,
        choices,
        vendor_session_id,
        route,
        controller,
        files,
    )
}

pub fn send(
    context: &CommandContext,
    session_arg: &str,
    sender_id: &str,
    title: &str,
    body: Option<&str>,
    choices: &[String],
    vendor_session_id: Option<&str>,
    route: Option<OriginHarness>,
    controller: bool,
    files: &[PathBuf],
) -> Result<(), CommandError> {
    let route = validate_send_route(session_arg, route, controller)?;
    choices_answer_ui(choices)?;
    let file_draft = json!({
        "session":session_arg,
        "sender_id":sender_id,
        "title":title,
        "body":body,
        "choices":choices,
        "vendor_session_id":vendor_session_id,
        "route":route.map(OriginHarness::as_str)
    });
    let session = if session_arg == "new" {
        Uuid::new_v4()
    } else {
        parse_session(session_arg)?
    };
    let message_id = Uuid::new_v4().to_string();
    let op_id = Uuid::new_v4();
    let mut store = context.open_store(session)?;
    let prepared_files =
        prepare_outgoing_files(context, &store, session, op_id, files, "send", file_draft)?;
    let request = build_send_request(
        op_id,
        session,
        &message_id,
        sender_id,
        title,
        body,
        choices,
        vendor_session_id,
        route,
        controller,
        &prepared_files.entries,
    )?;
    if let Err(error) = validate_request_pre_upload(&request) {
        files::cleanup_staged_uploads(&prepared_files.uploads);
        return Err(error);
    }
    let expected_session = session.to_string();
    let operation = prepare_with_uploads(
        &mut store,
        session,
        op_id,
        "send",
        request,
        prepared_files.uploads,
    )?;
    let response = submit_operation(context, &mut store, operation)?;
    validate_send_response_session(&response, &expected_session)?;
    emit(&send_result(sender_id, &message_id, session, &response))
}

fn validate_send_response_session(
    response: &Value,
    expected_session: &str,
) -> Result<(), CommandError> {
    let response_session = response["session_handle"]
        .as_str()
        .unwrap_or(expected_session);
    if response_session != expected_session {
        return Err(CommandError::new(
            "server send response named a different origin session",
            1,
        ));
    }
    Ok(())
}

fn build_send_request(
    op_id: Uuid,
    session: Uuid,
    message_id: &str,
    sender_id: &str,
    title: &str,
    body: Option<&str>,
    choices: &[String],
    vendor_session_id: Option<&str>,
    route: Option<OriginHarness>,
    controller: bool,
    file_entries: &[Value],
) -> Result<Value, CommandError> {
    let answer_ui = if choices.is_empty() {
        None
    } else {
        Some(choices_answer_ui(choices)?)
    };
    let mut message = json!({
        "sender_id": sender_id,
        "message_id": message_id,
        "created_at": now_ms(),
        "title": title
    });
    if let Some(body) = body {
        message["body"] = Value::String(body.to_owned());
    }
    if let Some(answer_ui) = answer_ui {
        message["answer_ui"] = answer_ui;
    }

    let mut request = json!({
        "v": 1,
        "type": "send",
        "op_id": op_id.to_string(),
        "session_handle": session.to_string(),
        "message": message
    });
    if let Some(route) = route {
        request["route"] = Value::String(route.as_str().to_owned());
        request["cli_version"] = Value::String(env!("CARGO_PKG_VERSION").to_owned());
        request["unsupported_reason"] = route.unsupported_reason(controller);
    }
    if let Some(vendor_session_id) = vendor_session_id {
        request["vendor_session_id"] = Value::String(vendor_session_id.to_owned());
    }
    if !file_entries.is_empty() {
        request["files"] = Value::Array(file_entries.to_vec());
    }
    Ok(request)
}

fn send_input_identity(
    session_arg: &str,
    sender_id: &str,
    title: &str,
    body: Option<&str>,
    choices: &[String],
    vendor_session_id: Option<&str>,
    route: Option<OriginHarness>,
    controller: bool,
    files: &[files::FileManifestEntry],
) -> Value {
    json!({
        "session":session_arg,
        "sender_id":sender_id,
        "title":title,
        "body":body,
        "choices":choices,
        "vendor_session_id":vendor_session_id,
        "route":route.map(OriginHarness::as_str),
        "controller":controller,
        "files":files
    })
}

pub fn prepare_send(
    context: &CommandContext,
    session_arg: &str,
    sender_id: &str,
    title: &str,
    body: Option<&str>,
    choices: &[String],
    vendor_session_id: Option<&str>,
    route: Option<OriginHarness>,
    controller: bool,
    files: &[PathBuf],
    op_id: Uuid,
) -> Result<(), CommandError> {
    if op_id.get_version_num() != 4 {
        return Err(CommandError::new("send op_id must be UUIDv4", EXIT_REFUSED));
    }
    choices_answer_ui(choices)?;
    let route = validate_send_route(session_arg, route, controller)?;
    let global = context.open_global_store()?;

    if let Some(existing) = global
        .operation(op_id)
        .map_err(|error| journal_error(error, 1))?
    {
        let saved = existing
            .prepared_send
            .as_ref()
            .ok_or_else(|| journal_error(JournalError::OperationConflict(op_id), EXIT_REFUSED))?;
        let manifest = files::identify(files).map_err(|reason| CommandError::new(reason, 1))?;
        let inputs = send_input_identity(
            session_arg,
            sender_id,
            title,
            body,
            choices,
            vendor_session_id,
            route,
            controller,
            &manifest,
        );
        if saved.inputs != inputs || saved.files != manifest {
            return Err(journal_error(
                JournalError::OperationConflict(op_id),
                EXIT_REFUSED,
            ));
        }
        return emit_prepared_or_saved_result(&existing, op_id);
    }

    let session = if session_arg == "new" {
        Uuid::new_v4()
    } else {
        parse_session(session_arg)?
    };
    let mut store = context.open_store(session)?;
    let draft = send_draft(
        session_arg,
        sender_id,
        title,
        body,
        choices,
        vendor_session_id,
        route,
    );
    let prepared_files =
        prepare_outgoing_files(context, &store, session, op_id, files, "send", draft)?;
    let message_id = Uuid::new_v4().to_string();
    let request = match build_send_request(
        op_id,
        session,
        &message_id,
        sender_id,
        title,
        body,
        choices,
        vendor_session_id,
        route,
        controller,
        &prepared_files.entries,
    ) {
        Ok(request) => request,
        Err(error) => {
            files::cleanup_staged_uploads(&prepared_files.uploads);
            return Err(error);
        }
    };
    if let Err(error) = validate_request_pre_upload(&request) {
        files::cleanup_staged_uploads(&prepared_files.uploads);
        return Err(error);
    }
    let inputs = send_input_identity(
        session_arg,
        sender_id,
        title,
        body,
        choices,
        vendor_session_id,
        route,
        controller,
        &prepared_files.manifest,
    );
    let prepared_send = PreparedSend {
        inputs,
        request: request.clone(),
        files: prepared_files.manifest,
    };
    let operation = store
        .prepare_send_operation(
            op_id,
            session,
            request,
            prepared_files.uploads.clone(),
            prepared_send,
        )
        .map_err(|error| journal_error(error, EXIT_REFUSED))?;
    if operation.session_handle != session {
        files::cleanup_staged_uploads(&prepared_files.uploads);
    }
    emit_prepared_or_saved_result(&operation, op_id)
}

fn send_draft(
    session_arg: &str,
    sender_id: &str,
    title: &str,
    body: Option<&str>,
    choices: &[String],
    vendor_session_id: Option<&str>,
    route: Option<OriginHarness>,
) -> Value {
    json!({
        "session":session_arg,
        "sender_id":sender_id,
        "title":title,
        "body":body,
        "choices":choices,
        "vendor_session_id":vendor_session_id,
        "route":route.map(OriginHarness::as_str)
    })
}

fn emit_prepared_or_saved_result(
    operation: &OperationRecord,
    op_id: Uuid,
) -> Result<(), CommandError> {
    let prepared = operation
        .prepared_send
        .as_ref()
        .ok_or_else(|| journal_error(JournalError::OperationConflict(op_id), EXIT_REFUSED))?;
    match operation.state {
        OperationState::Committed => {
            let response = operation
                .response
                .clone()
                .ok_or_else(|| CommandError::new("committed operation has no saved response", 1))?;
            emit(&retry_committed_result(
                "send",
                op_id,
                "send",
                &prepared.request,
                response,
            )?)
        }
        OperationState::Rejected => refused(
            "send",
            Some(op_id),
            operation.response.clone().unwrap_or(Value::Null),
        ),
        OperationState::Prepared | OperationState::Unknown => emit(&json!({
            "kind":"prepared",
            "op_id":op_id,
            "request":prepared.request,
            "files":prepared.files
        })),
    }
}

pub fn execute_send(context: &CommandContext, op_id: Uuid) -> Result<(), CommandError> {
    if op_id.get_version_num() != 4 {
        return Err(CommandError::new("send op_id must be UUIDv4", EXIT_REFUSED));
    }
    let global = context.open_global_store()?;
    let existing = global
        .operation(op_id)
        .map_err(|error| journal_error(error, 1))?
        .ok_or_else(|| CommandError::new(format!("no recorded operation with key {op_id}"), 1))?;
    if existing.kind != "send" || existing.prepared_send.is_none() {
        return Err(journal_error(
            JournalError::OperationConflict(op_id),
            EXIT_REFUSED,
        ));
    }
    let mut store = context.open_store(existing.session_handle)?;
    let operation = store
        .operation(op_id)
        .map_err(|error| journal_error(error, 1))?
        .ok_or_else(|| CommandError::new(format!("no recorded operation with key {op_id}"), 1))?;
    match operation.state {
        OperationState::Committed => {
            let response = operation
                .response
                .clone()
                .ok_or_else(|| CommandError::new("committed operation has no saved response", 1))?;
            validate_send_response_session(&response, &operation.session_handle.to_string())?;
            emit_committed("send", op_id, response)
        }
        OperationState::Rejected => refused(
            "send",
            Some(op_id),
            operation.response.clone().unwrap_or(Value::Null),
        ),
        OperationState::Prepared | OperationState::Unknown => {
            let response = submit_operation(context, &mut store, operation.clone())?;
            validate_send_response_session(&response, &operation.session_handle.to_string())?;
            emit_committed("send", op_id, response)
        }
    }
}

fn send_result(sender_id: &str, message_id: &str, session: Uuid, response: &Value) -> Value {
    json!({
        "request": message_id,
        "sender_id": sender_id,
        "message_url": message_url(sender_id, message_id),
        "session": session,
        "revision": response["revision"].as_u64().unwrap_or(1),
        "listening_support": response.get("listening_support").cloned().unwrap_or(Value::Null)
    })
}

fn message_url(sender_id: &str, message_id: &str) -> String {
    format!(
        "harklight://message/{}/{}",
        encode_path_segment(sender_id),
        encode_path_segment(message_id)
    )
}

fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

fn attachment_update_request(op_id: Uuid, session: Uuid, vendor_session_id: Option<&str>) -> Value {
    json!({
        "v":1,
        "type":"attachment_update",
        "op_id":op_id.to_string(),
        "session_handle":session.to_string(),
        "vendor_session_id":vendor_session_id
    })
}

fn attachment_support_request(
    op_id: Uuid,
    session: Uuid,
    route: OriginHarness,
    controller: bool,
) -> Value {
    json!({
        "v":1,
        "type":"support",
        "op_id":op_id.to_string(),
        "session_handle":session.to_string(),
        "route":route.as_str(),
        "cli_version":env!("CARGO_PKG_VERSION"),
        "unsupported_reason":route.unsupported_reason(controller)
    })
}

fn same_operation_payload(left: &Value, right: &Value) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    if let Some(object) = left.as_object_mut() {
        object.remove("op_id");
    }
    if let Some(object) = right.as_object_mut() {
        object.remove("op_id");
    }
    left == right
}

/// Reuse an unresolved operation only for the same semantic request. A
/// confirmed result can also be reused by a multi-step command that is being
/// resumed after the process stopped between phases.
fn prepare_or_resume_operation(
    store: &mut JournalStore,
    session: Uuid,
    kind: &str,
    request: Value,
    reuse_committed: bool,
) -> Result<OperationRecord, CommandError> {
    let operations = store
        .operations_for_session(session)
        .map_err(|error| journal_error(error, 1))?;
    let pending: Vec<_> = operations
        .iter()
        .filter(|operation| {
            operation.kind == kind
                && matches!(
                    operation.state,
                    OperationState::Prepared | OperationState::Unknown
                )
        })
        .collect();
    if !pending.is_empty() {
        if pending.len() == 1 && same_operation_payload(&pending[0].request, &request) {
            return Ok(pending[0].clone());
        }
        let operation = pending[0];
        return Err(CommandError::new(
            format!(
                "{} operation {} is unresolved; retry that exact operation before changing its fields",
                kind, operation.op_id
            ),
            EXIT_OUTCOME_UNKNOWN,
        ));
    }

    if reuse_committed {
        if let Some(operation) = operations.iter().rev().find(|operation| {
            operation.kind == kind && operation.state == OperationState::Committed
        }) {
            if same_operation_payload(&operation.request, &request) {
                return Ok(operation.clone());
            }
        }
    }

    let op_id = parse_uuid_field(&request, "op_id")?;
    prepare(store, session, op_id, kind, request)
}

fn handle_operation_error_response(
    store: &mut JournalStore,
    operation: &OperationRecord,
    response: Value,
    announce: bool,
) -> Result<Value, CommandError> {
    let reason = response["reason"].as_str().unwrap_or("refused").to_owned();
    if reason == "storage_unavailable"
        && matches!(
            operation.kind.as_str(),
            "attachment_update" | "attachment_support" | "support"
        )
    {
        if announce {
            emit(&json!({
                "kind":"outcome_unknown",
                "operation":operation.kind,
                "op_id":operation.op_id,
                "session_handle":operation.session_handle,
                "reason":reason,
                "response":response,
                "retry":"run harklight-agent retry --op-id with this exact key"
            }))?;
        }
        return Err(CommandError::new(
            format!(
                "{} outcome is unknown: storage_unavailable; retry the identical persisted operation",
                operation.kind
            ),
            EXIT_OUTCOME_UNKNOWN,
        ));
    }

    store
        .reject_operation(operation.op_id, response.clone())
        .map_err(|error| journal_error(error, 1))?;
    if announce {
        refused(&operation.kind, Some(operation.op_id), response)?;
    }
    Err(CommandError::new(
        format!("{} refused: {reason}", operation.kind),
        EXIT_REFUSED,
    ))
}

fn attachment_update_response_matches(request: &Value, response: &Value) -> bool {
    request["v"] == 1
        && request["type"] == "attachment_update"
        && response["v"] == 1
        && response["type"] == "ok"
        && response.get("session_handle") == request.get("session_handle")
        && request
            .get("session_handle")
            .and_then(Value::as_str)
            .is_some()
        && request.get("vendor_session_id").is_some_and(|vendor_id| {
            (vendor_id.is_null()
                || vendor_id
                    .as_str()
                    .is_some_and(|vendor_id| !vendor_id.is_empty()))
                && response.get("vendor_session_id") == Some(vendor_id)
        })
}

fn local_journal_proves_existing_session(
    store: &JournalStore,
    session: Uuid,
) -> Result<bool, CommandError> {
    let session_value = session.to_string();
    let operations = store
        .operations_for_session(session)
        .map_err(|error| journal_error(error, 1))?;
    Ok(operations.iter().any(|operation| {
        operation.kind == "send"
            && operation.state == OperationState::Committed
            && operation.request["session_handle"].as_str() == Some(session_value.as_str())
    }))
}

fn ensure_attach_recovery_compatible(
    store: &JournalStore,
    session: Uuid,
    vendor_session_id: Option<&str>,
    support_request: &Value,
) -> Result<(), CommandError> {
    let operations = store
        .operations_for_session(session)
        .map_err(|error| journal_error(error, 1))?;
    if let Some(operation) = operations.iter().find(|operation| {
        operation.kind == "support"
            && matches!(
                operation.state,
                OperationState::Prepared | OperationState::Unknown
            )
    }) {
        return Err(CommandError::new(
            format!(
                "H5 support operation {} is unresolved; retry that exact operation before attaching",
                operation.op_id
            ),
            EXIT_OUTCOME_UNKNOWN,
        ));
    }

    if let Some((index, operation)) = operations.iter().enumerate().find(|(_, operation)| {
        operation.kind == "attachment_support"
            && matches!(
                operation.state,
                OperationState::Prepared | OperationState::Unknown
            )
    }) {
        let update = operations[..index]
            .iter()
            .rev()
            .find(|prior| prior.kind == "attachment_update");
        let expected_vendor_id = vendor_session_id.map(Value::from).unwrap_or(Value::Null);
        let same_update = update.is_some_and(|update| {
            update.state == OperationState::Committed
                && update.request["vendor_session_id"] == expected_vendor_id
                && update.response.as_ref().is_some_and(|response| {
                    attachment_update_response_matches(&update.request, response)
                })
        });
        if !same_update || !same_operation_payload(&operation.request, support_request) {
            return Err(CommandError::new(
                format!(
                    "attachment support operation {} is unresolved; retry the same attach inputs before changing the handle or route",
                    operation.op_id
                ),
                EXIT_OUTCOME_UNKNOWN,
            ));
        }
    }
    Ok(())
}

pub fn attach(
    context: &CommandContext,
    session: Uuid,
    vendor_session_arg: &str,
    route: OriginHarness,
    controller: bool,
) -> Result<(), CommandError> {
    if session.get_version() != Some(Version::Random) {
        return Err(CommandError::new(
            "--session must be an existing UUIDv4 Harklight handle",
            EXIT_REFUSED,
        ));
    }
    if controller && route != OriginHarness::Other {
        return Err(CommandError::new(
            "--controller is valid only with --route other",
            EXIT_REFUSED,
        ));
    }
    let vendor_session_id = match vendor_session_arg {
        "none" => None,
        "" => {
            return Err(CommandError::new(
                "--vendor-session-id must be a non-empty ID or none",
                EXIT_REFUSED,
            ));
        }
        value => Some(value),
    };

    let mut store = context.open_store(session)?;
    store
        .acquire_listener()
        .map_err(|error| journal_error(error, EXIT_LISTENER_ACTIVE))?;
    if !local_journal_proves_existing_session(&store, session)? {
        return Err(CommandError::new(
            "the local origin journal does not prove this existing handle; attach requires the same enrolled origin and host journal",
            EXIT_REFUSED,
        ));
    }

    let support_request = attachment_support_request(Uuid::new_v4(), session, route, controller);
    ensure_attach_recovery_compatible(&store, session, vendor_session_id, &support_request)?;

    let attachment_request = attachment_update_request(Uuid::new_v4(), session, vendor_session_id);
    let attachment_operation = prepare_or_resume_operation(
        &mut store,
        session,
        "attachment_update",
        attachment_request,
        true,
    )?;
    let attachment_op_id = attachment_operation.op_id;
    let attachment_response = submit_operation(context, &mut store, attachment_operation.clone())?;
    if !attachment_update_response_matches(&attachment_operation.request, &attachment_response) {
        return Err(CommandError::new(
            "the saved attachment result does not confirm the requested handle and vendor ID; do not listen",
            EXIT_REFUSED,
        ));
    }

    let support_operation = prepare_or_resume_operation(
        &mut store,
        session,
        "attachment_support",
        support_request.clone(),
        false,
    )?;
    let support_op_id = support_operation.op_id;
    let _support_response = submit_operation(context, &mut store, support_operation)?;
    let status = read_status(context, session)?;
    if !support_record_matches(&status, &support_request) {
        emit(&json!({
            "kind":"unsupported",
            "operation":"attach",
            "session_handle":session,
            "attachment_op_id":attachment_op_id,
            "support_op_id":support_op_id,
            "reason":"origin did not confirm the requested route-support record; no listener was started",
            "listening_support":status.get("listening_support").cloned().unwrap_or(Value::Null)
        }))?;
        return Err(CommandError::new(
            "origin did not confirm the requested route-support record; no listener was started",
            EXIT_UNSUPPORTED,
        ));
    }

    let unsupported_reason = support_request["unsupported_reason"].as_str();
    emit(&json!({
        "kind":"attached",
        "session_handle":session,
        "vendor_session_id":vendor_session_id,
        "attachment_op_id":attachment_op_id,
        "support_op_id":support_op_id,
        "ready_for_listen":unsupported_reason.is_none(),
        "listening_support":status.get("listening_support").cloned().unwrap_or(Value::Null)
    }))
}

fn attachment_route_for_listen(
    store: &JournalStore,
    session: Uuid,
) -> Result<Option<String>, CommandError> {
    let operations = store
        .operations_for_session(session)
        .map_err(|error| journal_error(error, 1))?;
    let Some(attachment_index) = operations
        .iter()
        .rposition(|operation| operation.kind == "attachment_update")
    else {
        return Ok(None);
    };
    let attachment = &operations[attachment_index];
    if attachment.state != OperationState::Committed {
        let exit_code = if matches!(
            attachment.state,
            OperationState::Prepared | OperationState::Unknown
        ) {
            EXIT_OUTCOME_UNKNOWN
        } else {
            EXIT_REFUSED
        };
        return Err(CommandError::new(
            format!(
                "the latest attachment operation {} is not confirmed; reconcile it before listening",
                attachment.op_id
            ),
            exit_code,
        ));
    }
    if !attachment
        .response
        .as_ref()
        .is_some_and(|response| attachment_update_response_matches(&attachment.request, response))
    {
        return Err(CommandError::new(
            "the latest attachment result does not confirm its handle and vendor ID; do not listen",
            EXIT_REFUSED,
        ));
    }
    if !local_journal_proves_existing_session(store, session)? {
        return Err(CommandError::new(
            "the local origin journal no longer proves this existing handle; do not listen",
            EXIT_REFUSED,
        ));
    }

    let support = operations[attachment_index + 1..]
        .iter()
        .rev()
        .find(|operation| operation.kind == "attachment_support")
        .ok_or_else(|| {
            CommandError::new(
                "the confirmed attachment has no confirmed H5 support result; run attach before listening",
                EXIT_REFUSED,
            )
        })?;
    if support.state != OperationState::Committed {
        let exit_code = if matches!(
            support.state,
            OperationState::Prepared | OperationState::Unknown
        ) {
            EXIT_OUTCOME_UNKNOWN
        } else {
            EXIT_REFUSED
        };
        return Err(CommandError::new(
            format!(
                "the H5 support operation {} after attachment is not confirmed; reconcile it before listening",
                support.op_id
            ),
            exit_code,
        ));
    }
    if support.request["type"] != "support" {
        return Err(CommandError::new(
            "the journaled H5 support operation has an invalid request; do not listen",
            EXIT_REFUSED,
        ));
    }
    let route = support.request["route"]
        .as_str()
        .filter(|route| matches!(*route, "claude-code" | "codex" | "other"))
        .ok_or_else(|| {
            CommandError::new(
                "the confirmed H5 support operation has no recognized route; do not listen",
                EXIT_REFUSED,
            )
        })?;
    Ok(Some(route.to_owned()))
}

pub fn ack(context: &CommandContext, session: Uuid, event_id: &str) -> Result<(), CommandError> {
    validate_event_id(event_id)?;
    let mut store = context.open_global_store()?;
    let entry = store
        .entry_by_event(event_id)
        .map_err(|error| journal_error(error, 1))?
        .ok_or_else(|| {
            CommandError::new(
                "event is not in the durable journal; run listen --next first",
                1,
            )
        })?;
    if entry.session_handle != session {
        return Err(CommandError::new(
            "event belongs to a different origin session",
            EXIT_REFUSED,
        ));
    }
    if !entry.actionable || entry.own_entry() {
        return Err(CommandError::new(
            "only a person or server actionable event can be acknowledged",
            EXIT_REFUSED,
        ));
    }
    if entry.delivered_at_ms.is_some() && server_file_transfers_complete(&entry.raw) {
        return emit(&json!({
            "kind":"acknowledged",
            "event_id":event_id,
            "status":"already_delivered",
            "acted":entry.acted_at_ms.is_some() || entry.raw["acted"] == true
        }));
    }
    let response = acknowledge_event(context, &mut store, event_id, true)?;
    let status = response["status"].as_str().unwrap_or_default();
    let acted = response["acted"] == true;
    emit(&json!({
        "kind":"acknowledged",
        "event_id":event_id,
        "status":status,
        "acted":acted,
        "file_transfers":response.get("file_transfers").cloned().unwrap_or(Value::Null)
    }))
}

pub fn file_use(
    context: &CommandContext,
    session: Uuid,
    event_id: &str,
    file_name: &str,
    status: &str,
    reason: Option<&str>,
) -> Result<(), CommandError> {
    validate_event_id(event_id)?;
    let outcome = match (status, reason) {
        ("used", None) => json!({"status":"used"}),
        ("used", Some(_)) => {
            return Err(CommandError::new(
                "--reason is only valid with --unusable",
                EXIT_REFUSED,
            ));
        }
        ("unusable", Some(reason)) if !reason.trim().is_empty() => {
            json!({"status":"unusable","reason":reason})
        }
        ("unusable", _) => {
            return Err(CommandError::new(
                "--unusable requires a nonempty --reason",
                EXIT_REFUSED,
            ));
        }
        _ => return Err(CommandError::new("invalid file-use outcome", EXIT_REFUSED)),
    };

    let mut store = context.open_global_store()?;
    let entry = store
        .entry_by_event(event_id)
        .map_err(|error| journal_error(error, 1))?
        .ok_or_else(|| {
            CommandError::new(
                "event is not in the durable journal; listen for it before reporting file use",
                EXIT_REFUSED,
            )
        })?;
    if entry.session_handle != session || !entry.actionable || entry.own_entry() {
        return Err(CommandError::new(
            "file-use event does not belong to this origin session",
            EXIT_REFUSED,
        ));
    }
    if entry.delivered_at_ms.is_none() {
        return Err(CommandError::new(
            "ack the event before reporting file use",
            EXIT_REFUSED,
        ));
    }
    let file_present = entry.raw["files"]
        .as_array()
        .is_some_and(|files| files.iter().any(|file| file["name"] == file_name));
    if !file_present {
        return Err(CommandError::new(
            "file name is not attached to this event",
            EXIT_REFUSED,
        ));
    }
    if !entry
        .file_transfers
        .get(file_name)
        .is_some_and(|transfer| transfer.transferred)
    {
        return Err(CommandError::new(
            "the file was not transferred to this session; file use was not reported",
            EXIT_REFUSED,
        ));
    }
    if entry.raw["file_transfers"][file_name]["status"].as_str() != Some("transferred") {
        return Err(CommandError::new(
            "the server has not confirmed this file as transferred; file use was not reported",
            EXIT_REFUSED,
        ));
    }

    let op_id = Uuid::new_v4();
    let mut request = json!({
        "v":1,
        "type":"file_use",
        "op_id":op_id,
        "event_id":event_id,
        "file":file_name,
        "status":status
    });
    if let Some(reason) = reason {
        request["reason"] = Value::String(reason.to_owned());
    }
    let operation = prepare(&mut store, session, op_id, "file_use", request)?;
    let response = submit_operation(context, &mut store, operation)?;
    if response["status"].as_str() != Some(status) {
        return Err(CommandError::new(
            "origin file-use response did not confirm the requested outcome",
            EXIT_REFUSED,
        ));
    }
    store
        .record_server_file_use(event_id, file_name, outcome)
        .map_err(|error| journal_error(error, 1))?;
    emit_committed("file_use", op_id, response)
}

pub fn reply(
    context: &CommandContext,
    session: Uuid,
    request_id: &str,
    in_reply_to: &str,
    text: &str,
    files: &[PathBuf],
    op_id: Option<Uuid>,
) -> Result<(), CommandError> {
    let file_draft = json!({
        "request":request_id,
        "in_reply_to":in_reply_to,
        "text":text
    });
    validate_event_id(in_reply_to)?;
    let mut store = context.open_store(session)?;
    let snapshot = store
        .request_snapshot(session, request_id, Some(in_reply_to))
        .map_err(|error| journal_error(error, 1))?;
    let request = request_context(&snapshot, session, request_id)?;
    ensure_request_open(&snapshot, session, &request)?;
    let event = delivered_event(
        &snapshot,
        session,
        &request,
        in_reply_to,
        "discussion",
        false,
    )?;
    ensure_current_revision(request.revision, event.request_revision)?;
    let revision = request.revision;
    let op_id = op_id.unwrap_or_else(Uuid::new_v4);
    let prepared_files =
        prepare_outgoing_files(context, &store, session, op_id, files, "reply", file_draft)?;
    let mut body = json!({
        "v": 1,
        "type": "reply",
        "op_id": op_id.to_string(),
        "sender_id": request.sender_id,
        "message_id": request.message_id,
        "request_revision": revision,
        "in_reply_to": in_reply_to,
        "text": text
    });
    if !prepared_files.entries.is_empty() {
        body["files"] = Value::Array(prepared_files.entries.clone());
    }
    if let Err(error) = validate_request_pre_upload(&body) {
        files::cleanup_staged_uploads(&prepared_files.uploads);
        return Err(error);
    }
    let operation = prepare_with_uploads(
        &mut store,
        session,
        op_id,
        "reply",
        std::mem::take(&mut body),
        prepared_files.uploads,
    )?;
    let response = submit_operation(context, &mut store, operation)?;
    emit_committed("reply", op_id, response)
}

pub fn revise(
    context: &CommandContext,
    session: Uuid,
    request_id: &str,
    base_revision: u64,
    title: &str,
    body: Option<&str>,
    choices: &[String],
    clear_choices: bool,
    in_reply_to: Option<&str>,
    files: &[PathBuf],
    op_id: Option<Uuid>,
) -> Result<(), CommandError> {
    if clear_choices && !choices.is_empty() {
        return Err(CommandError::new(
            "use either --choice or --clear-choices, not both",
            1,
        ));
    }
    let file_draft = json!({
        "request":request_id,
        "base_revision":base_revision,
        "title":title,
        "body":body,
        "choices":choices,
        "clear_choices":clear_choices,
        "in_reply_to":in_reply_to
    });
    let mut store = context.open_store(session)?;
    let snapshot = store
        .request_snapshot(session, request_id, in_reply_to)
        .map_err(|error| journal_error(error, 1))?;
    let request = request_context(&snapshot, session, request_id)?;
    ensure_request_open(&snapshot, session, &request)?;
    if base_revision != request.revision {
        return Err(CommandError::new(
            format!(
                "request is at revision {}; revise from that base revision",
                request.revision
            ),
            EXIT_REFUSED,
        ));
    }
    if let Some(event_id) = in_reply_to {
        validate_event_id(event_id)?;
        let event = delivered_event(&snapshot, session, &request, event_id, "discussion", false)?;
        ensure_current_revision(request.revision, event.request_revision)?;
    }
    let mut message = revised_message(&request.message, title, body, choices, clear_choices)?;
    message["sender_id"] = Value::String(request.sender_id.clone());
    message["message_id"] = Value::String(request.message_id.clone());
    let op_id = op_id.unwrap_or_else(Uuid::new_v4);
    let prepared_files =
        prepare_outgoing_files(context, &store, session, op_id, files, "revise", file_draft)?;
    let mut command = json!({
        "v":1,
        "type":"revise",
        "op_id":op_id.to_string(),
        "sender_id":request.sender_id,
        "message_id":request.message_id,
        "base_revision":base_revision,
        "message":message
    });
    if let Some(event_id) = in_reply_to {
        command["in_reply_to"] = Value::String(event_id.to_owned());
    }
    if !prepared_files.entries.is_empty() {
        command["files"] = Value::Array(prepared_files.entries.clone());
    }
    if let Err(error) = validate_request_pre_upload(&command) {
        files::cleanup_staged_uploads(&prepared_files.uploads);
        return Err(error);
    }
    let operation = prepare_with_uploads(
        &mut store,
        session,
        op_id,
        "revise",
        std::mem::take(&mut command),
        prepared_files.uploads,
    )?;
    let response = submit_operation(context, &mut store, operation)?;
    emit_committed("revise", op_id, response)
}

pub fn withdraw(
    context: &CommandContext,
    session: Uuid,
    request_id: &str,
    in_reply_to: Option<&str>,
    op_id: Option<Uuid>,
) -> Result<(), CommandError> {
    let mut store = context.open_store(session)?;
    let snapshot = store
        .request_snapshot(session, request_id, in_reply_to)
        .map_err(|error| journal_error(error, 1))?;
    let request = request_context(&snapshot, session, request_id)?;
    ensure_request_open(&snapshot, session, &request)?;
    if let Some(event_id) = in_reply_to {
        validate_event_id(event_id)?;
        let event = delivered_event(&snapshot, session, &request, event_id, "discussion", false)?;
        ensure_current_revision(request.revision, event.request_revision)?;
    }
    let mut command = json!({
        "v":1,
        "type":"withdraw",
        "op_id":op_id.unwrap_or_else(Uuid::new_v4).to_string(),
        "sender_id":request.sender_id,
        "message_id":request.message_id
    });
    if let Some(event_id) = in_reply_to {
        command["in_reply_to"] = Value::String(event_id.to_owned());
    }
    let op_id = parse_uuid_field(&command, "op_id")?;
    let operation = prepare(&mut store, session, op_id, "withdraw", command)?;
    let response = submit_operation(context, &mut store, operation)?;
    emit_committed("withdraw", op_id, response)
}

/// Retract a sender-authored request using its durable journal identity.
/// This is the same v1 withdraw operation as the legacy command, without an
/// in_reply_to discussion-turn reference.
pub fn retract(
    context: &CommandContext,
    session: Uuid,
    request_id: &str,
    op_id: Option<Uuid>,
) -> Result<(), CommandError> {
    withdraw(context, session, request_id, None, op_id)
}

fn discard_uploads_request(
    op_id: Uuid,
    sender_id: &str,
    message_id: &str,
    revision: u64,
) -> Result<Value, CommandError> {
    if op_id.get_version_num() != 4 {
        return Err(CommandError::new(
            "cleanup operation key must be UUIDv4",
            EXIT_REFUSED,
        ));
    }
    if sender_id.trim().is_empty() || message_id.trim().is_empty() || revision == 0 {
        return Err(CommandError::new(
            "cleanup requires a sender, message identity and positive revision",
            EXIT_REFUSED,
        ));
    }
    Ok(json!({
        "v":1,
        "type":"discard_uploads",
        "op_id":op_id.to_string(),
        "sender_id":sender_id,
        "message_id":message_id,
        "revision":revision
    }))
}

fn valid_discard_uploads_response(response: &Value) -> bool {
    response["type"] == "ok"
        && matches!(response["state"].as_str(), Some("discarded" | "committed"))
        && response["discarded"].as_u64().is_some()
}

fn emit_discard_uploads_committed(
    op_id: Uuid,
    request: &Value,
    response: &Value,
) -> Result<(), CommandError> {
    emit(&json!({
        "kind":"committed",
        "operation":"discard_uploads",
        "op_id":op_id,
        "sender_id":request["sender_id"],
        "message_id":request["message_id"],
        "revision":request["revision"],
        "state":response["state"],
        "discarded":response["discarded"],
        "response":response
    }))
}

/// Perform cleanup only when the caller has observed explicit navigation
/// away from a message. The local journal makes the exact scope and op key
/// recoverable if the response is lost.
pub fn discard_uploads(
    context: &CommandContext,
    sender_id: &str,
    message_id: &str,
    revision: u64,
    op_id: Option<Uuid>,
) -> Result<(), CommandError> {
    let op_id = op_id.unwrap_or_else(Uuid::new_v4);
    let request = discard_uploads_request(op_id, sender_id, message_id, revision)?;
    let mut store = context.open_global_store()?;
    let operation = prepare(
        &mut store,
        Uuid::nil(),
        op_id,
        "discard_uploads",
        request.clone(),
    )?;
    let response = submit_operation(context, &mut store, operation)?;
    emit_discard_uploads_committed(op_id, &request, &response)
}

pub fn result(
    context: &CommandContext,
    session: Uuid,
    request_id: &str,
    in_reply_to: &str,
    status: &str,
    op_id: Option<Uuid>,
) -> Result<(), CommandError> {
    validate_event_id(in_reply_to)?;
    let mut store = context.open_store(session)?;
    let snapshot = store
        .request_snapshot(session, request_id, Some(in_reply_to))
        .map_err(|error| journal_error(error, 1))?;
    let request = request_context(&snapshot, session, request_id)?;
    let prior_received = snapshot.entries.values().any(|entry| {
        entry.session_handle == session
            && entry.sender_id.as_deref() == Some(request.sender_id.as_str())
            && entry.request_id.as_deref() == Some(request.message_id.as_str())
            && entry.kind == "result"
            && entry.raw["in_reply_to"].as_str() == Some(in_reply_to)
            && entry.raw["status"] == "received"
    });
    let may_update_received = prior_received && matches!(status, "applied" | "refused");
    let event = delivered_event(
        &snapshot,
        session,
        &request,
        in_reply_to,
        "terminal",
        may_update_received,
    )?;
    if event.raw["terminal_state"] != "answered" {
        return Err(CommandError::new(
            "result is valid only for an answered terminal event",
            EXIT_REFUSED,
        ));
    }
    if event.acted_at_ms.is_some() && !may_update_received {
        return Err(CommandError::new(
            "answered event is already acted; only a prior received result can be updated",
            EXIT_REFUSED,
        ));
    }
    let command = json!({
        "v":1,
        "type":"result",
        "op_id":op_id.unwrap_or_else(Uuid::new_v4).to_string(),
        "sender_id":request.sender_id,
        "message_id":request.message_id,
        "in_reply_to":in_reply_to,
        "status":status
    });
    let op_id = parse_uuid_field(&command, "op_id")?;
    let operation = prepare(&mut store, session, op_id, "result", command)?;
    let response = submit_operation(context, &mut store, operation)?;
    emit_committed("result", op_id, response)
}

pub fn status(context: &CommandContext, session: Uuid) -> Result<(), CommandError> {
    let response = read_status(context, session)?;
    emit(&response)
}

pub fn session_end(context: &CommandContext, session: Uuid) -> Result<(), CommandError> {
    let request = json!({
        "v":1,
        "type":"session_end",
        "session_handle":session.to_string(),
        "gone":true
    });
    let response = match context.api.post(&request) {
        Ok(response) => response,
        Err(error) if error.outcome_unknown() => {
            emit(&json!({"kind":"outcome_unknown","operation":"session-end","session":session}))?;
            return Err(CommandError::new(error.to_string(), EXIT_OUTCOME_UNKNOWN));
        }
        Err(error) => return Err(api_error(error, 1)),
    };
    if response["type"] == "error" {
        return refused("session-end", None, response);
    }
    emit(&response)
}

pub fn retry(context: &CommandContext, op_id: Uuid) -> Result<(), CommandError> {
    let mut store = context.open_global_store()?;
    let operation = store
        .operation(op_id)
        .map_err(|error| journal_error(error, 1))?
        .ok_or_else(|| CommandError::new(format!("no recorded operation with key {op_id}"), 1))?;
    match operation.state {
        OperationState::Committed => {
            let response = operation
                .response
                .clone()
                .ok_or_else(|| CommandError::new("committed operation has no saved response", 1))?;
            persist_file_use_response(&mut store, &operation, &response)?;
            if operation.kind == "discard_uploads" {
                emit_discard_uploads_committed(op_id, &operation.request, &response)
            } else {
                emit(&retry_committed_result(
                    &operation.kind,
                    op_id,
                    &operation.kind,
                    &operation.request,
                    response,
                )?)
            }
        }
        OperationState::Rejected => refused(
            &operation.kind,
            Some(op_id),
            operation.response.unwrap_or(Value::Null),
        ),
        OperationState::Prepared | OperationState::Unknown => {
            let mut scoped = context.open_store(operation.session_handle)?;
            let response = submit_operation(context, &mut scoped, operation.clone())?;
            persist_file_use_response(&mut scoped, &operation, &response)?;
            if operation.kind == "discard_uploads" {
                emit_discard_uploads_committed(op_id, &operation.request, &response)
            } else {
                emit(&retry_committed_result(
                    "retry",
                    op_id,
                    &operation.kind,
                    &operation.request,
                    response,
                )?)
            }
        }
    }
}

fn persist_file_use_response(
    store: &mut JournalStore,
    operation: &OperationRecord,
    response: &Value,
) -> Result<(), CommandError> {
    if operation.kind != "file_use"
        || response["type"] != "ok"
        || response["status"] != operation.request["status"]
    {
        return Ok(());
    }
    let event_id = operation.request["event_id"]
        .as_str()
        .ok_or_else(|| CommandError::new("file-use operation has no event id", 1))?;
    let file_name = operation.request["file"]
        .as_str()
        .ok_or_else(|| CommandError::new("file-use operation has no file name", 1))?;
    let mut outcome = json!({"status":operation.request["status"]});
    if let Some(reason) = operation.request["reason"].as_str() {
        outcome["reason"] = Value::String(reason.to_owned());
    }
    store
        .record_server_file_use(event_id, file_name, outcome)
        .map_err(|error| journal_error(error, 1))
}

pub fn listen(
    context: &CommandContext,
    session: Uuid,
    session_context: Option<SessionContext>,
    claude_code_version: Option<&str>,
    background_tasks_enabled: bool,
    follow: bool,
    controller: bool,
) -> Result<(), CommandError> {
    if controller && !follow {
        return Err(CommandError::new(
            "--controller requires listen --follow",
            EXIT_REFUSED,
        ));
    }
    let mut store = context.open_store(session)?;
    store.acquire_listener().map_err(|error| {
        let code = if matches!(
            error,
            JournalError::ListenerActive(_) | JournalError::ListenerActiveUnknown
        ) {
            EXIT_LISTENER_ACTIVE
        } else {
            1
        };
        journal_error(error, code)
    })?;

    let attached_route = attachment_route_for_listen(&store, session)?;

    let current_status = read_status(context, session)?;
    let support = current_status["listening_support"]
        .as_object()
        .ok_or_else(|| {
            let reason = "this handle has no listening support record; route cannot be inferred";
            let _ = emit(&json!({
                "kind":"unsupported",
                "operation":"listen",
                "session_handle":session,
                "reason":reason,
                "listening_support":null
            }));
            CommandError::new(reason, EXIT_UNSUPPORTED)
        })?;
    let route = match support.get("route").and_then(Value::as_str) {
        Some("claude-code") => OriginHarness::ClaudeCode,
        Some("codex") => OriginHarness::Codex,
        Some("other") => OriginHarness::Other,
        _ => {
            return unsupported(
                "listen",
                "the server support record has no recognized route; no stream was attached",
            );
        }
    };
    if attached_route
        .as_deref()
        .is_some_and(|attached| attached != route.as_str())
    {
        return unsupported(
            "listen",
            "the current server route differs from the confirmed local attach; run attach again before listening",
        );
    }
    if controller && route != OriginHarness::Other {
        return Err(CommandError::new(
            "--controller requires a session declared with route other",
            EXIT_REFUSED,
        ));
    }
    let unsupported_reason = listener_unsupported_reason(
        route,
        session_context,
        claude_code_version,
        background_tasks_enabled,
        controller,
    );
    let arm = json!({
        "v":1,
        "type":"support",
        "op_id":Uuid::new_v4().to_string(),
        "session_handle":session.to_string(),
        "route":route.as_str(),
        "cli_version":env!("CARGO_PKG_VERSION"),
        "unsupported_reason":unsupported_reason.clone()
    });
    let operation = prepare_or_resume_operation(&mut store, session, "support", arm, false)?;
    let arm_response = submit_operation(context, &mut store, operation)?;
    if let Some(reason) = unsupported_reason.as_str() {
        emit(&json!({
            "kind":"unsupported",
            "operation":"listen",
            "session_handle":session,
            "route":route.as_str(),
            "reason":reason,
            "listening_support":arm_response.get("listening_support").cloned().unwrap_or(Value::Null)
        }))?;
        return Err(CommandError::new(reason, EXIT_UNSUPPORTED));
    }
    let armed_status = read_status(context, session)?;
    if !support_record_matches(
        &armed_status,
        &json!({
            "route":route.as_str(),
            "cli_version":env!("CARGO_PKG_VERSION"),
            "unsupported_reason":Value::Null
        }),
    ) {
        let reason = "origin did not confirm the supported arm-time record; no stream was attached";
        emit(&json!({
            "kind":"unsupported",
            "operation":"listen",
            "session_handle":session,
            "route":route.as_str(),
            "reason":reason,
            "listening_support":armed_status.get("listening_support").cloned().unwrap_or(Value::Null)
        }))?;
        return Err(CommandError::new(reason, EXIT_UNSUPPORTED));
    }

    let mut offered = BTreeSet::new();
    let mut reconnect_delay = Duration::from_millis(250);
    let max_reconnect_delay = Duration::from_secs(30);
    loop {
        let mut stream = match context.api.connect_stream(session) {
            Ok(stream) => {
                // A reconnect is an interruption boundary. Re-offer all
                // still-pending entries from the durable journal, preserving
                // their delivered/acted facts in each line.
                offered.clear();
                reconnect_delay = Duration::from_millis(250);
                stream
            }
            Err(error) if stream_should_reconnect(&error) => {
                eprintln!(
                    "Harklight origin stream unavailable; retrying after disconnect: {error}"
                );
                thread::sleep(reconnect_delay);
                reconnect_delay = reconnect_delay.saturating_mul(2).min(max_reconnect_delay);
                continue;
            }
            Err(ApiError::Refused(reason)) if reason.contains("already_listening") => {
                return Err(CommandError::new(
                    "origin session already has a server listener attached",
                    EXIT_LISTENER_ACTIVE,
                ));
            }
            Err(error) => return Err(api_error(error, EXIT_LISTENER_ACTIVE)),
        };

        reconcile_session(context, &mut store, session)?;
        fetch_feed(context, &mut store)?;
        let status = read_status(context, session)?;
        if emit_pending(&mut store, session, follow, &mut offered)? {
            return Ok(());
        }
        if session_idle(&status)? {
            emit(&json!({"kind":"idle","open_requests":0}))?;
            return Ok(());
        }

        loop {
            match stream.next_change() {
                Ok(notice_cursor) => {
                    let journal_cursor = store.cursor().map_err(|error| journal_error(error, 1))?;
                    if notice_cursor <= journal_cursor {
                        continue;
                    }
                    fetch_feed(context, &mut store)?;
                    let status = read_status(context, session)?;
                    if emit_pending(&mut store, session, follow, &mut offered)? {
                        return Ok(());
                    }
                    if session_idle(&status)? {
                        emit(&json!({"kind":"idle","open_requests":0}))?;
                        return Ok(());
                    }
                }
                Err(ApiError::StreamEnded | ApiError::StreamIo(_)) => break,
                Err(error) => return Err(api_error(error, 1)),
            }
        }
        eprintln!("Harklight origin stream disconnected; reconnecting");
        thread::sleep(reconnect_delay);
        reconnect_delay = reconnect_delay.saturating_mul(2).min(max_reconnect_delay);
    }
}

fn listener_unsupported_reason(
    route: OriginHarness,
    session_context: Option<SessionContext>,
    claude_code_version: Option<&str>,
    background_tasks_enabled: bool,
    controller: bool,
) -> Value {
    let reason = match route {
        OriginHarness::ClaudeCode if session_context != Some(SessionContext::Interactive) => {
            Some("Claude Code listening requires an interactive terminal session")
        }
        OriginHarness::ClaudeCode if !background_tasks_enabled => {
            Some("Claude Code background tasks are not enabled for this session")
        }
        OriginHarness::ClaudeCode
            if !claude_code_version
                .is_some_and(|version| version_at_least(version, [2, 1, 234])) =>
        {
            Some("Claude Code 2.1.234 or later is required for between-turn notifications")
        }
        OriginHarness::ClaudeCode => None,
        OriginHarness::Codex => Some("Codex listening is not supported yet"),
        OriginHarness::Other if controller => None,
        OriginHarness::Other => Some("no controller declared for the other route"),
    };
    reason
        .map(|reason| Value::String(reason.into()))
        .unwrap_or(Value::Null)
}

fn support_record_matches(status: &Value, request: &Value) -> bool {
    let Some(support) = status.get("listening_support") else {
        return false;
    };
    support.get("route") == request.get("route")
        && support.get("cli_version") == request.get("cli_version")
        && support.get("unsupported_reason") == request.get("unsupported_reason")
}

fn version_at_least(version: &str, minimum: [u64; 3]) -> bool {
    version
        .split(|character: char| !(character.is_ascii_digit() || character == '.'))
        .filter_map(|token| {
            let components: Vec<u64> = token
                .split('.')
                .take(3)
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            (components.len() == 3).then_some([components[0], components[1], components[2]])
        })
        .any(|version| version >= minimum)
}

fn reconcile_session(
    context: &CommandContext,
    store: &mut JournalStore,
    session: Uuid,
) -> Result<(), CommandError> {
    let ack_ids: Vec<String> = store
        .pending_acknowledgements_for(session)
        .map_err(|error| journal_error(error, 1))?
        .into_iter()
        .map(|entry| entry.event_id.clone())
        .collect();
    for event_id in ack_ids {
        acknowledge_event(context, store, &event_id, false)?;
    }

    let mut operations = store
        .unknown_operations_for(session)
        .map_err(|error| journal_error(error, 1))?;
    operations.sort_by_key(|operation| (operation.created_at_ms, operation.op_id));
    for operation in operations {
        match submit_operation(context, store, operation) {
            Ok(_) => {}
            Err(error) if error.exit_code == EXIT_REFUSED => {
                // A definite refusal resolves the old submission. The
                // authoritative feed below will determine remaining work.
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn acknowledge_event(
    context: &CommandContext,
    store: &mut JournalStore,
    event_id: &str,
    announce: bool,
) -> Result<Value, CommandError> {
    let entry = store
        .entry_by_event(event_id)
        .map_err(|error| journal_error(error, 1))?
        .ok_or_else(|| CommandError::new("event is not in the durable journal", EXIT_REFUSED))?;
    let mut request = json!({"v":1,"type":"ack","event_id":event_id});
    if let Some(files) = entry.raw["files"]
        .as_array()
        .filter(|files| !files.is_empty())
    {
        if entry.file_transfers.len() != files.len() {
            return Err(CommandError::new(
                "not every received file has a recorded transfer outcome",
                EXIT_REFUSED,
            ));
        }
        let mut reports = Vec::with_capacity(files.len());
        for file in files {
            let name = file["name"]
                .as_str()
                .ok_or_else(|| CommandError::new("received file has no name", EXIT_REFUSED))?;
            let transfer = entry.file_transfers.get(name).ok_or_else(|| {
                CommandError::new(
                    format!("received file {name} has no recorded transfer outcome"),
                    EXIT_REFUSED,
                )
            })?;
            if transfer.transferred {
                if transfer.path.is_none() {
                    return Err(CommandError::new(
                        format!("received file {name} has no saved path"),
                        EXIT_REFUSED,
                    ));
                }
                reports.push(json!({"name":name,"status":"transferred"}));
            } else {
                let reason = transfer
                    .reason
                    .as_deref()
                    .filter(|reason| !reason.is_empty())
                    .unwrap_or("local file write failed");
                reports.push(json!({
                    "name":name,
                    "status":"not_transferred",
                    "reason":reason
                }));
            }
        }
        request["file_transfers"] = Value::Array(reports);
    }
    store
        .mark_ack_pending(event_id)
        .map_err(|error| journal_error(error, 1))?;
    let response = match context.api.post(&request) {
        Ok(response) => response,
        Err(error) if error.outcome_unknown() => {
            if announce {
                emit(
                    &json!({"kind":"outcome_unknown","operation":"ack","event_id":event_id,"retry":"run the same ack --event command"}),
                )?;
            }
            return Err(CommandError::new(error.to_string(), EXIT_OUTCOME_UNKNOWN));
        }
        Err(error) => return Err(api_error(error, 1)),
    };
    if response["type"] == "error" {
        let reason = response["reason"].as_str().unwrap_or("refused");
        if announce {
            emit(
                &json!({"kind":"refused","operation":"ack","event_id":event_id,"reason":reason,"response":response}),
            )?;
        }
        return Err(CommandError::new(
            format!("ack refused: {reason}"),
            EXIT_REFUSED,
        ));
    }
    let status = response["status"].as_str().unwrap_or_default();
    if !matches!(status, "delivered" | "already_delivered") {
        return Err(CommandError::new(
            "ack response did not contain a delivery status",
            1,
        ));
    }
    let transfers = response
        .get("file_transfers")
        .filter(|transfers| transfers.is_object())
        .cloned()
        .or_else(|| ack_file_transfer_map(&request));
    if let Some(transfers) = transfers {
        store
            .record_server_file_transfers(event_id, transfers)
            .map_err(|error| journal_error(error, 1))?;
    }
    store
        .mark_delivered(event_id)
        .map_err(|error| journal_error(error, 1))?;
    if response["acted"] == true {
        store
            .mark_acted(event_id)
            .map_err(|error| journal_error(error, 1))?;
    }
    Ok(response)
}

fn server_file_transfers_complete(entry: &Value) -> bool {
    let Some(files) = entry.get("files").and_then(Value::as_array) else {
        return true;
    };
    files.iter().all(|file| {
        file.get("name")
            .and_then(Value::as_str)
            .and_then(|name| entry["file_transfers"][name]["status"].as_str())
            .is_some_and(|status| matches!(status, "transferred" | "not_transferred"))
    })
}

/// The server returns the accepted per-file ACK facts keyed by file name. A
/// repeated ACK may return only `already_delivered`; in that case, the exact
/// report we just retried is authoritative because a conflicting report is
/// refused by the server.
fn ack_file_transfer_map(request: &Value) -> Option<Value> {
    let reports = request.get("file_transfers")?.as_array()?;
    let mut transfers = serde_json::Map::new();
    for report in reports {
        let name = report.get("name")?.as_str()?;
        let status = report.get("status")?.as_str()?;
        let mut outcome = serde_json::Map::new();
        outcome.insert("status".into(), Value::String(status.into()));
        if let Some(reason) = report.get("reason").and_then(Value::as_str) {
            outcome.insert("reason".into(), Value::String(reason.into()));
        }
        transfers.insert(name.into(), Value::Object(outcome));
    }
    Some(Value::Object(transfers))
}

trait OperationTransport {
    fn post(&self, request: &Value) -> Result<Value, ApiError>;

    fn upload_file(
        &self,
        op_id: Uuid,
        sender_id: &str,
        message_id: &str,
        request_revision: u64,
        name: &str,
        path: &Path,
        size: u64,
    ) -> Result<Value, ApiError>;
}

impl OperationTransport for OriginClient {
    fn post(&self, request: &Value) -> Result<Value, ApiError> {
        OriginClient::post(self, request)
    }

    fn upload_file(
        &self,
        op_id: Uuid,
        sender_id: &str,
        message_id: &str,
        request_revision: u64,
        name: &str,
        path: &Path,
        size: u64,
    ) -> Result<Value, ApiError> {
        OriginClient::upload_file(
            self,
            op_id,
            sender_id,
            message_id,
            request_revision,
            name,
            path,
            size,
        )
    }
}

fn submit_operation(
    context: &CommandContext,
    store: &mut JournalStore,
    operation: OperationRecord,
) -> Result<Value, CommandError> {
    submit_operation_with_transport(&context.api, store, operation)
}

fn submit_operation_with_transport(
    transport: &impl OperationTransport,
    store: &mut JournalStore,
    operation: OperationRecord,
) -> Result<Value, CommandError> {
    if operation.prepared_send.is_some() && !prepared_send_request_matches(&operation) {
        return Err(journal_error(
            JournalError::OperationConflict(operation.op_id),
            EXIT_REFUSED,
        ));
    }
    let op_id = operation.op_id;
    let staged_paths: Vec<PathBuf> = operation
        .uploads
        .iter()
        .filter_map(|upload| upload.staged_path.clone())
        .collect();
    let result = match upload_pending_files(transport, store, operation) {
        Ok(operation) => submit_operation_inner(transport, store, operation, true),
        Err(error) => Err(error),
    };
    let settled = store
        .operation(op_id)
        .ok()
        .flatten()
        .is_some_and(|operation| {
            matches!(
                operation.state,
                OperationState::Committed | OperationState::Rejected
            )
        });
    if settled {
        for path in staged_paths {
            let _ = std::fs::remove_file(path);
        }
    }
    result
}

fn prepared_send_request_matches(operation: &OperationRecord) -> bool {
    let Some(prepared_send) = operation.prepared_send.as_ref() else {
        return false;
    };
    let mut expected = prepared_send.request.clone();
    for upload in &operation.uploads {
        let Some(upload_id) = upload.upload_id.as_deref() else {
            continue;
        };
        let Some(files) = expected.get_mut("files").and_then(Value::as_array_mut) else {
            return false;
        };
        let Some(slot) = files.iter_mut().find(|file| {
            file["name"].as_str() == Some(upload.name.as_str())
                && file["upload_pending"] == true
                && file["size"].as_u64() == Some(upload.size_bytes)
                && file["sha256"].as_str() == Some(upload.sha256.as_str())
        }) else {
            return false;
        };
        *slot = json!({
            "name":upload.name,
            "upload_id":upload_id,
            "size":upload.size_bytes,
            "sha256":upload.sha256
        });
    }
    expected == operation.request
}

fn upload_pending_files(
    transport: &impl OperationTransport,
    store: &mut JournalStore,
    operation: OperationRecord,
) -> Result<OperationRecord, CommandError> {
    if operation.uploads.is_empty()
        || matches!(
            operation.state,
            OperationState::Committed | OperationState::Rejected
        )
    {
        return Ok(operation);
    }
    let (sender_id, message_id, request_revision) = upload_binding(&operation.request)?;
    let mut current = operation;
    for upload in current.uploads.clone() {
        if upload.upload_id.is_some() {
            continue;
        }
        let path = upload.staged_path.as_deref().ok_or_else(|| {
            CommandError::new(
                format!(
                    "staged bytes for file {} are unavailable; the entry was not submitted",
                    upload.name
                ),
                EXIT_OUTCOME_UNKNOWN,
            )
        })?;
        store
            .mark_operation_unknown(current.op_id)
            .map_err(|error| journal_error(error, 1))?;
        let response = match transport.upload_file(
            current.op_id,
            &sender_id,
            &message_id,
            request_revision,
            &upload.name,
            path,
            upload.size_bytes,
        ) {
            Ok(response) if response["type"] == "ok" => response,
            Ok(response) => {
                let reason = response["reason"]
                    .as_str()
                    .unwrap_or("origin refused the upload")
                    .to_owned();
                store
                    .reject_operation(current.op_id, response.clone())
                    .map_err(|error| journal_error(error, 1))?;
                emit(&json!({
                    "kind":"upload_refused",
                    "file":upload.name,
                    "op_id":current.op_id,
                    "sent":false,
                    "reason":reason,
                    "cap_bytes_per_file":upload.upload_cap_bytes_per_file,
                    "response":response
                }))?;
                return Err(CommandError::new(
                    format!(
                        "file {} upload refused: {reason}; no entry was submitted",
                        upload.name
                    ),
                    EXIT_REFUSED,
                ));
            }
            Err(error) => {
                if let Some(reason) = known_upload_refusal(&error) {
                    store
                        .reject_operation(
                            current.op_id,
                            json!({"kind":"upload_refused","file":upload.name,"reason":reason}),
                        )
                        .map_err(|error| journal_error(error, 1))?;
                    emit(&json!({
                        "kind":"upload_refused",
                        "file":upload.name,
                        "op_id":current.op_id,
                        "sent":false,
                        "reason":reason,
                        "cap_bytes_per_file":upload.upload_cap_bytes_per_file
                    }))?;
                    return Err(CommandError::new(
                        format!(
                            "file {} upload failed: {reason}; no entry was submitted",
                            upload.name
                        ),
                        EXIT_REFUSED,
                    ));
                }
                if error.outcome_unknown() {
                    emit(&json!({
                        "kind":"outcome_unknown",
                        "operation":"upload",
                        "file":upload.name,
                        "op_id":current.op_id,
                        "session_handle":current.session_handle,
                        "retry":"run harklight-agent retry --op-id with this exact key"
                    }))?;
                    return Err(CommandError::new(
                        format!("upload outcome for {} is unknown: {error}", upload.name),
                        EXIT_OUTCOME_UNKNOWN,
                    ));
                }
                let reason = error.to_string();
                store
                    .reject_operation(
                        current.op_id,
                        json!({"kind":"upload_refused","file":upload.name,"reason":reason}),
                    )
                    .map_err(|error| journal_error(error, 1))?;
                emit(&json!({
                    "kind":"upload_refused",
                    "file":upload.name,
                    "op_id":current.op_id,
                    "sent":false,
                    "reason":reason,
                    "cap_bytes_per_file":upload.upload_cap_bytes_per_file
                }))?;
                return Err(CommandError::new(
                    format!(
                        "file {} upload failed: {reason}; no entry was submitted",
                        upload.name
                    ),
                    EXIT_REFUSED,
                ));
            }
        };
        let upload_id = response["upload_id"]
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .filter(|value| value.get_version_num() == 4)
            .ok_or_else(|| {
                CommandError::new(
                    "upload response identity is invalid; retry the exact operation key",
                    EXIT_OUTCOME_UNKNOWN,
                )
            })?;
        if response["name"].as_str() != Some(upload.name.as_str())
            || response["size"].as_u64() != Some(upload.size_bytes)
            || response["sha256"].as_str() != Some(upload.sha256.as_str())
        {
            return Err(CommandError::new(
                format!(
                    "upload response for {} does not match the staged bytes; retry the exact operation key",
                    upload.name
                ),
                EXIT_OUTCOME_UNKNOWN,
            ));
        }
        store
            .record_operation_upload(current.op_id, &upload.name, upload_id.to_string())
            .map_err(|error| journal_error(error, 1))?;
        current = store
            .operation(current.op_id)
            .map_err(|error| journal_error(error, 1))?
            .ok_or_else(|| CommandError::new("operation disappeared after upload", 1))?;
    }
    if let Err(error) = validate_request_size(&current.request) {
        store
            .reject_operation(
                current.op_id,
                json!({"kind":"request_too_large","reason":error.message}),
            )
            .map_err(|error| journal_error(error, 1))?;
        return Err(error);
    }
    Ok(current)
}

fn known_upload_refusal(error: &ApiError) -> Option<String> {
    match error {
        ApiError::Local(_) | ApiError::Config(_) => Some(error.to_string()),
        ApiError::Http { status, body } if (400..500).contains(status) => Some(
            serde_json::from_str::<Value>(body)
                .ok()
                .and_then(|response| response["reason"].as_str().map(str::to_owned))
                .unwrap_or_else(|| format!("HTTP {status}: {body}")),
        ),
        ApiError::Http { status: 503, body }
            if serde_json::from_str::<Value>(body)
                .ok()
                .and_then(|response| response["reason"].as_str().map(str::to_owned))
                .as_deref()
                == Some("storage_unavailable") =>
        {
            Some("storage_unavailable".into())
        }
        _ => None,
    }
}

fn upload_binding(request: &Value) -> Result<(String, String, u64), CommandError> {
    let sender_id = request["message"]["sender_id"]
        .as_str()
        .or_else(|| request["sender_id"].as_str())
        .ok_or_else(|| CommandError::new("file upload operation has no sender_id", 1))?
        .to_owned();
    let message_id = request["message"]["message_id"]
        .as_str()
        .or_else(|| request["message_id"].as_str())
        .ok_or_else(|| CommandError::new("file upload operation has no message_id", 1))?
        .to_owned();
    let request_revision = match request["type"].as_str() {
        Some("send") => 1,
        Some("reply") => request["request_revision"]
            .as_u64()
            .ok_or_else(|| CommandError::new("reply has no request revision", 1))?,
        Some("revise") => request["base_revision"]
            .as_u64()
            .and_then(|revision| revision.checked_add(1))
            .ok_or_else(|| CommandError::new("revision has no valid next revision", 1))?,
        _ => {
            return Err(CommandError::new(
                "operation cannot carry uploaded files",
                1,
            ));
        }
    };
    Ok((sender_id, message_id, request_revision))
}

fn submit_operation_inner(
    transport: &impl OperationTransport,
    store: &mut JournalStore,
    operation: OperationRecord,
    announce: bool,
) -> Result<Value, CommandError> {
    if operation.prepared_send.is_some() && !prepared_send_request_matches(&operation) {
        return Err(journal_error(
            JournalError::OperationConflict(operation.op_id),
            EXIT_REFUSED,
        ));
    }
    match operation.state {
        OperationState::Committed => {
            return operation
                .response
                .ok_or_else(|| CommandError::new("committed operation has no saved response", 1));
        }
        OperationState::Rejected => {
            return Err(CommandError::new(
                "operation was previously refused",
                EXIT_REFUSED,
            ));
        }
        OperationState::Prepared | OperationState::Unknown => {}
    }
    store
        .mark_operation_unknown(operation.op_id)
        .map_err(|error| journal_error(error, 1))?;

    let response = match transport.post(&operation.request) {
        Ok(response) if response["type"] == "ok" => response,
        Ok(response) if response["type"] == "error" => {
            return handle_operation_error_response(store, &operation, response, announce);
        }
        Ok(response) => {
            store
                .reject_operation(operation.op_id, response.clone())
                .map_err(|error| journal_error(error, 1))?;
            let message = "origin returned an unexpected application envelope";
            if announce {
                emit(
                    &json!({"kind":"refused","operation":operation.kind,"op_id":operation.op_id,"response":response}),
                )?;
            }
            return Err(CommandError::new(message, EXIT_REFUSED));
        }
        Err(error) if error.outcome_unknown() => {
            if announce {
                let mut line = json!({
                    "kind":"outcome_unknown",
                    "operation":operation.kind,
                    "op_id":operation.op_id,
                    "retry":"run harklight-agent retry --op-id with this exact key"
                });
                if operation.session_handle != Uuid::nil() {
                    line["session_handle"] = Value::String(operation.session_handle.to_string());
                }
                if let Some(request_id) = operation.request["message"]["message_id"]
                    .as_str()
                    .or_else(|| operation.request["message_id"].as_str())
                {
                    line["request_id"] = Value::String(request_id.to_owned());
                }
                if let Some(sender_id) = operation.request["message"]["sender_id"]
                    .as_str()
                    .or_else(|| operation.request["sender_id"].as_str())
                {
                    line["sender_id"] = Value::String(sender_id.to_owned());
                }
                emit(&line)?;
            }
            return Err(CommandError::new(error.to_string(), EXIT_OUTCOME_UNKNOWN));
        }
        Err(error) => {
            let saved = json!({"kind":"transport_refusal","message":error.to_string()});
            store
                .reject_operation(operation.op_id, saved)
                .map_err(|error| journal_error(error, 1))?;
            return Err(api_error(error, 1));
        }
    };

    if operation.kind == "attachment_update"
        && !attachment_update_response_matches(&operation.request, &response)
    {
        if announce {
            emit(&json!({
                "kind":"outcome_unknown",
                "operation":"attachment_update",
                "op_id":operation.op_id,
                "session_handle":operation.session_handle,
                "response":response,
                "retry":"run harklight-agent retry --op-id with this exact key"
            }))?;
        }
        return Err(CommandError::new(
            "origin did not confirm the stored handle and vendor ID; retry the identical attachment operation",
            EXIT_OUTCOME_UNKNOWN,
        ));
    }

    if operation.kind == "discard_uploads" && !valid_discard_uploads_response(&response) {
        if announce {
            emit(&json!({
                "kind":"outcome_unknown",
                "operation":"discard_uploads",
                "op_id":operation.op_id,
                "sender_id":operation.request["sender_id"],
                "message_id":operation.request["message_id"],
                "revision":operation.request["revision"],
                "response":response,
                "retry":"run harklight-agent retry --op-id with this exact key"
            }))?;
        }
        return Err(CommandError::new(
            "origin accepted cleanup without a valid state and deleted-count result; retry the same operation key",
            EXIT_OUTCOME_UNKNOWN,
        ));
    }

    let acted_event = operation.request["in_reply_to"].as_str();
    store
        .settle_operation(operation.op_id, response.clone(), acted_event)
        .map_err(|error| journal_error(error, 1))?;
    Ok(response)
}

fn prepare(
    store: &mut JournalStore,
    session: Uuid,
    op_id: Uuid,
    kind: &str,
    request: Value,
) -> Result<OperationRecord, CommandError> {
    store
        .prepare_operation(op_id, session, kind.to_owned(), request)
        .map_err(|error| journal_error(error, 1))
}

fn prepare_with_uploads(
    store: &mut JournalStore,
    session: Uuid,
    op_id: Uuid,
    kind: &str,
    request: Value,
    uploads: Vec<crate::journal::UploadSource>,
) -> Result<OperationRecord, CommandError> {
    store
        .prepare_operation_with_uploads(op_id, session, kind.to_owned(), request, uploads)
        .map_err(|error| journal_error(error, 1))
}

fn fetch_feed(context: &CommandContext, store: &mut JournalStore) -> Result<(), CommandError> {
    let since = store.cursor().map_err(|error| journal_error(error, 1))?;
    let response = post_read(context, &json!({"v":1,"type":"feed","since":since}))?;
    let high_water = response["cursor"]
        .as_u64()
        .ok_or_else(|| CommandError::new("feed response has no integer cursor", 1))?;
    if high_water < since {
        return Err(CommandError::new(
            format!("feed cursor regressed from {since} to {high_water}"),
            1,
        ));
    }
    let entries = response["entries"]
        .as_array()
        .ok_or_else(|| CommandError::new("feed response has no entries array", 1))?;

    for raw in entries {
        let event_id = raw["event_id"]
            .as_str()
            .ok_or_else(|| CommandError::new("feed entry has no event_id", 1))?;
        let feed_seq = validate_event_id(event_id)?;
        let session_handle = raw["session_handle"]
            .as_str()
            .and_then(|value| Uuid::parse_str(value).ok())
            .filter(|value| value.get_version_num() == 4)
            .ok_or_else(|| CommandError::new("feed entry has an invalid session_handle", 1))?;
        let kind = raw["kind"]
            .as_str()
            .ok_or_else(|| CommandError::new("feed entry has no kind", 1))?
            .to_owned();
        let author = raw["author"]
            .as_str()
            .ok_or_else(|| CommandError::new("feed entry has no author", 1))?
            .to_owned();
        let actionable = matches!(kind.as_str(), "discussion" | "terminal") && author != "origin";
        let sender_id = raw["sender_id"].as_str().map(str::to_owned);
        let request_id = raw["message_id"].as_str().map(str::to_owned);
        let request_revision = raw["request_revision"].as_u64();
        let mut raw = raw.clone();
        let file_transfers = if actionable {
            let directory = store.event_file_directory(session_handle, event_id);
            files::materialize_incoming(&mut raw, &directory, &context.api)
                .map_err(|error| CommandError::new(error, 1))?
        } else {
            files::strip_file_payloads(&mut raw);
            BTreeMap::new()
        };
        let mut entry = JournalEntry::fetched(
            feed_seq,
            event_id.to_owned(),
            session_handle,
            kind,
            author,
            actionable,
            sender_id,
            request_id,
            request_revision,
            raw,
        );
        entry.file_transfers = file_transfers;
        store
            .record_entry(entry)
            .map_err(|error| journal_error(error, 1))?;
    }

    let cursor = store.cursor().map_err(|error| journal_error(error, 1))?;
    if cursor < high_water {
        return Err(CommandError::new(
            format!("feed returned cursor {high_water} but journal reached only {cursor}"),
            1,
        ));
    }
    Ok(())
}

fn emit_pending(
    store: &mut JournalStore,
    session: Uuid,
    follow: bool,
    offered: &mut BTreeSet<String>,
) -> Result<bool, CommandError> {
    let pending = store
        .pending_for(session)
        .map_err(|error| journal_error(error, 1))?;
    let mut candidates = Vec::new();
    for (index, entry) in pending.iter().enumerate() {
        if !offered.contains(&entry.event_id) {
            candidates.push((entry.event_id.clone(), index, entry.clone()));
            if !follow {
                break;
            }
        }
    }
    let printed = !candidates.is_empty();
    for (event_id, index, entry) in candidates {
        let line = event_line(&entry, pending.len().saturating_sub(index + 1));
        emit(&line)?;
        store
            .mark_emitted(&event_id)
            .map_err(|error| journal_error(error, 1))?;
        offered.insert(event_id);
    }
    Ok(!follow && printed)
}

#[derive(serde::Serialize)]
enum FinalTerminal {
    NoComment,
}

fn event_line(entry: &JournalEntry, pending_remaining: usize) -> Value {
    let raw = files::file_event_view(&entry.raw, &entry.file_transfers);
    let mut line = json!({
        "kind":"event",
        "session_handle":entry.session_handle,
        "event_id":entry.event_id,
        "feed_cursor":entry.feed_seq,
        "request":{
            "sender_id":entry.sender_id,
            "message_id":entry.request_id
        },
        "request_revision":entry.request_revision,
        "delivered":entry.delivered_at_ms.is_some() || entry.raw["delivered"] == true,
        "acted":entry.acted_at_ms.is_some() || entry.raw["acted"] == true,
        "pending_remaining":pending_remaining,
        "entry":raw
    });

    if let Some(channel) = raw["channel"].as_str() {
        line["channel"] = Value::String(channel.to_owned());
    }

    if entry.kind == "terminal" && raw["terminal_kind"] == "no_comment" {
        line["final_terminal"] = json!(FinalTerminal::NoComment);
    }

    line
}

fn read_status(context: &CommandContext, session: Uuid) -> Result<Value, CommandError> {
    post_read(
        context,
        &json!({"v":1,"type":"status","session_handle":session.to_string()}),
    )
}

fn post_read(context: &CommandContext, request: &Value) -> Result<Value, CommandError> {
    let response = context
        .api
        .post(request)
        .map_err(|error| api_error(error, 1))?;
    if response["type"] == "error" {
        let reason = response["reason"].as_str().unwrap_or("refused");
        return Err(CommandError::new(
            format!("origin refused the request: {reason}"),
            EXIT_REFUSED,
        ));
    }
    Ok(response)
}

fn session_idle(status: &Value) -> Result<bool, CommandError> {
    let open_requests = status["open_requests"]
        .as_u64()
        .ok_or_else(|| CommandError::new("status response has no open_requests count", 1))?;
    let open_chats = match status.get("open_chats") {
        Some(value) => value.as_u64().ok_or_else(|| {
            CommandError::new("status response has no integer open_chats count", 1)
        })?,
        None => open_requests,
    };
    let pending_undelivered = status["pending_undelivered"]
        .as_u64()
        .ok_or_else(|| CommandError::new("status response has no pending_undelivered count", 1))?;
    let pending_delivered_unacted =
        status["pending_delivered_unacted"]
            .as_u64()
            .ok_or_else(|| {
                CommandError::new("status response has no pending_delivered_unacted count", 1)
            })?;
    Ok(open_chats == 0 && pending_undelivered == 0 && pending_delivered_unacted == 0)
}

fn stream_should_reconnect(error: &ApiError) -> bool {
    matches!(error, ApiError::Transport(_) | ApiError::StreamIo(_))
        || matches!(error, ApiError::Http { status, .. } if *status >= 500)
}

struct RequestContext {
    sender_id: String,
    message_id: String,
    message: Value,
    revision: u64,
}

fn request_context(
    journal: &Journal,
    session: Uuid,
    message_id: &str,
) -> Result<RequestContext, CommandError> {
    let mut best: Option<(u64, Value)> = None;
    for entry in journal.entries.values().rev().filter(|entry| {
        entry.session_handle == session
            && entry.kind == "revision"
            && entry.request_id.as_deref() == Some(message_id)
    }) {
        if let (Some(revision), Some(message)) = (entry.request_revision, entry.raw.get("message"))
        {
            best = Some((revision, message.clone()));
            break;
        }
    }

    for operation in journal.operations.values() {
        if operation.session_handle != session
            || !matches!(
                operation.kind.as_str(),
                "send" | "revise" | "reply" | "withdraw" | "result"
            )
            || operation.state == OperationState::Rejected
            || operation.request["message"]["message_id"]
                .as_str()
                .or_else(|| operation.request["message_id"].as_str())
                != Some(message_id)
        {
            continue;
        }
        if operation.state != OperationState::Committed {
            return Err(CommandError::new(
                format!(
                    "request has unresolved operation {}; retry it before continuing",
                    operation.op_id
                ),
                EXIT_OUTCOME_UNKNOWN,
            ));
        }
        if !matches!(operation.kind.as_str(), "send" | "revise") {
            continue;
        }
        let revision = operation
            .response
            .as_ref()
            .and_then(|response| response["revision"].as_u64())
            .unwrap_or_else(|| {
                operation.request["base_revision"]
                    .as_u64()
                    .map(|revision| revision + 1)
                    .unwrap_or(1)
            });
        if best.as_ref().is_none_or(|(prior, _)| revision > *prior) {
            best = Some((revision, operation.request["message"].clone()));
        }
    }
    let (revision, message) = best.ok_or_else(|| {
        CommandError::new(
            format!("request {message_id} is not in session {session}'s journal"),
            1,
        )
    })?;
    let sender_id = message["sender_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| CommandError::new("recorded request has no sender_id", 1))?;
    Ok(RequestContext {
        sender_id,
        message_id: message_id.to_owned(),
        message,
        revision,
    })
}

fn ensure_request_open(
    journal: &Journal,
    session: Uuid,
    request: &RequestContext,
) -> Result<(), CommandError> {
    let closed_by = journal.entries.values().find(|entry| {
        entry.session_handle == session
            && entry.kind == "terminal"
            && entry.sender_id.as_deref() == Some(request.sender_id.as_str())
            && entry.request_id.as_deref() == Some(request.message_id.as_str())
    });
    if let Some(terminal) = closed_by {
        return Err(CommandError::new(
            format!(
                "request is closed by terminal event {}; no further reply, revision or withdrawal is allowed",
                terminal.event_id
            ),
            EXIT_REFUSED,
        ));
    }
    Ok(())
}

fn ensure_current_revision(
    request_revision: u64,
    event_revision: Option<u64>,
) -> Result<(), CommandError> {
    if event_revision != Some(request_revision) {
        return Err(CommandError::new(
            "event belongs to an older request revision; refresh the current request before acting",
            EXIT_REFUSED,
        ));
    }
    Ok(())
}

fn delivered_event<'a>(
    journal: &'a Journal,
    session: Uuid,
    request: &RequestContext,
    event_id: &str,
    expected_kind: &str,
    allow_acted: bool,
) -> Result<&'a JournalEntry, CommandError> {
    let entry = journal.entry_by_event(event_id).ok_or_else(|| {
        CommandError::new(format!("event {event_id} is not in the durable journal"), 1)
    })?;
    if entry.session_handle != session
        || entry.sender_id.as_deref() != Some(request.sender_id.as_str())
        || entry.request_id.as_deref() != Some(request.message_id.as_str())
        || entry.kind != expected_kind
        || entry.author == "origin"
        || !entry.actionable
    {
        return Err(CommandError::new(
            "event does not belong to this origin session and request",
            EXIT_REFUSED,
        ));
    }
    if entry.delivered_at_ms.is_none() {
        return Err(CommandError::new(
            "ack this event first; delivery is recorded only after server confirmation",
            EXIT_REFUSED,
        ));
    }
    if entry.acted_at_ms.is_some() && !allow_acted {
        return Err(CommandError::new(
            "event is already acted; do not submit another operation for it",
            EXIT_REFUSED,
        ));
    }
    Ok(entry)
}

fn emit_committed(kind: &str, op_id: Uuid, response: Value) -> Result<(), CommandError> {
    emit(&json!({"kind":"committed","operation":kind,"op_id":op_id,"response":response}))
}

fn retry_committed_result(
    output_operation: &str,
    op_id: Uuid,
    operation_kind: &str,
    request: &Value,
    response: Value,
) -> Result<Value, CommandError> {
    let mut output = json!({
        "kind":"committed",
        "operation":output_operation,
        "op_id":op_id,
        "response":response
    });
    if operation_kind == "send" {
        let sender_id = request["message"]["sender_id"]
            .as_str()
            .ok_or_else(|| CommandError::new("saved send operation has no sender ID", 1))?;
        let message_id = request["message"]["message_id"]
            .as_str()
            .ok_or_else(|| CommandError::new("saved send operation has no message ID", 1))?;
        output["message_url"] = Value::String(message_url(sender_id, message_id));
    }
    Ok(output)
}

fn refused(operation: &str, op_id: Option<Uuid>, response: Value) -> Result<(), CommandError> {
    let reason = response["reason"].as_str().unwrap_or("refused").to_owned();
    let mut output = Map::new();
    output.insert("kind".into(), Value::String("refused".into()));
    output.insert("operation".into(), Value::String(operation.into()));
    if let Some(op_id) = op_id {
        output.insert("op_id".into(), Value::String(op_id.to_string()));
    }
    output.insert("reason".into(), Value::String(reason.clone()));
    output.insert("response".into(), response);
    emit(&Value::Object(output))?;
    Err(CommandError::new(
        format!("{operation} refused: {reason}"),
        EXIT_REFUSED,
    ))
}

fn emit(value: &Value) -> Result<(), CommandError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| CommandError::new(format!("could not encode output: {error}"), 1))?;
    let stdout = io::stdout();
    let mut output = stdout.lock();
    output
        .write_all(&bytes)
        .and_then(|()| output.write_all(b"\n"))
        .and_then(|()| output.flush())
        .map_err(|error| CommandError::new(format!("stdout write failed: {error}"), 1))
}

fn api_error(error: ApiError, exit_code: i32) -> CommandError {
    CommandError::new(error.to_string(), exit_code)
}

fn journal_error(error: JournalError, fallback_code: i32) -> CommandError {
    let code = if matches!(
        error,
        JournalError::ListenerActive(_) | JournalError::ListenerActiveUnknown
    ) {
        EXIT_LISTENER_ACTIVE
    } else {
        fallback_code
    };
    CommandError::new(error.to_string(), code)
}

fn parse_session(value: &str) -> Result<Uuid, CommandError> {
    let session = Uuid::parse_str(value)
        .map_err(|_| CommandError::new("session must be a UUIDv4 or the word new", 1))?;
    if session.get_version_num() != 4 {
        return Err(CommandError::new("session handle must be UUIDv4", 1));
    }
    Ok(session)
}

fn validate_event_id(event_id: &str) -> Result<u64, CommandError> {
    let sequence = event_id.parse::<u64>().map_err(|_| {
        CommandError::new("event ID must be a canonical positive decimal string", 1)
    })?;
    if sequence == 0 || sequence.to_string() != event_id {
        return Err(CommandError::new(
            "event ID must be a canonical positive decimal string",
            1,
        ));
    }
    Ok(sequence)
}

fn parse_uuid_field(request: &Value, field: &str) -> Result<Uuid, CommandError> {
    let value = request[field]
        .as_str()
        .ok_or_else(|| CommandError::new(format!("operation request has no {field}"), 1))?;
    let id = Uuid::parse_str(value)
        .map_err(|_| CommandError::new(format!("operation {field} is not UUIDv4"), 1))?;
    if id.get_version_num() != 4 {
        return Err(CommandError::new(
            format!("operation {field} is not UUIDv4"),
            1,
        ));
    }
    Ok(id)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;
    use std::{cell::Cell, collections::VecDeque, sync::Mutex};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("harklight-agent-command-{}", Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn event_lines_preserve_reply_and_chat_channel_labels() {
        let session = Uuid::new_v4();
        let reply = JournalEntry::fetched(
            1,
            "1".into(),
            session,
            "terminal".into(),
            "person".into(),
            true,
            Some("app:example".into()),
            Some("message-1".into()),
            Some(1),
            json!({
                "event_id":"1",
                "session_handle":session.to_string(),
                "kind":"terminal",
                "author":"person",
                "sender_id":"app:example",
                "message_id":"message-1",
                "request_revision":1,
                "terminal_state":"answered",
                "channel":"reply",
                "answer":"Done"
            }),
        );
        let chat = JournalEntry::fetched(
            2,
            "2".into(),
            session,
            "discussion".into(),
            "person".into(),
            true,
            Some("app:example".into()),
            Some("message-1".into()),
            Some(1),
            json!({
                "event_id":"2",
                "session_handle":session.to_string(),
                "kind":"discussion",
                "author":"person",
                "sender_id":"app:example",
                "message_id":"message-1",
                "request_revision":1,
                "channel":"chat",
                "text":"Can you clarify?"
            }),
        );
        let mut revision = reply.clone();
        revision.feed_seq = 3;
        revision.event_id = "3".into();
        revision.kind = "revision".into();
        revision.raw["event_id"] = json!("3");
        revision.raw["kind"] = json!("revision");
        revision.raw["request_revision"] = json!(2);
        revision
            .raw
            .as_object_mut()
            .unwrap()
            .remove("terminal_state");
        revision.raw.as_object_mut().unwrap().remove("answer");

        let mut result = reply.clone();
        result.feed_seq = 4;
        result.event_id = "4".into();
        result.kind = "result".into();
        result.author = "origin".into();
        result.raw["event_id"] = json!("4");
        result.raw["kind"] = json!("result");
        result.raw["author"] = json!("origin");
        result.raw["request_revision"] = json!(2);
        result.raw["status"] = json!("applied");
        result.raw.as_object_mut().unwrap().remove("terminal_state");
        result.raw.as_object_mut().unwrap().remove("answer");

        let mut origin_chat_reply = chat.clone();
        origin_chat_reply.feed_seq = 5;
        origin_chat_reply.event_id = "5".into();
        origin_chat_reply.kind = "reply".into();
        origin_chat_reply.author = "origin".into();
        origin_chat_reply.raw["event_id"] = json!("5");
        origin_chat_reply.raw["kind"] = json!("reply");
        origin_chat_reply.raw["author"] = json!("origin");

        let reply_line = event_line(&reply, 1);
        let chat_line = event_line(&chat, 0);
        let revision_line = event_line(&revision, 0);
        let result_line = event_line(&result, 0);
        let origin_chat_reply_line = event_line(&origin_chat_reply, 0);
        assert_eq!(reply_line["channel"], "reply");
        assert_eq!(reply_line["entry"]["channel"], "reply");
        assert_eq!(chat_line["channel"], "chat");
        assert_eq!(chat_line["entry"]["channel"], "chat");
        assert_eq!(revision_line["channel"], "reply");
        assert_eq!(revision_line["entry"]["channel"], "reply");
        assert_eq!(result_line["channel"], "reply");
        assert_eq!(result_line["entry"]["channel"], "reply");
        assert_eq!(origin_chat_reply_line["channel"], "chat");
        assert_eq!(origin_chat_reply_line["entry"]["channel"], "chat");

        let mut no_comment = reply.clone();
        no_comment.feed_seq = 6;
        no_comment.event_id = "6".into();
        no_comment.raw["event_id"] = json!("6");
        no_comment.raw["terminal_kind"] = json!("no_comment");
        no_comment.raw.as_object_mut().unwrap().remove("answer");
        let no_comment_line = event_line(&no_comment, 1);
        assert_eq!(no_comment_line["channel"], "reply");
        assert_eq!(no_comment_line["final_terminal"], "NoComment");

        for (index, terminal_state) in ["retracted", "expired", "superseded"]
            .into_iter()
            .enumerate()
        {
            let mut terminal = reply.clone();
            let event_id = (7 + index).to_string();
            terminal.feed_seq = 7 + index as u64;
            terminal.event_id = event_id.clone();
            terminal.raw["event_id"] = json!(event_id);
            terminal.raw["terminal_state"] = json!(terminal_state);
            terminal.raw.as_object_mut().unwrap().remove("answer");
            assert_eq!(event_line(&terminal, 0)["channel"], "reply");
        }
    }

    #[test]
    fn open_chats_keeps_idle_listener_active_after_answer_or_no_comment() {
        let status = json!({
            "open_requests":0,
            "open_chats":1,
            "pending_undelivered":0,
            "pending_delivered_unacted":0
        });
        assert!(!session_idle(&status).unwrap());

        let closed = json!({
            "open_requests":0,
            "open_chats":0,
            "pending_undelivered":0,
            "pending_delivered_unacted":0
        });
        assert!(session_idle(&closed).unwrap());
    }

    struct MockTransport {
        posts: Mutex<Vec<Value>>,
        uploads: Mutex<Vec<(Uuid, String, u64)>>,
        post_results: Mutex<VecDeque<Result<Value, ApiError>>>,
        upload_id: Uuid,
    }

    impl MockTransport {
        fn new(post_results: impl IntoIterator<Item = Result<Value, ApiError>>) -> Self {
            Self {
                posts: Mutex::new(Vec::new()),
                uploads: Mutex::new(Vec::new()),
                post_results: Mutex::new(post_results.into_iter().collect()),
                upload_id: Uuid::new_v4(),
            }
        }
    }

    impl OperationTransport for MockTransport {
        fn post(&self, request: &Value) -> Result<Value, ApiError> {
            self.posts.lock().unwrap().push(request.clone());
            self.post_results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(json!({"type":"ok","revision":1})))
        }

        fn upload_file(
            &self,
            op_id: Uuid,
            _sender_id: &str,
            _message_id: &str,
            _request_revision: u64,
            name: &str,
            path: &Path,
            size: u64,
        ) -> Result<Value, ApiError> {
            self.uploads
                .lock()
                .unwrap()
                .push((op_id, name.to_owned(), size));
            let bytes = std::fs::read(path).map_err(|error| ApiError::Local(error.to_string()))?;
            Ok(json!({
                "type":"ok",
                "upload_id":self.upload_id,
                "name":name,
                "size":size,
                "sha256":hex::encode(sha2::Sha256::digest(bytes))
            }))
        }
    }

    fn prepared_send_record(
        store: &mut JournalStore,
        op_id: Uuid,
        session: Uuid,
        request: Value,
        uploads: Vec<crate::journal::UploadSource>,
        manifest: Vec<files::FileManifestEntry>,
        inputs: Value,
    ) -> OperationRecord {
        store
            .prepare_send_operation(
                op_id,
                session,
                request.clone(),
                uploads,
                PreparedSend {
                    request,
                    files: manifest,
                    inputs,
                },
            )
            .unwrap()
    }

    #[test]
    fn prepared_send_persists_exact_request_and_stable_file_manifest_without_uploading() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let path = scratch.0.join("attachment.txt");
        std::fs::write(&path, b"stable staged bytes").unwrap();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let file_limits_calls = Cell::new(0);
        let prepared_files = prepare_outgoing_files_with_lookup(
            &store,
            session,
            op_id,
            std::slice::from_ref(&path),
            "send",
            json!({}),
            || {
                file_limits_calls.set(file_limits_calls.get() + 1);
                Ok(json!({
                    "type":"ok",
                    "inline_decoded_bytes_per_entry":1,
                    "upload_cap_bytes_per_file":"none"
                }))
            },
        )
        .unwrap();
        let request = build_send_request(
            op_id,
            session,
            "message-1",
            "app:example",
            "A title",
            Some("A body"),
            &[],
            None,
            None,
            false,
            &prepared_files.entries,
        )
        .unwrap();
        let manifest = prepared_files.manifest.clone();
        let inputs = send_input_identity(
            "new",
            "app:example",
            "A title",
            Some("A body"),
            &[],
            None,
            None,
            false,
            &manifest,
        );
        let operation = prepared_send_record(
            &mut store,
            op_id,
            session,
            request.clone(),
            prepared_files.uploads.clone(),
            manifest.clone(),
            inputs.clone(),
        );

        assert_eq!(file_limits_calls.get(), 1);
        assert_eq!(operation.state, OperationState::Prepared);
        assert_eq!(operation.prepared_send.as_ref().unwrap().request, request);
        assert_eq!(operation.prepared_send.as_ref().unwrap().files, manifest);
        assert_eq!(operation.request["files"][0]["upload_pending"], true);
        assert_eq!(operation.uploads.len(), 1);
        assert_eq!(
            std::fs::read(operation.uploads[0].staged_path.as_ref().unwrap()).unwrap(),
            b"stable staged bytes"
        );

        let resumed_request = build_send_request(
            op_id,
            Uuid::new_v4(),
            "a-new-generated-message-id",
            "app:example",
            "A title",
            Some("A body"),
            &[],
            None,
            None,
            false,
            &prepared_files.entries,
        )
        .unwrap();
        let resumed = prepared_send_record(
            &mut store,
            op_id,
            Uuid::new_v4(),
            resumed_request,
            prepared_files.uploads,
            manifest.clone(),
            inputs,
        );
        assert_eq!(resumed.session_handle, session);
        assert_eq!(resumed.prepared_send.as_ref().unwrap().request, request);
        assert_eq!(resumed.prepared_send.as_ref().unwrap().files, manifest);
        assert!(MockTransport::new([]).posts.lock().unwrap().is_empty());
    }

    #[test]
    fn prepared_send_conflicts_when_reused_with_changed_inputs() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let request = build_send_request(
            op_id,
            session,
            "message-1",
            "app:example",
            "Original",
            None,
            &[],
            None,
            None,
            false,
            &[],
        )
        .unwrap();
        let inputs = send_input_identity(
            &session.to_string(),
            "app:example",
            "Original",
            None,
            &[],
            None,
            None,
            false,
            &[],
        );
        prepared_send_record(
            &mut store,
            op_id,
            session,
            request,
            Vec::new(),
            Vec::new(),
            inputs,
        );

        let changed_request = build_send_request(
            op_id,
            session,
            "message-2",
            "app:example",
            "Changed",
            None,
            &[],
            None,
            None,
            false,
            &[],
        )
        .unwrap();
        let changed_inputs = send_input_identity(
            &session.to_string(),
            "app:example",
            "Changed",
            None,
            &[],
            None,
            None,
            false,
            &[],
        );
        assert!(matches!(
            store.prepare_send_operation(
                op_id,
                session,
                changed_request.clone(),
                Vec::new(),
                PreparedSend {
                    request: changed_request,
                    files: Vec::new(),
                    inputs: changed_inputs,
                }
            ),
            Err(JournalError::OperationConflict(id)) if id == op_id
        ));
    }

    #[test]
    fn send_response_session_must_match_the_prepared_handle() {
        let expected = Uuid::new_v4().to_string();
        assert!(
            validate_send_response_session(
                &json!({"type":"ok","session_handle":expected}),
                &expected
            )
            .is_ok()
        );
        assert!(validate_send_response_session(&json!({"type":"ok"}), &expected).is_ok());
        assert!(
            validate_send_response_session(
                &json!({"type":"ok","session_handle":Uuid::new_v4().to_string()}),
                &expected
            )
            .is_err()
        );
    }

    #[test]
    fn uploaded_send_retries_the_same_final_payload_and_original_operation_id() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let path = scratch.0.join("large.txt");
        std::fs::write(&path, b"larger than inline").unwrap();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let prepared_files = prepare_outgoing_files_with_lookup(
            &store,
            session,
            op_id,
            std::slice::from_ref(&path),
            "send",
            json!({}),
            || {
                Ok(json!({
                    "type":"ok",
                    "inline_decoded_bytes_per_entry":1,
                    "upload_cap_bytes_per_file":"none"
                }))
            },
        )
        .unwrap();
        let request = build_send_request(
            op_id,
            session,
            "message-1",
            "app:example",
            "A title",
            Some("A body"),
            &[],
            None,
            None,
            false,
            &prepared_files.entries,
        )
        .unwrap();
        let inputs = send_input_identity(
            &session.to_string(),
            "app:example",
            "A title",
            Some("A body"),
            &[],
            None,
            None,
            false,
            &prepared_files.manifest,
        );
        let operation = prepared_send_record(
            &mut store,
            op_id,
            session,
            request.clone(),
            prepared_files.uploads,
            prepared_files.manifest,
            inputs,
        );
        let first = MockTransport::new([Err(ApiError::InvalidResponse("lost response".into()))]);

        assert!(matches!(
            submit_operation_with_transport(&first, &mut store, operation),
            Err(error) if error.exit_code == EXIT_OUTCOME_UNKNOWN
        ));
        assert_eq!(first.uploads.lock().unwrap().len(), 1);
        let first_post = first.posts.lock().unwrap()[0].clone();
        assert_eq!(first_post["op_id"], op_id.to_string());
        assert_ne!(first_post, request);
        assert!(first_post["files"][0]["upload_id"].is_string());

        let saved = store.operation(op_id).unwrap().unwrap();
        assert_eq!(saved.state, OperationState::Unknown);
        assert_eq!(saved.prepared_send.as_ref().unwrap().request, request);
        let second = MockTransport::new([Ok(json!({"type":"ok","revision":1}))]);
        submit_operation_with_transport(&second, &mut store, saved).unwrap();

        assert!(second.uploads.lock().unwrap().is_empty());
        assert_eq!(second.posts.lock().unwrap().as_slice(), &[first_post]);
        assert_eq!(
            store.operation(op_id).unwrap().unwrap().state,
            OperationState::Committed
        );
    }

    #[test]
    fn choices_become_value_actions_in_the_order_given() {
        let choices = vec!["Approve".to_string(), "Hold".to_string()];
        assert_eq!(
            choices_answer_ui(&choices).unwrap(),
            json!({
                "schema":"attention-client.actions/1",
                "actions":[
                    {"id":"choice-1","label":"Approve","kind":"value","value":"Approve"},
                    {"id":"choice-2","label":"Hold","kind":"value","value":"Hold"}
                ]
            })
        );
        let descriptor = choices_answer_ui(&["Hold until Monday".to_string()]).unwrap();
        let action = &descriptor["actions"][0];
        assert_eq!(action["label"], action["value"]);
        assert_eq!(action.as_object().unwrap().len(), 4);
    }

    #[test]
    fn message_url_percent_encodes_sender_and_message_as_separate_segments() {
        let session = Uuid::new_v4();
        let result = send_result(
            "app:example/µ",
            "message id%/1",
            session,
            &json!({"revision":2,"listening_support":{"route":"claude-code"}}),
        );
        assert_eq!(
            result,
            json!({
                "request":"message id%/1",
                "sender_id":"app:example/µ",
                "message_url":"harklight://message/app%3Aexample%2F%C2%B5/message%20id%25%2F1",
                "session":session,
                "revision":2,
                "listening_support":{"route":"claude-code"}
            })
        );
    }

    #[test]
    fn committed_send_retry_result_includes_the_message_url() {
        let op_id = Uuid::new_v4();
        let request = json!({"message":{"sender_id":"app:example","message_id":"message-1"}});
        let response = json!({"type":"ok","revision":1});

        for output_operation in ["send", "retry"] {
            let result =
                retry_committed_result(output_operation, op_id, "send", &request, response.clone())
                    .unwrap();
            assert_eq!(
                result["message_url"],
                "harklight://message/app%3Aexample/message-1"
            );
            assert_eq!(result["operation"], output_operation);
            assert_eq!(result["response"], response);
        }
    }

    #[test]
    fn empty_and_duplicate_choices_are_refused_by_name() {
        let empty = choices_answer_ui(&["Approve".to_string(), "  ".to_string()]).unwrap_err();
        assert_eq!(empty.exit_code, EXIT_REFUSED);
        assert_eq!(empty.message, "--choice 2 is empty");
        let duplicate =
            choices_answer_ui(&["Approve".to_string(), "Approve".to_string()]).unwrap_err();
        assert_eq!(duplicate.exit_code, EXIT_REFUSED);
        assert_eq!(
            duplicate.message,
            "--choice \"Approve\" is given more than once"
        );
        assert_eq!(
            revised_message(&json!({}), "t", None, &["".to_string()], false)
                .unwrap_err()
                .message,
            "--choice 1 is empty"
        );
    }

    #[test]
    fn revise_replaces_removes_or_keeps_the_answer_descriptor() {
        let original = choices_answer_ui(&["Approve".to_string(), "Hold".to_string()]).unwrap();
        let current = json!({
            "sender_id":"app:example",
            "message_id":"message-1",
            "title":"Deploy the fix?",
            "body":"first wording",
            "answer_ui":original,
            "choices":["Approve","Hold"]
        });

        let replaced = revised_message(
            &current,
            "Deploy the fix?",
            None,
            &["Approve".to_string(), "Hold until Monday".to_string()],
            false,
        )
        .unwrap();
        assert_eq!(
            replaced["answer_ui"],
            choices_answer_ui(&["Approve".to_string(), "Hold until Monday".to_string()]).unwrap()
        );
        assert!(replaced.get("choices").is_none());
        assert_eq!(replaced["body"], "first wording");

        let cleared = revised_message(&current, "Deploy the fix?", Some("new"), &[], true).unwrap();
        assert!(cleared.get("answer_ui").is_none());
        assert!(cleared.get("choices").is_none());
        assert_eq!(cleared["body"], "new");

        let kept = revised_message(&current, "Retitled", None, &[], false).unwrap();
        assert_eq!(kept["answer_ui"], original);
        assert!(kept.get("choices").is_none());
        assert_eq!(kept["title"], "Retitled");

        let plain = revised_message(&json!({"title":"t"}), "t", None, &[], false).unwrap();
        assert!(plain.get("answer_ui").is_none());
    }

    #[test]
    fn route_is_required_only_when_minting_a_session_handle() {
        assert!(matches!(
            validate_send_route("new", None, false),
            Err(error) if error.exit_code == EXIT_REFUSED
        ));
        assert_eq!(
            validate_send_route("new", Some(OriginHarness::ClaudeCode), false).unwrap(),
            Some(OriginHarness::ClaudeCode)
        );
        assert_eq!(
            validate_send_route(&Uuid::new_v4().to_string(), None, false).unwrap(),
            None
        );
        assert!(matches!(
            validate_send_route("existing", Some(OriginHarness::ClaudeCode), false),
            Err(error) if error.exit_code == EXIT_REFUSED
        ));
        assert_eq!(
            validate_send_route("new", Some(OriginHarness::Other), true).unwrap(),
            Some(OriginHarness::Other)
        );
        assert!(matches!(
            validate_send_route("new", Some(OriginHarness::ClaudeCode), true),
            Err(error) if error.exit_code == EXIT_REFUSED
        ));
        assert!(matches!(
            validate_send_route("existing", Some(OriginHarness::Other), true),
            Err(error) if error.exit_code == EXIT_REFUSED
        ));
    }

    #[test]
    fn attachment_update_frame_uses_the_proposed_v1_fields_and_null_vendor_id() {
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        assert_eq!(
            attachment_update_request(op_id, session, None),
            json!({
                "v":1,
                "type":"attachment_update",
                "op_id":op_id.to_string(),
                "session_handle":session.to_string(),
                "vendor_session_id":null
            })
        );
        assert_eq!(
            attachment_update_request(op_id, session, Some("vendor-thread"))["vendor_session_id"],
            "vendor-thread"
        );
    }

    #[test]
    fn attachment_result_must_echo_the_exact_handle_and_vendor_id() {
        let session = Uuid::new_v4();
        let request = attachment_update_request(Uuid::new_v4(), session, None);
        let confirmed = json!({
            "v":1,
            "type":"ok",
            "session_handle":session.to_string(),
            "vendor_session_id":null
        });
        assert!(attachment_update_response_matches(&request, &confirmed));

        let wrong_handle = json!({
            "v":1,
            "type":"ok",
            "session_handle":Uuid::new_v4().to_string(),
            "vendor_session_id":null
        });
        assert!(!attachment_update_response_matches(&request, &wrong_handle));

        let wrong_vendor = json!({
            "v":1,
            "type":"ok",
            "session_handle":session.to_string(),
            "vendor_session_id":"stale-thread"
        });
        assert!(!attachment_update_response_matches(&request, &wrong_vendor));
        assert!(!attachment_update_response_matches(
            &request,
            &json!({"v":1,"type":"ok","session_handle":session.to_string()})
        ));
    }

    #[test]
    fn attach_retry_reuses_an_unknown_operation_and_blocks_changed_payloads() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let request = attachment_update_request(op_id, session, Some("thread-a"));
        store
            .prepare_operation(op_id, session, "attachment_update".into(), request.clone())
            .unwrap();
        store.mark_operation_unknown(op_id).unwrap();

        let resumed = prepare_or_resume_operation(
            &mut store,
            session,
            "attachment_update",
            attachment_update_request(Uuid::new_v4(), session, Some("thread-a")),
            true,
        )
        .unwrap();
        assert_eq!(resumed.op_id, op_id);
        assert_eq!(resumed.request, request);

        let changed = prepare_or_resume_operation(
            &mut store,
            session,
            "attachment_update",
            attachment_update_request(Uuid::new_v4(), session, Some("thread-b")),
            true,
        )
        .unwrap_err();
        assert_eq!(changed.exit_code, EXIT_OUTCOME_UNKNOWN);
        assert!(changed.message.contains(&op_id.to_string()));
    }

    #[test]
    fn reattach_with_a_new_vendor_id_records_new_support_before_listen() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();

        let send_id = Uuid::new_v4();
        store
            .prepare_operation(
                send_id,
                session,
                "send".into(),
                json!({"v":1,"type":"send","op_id":send_id,"session_handle":session}),
            )
            .unwrap();
        store
            .settle_operation(send_id, json!({"v":1,"type":"ok"}), None)
            .unwrap();

        let first_update = prepare_or_resume_operation(
            &mut store,
            session,
            "attachment_update",
            attachment_update_request(Uuid::new_v4(), session, Some("vendor-thread-1")),
            true,
        )
        .unwrap();
        store
            .settle_operation(
                first_update.op_id,
                json!({
                    "v":1,
                    "type":"ok",
                    "session_handle":session.to_string(),
                    "vendor_session_id":"vendor-thread-1"
                }),
                None,
            )
            .unwrap();
        let first_support = prepare_or_resume_operation(
            &mut store,
            session,
            "attachment_support",
            attachment_support_request(Uuid::new_v4(), session, OriginHarness::ClaudeCode, false),
            false,
        )
        .unwrap();
        store
            .settle_operation(first_support.op_id, json!({"v":1,"type":"ok"}), None)
            .unwrap();
        assert_eq!(
            attachment_route_for_listen(&store, session).unwrap(),
            Some("claude-code".into())
        );

        let second_update = prepare_or_resume_operation(
            &mut store,
            session,
            "attachment_update",
            attachment_update_request(Uuid::new_v4(), session, Some("vendor-thread-2")),
            true,
        )
        .unwrap();
        assert_ne!(first_update.op_id, second_update.op_id);
        store
            .settle_operation(
                second_update.op_id,
                json!({
                    "v":1,
                    "type":"ok",
                    "session_handle":session.to_string(),
                    "vendor_session_id":"vendor-thread-2"
                }),
                None,
            )
            .unwrap();
        assert_eq!(
            attachment_route_for_listen(&store, session)
                .unwrap_err()
                .exit_code,
            EXIT_REFUSED
        );

        let second_support = prepare_or_resume_operation(
            &mut store,
            session,
            "attachment_support",
            attachment_support_request(Uuid::new_v4(), session, OriginHarness::ClaudeCode, false),
            false,
        )
        .unwrap();
        assert_ne!(first_support.op_id, second_support.op_id);
        store
            .settle_operation(second_support.op_id, json!({"v":1,"type":"ok"}), None)
            .unwrap();
        assert_eq!(
            attachment_route_for_listen(&store, session).unwrap(),
            Some("claude-code".into())
        );
    }

    #[test]
    fn storage_unavailable_keeps_attach_and_h5_operations_retryable() {
        let session = Uuid::new_v4();
        for kind in ["attachment_update", "attachment_support", "support"] {
            let scratch = Scratch::new();
            let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
            let op_id = Uuid::new_v4();
            let request = match kind {
                "attachment_update" => {
                    attachment_update_request(op_id, session, Some("vendor-thread"))
                }
                _ => attachment_support_request(op_id, session, OriginHarness::ClaudeCode, false),
            };
            store
                .prepare_operation(op_id, session, kind.into(), request.clone())
                .unwrap();
            store.mark_operation_unknown(op_id).unwrap();
            let operation = store.operation(op_id).unwrap().unwrap();

            let error = handle_operation_error_response(
                &mut store,
                &operation,
                json!({"v":1,"type":"error","reason":"storage_unavailable"}),
                false,
            )
            .unwrap_err();
            assert_eq!(error.exit_code, EXIT_OUTCOME_UNKNOWN, "{kind}");

            let saved = store.operation(op_id).unwrap().unwrap();
            assert_eq!(saved.state, OperationState::Unknown, "{kind}");
            assert_eq!(saved.request, request, "{kind}");

            let mut retry_request = request;
            retry_request["op_id"] = json!(Uuid::new_v4().to_string());
            let retry =
                prepare_or_resume_operation(&mut store, session, kind, retry_request, false)
                    .unwrap();
            assert_eq!(retry.op_id, op_id, "{kind}");
            assert_eq!(retry.request, saved.request, "{kind}");
        }
    }

    #[test]
    fn listen_after_attach_requires_confirmed_update_and_h5_support() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        assert_eq!(attachment_route_for_listen(&store, session).unwrap(), None);
        assert!(!local_journal_proves_existing_session(&store, session).unwrap());

        let send_id = Uuid::new_v4();
        store
            .prepare_operation(
                send_id,
                session,
                "send".into(),
                json!({"v":1,"type":"send","op_id":send_id,"session_handle":session}),
            )
            .unwrap();
        store
            .settle_operation(send_id, json!({"v":1,"type":"ok"}), None)
            .unwrap();
        assert!(local_journal_proves_existing_session(&store, session).unwrap());

        let attachment_id = Uuid::new_v4();
        let attachment_request =
            attachment_update_request(attachment_id, session, Some("thread-a"));
        store
            .prepare_operation(
                attachment_id,
                session,
                "attachment_update".into(),
                attachment_request.clone(),
            )
            .unwrap();
        store.mark_operation_unknown(attachment_id).unwrap();
        assert_eq!(
            attachment_route_for_listen(&store, session)
                .unwrap_err()
                .exit_code,
            EXIT_OUTCOME_UNKNOWN
        );

        store
            .settle_operation(
                attachment_id,
                json!({
                    "v":1,
                    "type":"ok",
                    "session_handle":session.to_string(),
                    "vendor_session_id":"thread-a"
                }),
                None,
            )
            .unwrap();
        assert_eq!(
            attachment_route_for_listen(&store, session)
                .unwrap_err()
                .exit_code,
            EXIT_REFUSED
        );

        let support_id = Uuid::new_v4();
        let support_request =
            attachment_support_request(support_id, session, OriginHarness::ClaudeCode, false);
        store
            .prepare_operation(
                support_id,
                session,
                "attachment_support".into(),
                support_request,
            )
            .unwrap();
        store.mark_operation_unknown(support_id).unwrap();
        assert_eq!(
            attachment_route_for_listen(&store, session)
                .unwrap_err()
                .exit_code,
            EXIT_OUTCOME_UNKNOWN
        );
        store
            .settle_operation(support_id, json!({"v":1,"type":"ok"}), None)
            .unwrap();
        assert_eq!(
            attachment_route_for_listen(&store, session).unwrap(),
            Some("claude-code".into())
        );
    }

    #[test]
    fn attachment_operations_preserve_cursor_and_pending_turn_identity() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        for (sequence, event_id, message_id, revision) in
            [(1, "1", "request-a", 1), (2, "2", "request-b", 3)]
        {
            store
                .record_entry(JournalEntry::fetched(
                    sequence,
                    event_id.into(),
                    session,
                    "discussion".into(),
                    "person".into(),
                    true,
                    Some("app:example".into()),
                    Some(message_id.into()),
                    Some(revision),
                    json!({
                        "event_id":event_id,
                        "session_handle":session.to_string(),
                        "kind":"discussion",
                        "author":"person",
                        "sender_id":"app:example",
                        "message_id":message_id,
                        "request_revision":revision,
                        "text":format!("turn {sequence}")
                    }),
                ))
                .unwrap();
        }

        let send_id = Uuid::new_v4();
        store
            .prepare_operation(
                send_id,
                session,
                "send".into(),
                json!({"v":1,"type":"send","op_id":send_id,"session_handle":session}),
            )
            .unwrap();
        store
            .settle_operation(send_id, json!({"v":1,"type":"ok"}), None)
            .unwrap();
        let attachment_id = Uuid::new_v4();
        let attachment_request =
            attachment_update_request(attachment_id, session, Some("new-thread"));
        store
            .prepare_operation(
                attachment_id,
                session,
                "attachment_update".into(),
                attachment_request.clone(),
            )
            .unwrap();
        store
            .settle_operation(
                attachment_id,
                json!({
                    "v":1,
                    "type":"ok",
                    "session_handle":session.to_string(),
                    "vendor_session_id":"new-thread"
                }),
                None,
            )
            .unwrap();
        let support_id = Uuid::new_v4();
        store
            .prepare_operation(
                support_id,
                session,
                "attachment_support".into(),
                attachment_support_request(support_id, session, OriginHarness::ClaudeCode, false),
            )
            .unwrap();
        store
            .settle_operation(support_id, json!({"v":1,"type":"ok"}), None)
            .unwrap();

        let snapshot = store.snapshot().unwrap();
        assert_eq!(snapshot.cursor, 2);
        let pending = snapshot.pending_for(session);
        assert_eq!(
            pending
                .iter()
                .map(|entry| entry.event_id.as_str())
                .collect::<Vec<_>>(),
            ["1", "2"]
        );
        assert_eq!(pending[0].request_id.as_deref(), Some("request-a"));
        assert_eq!(pending[0].request_revision, Some(1));
        assert_eq!(pending[1].request_id.as_deref(), Some("request-b"));
        assert_eq!(pending[1].request_revision, Some(3));
        assert!(pending.iter().all(|entry| entry.delivered_at_ms.is_none()));
    }

    #[test]
    fn attach_support_does_not_claim_codex_listening() {
        let request =
            attachment_support_request(Uuid::new_v4(), Uuid::new_v4(), OriginHarness::Codex, false);
        assert_eq!(request["route"], "codex");
        assert!(
            request["unsupported_reason"]
                .as_str()
                .unwrap()
                .contains("not supported yet")
        );
        assert!(
            attachment_support_request(Uuid::new_v4(), Uuid::new_v4(), OriginHarness::Other, true,)
                ["unsupported_reason"]
                .is_null()
        );
        assert!(
            !attachment_support_request(
                Uuid::new_v4(),
                Uuid::new_v4(),
                OriginHarness::Other,
                false,
            )["unsupported_reason"]
                .is_null()
        );
    }

    #[test]
    fn support_confirmation_requires_the_exact_route_version_and_reason() {
        let expected = json!({
            "route":"claude-code",
            "cli_version":env!("CARGO_PKG_VERSION"),
            "unsupported_reason":null
        });
        assert!(support_record_matches(
            &json!({"listening_support":expected}),
            &expected
        ));
        assert!(!support_record_matches(
            &json!({"listening_support":{
                "route":"codex",
                "cli_version":env!("CARGO_PKG_VERSION"),
                "unsupported_reason":"Codex listening is not supported yet"
            }}),
            &expected
        ));
    }

    #[test]
    fn navigation_cleanup_request_keeps_exact_scope_and_recovers_the_same_operation() {
        let scratch = Scratch::new();
        let op_id = Uuid::new_v4();
        let request = discard_uploads_request(op_id, "app:example", "message-1", 3).unwrap();
        assert_eq!(
            request,
            json!({
                "v":1,
                "type":"discard_uploads",
                "op_id":op_id.to_string(),
                "sender_id":"app:example",
                "message_id":"message-1",
                "revision":3
            })
        );
        assert!(discard_uploads_request(op_id, " ", "message-1", 3).is_err());
        assert!(discard_uploads_request(op_id, "app:example", "message-1", 0).is_err());

        {
            let mut store = JournalStore::open(&scratch.0, "device", Uuid::nil()).unwrap();
            store
                .prepare_operation(
                    op_id,
                    Uuid::nil(),
                    "discard_uploads".into(),
                    request.clone(),
                )
                .unwrap();
            store.mark_operation_unknown(op_id).unwrap();
        }

        let mut reopened = JournalStore::open(&scratch.0, "device", Uuid::nil()).unwrap();
        let saved = reopened
            .snapshot()
            .unwrap()
            .operations
            .get(&op_id)
            .unwrap()
            .clone();
        assert_eq!(saved.state, OperationState::Unknown);
        assert_eq!(saved.request, request);
        assert_eq!(
            reopened
                .prepare_operation(
                    op_id,
                    Uuid::nil(),
                    "discard_uploads".into(),
                    request.clone(),
                )
                .unwrap()
                .request,
            request
        );
    }

    #[test]
    fn cleanup_response_requires_authoritative_state_and_count() {
        assert!(valid_discard_uploads_response(&json!({
            "type":"ok",
            "state":"discarded",
            "discarded":0
        })));
        assert!(valid_discard_uploads_response(&json!({
            "type":"ok",
            "state":"committed",
            "discarded":2
        })));
        assert!(!valid_discard_uploads_response(&json!({
            "type":"ok",
            "state":"deleted",
            "discarded":2
        })));
        assert!(!valid_discard_uploads_response(&json!({
            "type":"ok",
            "state":"discarded"
        })));
    }

    #[test]
    fn current_request_revision_and_terminal_state_gate_stale_actions() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        let initial = JournalEntry::fetched(
            1,
            "1".into(),
            session,
            "revision".into(),
            "origin".into(),
            false,
            Some("app:example".into()),
            Some("message-1".into()),
            Some(1),
            json!({
                "event_id":"1",
                "session_handle":session.to_string(),
                "kind":"revision",
                "author":"origin",
                "sender_id":"app:example",
                "message_id":"message-1",
                "request_revision":1,
                "message":{"sender_id":"app:example","message_id":"message-1","title":"old"}
            }),
        );
        let old_turn = JournalEntry::fetched(
            2,
            "2".into(),
            session,
            "discussion".into(),
            "person".into(),
            true,
            Some("app:example".into()),
            Some("message-1".into()),
            Some(1),
            json!({
                "event_id":"2",
                "session_handle":session.to_string(),
                "kind":"discussion",
                "author":"person",
                "sender_id":"app:example",
                "message_id":"message-1",
                "request_revision":1,
                "text":"old turn"
            }),
        );
        let updated_request = JournalEntry::fetched(
            3,
            "3".into(),
            session,
            "revision".into(),
            "person".into(),
            false,
            Some("app:example".into()),
            Some("message-1".into()),
            Some(2),
            json!({
                "event_id":"3",
                "session_handle":session.to_string(),
                "kind":"revision",
                "author":"person",
                "sender_id":"app:example",
                "message_id":"message-1",
                "request_revision":2,
                "message":{"sender_id":"app:example","message_id":"message-1","title":"new"}
            }),
        );
        let retracted = JournalEntry::fetched(
            4,
            "4".into(),
            session,
            "terminal".into(),
            "origin".into(),
            false,
            Some("app:example".into()),
            Some("message-1".into()),
            Some(2),
            json!({
                "event_id":"4",
                "session_handle":session.to_string(),
                "kind":"terminal",
                "author":"origin",
                "sender_id":"app:example",
                "message_id":"message-1",
                "request_revision":2,
                "terminal_state":"retracted"
            }),
        );
        for entry in [initial, old_turn, updated_request, retracted] {
            store.record_entry(entry).unwrap();
        }

        let snapshot = store.snapshot().unwrap();
        let request = request_context(&snapshot, session, "message-1").unwrap();
        assert_eq!(request.revision, 2);
        assert_eq!(request.message["title"], "new");
        assert!(ensure_current_revision(request.revision, Some(1)).is_err());
        assert!(ensure_current_revision(request.revision, Some(2)).is_ok());
        assert!(ensure_request_open(&snapshot, session, &request).is_err());
    }

    #[test]
    fn claude_listener_support_requires_interactive_background_prerequisites() {
        assert!(
            listener_unsupported_reason(
                OriginHarness::ClaudeCode,
                Some(SessionContext::Interactive),
                Some("2.1.234 (Claude Code)"),
                true,
                false,
            )
            .is_null()
        );
        assert!(
            listener_unsupported_reason(
                OriginHarness::ClaudeCode,
                Some(SessionContext::Bare),
                Some("2.1.234"),
                true,
                false,
            )
            .as_str()
            .unwrap()
            .contains("interactive")
        );
        assert!(
            listener_unsupported_reason(
                OriginHarness::ClaudeCode,
                Some(SessionContext::Interactive),
                Some("2.1.234"),
                false,
                false,
            )
            .as_str()
            .unwrap()
            .contains("background tasks")
        );
        assert!(
            listener_unsupported_reason(
                OriginHarness::ClaudeCode,
                Some(SessionContext::Interactive),
                Some("2.1.233"),
                true,
                false,
            )
            .as_str()
            .unwrap()
            .contains("2.1.234")
        );
        assert!(
            listener_unsupported_reason(
                OriginHarness::Codex,
                Some(SessionContext::Interactive),
                Some("2.1.234"),
                true,
                false,
            )
            .as_str()
            .unwrap()
            .contains("not supported yet")
        );
        assert!(
            listener_unsupported_reason(OriginHarness::Other, None, None, false, true).is_null()
        );
        assert!(
            listener_unsupported_reason(OriginHarness::Other, None, None, false, false)
                .as_str()
                .unwrap()
                .contains("no controller declared")
        );
    }

    #[test]
    fn request_preflight_accounts_for_the_server_assigned_upload_identity() {
        let mut request = json!({
            "v":1,
            "type":"send",
            "files":[{
                "name":"file.bin",
                "upload_pending":true,
                "size":1,
                "sha256":"0000000000000000000000000000000000000000000000000000000000000000"
            }]
        });
        request["body"] = Value::String(String::new());
        let base_len = serde_json::to_vec(&request).unwrap().len() as u64;
        let body_len = files::MAX_ORIGIN_REQUEST_BYTES - 4 - base_len;
        request["body"] = Value::String("x".repeat(body_len as usize));

        assert!(validate_request_size(&request).is_ok());
        assert!(validate_request_pre_upload(&request).is_err());
    }

    #[test]
    fn upload_binding_uses_the_entry_revision_and_reports_storage_refusal() {
        assert_eq!(
            upload_binding(&json!({
                "type":"send",
                "message":{"sender_id":"app:test","message_id":"m1"}
            }))
            .unwrap(),
            ("app:test".into(), "m1".into(), 1)
        );
        assert_eq!(
            upload_binding(&json!({
                "type":"reply",
                "sender_id":"app:test",
                "message_id":"m1",
                "request_revision":4
            }))
            .unwrap(),
            ("app:test".into(), "m1".into(), 4)
        );
        assert_eq!(
            upload_binding(&json!({
                "type":"revise",
                "sender_id":"app:test",
                "message_id":"m1",
                "base_revision":4
            }))
            .unwrap(),
            ("app:test".into(), "m1".into(), 5)
        );
        assert_eq!(
            known_upload_refusal(&ApiError::Http {
                status: 503,
                body: json!({"reason":"storage_unavailable"}).to_string()
            })
            .as_deref(),
            Some("storage_unavailable")
        );
        assert!(
            known_upload_refusal(&ApiError::Http {
                status: 503,
                body: json!({"reason":"unexpected"}).to_string()
            })
            .is_none()
        );
    }

    #[test]
    fn repeated_ack_recovers_server_file_outcomes_from_the_same_report() {
        let request = json!({
            "file_transfers":[
                {"name":"a.txt","status":"transferred"},
                {"name":"b.txt","status":"not_transferred","reason":"disk full"}
            ]
        });
        let expected = json!({
            "a.txt":{"status":"transferred"},
            "b.txt":{"status":"not_transferred","reason":"disk full"}
        });
        assert_eq!(ack_file_transfer_map(&request), Some(expected.clone()));

        let entry = json!({
            "files":[{"name":"a.txt"},{"name":"b.txt"}],
            "file_transfers":expected
        });
        assert!(server_file_transfers_complete(&entry));
        assert!(!server_file_transfers_complete(&json!({
            "files":[{"name":"a.txt"}],
            "file_transfers":{}
        })));
    }

    #[test]
    fn unresolved_request_operation_requires_same_key_retry_before_new_action() {
        let scratch = Scratch::new();
        let session = Uuid::new_v4();
        let op_id = Uuid::new_v4();
        let mut store = JournalStore::open(&scratch.0, "device", session).unwrap();
        store
            .prepare_operation(
                op_id,
                session,
                "reply".into(),
                json!({
                    "v":1,
                    "type":"reply",
                    "op_id":op_id,
                    "sender_id":"app:example",
                    "message_id":"request-1",
                    "in_reply_to":"9",
                    "text":"done"
                }),
            )
            .unwrap();
        store.mark_operation_unknown(op_id).unwrap();
        let snapshot = store.snapshot().unwrap();

        assert!(matches!(
            request_context(&snapshot, session, "request-1"),
            Err(error)
                if error.exit_code == EXIT_OUTCOME_UNKNOWN
                    && error.message.contains(&op_id.to_string())
        ));
    }
}
