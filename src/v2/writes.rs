use std::path::Path;

use reqwest::Method;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::{Uuid, Variant};

use crate::files;
use crate::journal::{
    JournalError, JournalStore, OperationLock, OperationRecord, OperationState, PreparedRequest,
    RequestMethod,
};

use super::{AgentClient, JsonWrite, V2Error};

#[derive(Debug, Error)]
#[error("{detail}")]
pub struct WriteFailure {
    pub exit_code: i32,
    pub output: Option<Value>,
    detail: String,
}

impl WriteFailure {
    pub(super) fn local(detail: impl Into<String>) -> Self {
        Self {
            exit_code: 1,
            output: None,
            detail: detail.into(),
        }
    }

    pub(super) fn local_refusal(detail: impl Into<String>) -> Self {
        Self {
            exit_code: 2,
            output: None,
            detail: detail.into(),
        }
    }

    pub(super) fn from_saved_refusal(output: Value) -> Self {
        Self {
            exit_code: if output["error"] == "unsupported" {
                5
            } else {
                2
            },
            output: Some(output),
            detail: "the server refused this operation".into(),
        }
    }
}

impl From<JournalError> for WriteFailure {
    fn from(error: JournalError) -> Self {
        Self::local(error.to_string())
    }
}

pub enum ControlWrite<'a> {
    Retract {
        item_id: &'a str,
        in_reply_to: Option<&'a str>,
    },
    Lifecycle {
        item_id: &'a str,
        action: &'a str,
    },
    EndSession(Uuid),
    Result {
        item_id: &'a str,
        status: &'a str,
    },
    Receipt {
        item_id: &'a str,
        entry_id: Option<&'a str>,
        not_handed: Option<&'a str>,
        agent: Option<&'a str>,
        rerouted: bool,
    },
    FileUse {
        item_id: &'a str,
        entry_id: &'a str,
        file_id: &'a str,
        transfer: Option<&'a str>,
        usage: Option<&'a str>,
        reason: Option<&'a str>,
    },
    Show {
        item_id: &'a str,
        chat: bool,
        device_name: Option<&'a str>,
        device_id: Option<&'a str>,
    },
}

pub fn control(
    client: &AgentClient,
    journal: &mut JournalStore,
    op_id: Option<Uuid>,
    command: ControlWrite<'_>,
) -> Result<Value, WriteFailure> {
    match command {
        ControlWrite::Retract {
            item_id,
            in_reply_to,
        } => {
            let body = in_reply_to.map_or_else(|| json!({}), |id| json!({"in_reply_to":id}));
            post(
                client,
                journal,
                "retract",
                None,
                op_id,
                &["agent", "messages", item_id, "retract"],
                body,
            )
        }
        ControlWrite::Lifecycle { item_id, action } => post(
            client,
            journal,
            "lifecycle",
            None,
            op_id,
            &["agent", "messages", item_id, "lifecycle"],
            json!({"action":action}),
        ),
        ControlWrite::EndSession(session) => post(
            client,
            journal,
            "session-end",
            Some(session),
            op_id,
            &["agent", "sessions", &session.to_string(), "end"],
            json!({}),
        ),
        ControlWrite::Result { item_id, status } => post(
            client,
            journal,
            "result",
            None,
            op_id,
            &["agent", "messages", item_id, "result"],
            json!({"status":status}),
        ),
        ControlWrite::Receipt {
            item_id,
            entry_id,
            not_handed,
            agent,
            rerouted,
        } => {
            let mut body = json!({"outcome": "handed"});
            if let Some(reason) = not_handed {
                body["outcome"] = "not_handed".into();
                body["reason"] = reason.into();
            }
            if let Some(agent) = agent {
                body["agent"] = agent.into();
            }
            if rerouted {
                body["rerouted"] = true.into();
            }
            let path = match entry_id {
                Some(id) => vec!["agent", "messages", item_id, "entries", id, "receipt"],
                None => vec!["agent", "messages", item_id, "receipt"],
            };
            post(client, journal, "receipt", None, op_id, &path, body)
        }
        ControlWrite::FileUse {
            item_id,
            entry_id,
            file_id,
            transfer,
            usage,
            reason,
        } => {
            if transfer.is_none() && usage.is_none() {
                return Err(WriteFailure::local(
                    "file-use needs a transfer or use outcome",
                ));
            }
            let needs_reason = transfer == Some("not_transferred") || usage == Some("unusable");
            if needs_reason != reason.is_some() || reason.is_some_and(|text| text.trim().is_empty())
            {
                return Err(WriteFailure::local(
                    "a failed transfer or unusable file needs a reason",
                ));
            }
            let mut body = json!({"file_id":file_id});
            if let Some(transfer) = transfer {
                body["transfer"] = transfer.into();
            }
            if let Some(usage) = usage {
                body["use"] = usage.into();
            }
            if let Some(reason) = reason {
                body["reason"] = reason.into();
            }
            post(
                client,
                journal,
                "file-use",
                None,
                op_id,
                &[
                    "agent", "messages", item_id, "entries", entry_id, "file-use",
                ],
                body,
            )
        }
        ControlWrite::Show {
            item_id,
            chat,
            device_name,
            device_id,
        } => {
            let mut body =
                json!({"item_id":item_id,"target":if chat { "chat" } else { "message" }});
            if let Some(name) = device_name {
                body["device_name"] = name.into();
            }
            if let Some(id) = device_id {
                body["device_id"] = id.into();
            }
            post(
                client,
                journal,
                "show",
                None,
                op_id,
                &["agent", "show"],
                body,
            )
        }
    }
}

/// Prepare and submit a small JSON write. Commands that stage uploads use
/// the same delivery path after saving their final request.
pub fn post(
    client: &AgentClient,
    journal: &mut JournalStore,
    operation: &str,
    session: Option<Uuid>,
    op_id: Option<Uuid>,
    path: &[&str],
    body: Value,
) -> Result<Value, WriteFailure> {
    write(
        client,
        journal,
        operation,
        session,
        op_id,
        RequestMethod::Post,
        path,
        body,
    )
}

pub fn discard_upload(
    client: &AgentClient,
    journal: &mut JournalStore,
    file_id: &str,
    op_id: Option<Uuid>,
) -> Result<Value, WriteFailure> {
    write(
        client,
        journal,
        "discard-uploads",
        None,
        op_id,
        RequestMethod::Delete,
        &["files", file_id],
        Value::Null,
    )
}

fn write(
    client: &AgentClient,
    journal: &mut JournalStore,
    operation: &str,
    session: Option<Uuid>,
    op_id: Option<Uuid>,
    method: RequestMethod,
    path: &[&str],
    body: Value,
) -> Result<Value, WriteFailure> {
    let op_id = operation_key(op_id)?;
    if path
        .iter()
        .any(|segment| matches!(*segment, "" | "." | ".."))
    {
        return Err(WriteFailure::local("invalid server request path"));
    }
    let request = PreparedRequest {
        body: if method == RequestMethod::Delete {
            String::new()
        } else {
            body.to_string()
        },
        method,
        path: path.iter().map(|part| (*part).to_owned()).collect(),
        if_match: None,
    };
    let lock = journal.lock_operation(op_id)?;
    let operation = journal.prepare_v2_operation(
        op_id,
        session.unwrap_or_else(Uuid::nil),
        operation.to_owned(),
        json!({"method":request.method,"path":path,"body":body}),
        Vec::new(),
    )?;
    let operation = match operation.state {
        OperationState::Prepared | OperationState::Unknown => {
            journal.begin_v2_write(&lock, request)?
        }
        OperationState::Committed | OperationState::Rejected => operation,
    };
    deliver(client, journal, &lock, operation)
}

pub(super) fn operation_key(op_id: Option<Uuid>) -> Result<Uuid, WriteFailure> {
    let key = op_id.unwrap_or_else(Uuid::new_v4);
    if key.get_version_num() != 4 || key.get_variant() != Variant::RFC4122 {
        return Err(WriteFailure::local("--op-id must be a UUIDv4"));
    }
    Ok(key)
}

pub(super) fn upload_cap(client: &AgentClient) -> Result<Option<u64>, WriteFailure> {
    let me = client
        .read(&["me"], &[])
        .map_err(|error| WriteFailure::local(error.to_string()))?;
    match &me["limits"]["upload_cap_bytes_per_file"] {
        Value::String(value) if value == "none" => Ok(None),
        value => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| WriteFailure::local("invalid Server upload limit")),
    }
}

pub fn upload_file(
    client: &AgentClient,
    journal: &mut JournalStore,
    file: &Path,
    op_id: Option<Uuid>,
) -> Result<Value, WriteFailure> {
    let op_id = operation_key(op_id)?;
    let lock = journal.lock_operation(op_id)?;
    let paths = [file.to_owned()];
    let manifest = files::identify(&paths).map_err(WriteFailure::local)?;
    let request = json!({"files":manifest});
    if let Some(operation) = journal.operation(op_id)? {
        if operation.protocol_v2 && operation.kind == "file-upload" && operation.request == request
        {
            return deliver(client, journal, &lock, operation);
        }
        return Err(JournalError::OperationConflict(op_id).into());
    }

    let cap = upload_cap(client)?;
    let staging = journal.upload_staging_directory(Uuid::nil(), op_id);
    let uploads = files::stage_v2_uploads(&paths, &staging, cap).map_err(WriteFailure::local)?;
    let upload = &uploads[0];
    if upload.sha256 != manifest[0].sha256 || upload.size_bytes != manifest[0].size_bytes {
        files::cleanup_staged_uploads(&uploads);
        return Err(WriteFailure::local("file changed during staging"));
    }

    let operation =
        journal.prepare_v2_operation(op_id, Uuid::nil(), "file-upload".into(), request, uploads)?;
    deliver(client, journal, &lock, operation)
}

pub fn retry(
    client: &AgentClient,
    journal: &mut JournalStore,
    op_id: Uuid,
    expected_journal_id: Option<&str>,
) -> Result<Value, WriteFailure> {
    let on_error =
        |error: JournalError| unavailable_retry(op_id, expected_journal_id, &error.to_string());
    let lock = journal.lock_operation(op_id).map_err(on_error)?;
    let journal_id = journal.journal_id().map_err(on_error)?;
    if expected_journal_id.is_some_and(|id| id != journal_id) {
        return Err(operation_unresolved(
            op_id,
            &journal_id,
            "journal identity changed",
        ));
    }
    let Some(operation) = journal.operation(op_id).map_err(on_error)? else {
        if expected_journal_id == Some(journal_id.as_str()) {
            return Err(WriteFailure {
                exit_code: 1,
                output: Some(
                    json!({"kind":"operation_absent","op_id":op_id,"journal_id":journal_id}),
                ),
                detail: "no such operation in the expected journal".into(),
            });
        }
        return Err(operation_unresolved(
            op_id,
            &journal_id,
            "operation cannot be located",
        ));
    };
    deliver(client, journal, &lock, operation)
}

pub fn unavailable_retry(op_id: Uuid, journal_id: Option<&str>, detail: &str) -> WriteFailure {
    WriteFailure {
        exit_code: 7,
        output: Some(json!({"kind":"operation_unresolved","op_id":op_id,"journal_id":journal_id})),
        detail: detail.into(),
    }
}

fn operation_unresolved(op_id: Uuid, journal_id: &str, detail: &str) -> WriteFailure {
    unavailable_retry(op_id, Some(journal_id), detail)
}

pub fn abandon(journal: &mut JournalStore, op_id: Uuid) -> Result<Value, WriteFailure> {
    let _lock = journal.lock_operation(op_id)?;
    let operation = journal.abandon_operation(op_id).map_err(|error| {
        if matches!(error, JournalError::NotUnresolved { .. }) {
            let mut failure = WriteFailure::local(error.to_string());
            failure.exit_code = 2;
            failure
        } else {
            WriteFailure::from(error)
        }
    })?;
    Ok(json!({
        "kind":"abandoned","op_id":op_id,"operation":operation.kind,
        "session_handle":operation.session_handle,
    }))
}

pub(super) fn deliver(
    client: &AgentClient,
    journal: &mut JournalStore,
    lock: &OperationLock,
    mut operation: OperationRecord,
) -> Result<Value, WriteFailure> {
    let key = operation.op_id;
    if !operation.protocol_v2 {
        return Err(WriteFailure::local(
            "operation belongs to the earlier protocol",
        ));
    }
    if operation.kind == "send"
        && operation
            .prepared_send
            .as_ref()
            .is_none_or(|prepared| prepared.inputs["server_binding"] != client.server_binding())
    {
        return Err(WriteFailure::local(
            "prepared send belongs to a different enrolled Server",
        ));
    }
    match operation.state {
        OperationState::Committed => {
            let response = operation
                .response
                .clone()
                .ok_or_else(|| WriteFailure::local("committed operation has no saved response"))?;
            return Ok(committed(&operation, response));
        }
        OperationState::Rejected => {
            return Err(operation.response.map_or_else(
                || WriteFailure::local("operation was refused or abandoned"),
                |output| {
                    if output["kind"] == "refused" {
                        WriteFailure::from_saved_refusal(output)
                    } else {
                        WriteFailure::local("operation was refused or abandoned")
                    }
                },
            ));
        }
        OperationState::Prepared | OperationState::Unknown => {}
    }

    if operation.kind == "file-upload" {
        let upload = operation
            .uploads
            .first()
            .ok_or_else(|| WriteFailure::local("operation has no staged upload"))?;
        let path = files::verified_staged_path(upload).map_err(|error| {
            if operation.state == OperationState::Unknown {
                unknown(&operation, &error, None)
            } else {
                WriteFailure::local(error)
            }
        })?;
        journal.mark_operation_unknown(key)?;
        return match client.upload_file(key, path, &upload.name, upload.size_bytes) {
            Ok(response)
                if response["file_id"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty())
                    && response["name"] == upload.name
                    && response["size"] == upload.size_bytes
                    && response["sha256"] == upload.sha256 =>
            {
                journal
                    .settle_operation(key, response.clone(), None)
                    .map_err(|error| unknown(&operation, &error.to_string(), None))?;
                files::cleanup_staged_uploads(&operation.uploads);
                Ok(committed(&operation, response))
            }
            Ok(_) => Err(unknown(
                &operation,
                "upload response did not match the saved file",
                None,
            )),
            Err(error) => Err(record_failure(journal, &operation, error)?),
        };
    }

    if operation.kind == "send" && operation.prepared_request.is_none() {
        operation = super::sends::freeze_send(client, journal, lock, operation)?;
    }

    if operation.prepared_request.is_none() && operation.request.get("wire").is_some() {
        operation = super::mutations::freeze(client, journal, lock, operation)?;
    }
    if operation.prepared_request.is_none() && operation.request.get("method").is_some() {
        let method: RequestMethod = serde_json::from_value(operation.request["method"].clone())
            .map_err(|error| WriteFailure::local(error.to_string()))?;
        let path: Vec<String> = serde_json::from_value(operation.request["path"].clone())
            .map_err(|error| WriteFailure::local(error.to_string()))?;
        let body = if method == RequestMethod::Delete {
            String::new()
        } else {
            operation.request["body"].to_string()
        };
        operation = journal.begin_v2_write(
            lock,
            PreparedRequest {
                method,
                path,
                body,
                if_match: None,
            },
        )?;
    }
    let request = operation
        .prepared_request
        .as_ref()
        .ok_or_else(|| WriteFailure::local("operation has no saved v2 request"))?;
    let path: Vec<_> = request.path.iter().map(String::as_str).collect();
    let method = match request.method {
        RequestMethod::Post => Method::POST,
        RequestMethod::Put => Method::PUT,
        RequestMethod::Delete => Method::DELETE,
    };
    let response = if method == Method::DELETE {
        client.delete(&path, key).map(|()| (json!({}), None))
    } else {
        client.write_correlated(
            method.clone(),
            &path,
            JsonWrite {
                key,
                body: request.body.as_bytes(),
                if_match: request.if_match.as_deref(),
            },
        )
    };
    match response {
        Ok((response, _)) if valid_write_response(&operation.kind, request, &response) => {
            journal
                .settle_operation(key, response.clone(), None)
                .map_err(|error| unknown(&operation, &error.to_string(), None))?;
            if !crate::journal::is_progress(&operation.request["message"]) {
                files::cleanup_staged_uploads(&operation.uploads);
            }
            Ok(committed(&operation, response))
        }
        Ok((_, request_id)) => {
            let mut failure = unknown(
                &operation,
                "server response did not match the operation",
                None,
            );
            if let Some(id) = request_id
                && let Some(output) = failure.output.as_mut()
            {
                output["request_id"] = id.into();
            }
            Err(failure)
        }
        Err(error) => Err(record_failure(journal, &operation, error)?),
    }
}

fn valid_write_response(kind: &str, request: &PreparedRequest, response: &Value) -> bool {
    if kind == "show" {
        return response["show_id"].is_string()
            && response["device_id"].is_string()
            && response["device_name"].is_string()
            && matches!(response["state"].as_str(), Some("sent" | "held"));
    }
    if request.method == RequestMethod::Delete {
        return response.is_object();
    }
    if !response["rev"].is_u64() {
        return false;
    }
    if matches!(kind, "send" | "revise" | "progress") {
        return (kind == "send"
            || request
                .path
                .get(2)
                .is_some_and(|item| response["message"]["item_id"] == *item))
            && super::reads::valid_sent_message(
                &response["message"],
                response["rev"].as_u64().unwrap(),
            );
    }
    if kind == "reply" {
        let entry = &response["entry"];
        return request
            .path
            .get(2)
            .is_some_and(|item| entry["item_id"] == *item)
            && entry["entry_id"].as_str().is_some_and(|id| !id.is_empty());
    }
    if kind != "receipt" {
        return true;
    }
    if !matches!(
        response["status"].as_str(),
        Some("recorded" | "already_recorded" | "superseded")
    ) || !matches!(
        response["stage_outcome"].as_str(),
        Some("handed" | "not_handed")
    ) {
        return false;
    }
    match request.path.as_slice() {
        [_, _, item, _, entry, _] => {
            response["entry"]["entry_id"] == *entry && response["entry"]["item_id"] == *item
        }
        [_, _, item, _] => response["message"]["item_id"] == *item,
        _ => false,
    }
}

pub(super) fn record_failure(
    journal: &mut JournalStore,
    operation: &OperationRecord,
    error: V2Error,
) -> Result<WriteFailure, WriteFailure> {
    if let V2Error::Http {
        status,
        ref token,
        ref field,
        ref request_id,
        retry_after,
    } = error
    {
        if (400..500).contains(&status) {
            let mut output = json!({
                "kind":"refused","operation":operation.kind,"op_id":operation.op_id,
                "status":status,"error":token,
            });
            if let Some(field) = field {
                output["field"] = field.as_str().into();
            }
            if let Some(id) = request_id {
                output["request_id"] = id.as_str().into();
            }
            journal.reject_operation(operation.op_id, output.clone())?;
            files::cleanup_staged_uploads(&operation.uploads);
            return Ok(WriteFailure::from_saved_refusal(output));
        }
        let mut failure = unknown(operation, &error.to_string(), retry_after);
        if let Some(output) = failure.output.as_mut() {
            output["status"] = status.into();
            if let Some(id) = request_id {
                output["request_id"] = id.as_str().into();
            }
        }
        return Ok(failure);
    }
    if error.outcome_unknown() {
        let mut failure = unknown(operation, &error.to_string(), None);
        if let V2Error::ResponseRead {
            status,
            ref request_id,
            ..
        } = error
            && let Some(output) = failure.output.as_mut()
        {
            output["status"] = status.into();
            if let Some(id) = request_id {
                output["request_id"] = id.as_str().into();
            }
        }
        return Ok(failure);
    }
    Ok(WriteFailure::local(error.to_string()))
}

pub(super) fn committed(operation: &OperationRecord, response: Value) -> Value {
    let mut output = json!({
        "kind":"committed","operation":operation.kind,"op_id":operation.op_id,"response":response,
    });
    if !operation.session_handle.is_nil() {
        output["session_handle"] = operation.session_handle.to_string().into();
    }
    if operation.kind == "send" {
        output["request_id"] = operation.op_id.to_string().into();
        output["item_id"] = output["response"]["message"]["item_id"].clone();
        if let Some(prepared) = &operation.prepared_send {
            output["prepared"] = super::sends::prepared_metadata(prepared);
        }
    }
    output
}

pub(super) fn unknown(
    operation: &OperationRecord,
    detail: &str,
    retry_after: Option<u64>,
) -> WriteFailure {
    let mut output = json!({
        "kind":"outcome_unknown","operation":operation.kind,"op_id":operation.op_id,
    });
    if let Some(seconds) = retry_after {
        output["retry_after"] = seconds.into();
    }
    WriteFailure {
        exit_code: 7,
        output: Some(output),
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use crate::{
        config::EnrolledIdentity,
        test_tls::{SlowServer, TLS_NAME},
    };

    use super::*;

    struct TestJournal {
        root: PathBuf,
        store: JournalStore,
    }

    impl TestJournal {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("agent-write-test-{}", Uuid::new_v4()));
            let store = JournalStore::open(&root, "fixture", Uuid::nil()).unwrap();
            Self { root, store }
        }
    }

    impl Drop for TestJournal {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn client_for(server: &SlowServer) -> AgentClient {
        AgentClient::new(EnrolledIdentity::for_test(
            "127.0.0.1",
            TLS_NAME,
            server.port,
            server.ca_pem.as_bytes(),
        ))
        .unwrap()
    }

    #[test]
    fn correlated_retract_posts_the_entry_id() {
        let mut journal = TestJournal::new();
        let server = SlowServer::answer_after(Duration::ZERO, r#"{"rev":2}"#);
        let key = Uuid::new_v4();
        let output = control(
            &client_for(&server),
            &mut journal.store,
            Some(key),
            ControlWrite::Retract {
                item_id: "item/a",
                in_reply_to: Some("entry/one"),
            },
        )
        .unwrap();
        assert_eq!(output["kind"], "committed");
        assert_eq!(output["op_id"], key.to_string());
        assert_eq!(output["response"]["rev"], 2);
        let request = server.request();
        assert_eq!(
            request.line.trim_end(),
            "POST /v2/agent/messages/item%2Fa/retract HTTP/1.1"
        );
        assert_eq!(request.body, br#"{"in_reply_to":"entry/one"}"#);
        assert!(
            request
                .headers
                .contains(&("idempotency-key".into(), key.to_string()))
        );
    }

    #[test]
    fn lifecycle_refusal_is_saved_without_retrying_it() {
        let mut journal = TestJournal::new();
        let server = SlowServer::respond(
            Duration::ZERO,
            "409 Conflict",
            &[("Request-Id", "lifecycle-req")],
            r#"{"error":"already_closed","retryable":false,"request_id":"lifecycle-req"}"#,
        );
        let client = client_for(&server);
        let key = Uuid::new_v4();
        let refused = control(
            &client,
            &mut journal.store,
            Some(key),
            ControlWrite::Lifecycle {
                item_id: "item",
                action: "expired",
            },
        )
        .unwrap_err();

        let request = server.request();
        assert_eq!(
            request.line.trim_end(),
            "POST /v2/agent/messages/item/lifecycle HTTP/1.1"
        );
        assert_eq!(request.body, br#"{"action":"expired"}"#);
        assert_eq!(refused.exit_code, 2);
        assert_eq!(refused.output.as_ref().unwrap()["error"], "already_closed");
        assert_eq!(
            refused.output.as_ref().unwrap()["request_id"],
            "lifecycle-req"
        );
        assert_eq!(
            retry(&client, &mut journal.store, key, None)
                .unwrap_err()
                .output,
            refused.output
        );
    }

    #[test]
    fn session_end_replays_the_saved_request_after_a_retryable_response() {
        let mut journal = TestJournal::new();
        let session = Uuid::new_v4();
        let key = Uuid::new_v4();
        let unavailable = SlowServer::respond(
            Duration::ZERO,
            "503 Service Unavailable",
            &[("Retry-After", "1")],
            r#"{"error":"retry_later","retryable":true,"request_id":"req-1"}"#,
        );
        let failure = control(
            &client_for(&unavailable),
            &mut journal.store,
            Some(key),
            ControlWrite::EndSession(session),
        )
        .unwrap_err();
        assert_eq!(failure.exit_code, 7);
        assert_eq!(failure.output.unwrap()["retry_after"], 1);

        let first = unavailable.request();
        assert_eq!(
            first.line.trim_end(),
            format!("POST /v2/agent/sessions/{session}/end HTTP/1.1")
        );
        assert_eq!(first.body, b"{}");

        let mut reopened = JournalStore::open(&journal.root, "fixture", Uuid::nil()).unwrap();
        let replayed = SlowServer::respond(
            Duration::ZERO,
            "200 OK",
            &[("Idempotent-Replayed", "true")],
            r#"{"rev":2}"#,
        );
        let output = retry(&client_for(&replayed), &mut reopened, key, None).unwrap();
        let second = replayed.request();
        assert_eq!(output["session_handle"], session.to_string());
        assert_eq!(first.line, second.line);
        assert_eq!(first.body, second.body);
        for request in [first, second] {
            assert!(
                request
                    .headers
                    .contains(&("idempotency-key".into(), key.to_string()))
            );
        }
    }
    #[test]
    fn answer_and_chat_receipts_use_distinct_targets() {
        let mut journal = TestJournal::new();
        for (entry_id, target) in [
            (None, "POST /v2/agent/messages/item/receipt HTTP/1.1"),
            (
                Some("entry/1"),
                "POST /v2/agent/messages/item/entries/entry%2F1/receipt HTTP/1.1",
            ),
        ] {
            let projection = entry_id.map_or_else(
                || json!({"message":{"item_id":"item"}}),
                |id| json!({"entry":{"item_id":"item","entry_id":id}}),
            );
            let mut body = json!({"rev":3,"status":"recorded","stage_outcome":"handed"});
            body.as_object_mut()
                .unwrap()
                .extend(projection.as_object().unwrap().clone());
            let server = SlowServer::sequence(vec![crate::test_tls::FixtureResponse::json(body)]);
            let output = control(
                &client_for(&server),
                &mut journal.store,
                None,
                ControlWrite::Receipt {
                    item_id: "item",
                    entry_id,
                    not_handed: None,
                    agent: Some("Desk"),
                    rerouted: true,
                },
            )
            .unwrap();
            assert_eq!(output["response"]["status"], "recorded");
            let request = server.request();
            assert_eq!(request.line.trim_end(), target);
            assert_eq!(
                serde_json::from_slice::<Value>(&request.body).unwrap(),
                json!({"outcome":"handed","agent":"Desk","rerouted":true})
            );
        }
    }

    #[test]
    fn file_use_reports_one_file_on_the_separate_route() {
        let mut journal = TestJournal::new();
        let server = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"rev":4,"entry":{},"file_id":"file/1","outcome":"unusable"}"#,
        );
        let output = control(
            &client_for(&server),
            &mut journal.store,
            None,
            ControlWrite::FileUse {
                item_id: "item",
                entry_id: "entry",
                file_id: "file/1",
                transfer: None,
                usage: Some("unusable"),
                reason: Some("wrong format"),
            },
        )
        .unwrap();
        assert_eq!(output["response"]["outcome"], "unusable");
        let request = server.request();
        assert_eq!(
            request.line.trim_end(),
            "POST /v2/agent/messages/item/entries/entry/file-use HTTP/1.1"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({"file_id":"file/1","use":"unusable","reason":"wrong format"})
        );
    }

    #[test]
    fn result_uses_the_item_and_keeps_not_answered_as_a_definite_refusal() {
        let mut journal = TestJournal::new();
        let server = SlowServer::respond(
            Duration::ZERO,
            "409 Conflict",
            &[],
            r#"{"error":"not_answered","retryable":false,"request_id":"result-req"}"#,
        );
        let failure = control(
            &client_for(&server),
            &mut journal.store,
            None,
            ControlWrite::Result {
                item_id: "item",
                status: "applied",
            },
        )
        .unwrap_err();
        assert_eq!(failure.exit_code, 2);
        assert_eq!(failure.output.unwrap()["error"], "not_answered");
        let request = server.request();
        assert_eq!(
            request.line.trim_end(),
            "POST /v2/agent/messages/item/result HTTP/1.1"
        );
        assert_eq!(request.body, br#"{"status":"applied"}"#);
    }

    #[test]
    fn show_targets_device_name_and_accepts_a_held_response_without_a_position() {
        let mut journal = TestJournal::new();
        let server = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"show_id":"s","device_id":"d","device_name":"Desk","state":"held"}"#,
        );
        let output = control(
            &client_for(&server),
            &mut journal.store,
            None,
            ControlWrite::Show {
                item_id: "item",
                chat: true,
                device_name: Some("Desk"),
                device_id: None,
            },
        )
        .unwrap();
        assert_eq!(output["response"]["state"], "held");
        let request = server.request();
        assert_eq!(request.line.trim_end(), "POST /v2/agent/show HTTP/1.1");
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({"item_id":"item","target":"chat","device_name":"Desk"})
        );
    }

    #[test]
    fn only_a_matching_journal_proves_a_missing_operation_never_started() {
        let mut journal = TestJournal::new();
        let server = SlowServer::answer_after(Duration::ZERO, "{}");
        let client = client_for(&server);
        let journal_id = journal.store.journal_id().unwrap();
        let op_id = Uuid::new_v4();
        let missing = retry(&client, &mut journal.store, op_id, Some(&journal_id)).unwrap_err();
        assert_eq!(missing.exit_code, 1);
        assert_eq!(missing.output.unwrap()["kind"], "operation_absent");

        for expected in [None, Some("different-journal")] {
            let unresolved = retry(&client, &mut journal.store, op_id, expected).unwrap_err();
            assert_eq!(unresolved.exit_code, 7);
            assert_eq!(unresolved.output.unwrap()["kind"], "operation_unresolved");
        }
    }

    #[test]
    fn simultaneous_same_key_writes_share_a_single_http_request() {
        use std::sync::{Arc, Barrier};
        let mut journal = TestJournal::new();
        let server = SlowServer::answer_after(Duration::from_millis(120), r#"{"rev":3}"#);
        let client = client_for(&server);
        let second_client = client.clone();
        let op_id = Uuid::new_v4();
        let root = journal.root.clone();
        let barrier = Arc::new(Barrier::new(2));
        let second_barrier = Arc::clone(&barrier);
        let other = std::thread::spawn(move || {
            let mut reopened = JournalStore::open(&root, "fixture", Uuid::nil()).unwrap();
            second_barrier.wait();
            control(
                &second_client,
                &mut reopened,
                Some(op_id),
                ControlWrite::Retract {
                    item_id: "item",
                    in_reply_to: None,
                },
            )
            .unwrap()
        });
        barrier.wait();
        let first = control(
            &client,
            &mut journal.store,
            Some(op_id),
            ControlWrite::Retract {
                item_id: "item",
                in_reply_to: None,
            },
        )
        .unwrap();
        assert_eq!(first, other.join().unwrap());
        assert_eq!(
            server.request_line().trim_end(),
            "POST /v2/agent/messages/item/retract HTTP/1.1"
        );
    }
    #[test]
    fn lost_upload_response_replays_the_staged_bytes_even_after_source_changes() {
        let mut journal = TestJournal::new();
        let key = Uuid::new_v4();
        let source = journal.root.join("report.txt");
        std::fs::write(&source, b"original contents").unwrap();
        let files = [source.clone()];
        let staging = journal.store.upload_staging_directory(Uuid::nil(), key);
        let uploads = files::stage_v2_uploads(&files, &staging, None).unwrap();
        let digest = uploads[0].sha256.clone();
        journal
            .store
            .prepare_v2_operation(key, Uuid::nil(), "file-upload".into(), json!({}), uploads)
            .unwrap();
        let dropped = SlowServer::truncated(
            "201 Created",
            &[("Request-Id", "upload-request")],
            r#"{"file_id":"opaque"#,
        );
        let fail = retry(&client_for(&dropped), &mut journal.store, key, None).unwrap_err();
        assert_eq!(fail.exit_code, 7);
        assert_eq!(fail.output.unwrap()["request_id"], "upload-request");
        let first = dropped.request();
        std::fs::write(source, b"changed caller file").unwrap();
        let response = Box::leak(
            json!({
                "file_id":"opaque","name":"report.txt","size":17,"sha256":digest,
            })
            .to_string()
            .into_boxed_str(),
        );
        let replay = SlowServer::respond(
            Duration::ZERO,
            "201 Created",
            &[("Idempotent-Replayed", "true")],
            response,
        );
        let recovered = retry(&client_for(&replay), &mut journal.store, key, None).unwrap();
        assert_eq!(recovered["response"]["file_id"], "opaque");
        let second = replay.request();
        assert_eq!(first.body, b"original contents");
        assert_eq!(first.body, second.body);
        for attempt in [first, second] {
            assert!(
                attempt
                    .headers
                    .contains(&("idempotency-key".into(), key.to_string()))
            );
        }
    }

    #[test]
    fn discard_upload_commits_a_bodyless_result_and_reuses_its_saved_key() {
        let mut journal = TestJournal::new();
        let key = Uuid::new_v4();
        let server = SlowServer::respond(Duration::ZERO, "204 No Content", &[], "");
        let client = client_for(&server);
        let output = discard_upload(&client, &mut journal.store, "file/one", Some(key)).unwrap();
        assert_eq!(output["response"], json!({}));
        assert_eq!(
            retry(&client, &mut journal.store, key, None).unwrap(),
            output
        );
        let request = server.request();
        assert_eq!(
            request.line.trim_end(),
            "DELETE /v2/files/file%2Fone HTTP/1.1"
        );
        assert!(
            request
                .headers
                .contains(&("idempotency-key".into(), key.to_string()))
        );
    }
    #[test]
    fn retry_recovers_an_intent_reserved_before_freezing() {
        let mut journal = TestJournal::new();
        let key = Uuid::new_v4();
        let session = Uuid::new_v4();
        let id = journal.store.journal_id().unwrap();
        journal
            .store
            .prepare_v2_operation(
                key,
                session,
                "session-end".into(),
                json!({"method":"post","path":["agent","sessions",session,"end"],"body":{}}),
                Vec::new(),
            )
            .unwrap();
        let server = SlowServer::answer_after(Duration::ZERO, r#"{"rev":7}"#);
        assert_eq!(
            retry(&client_for(&server), &mut journal.store, key, Some(&id)).unwrap()["kind"],
            "committed"
        );
        let request = server.request();
        assert_eq!(
            request.line.trim_end(),
            format!("POST /v2/agent/sessions/{session}/end HTTP/1.1")
        );
        assert!(
            request
                .headers
                .contains(&("idempotency-key".into(), key.to_string()))
        );
        assert_eq!(request.body, b"{}");
    }

    #[test]
    fn receipt_without_its_affected_target_remains_unresolved() {
        let mut journal = TestJournal::new();
        let key = Uuid::new_v4();
        let server = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"rev":9,"status":"recorded","stage_outcome":"handed"}"#,
        );
        let failure = control(
            &client_for(&server),
            &mut journal.store,
            Some(key),
            ControlWrite::Receipt {
                item_id: "item",
                entry_id: None,
                not_handed: None,
                agent: None,
                rerouted: false,
            },
        )
        .unwrap_err();
        assert_eq!(failure.exit_code, 7);
        assert_eq!(
            journal.store.operation(key).unwrap().unwrap().state,
            OperationState::Unknown
        );
        server.request();
    }
}
