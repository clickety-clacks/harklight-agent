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
    journal.prepare_operation(
        op_id,
        session.unwrap_or_else(Uuid::nil),
        operation.to_owned(),
        json!({"api":2,"method":"POST","path":path,"body":body}),
    )?;
    let operation = journal.begin_v2_write(op_id, request)?;
    deliver(client, journal, operation)
}

pub fn retry(
    client: &AgentClient,
    journal: &mut JournalStore,
    op_id: Uuid,
) -> Result<Value, WriteFailure> {
    let operation = journal
        .operation(op_id)?
        .ok_or(JournalError::MissingOperation(op_id))?;
    deliver(client, journal, operation)
}

pub fn abandon(journal: &mut JournalStore, op_id: Uuid) -> Result<Value, WriteFailure> {
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
    let request = operation
        .prepared_request
        .as_ref()
        .ok_or_else(|| WriteFailure::local("operation has no saved v2 request"))?;
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

    let path: Vec<_> = request.path.iter().map(String::as_str).collect();
    let method = match request.method {
        RequestMethod::Post => Method::POST,
        RequestMethod::Put => Method::PUT,
        RequestMethod::Delete => Method::DELETE,
    };
    let response = client.write(
        method,
        &path,
        JsonWrite {
            key,
            body: request.body.as_bytes(),
            if_match: request.if_match.as_deref(),
        },
    );
    match response {
        Ok(response) if response["rev"].as_u64().is_some() => {
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
                ref request_id,
                retry_after,
            } = error
            {
                if (400..500).contains(&status) {
                    let mut output = json!({
                        "kind":"refused","operation":operation.kind,"op_id":key,
                        "status":status,"error":token,
                    });
                    if let Some(request_id) = request_id {
                        output["request_id"] = request_id.as_str().into();
                    }
                    journal.reject_operation(key, output.clone())?;
                    return Err(WriteFailure::from_saved_refusal(output));
                }
                return Err(unknown(&operation, &error.to_string(), retry_after));
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
            retry(&client, &mut journal.store, key).unwrap_err().output,
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
        let output = retry(&client_for(&replayed), &mut reopened, key).unwrap();
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
}
