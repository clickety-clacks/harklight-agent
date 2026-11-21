// The fixture also serves the library's wire-level tests.
#[allow(dead_code)]
#[path = "../src/test_tls.rs"]
mod test_tls;

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use test_tls::{SlowServer, TLS_NAME, TestIdentity};

struct CliFixture(PathBuf);

impl CliFixture {
    fn new(server: &SlowServer, identity: TestIdentity) -> Self {
        let root = std::env::temp_dir().join(format!("harklight-cli-fixture-{}", Uuid::new_v4()));
        let enrollment = root.join("identity");
        fs::create_dir_all(&enrollment).unwrap();
        let ca = server.ca_pem.as_bytes();
        fs::write(root.join("ca.pem"), ca).unwrap();
        fs::write(enrollment.join("identity.pem"), identity.certificate).unwrap();
        fs::write(enrollment.join("identity.key"), identity.key).unwrap();
        fs::write(
            enrollment.join("enrollment.json"),
            r#"{"device_id":"fixture"}"#,
        )
        .unwrap();
        fs::write(
            enrollment.join("server.json"),
            json!({
                "address":"127.0.0.1",
                "name":TLS_NAME,
                "port":1,
                "device_port":server.port,
                "ca_sha256":hex::encode(Sha256::digest(ca)),
            })
            .to_string(),
        )
        .unwrap();
        Self(root)
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_harklight-agent"))
            .arg("--identity-dir")
            .arg(self.0.join("identity"))
            .arg("--ca-file")
            .arg(self.0.join("ca.pem"))
            .arg("--state-dir")
            .arg(self.0.join("state"))
            .args(args)
            .output()
            .unwrap()
    }
}

impl Drop for CliFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn binary_uses_the_device_listener_for_the_historical_message_selector() {
    let (server, identity) = SlowServer::mutual_tls(
        r#"{"rev":12,"revision":{"request_revision":2,"message":{"title":"Old","created_at":10},"files":[]}}"#,
    );
    let fixture = CliFixture::new(&server, identity);
    let output = fixture.run(&["message-get", "--item-id", "opaque/item", "--revision", "2"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["revision"]["request_revision"], 2);
    assert_eq!(
        server.request_line().trim_end(),
        "GET /v2/agent/messages/opaque%2Fitem/revisions/2 HTTP/1.1"
    );
}

#[test]
fn binary_session_read_emits_an_empty_caught_up_boundary_from_caller_cursor() {
    let (server, identity) = SlowServer::mutual_tls(r#"{"rev":12,"changes":[],"more":false}"#);
    let fixture = CliFixture::new(&server, identity);
    let session = Uuid::new_v4();
    let output = fixture.run(&["read", "--session", &session.to_string(), "--after", "5"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result, json!({"kind":"caught_up","rev":12}));
    assert_eq!(
        server.request_line().trim_end(),
        format!("GET /v2/agent/changes?after=5&session={session} HTTP/1.1")
    );
}

#[test]
fn binary_progress_preserves_attachments_by_omitting_file_ids_under_if_match() {
    use test_tls::FixtureResponse;

    let session = Uuid::new_v4().to_string();
    let operation = Uuid::new_v4().to_string();
    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse::json(json!({"rev":8,"message":{
            "item_id":"item/opaque","revision":3,"rev":7,"session":session,"sender":"app:fixture",
            "seen":false,"files":[{"file_id":"read-only-id","name":"a","size":1,"sha256":"a".repeat(64)}],
            "message":{"title":"Progress","created_at":10,"message_type":"fyi",
                "progress":{"percent":10,"segments":[{"stage":"Build","width":10}]}}
        }})),
        FixtureResponse::json(
            json!({"rev":9,"message":{"item_id":"item/opaque","revision":4,"sender":"app:fixture","rev":9,"message":{},"seen":false,"files":[]}}),
        ),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let output = fixture.run(&[
        "progress",
        "--session",
        &session,
        "--request",
        "item/opaque",
        "--percent",
        "20",
        "--stage",
        "Verify",
        "--op-id",
        &operation,
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.requests();
    assert_eq!(
        requests[0].line.trim_end(),
        "GET /v2/agent/messages/item%2Fopaque HTTP/1.1"
    );
    assert_eq!(
        requests[1].line.trim_end(),
        "PUT /v2/agent/messages/item%2Fopaque HTTP/1.1"
    );
    assert!(
        requests[1]
            .headers
            .contains(&("if-match".into(), "3".into()))
    );
    assert!(
        requests[1]
            .headers
            .contains(&("idempotency-key".into(), operation))
    );
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert!(body.get("file_ids").is_none());
    assert_eq!(body["message"]["progress"]["percent"], 20);
}

#[test]
fn binary_device_port_overrides_an_existing_enrollment_port() {
    let (server, identity) = SlowServer::mutual_tls(r#"{"rev":0,"messages":[]}"#);
    let fixture = CliFixture::new(&server, identity);
    let path = fixture.0.join("identity/server.json");
    let mut server_config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    server_config.as_object_mut().unwrap().remove("device_port");
    fs::write(path, server_config.to_string()).unwrap();

    let output = fixture.run(&["--device-port", &server.port.to_string(), "messages"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        server.request_line().trim_end(),
        "GET /v2/agent/messages HTTP/1.1"
    );
}

#[test]
fn general_progress_revision_does_not_prevent_journal_reopen() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![FixtureResponse::json(json!({
        "rev":2,"message":{"item_id":"item","revision":2,"rev":2,"sender":"app:test","seen":false,
         "files":[],"message":{"title":"Progress","created_at":10,"message_type":"fyi","progress":{}}}
    }))]);
    let fixture = CliFixture::new(&server, identity);
    let draft = fixture.0.join("revision.json");
    fs::write(
        &draft,
        r#"{"title":"Progress","created_at":10,"message_type":"fyi","progress":{}}"#,
    )
    .unwrap();
    let first = fixture.run(&[
        "revise",
        "--item-id",
        "item",
        "--base-revision",
        "1",
        "--message-json",
        draft.to_str().unwrap(),
    ]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = fixture.run(&["journal-id"]);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn explicit_legacy_progress_miss_does_not_fall_through_to_current_id() {
    let (server, identity) = SlowServer::mutual_tls(r#"{"rev":2,"messages":[]}"#);
    let fixture = CliFixture::new(&server, identity);
    let session = Uuid::new_v4().to_string();
    let output = fixture.run(&[
        "progress",
        "--session",
        &session,
        "--request",
        "opaque",
        "--sender-id",
        "app:other",
        "--percent",
        "25",
        "--stage",
        "Verify",
    ]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "not_found");
    assert_eq!(
        server.request_line().trim_end(),
        "GET /v2/agent/messages?sender=app%3Aother&legacy_message_id=opaque HTTP/1.1"
    );
}

#[test]
fn journal_known_legacy_progress_miss_does_not_fall_through_to_current_id() {
    use harklight_agent::journal::{JournalEntry, JournalStore};

    let session = Uuid::new_v4();
    let (server, identity) = SlowServer::mutual_tls(r#"{"rev":2,"messages":[]}"#);
    let fixture = CliFixture::new(&server, identity);
    let mut journal = JournalStore::open(&fixture.0.join("state"), "fixture", session).unwrap();
    journal
        .record_entry(JournalEntry::fetched(
            1,
            "old-event".into(),
            session,
            "reply".into(),
            "person".into(),
            true,
            Some("app:test".into()),
            Some("opaque".into()),
            Some(1),
            json!({}),
        ))
        .unwrap();

    let output = fixture.run(&[
        "progress",
        "--session",
        &session.to_string(),
        "--request",
        "opaque",
        "--percent",
        "25",
        "--stage",
        "Verify",
    ]);
    assert_eq!(output.status.code(), Some(2), "{:?}", output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "not_found");
    assert_eq!(
        server.request_line().trim_end(),
        "GET /v2/agent/messages?sender=app%3Atest&legacy_message_id=opaque HTTP/1.1"
    );
}

#[test]
fn malformed_or_mismatched_write_responses_keep_the_saved_operation() {
    use harklight_agent::journal::{JournalStore, OperationState};
    use test_tls::FixtureResponse;

    let reply = [
        "reply",
        "--item-id",
        "item",
        "--body",
        "Expected",
        "--in-reply-to",
        "original",
    ];
    let wrong_reply = |field: &str, value: &str| {
        let mut entry = json!({
            "entry_id":"new","item_id":"item","rev":4,"position":1,"request_revision":1,
            "author":"origin","kind":"reply","text":"Expected","in_reply_to":"original",
        });
        entry[field] = value.into();
        json!({"rev":4,"entry":entry})
    };
    let cases: &[(&[&str], Value)] = &[
        (
            &[
                "file-use",
                "--item-id",
                "item",
                "--entry-id",
                "entry",
                "--file-id",
                "file",
                "--used",
            ],
            json!({"rev":4}),
        ),
        (
            &[
                "advisory-ack",
                "--item-id",
                "item",
                "--advisory-id",
                "pending",
            ],
            json!({"rev":4}),
        ),
        (&reply, wrong_reply("author", "person")),
        (&reply, wrong_reply("text", "Different")),
        (&reply, wrong_reply("in_reply_to", "wrong")),
        (
            &["show", "--item-id", "item", "--device-id", "wanted"],
            json!({"show_id":"show","device_id":"different","device_name":"Example","state":"held"}),
        ),
    ];
    for (command, response) in cases {
        let (server, identity) = SlowServer::mutual_tls_sequence(vec![FixtureResponse {
            status: "200 OK",
            headers: &[("Request-Id", "uncorrelated-response")],
            body: response.to_string(),
            truncate: false,
        }]);
        let fixture = CliFixture::new(&server, identity);
        let op_id = Uuid::new_v4().to_string();
        let mut args = command.to_vec();
        args.extend(["--op-id", &op_id]);
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(7), "{args:?}: {output:?}");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["kind"], "outcome_unknown");
        assert_eq!(result["op_id"], op_id);
        assert_eq!(result["request_id"], "uncorrelated-response");
        let journal = JournalStore::open(&fixture.0.join("state"), "fixture", Uuid::nil()).unwrap();
        let saved = journal
            .operation(Uuid::parse_str(&op_id).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(saved.state, OperationState::Unknown);
        assert_eq!(server.requests().len(), 1);
    }
}

#[test]
fn internal_feed_error_pauses_without_reconnect_and_keeps_cursor() {
    use harklight_agent::journal::JournalStore;
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse::json(json!({"rev":10,"messages":[]})),
        FixtureResponse::json(json!({"rev":10,"changes":[],"more":false})),
        internal_error_response(),
        FixtureResponse::json(json!({"rev":10,"changes":[],"more":false})),
        FixtureResponse {
            status: "200 OK",
            headers: &[("Content-Type", "text/event-stream"), ("Request-Id", "stream-fault")],
            body: concat!(
                "id: 12\nevent: entry\ndata: {\"entry_id\":\"one\",\"item_id\":\"item\",\"rev\":12}\n\n",
                "event: closing\ndata: {\"reason\":\"internal_error\"}\n\n"
            ).into(),
            truncate: false,
        },
        FixtureResponse::json(json!({"rev":13,"changes":[],"more":false})),
    ]);
    let fixture = CliFixture::new(&server, identity);
    assert!(fixture.run(&["read"]).status.success());
    let journal = JournalStore::open(&fixture.0.join("state"), "fixture", Uuid::nil()).unwrap();
    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(10));

    let batch = fixture.run(&["listen", "--follow"]);
    assert_eq!(batch.status.code(), Some(1), "{batch:?}");
    assert!(batch.stdout.is_empty());
    let diagnostic = String::from_utf8(batch.stderr).unwrap();
    for detail in [
        "listen",
        "live updates paused",
        "internal_error",
        "boundary-fault",
    ] {
        assert!(diagnostic.contains(detail), "{diagnostic}");
    }
    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(10));

    let stream = fixture.run(&["listen", "--follow"]);
    assert_eq!(stream.status.code(), Some(1), "{stream:?}");
    let diagnostic = String::from_utf8(stream.stderr).unwrap();
    for detail in [
        "listen",
        "live updates paused",
        "internal_error",
        "stream-fault",
    ] {
        assert!(diagnostic.contains(detail), "{diagnostic}");
    }
    let output = String::from_utf8(stream.stdout).unwrap();
    let rows: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["rev"], 12);
    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(12));
    assert!(fixture.run(&["read"]).status.success());
    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(13));

    let requests = server.requests();
    assert!(requests[2].line.contains("after=10"));
    assert!(requests[3].line.contains("after=10"));
    assert!(
        requests[4]
            .headers
            .contains(&("last-event-id".into(), "10".into()))
    );
    assert!(requests[5].line.contains("after=12"));
}

#[test]
fn internal_write_error_preserves_operation_for_explicit_retry() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        internal_error_response(),
        FixtureResponse::json(json!({"rev":14})),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let key = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    let failed = fixture.run(&["session", "end", "--session", &session, "--op-id", &key]);
    assert_eq!(failed.status.code(), Some(7), "{failed:?}");
    let retry = fixture.run(&["retry", "--op-id", &key]);
    assert!(retry.status.success(), "{retry:?}");
    let output: Value = serde_json::from_slice(&failed.stdout).unwrap();
    assert_eq!(
        output,
        json!({
            "kind":"outcome_unknown","operation":"session-end","op_id":key,
            "status":500,"error":"internal_error","retryable":false,"request_id":"boundary-fault",
        })
    );
    let requests = server.requests();
    assert_eq!(requests[0].line, requests[1].line);
    assert_eq!(requests[0].body, requests[1].body);
    for request in requests {
        assert!(
            request
                .headers
                .contains(&("idempotency-key".into(), key.clone()))
        );
    }
}

fn internal_error_response() -> test_tls::FixtureResponse {
    test_tls::FixtureResponse {
        status: "500 Internal Server Error",
        headers: &[("Request-Id", "boundary-fault")],
        body: r#"{"error":"internal_error","retryable":false,"request_id":"boundary-fault"}"#
            .into(),
        truncate: false,
    }
}
