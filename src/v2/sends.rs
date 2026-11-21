use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    commands::{MessageFieldOptions, build_send_content, read_answer_ui_file},
    files,
    journal::{
        JournalError, JournalStore, OperationLock, OperationRecord, PreparedRequest, PreparedSend,
        RequestMethod, now_ms,
    },
};

use super::{
    AgentClient,
    writes::{self, WriteFailure},
};

pub struct SendInput<'a> {
    pub session: Option<&'a str>,
    pub sender: &'a str,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub choices: &'a [String],
    pub supersedes: &'a [String],
    pub files: &'a [PathBuf],
    pub fields: &'a MessageFieldOptions,
    pub answer_ui: Option<&'a Path>,
}

pub fn send(
    client: &AgentClient,
    journal: &mut JournalStore,
    input: &SendInput<'_>,
    op_id: Option<Uuid>,
    prepare_only: bool,
) -> Result<Value, WriteFailure> {
    if prepare_only && op_id.is_none() {
        return Err(WriteFailure::local("--prepare requires --op-id"));
    }
    let key = writes::operation_key(op_id)?;
    let lock = journal.lock_operation(key)?;
    let mut fields = input.fields.clone();
    if let Some(path) = input.answer_ui {
        fields.answer_ui =
            Some(read_answer_ui_file(path).map_err(|error| WriteFailure::local(error.message))?);
    }
    let normalized = build_send_content(input.title, input.body, input.choices, &fields, 0)
        .map_err(|error| WriteFailure::local(error.message))?;
    let manifest = files::identify(input.files).map_err(WriteFailure::local)?;
    let inputs = json!({
        "session":input.session,"sender":input.sender,"message":normalized,
        "supersedes":input.supersedes,"files":manifest,
    });
    if let Some(saved) = journal.operation(key)? {
        if saved.protocol_v2
            && saved.kind == "send"
            && saved
                .prepared_send
                .as_ref()
                .is_some_and(|prepared| prepared.inputs == inputs)
        {
            return if prepare_only {
                prepared_output(saved)
            } else {
                writes::deliver(client, journal, &lock, saved)
            };
        }
        return Err(JournalError::OperationConflict(key).into());
    }

    let session = match input.session {
        None => Uuid::nil(),
        Some("new") => Uuid::new_v4(),
        Some(id) => Uuid::parse_str(id)
            .ok()
            .filter(|id| !id.is_nil())
            .ok_or_else(|| WriteFailure::local("--session must be a UUID or new"))?,
    };
    let mut supersedes = Vec::new();
    for local in input.supersedes {
        if let Some(item) = journal.v2_item_id(input.sender, local)? {
            supersedes.push(item);
            continue;
        }
        let existing = client
            .legacy_message(input.sender, local)
            .map_err(|error| WriteFailure::local(error.to_string()))?;
        if let Some(found) = existing {
            let item_id = found["message"]["item_id"]
                .as_str()
                .ok_or_else(|| WriteFailure::local("legacy message lacked its item ID"))?;
            journal.remember_v2_item(input.sender, local, item_id)?;
            supersedes.push(item_id.to_owned());
        } else {
            supersedes.push(local.clone());
        }
    }
    let content = build_send_content(input.title, input.body, input.choices, &fields, now_ms())
        .map_err(|error| WriteFailure::local(error.message))?;
    let mut body = json!({"sender":input.sender,"message":content});
    if !session.is_nil() {
        body["session"] = session.to_string().into();
    }
    if !supersedes.is_empty() {
        body["supersedes"] = json!(supersedes);
    }
    let uploads = if input.files.is_empty() {
        Vec::new()
    } else {
        let cap = writes::upload_cap(client)?;
        files::stage_v2_uploads(
            input.files,
            &journal.upload_staging_directory(session, key),
            cap,
        )
        .map_err(WriteFailure::local)?
    };
    if uploads.iter().zip(&manifest).any(|(upload, manifest)| {
        upload.sha256 != manifest.sha256
            || upload.size_bytes != manifest.size_bytes
            || upload.name != manifest.name
    }) {
        files::cleanup_staged_uploads(&uploads);
        return Err(WriteFailure::local("file changed during staging"));
    }

    let prepared = PreparedSend {
        inputs,
        request: body,
        files: manifest,
    };
    let saved = journal.prepare_v2_send(&lock, session, prepared, uploads)?;
    if prepare_only {
        prepared_output(saved)
    } else {
        writes::deliver(client, journal, &lock, saved)
    }
}

pub fn execute(
    client: &AgentClient,
    journal: &mut JournalStore,
    key: Uuid,
) -> Result<Value, WriteFailure> {
    let lock = journal.lock_operation(key)?;
    let operation = journal
        .operation(key)?
        .ok_or(JournalError::MissingOperation(key))?;
    if !operation.protocol_v2 || operation.kind != "send" {
        return Err(JournalError::OperationConflict(key).into());
    }
    writes::deliver(client, journal, &lock, operation)
}

fn prepared_output(operation: OperationRecord) -> Result<Value, WriteFailure> {
    if let Some(response) = &operation.response
        && operation.state == crate::journal::OperationState::Committed
    {
        return Ok(writes::committed(&operation, response.clone()));
    }
    if operation.state == crate::journal::OperationState::Rejected {
        return Err(WriteFailure::local(
            "the operation was refused or abandoned",
        ));
    }
    let prepared = operation
        .prepared_send
        .as_ref()
        .ok_or_else(|| WriteFailure::local("missing prepared send"))?;
    Ok(json!({
        "kind":"prepared","operation":"send","op_id":operation.op_id,
        "session_handle":(!operation.session_handle.is_nil()).then_some(operation.session_handle),
        "request_id":operation.op_id,"prepared":prepared_metadata(prepared),
    }))
}

pub(super) fn prepared_metadata(prepared: &PreparedSend) -> Value {
    json!({
        "sender":prepared.request["sender"],
        "session":prepared.request["session"],
        "message":prepared.request["message"],
        "supersedes":prepared.request["supersedes"],
        "files":prepared.files,
    })
}

pub(super) fn freeze_send(
    client: &AgentClient,
    journal: &mut JournalStore,
    lock: &OperationLock,
    operation: OperationRecord,
) -> Result<OperationRecord, WriteFailure> {
    let file_ids = staged_file_ids(client, journal, &operation)?;
    let mut body = operation.request;
    if !file_ids.is_empty() {
        body["file_ids"] = json!(file_ids);
    }
    Ok(journal.begin_v2_write(
        lock,
        PreparedRequest {
            method: RequestMethod::Post,
            path: vec!["agent".into(), "messages".into()],
            if_match: None,
            body: body.to_string(),
        },
    )?)
}

pub(super) fn staged_file_ids(
    client: &AgentClient,
    journal: &mut JournalStore,
    operation: &OperationRecord,
) -> Result<Vec<String>, WriteFailure> {
    let mut file_ids = Vec::with_capacity(operation.uploads.len());
    for upload in &operation.uploads {
        if let Some(file_id) = &upload.upload_id {
            file_ids.push(file_id.clone());
            continue;
        }
        let key = upload
            .upload_key
            .ok_or_else(|| WriteFailure::local("upload has no saved key"))?;
        let path = files::verified_staged_path(upload)
            .map_err(|error| writes::unknown(operation, &error, None))?;
        journal.mark_operation_unknown(operation.op_id)?;
        let response = match client.upload_file(key, path, &upload.name, upload.size_bytes) {
            Ok(response) => response,
            Err(error) => return Err(writes::record_failure(journal, operation, error)?),
        };
        let Some(file_id) = response["file_id"].as_str().filter(|id| !id.is_empty()) else {
            return Err(writes::unknown(
                operation,
                "upload response lacked its file ID",
                None,
            ));
        };
        if response["name"] != upload.name
            || response["sha256"] != upload.sha256
            || response["size"] != upload.size_bytes
        {
            return Err(writes::unknown(
                operation,
                "upload response did not match the saved file",
                None,
            ));
        }
        journal
            .record_v2_upload(operation.op_id, key, file_id.to_owned())
            .map_err(|error| writes::unknown(operation, &error.to_string(), None))?;
        file_ids.push(file_id.to_owned());
    }

    Ok(file_ids)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        config::EnrolledIdentity,
        test_tls::{SlowServer, TLS_NAME},
    };

    use super::*;

    fn client_for(server: &SlowServer) -> AgentClient {
        AgentClient::new(EnrolledIdentity::for_test(
            "127.0.0.1",
            TLS_NAME,
            server.port,
            server.ca_pem.as_bytes(),
        ))
        .unwrap()
    }

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prepare_execute_retry_and_lookup_bind_the_same_send_to_the_server_item() {
        let root =
            Scratch(std::env::temp_dir().join(format!("agent-send-test-{}", Uuid::new_v4())));
        let mut journal = JournalStore::open(&root.0, "fixture", Uuid::nil()).unwrap();
        let key = Uuid::new_v4();
        let input = SendInput {
            session: Some("new"),
            sender: "app:sample",
            title: "Update",
            body: Some("Body"),
            choices: &[],
            supersedes: &[],
            files: &[],
            fields: &MessageFieldOptions {
                message_type: Some("fyi".into()),
                ..MessageFieldOptions::default()
            },
            answer_ui: None,
        };
        let server = SlowServer::truncated(
            "200 OK",
            &[("Request-Id", "send-req")],
            r#"{"rev":1,"message":"#,
        );
        let client = client_for(&server);
        let prepared = send(&client, &mut journal, &input, Some(key), true).unwrap();
        assert_eq!(prepared["kind"], "prepared");
        assert_eq!(prepared["request_id"], key.to_string());
        assert_eq!(prepared["prepared"]["sender"], "app:sample");
        assert_eq!(prepared["prepared"]["message"]["title"], "Update");
        assert_eq!(
            send(&client, &mut journal, &input, Some(key), true).unwrap(),
            prepared
        );

        let outcome = execute(&client, &mut journal, key).unwrap_err();
        assert_eq!(outcome.exit_code, 7);
        let first = server.request();
        assert_eq!(first.line.trim_end(), "POST /v2/agent/messages HTTP/1.1");
        let body: Value = serde_json::from_slice(&first.body).unwrap();
        assert_eq!(body["session"], prepared["session_handle"]);
        assert_eq!(body["message"], prepared["prepared"]["message"]);
        assert!(body["message"].get("message_id").is_none());
        assert!(body["message"].get("sender_id").is_none());

        let response = Box::leak(
            json!({"rev":1,"message":{
                "item_id":"opaque/id","sender":"app:sample","session":body["session"],
            }})
            .to_string()
            .into_boxed_str(),
        );
        let replay = SlowServer::respond(
            Duration::ZERO,
            "200 OK",
            &[("Idempotent-Replayed", "true")],
            response,
        );
        let revived_client = client_for(&replay);
        let mut reopened = JournalStore::open(&root.0, "fixture", Uuid::nil()).unwrap();
        let committed = writes::retry(&revived_client, &mut reopened, key, None).unwrap();
        assert_eq!(committed["item_id"], "opaque/id");
        assert_eq!(committed["prepared"], prepared["prepared"]);
        assert_eq!(
            reopened
                .v2_item_id("app:sample", &key.to_string())
                .unwrap()
                .as_deref(),
            Some("opaque/id")
        );
        assert_eq!(
            send(&revived_client, &mut reopened, &input, Some(key), true).unwrap(),
            committed
        );
        let second = replay.request();
        assert_eq!(first.body, second.body);
        for attempt in [first, second] {
            assert!(
                attempt
                    .headers
                    .contains(&("idempotency-key".into(), key.to_string()))
            );
        }
    }
}
