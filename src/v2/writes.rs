use reqwest::Method;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::{Uuid, Variant};

use crate::journal::{
    JournalError, JournalStore, OperationRecord, OperationState, PreparedRequest, RequestMethod,
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
    fn local(detail: impl Into<String>) -> Self {
        Self {
            exit_code: 1,
            output: None,
            detail: detail.into(),
        }
    }

    fn from_saved_refusal(output: Value) -> Self {
        Self {
            exit_code: 2,
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
        reason: Option<&'a str>,
    },
    Show {
        item_id: &'a str,
        chat: bool,
        device_name: Option<&'a str>,
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
            reason,
        } => {
            let mut body = json!({"file_id":file_id, "outcome":"used"});
            if let Some(reason) = reason {
                body["outcome"] = "unusable".into();
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
        } => {
            let mut body =
                json!({"item_id":item_id,"target":if chat { "chat" } else { "message" }});
            if let Some(name) = device_name {
                body["device_name"] = name.into();
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
    let op_id = op_id.unwrap_or_else(Uuid::new_v4);
    if op_id.get_version_num() != 4 || op_id.get_variant() != Variant::RFC4122 {
        return Err(WriteFailure::local("--op-id must be a UUIDv4"));
    }
    if path
        .iter()
        .any(|segment| matches!(*segment, "" | "." | ".."))
    {
        return Err(WriteFailure::local("invalid server request path"));
    }
    let request = PreparedRequest {
        method: RequestMethod::Post,
        path: path.iter().map(|part| (*part).to_owned()).collect(),
        body: body.to_string(),
        if_match: None,
    };
    let lock = journal.lock_operation(op_id)?;
    let operation = journal.prepare_v2_operation(
        op_id,
        session.unwrap_or_else(Uuid::nil),
        operation.to_owned(),
        json!({"method":"POST","path":path,"body":body}),
        Vec::new(),
    )?;
    let operation = match operation.state {
        OperationState::Prepared | OperationState::Unknown => {
            journal.begin_v2_write(&lock, request)?
        }
        OperationState::Committed | OperationState::Rejected => operation,
    };
    deliver(client, journal, operation)
}

pub fn retry(
    client: &AgentClient,
    journal: &mut JournalStore,
    op_id: Uuid,
    expected_journal_id: Option<&str>,
) -> Result<Value, WriteFailure> {
    let _lock = journal.lock_operation(op_id)?;
    let journal_id = journal.journal_id()?;
    if expected_journal_id.is_some_and(|id| id != journal_id) {
        return Err(operation_unresolved(
            op_id,
            &journal_id,
            "journal identity changed",
        ));
    }
    let Some(operation) = journal.operation(op_id)? else {
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
    deliver(client, journal, operation)
}

fn operation_unresolved(op_id: Uuid, journal_id: &str, detail: &str) -> WriteFailure {
    WriteFailure {
        exit_code: 7,
        output: Some(json!({"kind":"operation_unresolved","op_id":op_id,"journal_id":journal_id})),
        detail: detail.into(),
    }
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

fn deliver(
    client: &AgentClient,
    journal: &mut JournalStore,
    operation: OperationRecord,
) -> Result<Value, WriteFailure> {
    let key = operation.op_id;
    if !operation.protocol_v2 {
        return Err(WriteFailure::local(
            "operation belongs to the earlier protocol",
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
        client.delete(&path, key).map(|()| json!({}))
    } else {
        client.write(
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
        Ok(response)
            if method == Method::DELETE
                || response["rev"].as_u64().is_some()
                || (operation.kind == "show"
                    && response["show_id"].is_string()
                    && response["device_id"].is_string()
                    && response["device_name"].is_string()
                    && matches!(response["state"].as_str(), Some("sent" | "held"))) =>
        {
            journal
                .settle_operation(key, response.clone(), None)
                .map_err(|error| unknown(&operation, &error.to_string(), None))?;
            Ok(committed(&operation, response))
        }
        Ok(_) => Err(unknown(
            &operation,
            "server response lacked a position",
            None,
        )),
        Err(error) => {
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
                        "kind":"refused","operation":operation.kind,"op_id":key,
                        "status":status,"error":token,
                    });
                    if let Some(field) = field {
                        output["field"] = field.as_str().into();
                    }
                    if let Some(request_id) = request_id {
                        output["request_id"] = request_id.as_str().into();
                    }
                    journal.reject_operation(key, output.clone())?;
                    return Err(WriteFailure::from_saved_refusal(output));
                }
                let mut failure = unknown(&operation, &error.to_string(), retry_after);
                if let Some(output) = failure.output.as_mut() {
                    output["status"] = status.into();
                    if let Some(id) = request_id {
                        output["request_id"] = id.as_str().into();
                    }
                }
                return Err(failure);
            }
            if error.outcome_unknown() {
                return Err(unknown(&operation, &error.to_string(), None));
            }
            Err(WriteFailure::local(error.to_string()))
        }
    }
}

fn committed(operation: &OperationRecord, response: Value) -> Value {
    let mut output = json!({
        "kind":"committed","operation":operation.kind,"op_id":operation.op_id,"response":response,
    });
    if !operation.session_handle.is_nil() {
        output["session_handle"] = operation.session_handle.to_string().into();
    }
    output
}

fn unknown(operation: &OperationRecord, detail: &str, retry_after: Option<u64>) -> WriteFailure {
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
            let server = SlowServer::answer_after(
                Duration::ZERO,
                r#"{"rev":3,"status":"recorded","stage_outcome":"handed"}"#,
            );
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
            json!({"file_id":"file/1","outcome":"unusable","reason":"wrong format"})
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
}
