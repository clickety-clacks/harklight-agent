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
    pub supersedes_local: &'a [String],
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
        "supersedes":input.supersedes,"supersedes_local":input.supersedes_local,
        "server_binding":client.server_binding(),"files":manifest,
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
    let content = build_send_content(input.title, input.body, input.choices, &fields, now_ms())
        .map_err(|error| WriteFailure::local(error.message))?;
    let mut body = json!({"sender":input.sender,"message":content});
    if !session.is_nil() {
        body["session"] = session.to_string().into();
    }
    if !input.supersedes.is_empty() {
        body["supersedes"] = json!(input.supersedes);
    }
    let uploads = if input.files.is_empty() {
        Vec::new()
    } else {
        files::stage_v2_uploads(
            input.files,
            &journal.upload_staging_directory(session, key),
            None,
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
        return writes::cached_committed(&operation, response.clone());
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
        "supersedes_local":prepared.inputs["supersedes_local"],
        "files":prepared.files,
    })
}

pub(super) fn freeze_send(
    client: &AgentClient,
    journal: &mut JournalStore,
    lock: &OperationLock,
    operation: OperationRecord,
) -> Result<OperationRecord, WriteFailure> {
    let mut body = operation
        .prepared_send
        .as_ref()
        .ok_or_else(|| WriteFailure::local("missing prepared send"))?
        .request
        .clone();
    let local_targets = operation
        .prepared_send
        .as_ref()
        .and_then(|prepared| prepared.inputs["supersedes_local"].as_array());
    if let Some(local_targets) = local_targets.filter(|targets| !targets.is_empty()) {
        if let Some(resolved) = operation.request.get("_resolved_supersedes") {
            body["supersedes"] = resolved.clone();
        } else {
            let sender = body["sender"]
                .as_str()
                .ok_or_else(|| WriteFailure::local("prepared send lacked a sender"))?;
            let mut targets = body["supersedes"].as_array().cloned().unwrap_or_default();
            for local in local_targets {
                let local = local
                    .as_str()
                    .ok_or_else(|| WriteFailure::local("invalid local supersedes reference"))?;
                if let Some(id) = journal.v2_item_id(sender, local)? {
                    targets.push(id.into());
                } else {
                    let matched = match client.legacy_message(sender, local) {
                        Ok(matched) => matched,
                        Err(error) if error.is_internal_error() => {
                            return Err(writes::record_failure(journal, &operation, error)?);
                        }
                        Err(error) => {
                            return Err(writes::unknown(&operation, &error.to_string(), None));
                        }
                    };
                    let found = matched
                        .as_ref()
                        .and_then(|found| found["message"]["item_id"].as_str())
                        .ok_or_else(|| {
                            WriteFailure::local("local supersedes reference could not be resolved")
                        })?;
                    journal.remember_v2_item(sender, local, found)?;
                    targets.push(found.into());
                }
            }
            body["supersedes"] = Value::Array(targets);
            journal.record_v2_supersedes(operation.op_id, body["supersedes"].clone())?;
        }
    }
    let file_ids = staged_file_ids(client, journal, &operation)?;
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
            supersedes_local: &[],
            files: &[],
            fields: &MessageFieldOptions {
                message_type: Some("fyi".into()),
                ..MessageFieldOptions::default()
            },
            answer_ui: None,
        };
        use crate::test_tls::FixtureResponse;
        let session = Uuid::new_v4().to_string();
        let input = SendInput {
            session: Some(&session),
            ..input
        };
        let (responses, responses_receiver) = std::sync::mpsc::channel();
        let server = SlowServer::sequence_from(responses_receiver);
        let client = client_for(&server);
        let prepared = send(&client, &mut journal, &input, Some(key), true).unwrap();
        let content = prepared["prepared"]["message"].clone();
        let sent = |content: Value| {
            json!({"rev":1,"message":{
                "item_id":"opaque/id","sender":"app:sample","session":session,
                "revision":1,"rev":1,"message":content,"seen":false,"files":[],
            }})
        };
        let mut wrong_title = content.clone();
        wrong_title["title"] = "Different title".into();
        let mut wrong_time = content.clone();
        wrong_time["created_at"] = 1.into();
        responses
            .send(vec![
                FixtureResponse {
                    status: "200 OK",
                    headers: &[("Request-Id", "send-req")],
                    body: r#"{"rev":1,"message":"#.into(),
                    truncate: true,
                },
                FixtureResponse {
                    status: "200 OK",
                    headers: &[("Request-Id", "wrong-content")],
                    body: sent(wrong_title).to_string(),
                    truncate: false,
                },
                FixtureResponse {
                    status: "200 OK",
                    headers: &[("Request-Id", "wrong-created-at")],
                    body: sent(wrong_time).to_string(),
                    truncate: false,
                },
                FixtureResponse::json(sent(content)),
            ])
            .ok()
            .unwrap();
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
        let revived_client = client_for(&server);
        let mut reopened = JournalStore::open(&root.0, "fixture", Uuid::nil()).unwrap();
        let malformed = writes::retry(&revived_client, &mut reopened, key, None).unwrap_err();
        assert_eq!(malformed.exit_code, 7);
        assert_eq!(malformed.output.unwrap()["request_id"], "wrong-content");
        let wrong_time = writes::retry(&revived_client, &mut reopened, key, None).unwrap_err();
        assert_eq!(wrong_time.exit_code, 7);
        assert_eq!(wrong_time.output.unwrap()["request_id"], "wrong-created-at");
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
        let mut requests = server.requests().into_iter();
        let first = requests.next().unwrap();
        let wrong_content = requests.next().unwrap();
        let wrong_time = requests.next().unwrap();
        let second = requests.next().unwrap();
        assert_eq!(first.line.trim_end(), "POST /v2/agent/messages HTTP/1.1");
        let body: Value = serde_json::from_slice(&first.body).unwrap();
        assert_eq!(body["session"], prepared["session_handle"]);
        assert_eq!(body["message"], prepared["prepared"]["message"]);
        assert!(body["message"].get("message_id").is_none());
        assert_eq!(first.body, second.body);
        assert_eq!(first.body, wrong_content.body);
        assert_eq!(first.body, wrong_time.body);
        for attempt in [first, wrong_content, wrong_time, second] {
            assert!(
                attempt
                    .headers
                    .contains(&("idempotency-key".into(), key.to_string()))
            );
        }
    }
    #[test]
    fn prepare_with_files_is_offline_and_execute_rejects_a_different_server() {
        let root =
            Scratch(std::env::temp_dir().join(format!("agent-prepare-test-{}", Uuid::new_v4())));
        let mut journal = JournalStore::open(&root.0, "fixture", Uuid::nil()).unwrap();
        let source = root.0.join("attachment.txt");
        std::fs::write(&source, b"saved bytes").unwrap();
        let paths = [source.clone()];
        let targets = ["server/opaque".to_owned()];
        let fields = MessageFieldOptions {
            message_type: Some("fyi".into()),
            ..MessageFieldOptions::default()
        };
        let input = SendInput {
            session: Some("new"),
            sender: "app:sample",
            title: "Prepare",
            body: None,
            choices: &[],
            supersedes: &targets,
            supersedes_local: &[],
            files: &paths,
            fields: &fields,
            answer_ui: None,
        };
        let first = SlowServer::sequence(Vec::new());
        let target = client_for(&first);
        assert!(first.requests().is_empty());
        let key = Uuid::new_v4();
        let prepared = send(&target, &mut journal, &input, Some(key), true).unwrap();
        assert_eq!(prepared["prepared"]["supersedes"], json!(targets));
        assert_eq!(prepared["prepared"]["files"][0]["name"], "attachment.txt");
        let operation = journal.operation(key).unwrap().unwrap();
        assert!(operation.response.is_none());
        assert!(operation.prepared_request.is_none());
        assert_eq!(
            std::fs::read(operation.uploads[0].staged_path.as_ref().unwrap()).unwrap(),
            b"saved bytes"
        );
        std::fs::remove_file(source).unwrap();

        let second = SlowServer::sequence(Vec::new());
        let conflicting = client_for(&second);
        assert!(second.requests().is_empty());
        let error = execute(&conflicting, &mut journal, key).unwrap_err();
        assert!(error.to_string().contains("different enrolled Server"));
        assert_eq!(
            journal.operation(key).unwrap().unwrap().state,
            crate::journal::OperationState::Prepared
        );
    }
}
