use std::path::PathBuf;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    commands::{
        MessageFieldOptions, ProgressReport, progress_content, revised_message_with_fields,
    },
    files,
    journal::{
        JournalError, JournalStore, OperationLock, OperationRecord, OperationState,
        PreparedRequest, RequestMethod, now_ms,
    },
};

use super::{
    AgentClient, sends,
    writes::{self, WriteFailure},
};

pub struct Mutation<'a> {
    pub operation: &'a str,
    pub item_id: &'a str,
    pub session: Option<Uuid>,
    pub draft: PreparedRequest,
    pub files: &'a [PathBuf],
}

pub fn submit(
    client: &AgentClient,
    journal: &mut JournalStore,
    op_id: Option<Uuid>,
    mutation: Mutation<'_>,
) -> Result<Value, WriteFailure> {
    let op_id = writes::operation_key(op_id)?;
    let lock = journal.lock_operation(op_id)?;
    let manifest = files::identify(mutation.files).map_err(WriteFailure::local)?;
    let mut request = json!({
        "wire":mutation.draft,
        "file_inputs":manifest,
    });
    if client.shared {
        request["shared"] = json!(true);
    }
    if mutation.draft.method == RequestMethod::Put {
        request["revision_intent"] = mutation.item_id.into();
        if let Ok(body) = serde_json::from_str::<Value>(&mutation.draft.body) {
            request["message"] = body["message"].clone();
        }
    }
    if let Some(saved) = journal.operation(op_id)? {
        if saved.protocol_v2 && saved.kind == mutation.operation && saved.request == request {
            return writes::deliver(client, journal, &lock, saved);
        }
        return Err(JournalError::OperationConflict(op_id).into());
    }
    let session = mutation.session.unwrap_or_else(Uuid::nil);
    let uploads = if mutation.files.is_empty() {
        Vec::new()
    } else {
        let cap = writes::upload_cap(client, mutation.operation)?;
        files::stage_v2_uploads(
            mutation.files,
            &journal.upload_staging_directory(session, op_id),
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
    let saved = match journal.prepare_v2_operation(
        op_id,
        session,
        mutation.operation.into(),
        request,
        uploads.clone(),
    ) {
        Ok(saved) => saved,
        Err(JournalError::PendingRevision(key)) => {
            files::cleanup_staged_uploads(&uploads);
            let pending = journal
                .operation(key)?
                .ok_or(JournalError::MissingOperation(key))?;
            return Err(writes::unknown(
                &pending,
                &format!("revision unresolved; retry operation {key}"),
                None,
            ));
        }
        Err(error) => {
            files::cleanup_staged_uploads(&uploads);
            return Err(error.into());
        }
    };
    writes::deliver(client, journal, &lock, saved)
}

pub fn reply(
    client: &AgentClient,
    journal: &mut JournalStore,
    item_id: &str,
    text: Option<&str>,
    in_reply_to: Option<&str>,
    notify_person_id: Option<&str>,
    file_ids: &[String],
    files: &[PathBuf],
    op_id: Option<Uuid>,
) -> Result<Value, WriteFailure> {
    if text.is_none_or(str::is_empty) && file_ids.is_empty() && files.is_empty() {
        return Err(WriteFailure::local("reply requires text or a file"));
    }
    let mut body = json!({});
    if let Some(text) = text {
        body["text"] = text.into();
    }
    if let Some(entry) = in_reply_to {
        body["in_reply_to"] = entry.into();
    }
    if let Some(person_id) = notify_person_id {
        body["notify_person_id"] = person_id.into();
    }
    if !file_ids.is_empty() {
        body["file_ids"] = json!(file_ids);
    }
    submit(
        client,
        journal,
        op_id,
        Mutation {
            operation: "reply",
            item_id,
            session: None,
            draft: PreparedRequest {
                method: RequestMethod::Post,
                path: vec![
                    "agent".into(),
                    "messages".into(),
                    item_id.into(),
                    "replies".into(),
                ],
                if_match: None,
                body: body.to_string(),
            },
            files,
        },
    )
}

pub struct Revision<'a> {
    pub item_id: &'a str,
    pub revision: u64,
    pub message: Value,
    pub file_ids: Option<&'a [String]>,
    pub files: &'a [PathBuf],
    pub operation: &'a str,
    pub session: Option<Uuid>,
    pub in_reply_to: Option<&'a str>,
}

pub fn revise(
    client: &AgentClient,
    journal: &mut JournalStore,
    revision: Revision<'_>,
    op_id: Option<Uuid>,
) -> Result<Value, WriteFailure> {
    let mut message = revision.message;
    let content = message
        .as_object_mut()
        .ok_or_else(|| WriteFailure::local("a revision message must be a JSON object"))?;
    content.remove("sender_id");
    content.remove("message_id");
    let mut body = json!({"message":message});
    if let Some(ids) = revision.file_ids {
        body["file_ids"] = json!(ids);
    }
    if let Some(entry) = revision.in_reply_to {
        body["in_reply_to"] = entry.into();
    }
    submit(
        client,
        journal,
        op_id,
        Mutation {
            operation: revision.operation,
            item_id: revision.item_id,
            session: revision.session,
            draft: PreparedRequest {
                method: RequestMethod::Put,
                path: vec!["agent".into(), "messages".into(), revision.item_id.into()],
                if_match: Some(revision.revision.to_string()),
                body: body.to_string(),
            },
            files: revision.files,
        },
    )
}

pub struct InteractiveRevision<'a> {
    pub item_id: &'a str,
    pub revision: u64,
    pub title: &'a str,
    pub body: Option<&'a str>,
    pub choices: &'a [String],
    pub clear_choices: bool,
    pub fields: &'a MessageFieldOptions,
    pub file_ids: Option<&'a [String]>,
    pub files: &'a [PathBuf],
    pub in_reply_to: Option<&'a str>,
}

pub fn revise_interactive(
    client: &AgentClient,
    journal: &mut JournalStore,
    revision: InteractiveRevision<'_>,
    op_id: Option<Uuid>,
) -> Result<Value, WriteFailure> {
    let snapshot = client
        .message(revision.item_id)
        .map_err(|error| read_failure("revise", error))?;
    let item = &snapshot["message"];
    if item["revision"] != revision.revision {
        return Err(WriteFailure::local_refusal(
            "base revision does not match the current message",
        ));
    }
    let (message, warnings) = revised_message_with_fields(
        &item["message"],
        revision.title,
        revision.body,
        revision.choices,
        revision.clear_choices,
        revision.fields,
    )
    .map_err(|error| WriteFailure::local_refusal(error.message))?;
    for warning in warnings {
        eprintln!("{warning}");
    }
    revise(
        client,
        journal,
        Revision {
            item_id: revision.item_id,
            revision: revision.revision,
            message,
            file_ids: revision.file_ids,
            files: revision.files,
            operation: "revise",
            session: None,
            in_reply_to: revision.in_reply_to,
        },
        op_id,
    )
}

pub fn progress(
    client: &AgentClient,
    journal: &mut JournalStore,
    session: Uuid,
    request: &str,
    report: &ProgressReport,
    op_id: Option<Uuid>,
) -> Result<Value, WriteFailure> {
    if let Some(key) = op_id
        && let Some(existing) = journal.operation(key)?
    {
        return match existing.state {
            OperationState::Prepared | OperationState::Unknown => Err(writes::unknown(
                &existing,
                "operation is unresolved; retry the saved key",
                None,
            )),
            _ => Err(JournalError::OperationConflict(key).into()),
        };
    }
    let mut item_id = request.to_owned();
    if let Ok(key) = Uuid::parse_str(request)
        && let Some(saved) = journal.operation(key)?
        && saved.protocol_v2
        && saved.kind == "send"
        && let Some(remote) = saved
            .response
            .as_ref()
            .and_then(|response| response["message"]["item_id"].as_str())
    {
        item_id = remote.to_owned();
    }
    let snapshot = client
        .message(&item_id)
        .map_err(|error| read_failure("progress", error))?;
    let item = &snapshot["message"];
    if item["item_id"] != item_id || item["session"] != session.to_string() {
        return Err(WriteFailure::local_refusal(
            "the message is outside this session",
        ));
    }
    let revision = item["revision"]
        .as_u64()
        .filter(|rev| *rev > 0)
        .ok_or_else(|| WriteFailure::local("message read has no revision"))?;
    let message = progress_content(&item["message"], report, now_ms())
        .map_err(|error| WriteFailure::local_refusal(error.message))?;
    revise(
        client,
        journal,
        Revision {
            item_id: &item_id,
            revision,
            message,
            file_ids: None,
            files: &[],
            operation: "progress",
            session: Some(session),
            in_reply_to: None,
        },
        op_id,
    )
}

fn read_failure(operation: &str, error: super::V2Error) -> WriteFailure {
    if error.is_internal_error() {
        return WriteFailure::local(format!("{operation}: {error}"));
    }
    match error {
        super::V2Error::Http {
            status,
            token,
            request_id,
            field,
            ..
        } if (400..500).contains(&status) => {
            let mut output = json!({
                "kind":"refused","operation":operation,"status":status,"error":token,
            });
            if let Some(id) = request_id {
                output["request_id"] = id.into();
            }
            if let Some(field) = field {
                output["field"] = field.into();
            }
            WriteFailure::from_saved_refusal(output)
        }
        _ => WriteFailure::local(error.to_string()),
    }
}

pub(super) fn freeze(
    client: &AgentClient,
    journal: &mut JournalStore,
    lock: &OperationLock,
    operation: OperationRecord,
) -> Result<OperationRecord, WriteFailure> {
    let mut wire: PreparedRequest = serde_json::from_value(operation.request["wire"].clone())
        .map_err(|error| WriteFailure::local(error.to_string()))?;
    if !operation.uploads.is_empty() {
        let ids = sends::staged_file_ids(client, journal, &operation)?;
        let mut body: Value = serde_json::from_str(&wire.body)
            .map_err(|error| WriteFailure::local(error.to_string()))?;
        let mut attached = body["file_ids"].as_array().cloned().unwrap_or_default();
        attached.extend(ids.into_iter().map(Value::String));
        body["file_ids"] = Value::Array(attached);
        wire.body = body.to_string();
    }
    Ok(journal.begin_v2_write(lock, wire)?)
}

#[cfg(test)]
mod tests {
    use crate::{
        config::EnrolledIdentity,
        test_tls::{FixtureResponse, SlowServer, TLS_NAME},
    };

    use super::*;

    struct TestJournal {
        root: PathBuf,
        store: JournalStore,
    }

    impl TestJournal {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("agent-mutation-test-{}", Uuid::new_v4()));
            let store = JournalStore::open(&root, "fixture", Uuid::nil()).unwrap();
            Self { root, store }
        }
    }

    impl Drop for TestJournal {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn client(server: &SlowServer) -> AgentClient {
        AgentClient::new(EnrolledIdentity::for_test(
            "127.0.0.1",
            TLS_NAME,
            server.port,
            server.ca_pem.as_bytes(),
        ))
        .unwrap()
    }

    fn snapshot(session: Uuid, file_manifest: Value) -> Value {
        json!({"rev":8,"message":{
            "item_id":"opaque/item","session":session,"revision":3,"files":file_manifest,
            "rev":7,"sender":"app:fixture","seen":false,
            "message":{
                "title":"Building", "body":"Keep this body", "created_at":100,
                "message_type":"fyi",
                "progress":{"percent":10,"segments":[{"stage":"Compile","width":10,
                    "at":"2026-01-01T00:00:00Z"}]}
            }
        }})
    }

    #[test]
    fn progress_put_replays_current_revision_without_replacing_attachments() {
        let mut journal = TestJournal::new();
        let session = Uuid::new_v4();
        let key = Uuid::new_v4();
        let report = ProgressReport {
            percent: 30,
            stage: "Verify".into(),
            note: None,
        };
        let server = SlowServer::sequence(vec![
            FixtureResponse::json(snapshot(
                session,
                json!([
                    {"file_id":"retained-a","name":"a","size":1,"sha256":"a".repeat(64)},
                    {"file_id":"retained-b","name":"b","size":1,"sha256":"b".repeat(64)},
                ]),
            )),
            FixtureResponse {
                status: "200 OK",
                headers: &[("Request-Id", "revise-req")],
                body: r#"{"rev":9,"message":"#.into(),
                truncate: true,
            },
        ]);
        let refused = progress(
            &client(&server),
            &mut journal.store,
            session,
            "opaque/item",
            &report,
            Some(key),
        )
        .unwrap_err();
        assert_eq!(refused.exit_code, 7);
        let requests = server.requests();
        assert_eq!(
            requests[0].line.trim_end(),
            "GET /v2/agent/messages/opaque%2Fitem HTTP/1.1"
        );
        let put = &requests[1];
        assert_eq!(
            put.line.trim_end(),
            "PUT /v2/agent/messages/opaque%2Fitem HTTP/1.1"
        );
        assert!(put.headers.contains(&("if-match".into(), "3".into())));
        let body: Value = serde_json::from_slice(&put.body).unwrap();
        assert!(body.get("file_ids").is_none());
        assert_eq!(body["message"]["body"], "Keep this body");
        assert_eq!(body["message"]["progress"]["percent"], 30);

        let replay = SlowServer::sequence(vec![FixtureResponse {
            status: "200 OK",
            headers: &[("Idempotent-Replayed", "true")],
            body: r#"{"rev":9,"message":{"item_id":"opaque/item","revision":4,"sender":"app:fixture","rev":9,"message":{},"seen":false,"files":[]}}"#.into(),
            truncate: false,
        }]);
        let output = writes::retry(&client(&replay), &mut journal.store, key, None).unwrap();
        assert_eq!(output["response"]["message"]["revision"], 4);
        let retried = replay.request();
        assert_eq!(retried.body, put.body);
        assert_eq!(retried.line, put.line);
        for request in [put, &retried] {
            assert!(
                request
                    .headers
                    .contains(&("idempotency-key".into(), key.to_string()))
            );
        }
    }

    #[test]
    fn progress_with_missing_file_id_does_not_put_a_revision() {
        let mut journal = TestJournal::new();
        let session = Uuid::new_v4();
        let server = SlowServer::sequence(vec![FixtureResponse::json(snapshot(
            session,
            json!([{"name":"missing"}]),
        ))]);
        let report = ProgressReport {
            percent: 30,
            stage: "Verify".into(),
            note: None,
        };
        let failure = progress(
            &client(&server),
            &mut journal.store,
            session,
            "opaque/item",
            &report,
            None,
        )
        .unwrap_err();
        assert_eq!(failure.exit_code, 1);
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn conditional_revise_does_not_replace_an_unresolved_intent_with_a_new_key() {
        let mut journal = TestJournal::new();
        let server = SlowServer::sequence(vec![FixtureResponse {
            status: "503 Service Unavailable",
            headers: &[("Retry-After", "1")],
            body: r#"{"error":"storage_unavailable","request_id":"first"}"#.into(),
            truncate: false,
        }]);
        let client = client(&server);
        let key = Uuid::new_v4();
        let make = || Revision {
            item_id: "opaque",
            revision: 4,
            message: json!({"title":"Changed","created_at":1}),
            file_ids: None,
            files: &[],
            operation: "revise",
            session: None,
            in_reply_to: None,
        };
        assert_eq!(
            revise(&client, &mut journal.store, make(), Some(key))
                .unwrap_err()
                .exit_code,
            7
        );
        let second = revise(&client, &mut journal.store, make(), Some(Uuid::new_v4())).unwrap_err();
        assert!(second.to_string().contains(&key.to_string()));
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn reply_preserves_the_correlation_and_uses_opaque_file_ids() {
        let mut journal = TestJournal::new();
        let server = SlowServer::sequence(vec![FixtureResponse::json(
            json!({"rev":9,"entry":{"entry_id":"new","item_id":"item","rev":9,"position":1,"request_revision":1,"author":"origin","kind":"reply","in_reply_to":"entry/1","text":""}}),
        )]);
        let output = reply(
            &client(&server),
            &mut journal.store,
            "item",
            None,
            Some("entry/1"),
            None,
            &["file-id".into()],
            &[],
            None,
        )
        .unwrap();
        assert_eq!(output["response"]["entry"]["entry_id"], "new");
        let request = server.request();
        assert_eq!(
            request.line.trim_end(),
            "POST /v2/agent/messages/item/replies HTTP/1.1"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap(),
            json!({"file_ids":["file-id"],"in_reply_to":"entry/1"})
        );
    }
}
