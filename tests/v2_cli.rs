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
fn feed_refuses_missing_or_conflicting_scope_before_io() {
    let root = std::env::temp_dir().join(Uuid::new_v4().to_string());
    let session = Uuid::new_v4().to_string();
    for command in [
        &["read"][..],
        &["listen"][..],
        &["listen", "--next"][..],
        &["listen", "--follow"][..],
    ] {
        for start in [&[][..], &["--snapshot"][..], &["--after", "5"][..]] {
            for scope in [&[][..], &["--all-sessions", "--session", &session][..]] {
                let output = Command::new(env!("CARGO_BIN_EXE_harklight-agent"))
                    .arg("--identity-dir")
                    .arg(root.join("identity"))
                    .arg("--ca-file")
                    .arg(root.join("ca.pem"))
                    .arg("--state-dir")
                    .arg(root.join("state"))
                    .args(command)
                    .args(start)
                    .args(scope)
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(2), "{output:?}");
                assert!(output.stdout.is_empty(), "{output:?}");
                assert_eq!(
                    String::from_utf8(output.stderr).unwrap().trim_end(),
                    "Choose exactly one of --session UUID or --all-sessions. --all-sessions takes every session's waiting work."
                );
                assert!(!root.exists());
            }
        }
    }
}

#[test]
fn shared_identity_reads_pending_replies_separately_by_session() {
    use harklight_agent::journal::JournalStore;
    use test_tls::FixtureResponse;

    let one = Uuid::new_v4().to_string();
    let two = Uuid::new_v4().to_string();
    let responses = [(&one, "reply-one"), (&two, "reply-two")]
        .into_iter()
        .flat_map(|(session, entry)| {
            [
                FixtureResponse::json(json!({"rev":10,"next_page":entry})),
                FixtureResponse::json(json!({"rev":10,"changes":[{
                    "kind":"entry","rev":7,"data":{
                        "entry_id":entry,"item_id":"item","rev":7,
                        "session":session,"author":"person","kind":"reply"
                    }
                }]})),
                FixtureResponse::json(json!({"rev":10,"changes":[],"more":false})),
            ]
        })
        .collect();
    let (server, identity) = SlowServer::mutual_tls_sequence(responses);
    let fixture = CliFixture::new(&server, identity);
    let journal = JournalStore::open(&fixture.0.join("state"), "fixture", Uuid::nil()).unwrap();
    for (session, entry, other) in [(&one, "reply-one", &two), (&two, "reply-two", &one)] {
        let output = fixture.run(&["read", "--session", session]);
        assert!(output.status.success(), "{output:?}");
        let entries: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["kind"] == "entry")
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["data"]["entry_id"], entry);
        assert_eq!(journal.v2_feed_cursor(session).unwrap(), Some(10));
        if session == &one {
            assert_eq!(journal.v2_feed_cursor(other).unwrap(), None);
        }
    }
    assert_eq!(journal.v2_feed_cursor("").unwrap(), None);
    for (requests, session) in server.requests().chunks(3).zip([one, two]) {
        for request in requests {
            assert!(request.line.contains(&format!("session={session}")));
        }
    }
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
fn ack2_opt_in_covers_the_baseline_json_and_sse_delivery_requests() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse::json(json!({"rev":5,"next_page":"attempt"})),
        FixtureResponse::json(json!({"rev":5,"changes":[]})),
        FixtureResponse::json(json!({"rev":5,"changes":[],"more":false})),
        FixtureResponse {
            status: "200 OK",
            headers: &[
                ("Content-Type", "text/event-stream"),
                ("Harklight-Ack2", "true"),
            ],
            body: concat!(
                "id: 6\nevent: entry\n",
                r#"data: {"item_id":"item","entry_id":"new","rev":6}"#,
                "\n\n",
            )
            .into(),
            truncate: false,
        },
        FixtureResponse::json(json!({"rev":6,"messages":[]})),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let output = fixture.run(&["listen", "--next", "--all-sessions", "--ack2"]);
    assert!(output.status.success(), "{output:?}");
    let rows: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.last().unwrap()["data"]["entry_id"], "new");
    assert!(fixture.run(&["messages"]).status.success());
    let requests = server.requests();
    for request in &requests[..4] {
        assert!(
            request
                .headers
                .contains(&("harklight-ack2".into(), "true".into()))
        );
    }
    assert!(
        !requests[4]
            .headers
            .iter()
            .any(|(name, _)| name == "harklight-ack2")
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
fn wrong_port_http_error_keeps_a_staged_upload_retryable() {
    use harklight_agent::journal::{JournalStore, OperationState};
    use test_tls::FixtureResponse;

    let bytes = b"synthetic attachment";
    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse::json(json!({"limits":{"upload_cap_bytes_per_file":"none"}})),
        wrong_port_response(),
        FixtureResponse::json(json!({
            "file_id":"uploaded","name":"fixture.txt","size":bytes.len(),
            "sha256":hex::encode(Sha256::digest(bytes)),
        })),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let file = fixture.0.join("fixture.txt");
    fs::write(&file, bytes).unwrap();
    let key = Uuid::new_v4();
    let operation = key.to_string();
    let output = fixture.run(&[
        "file-upload",
        "--file",
        file.to_str().unwrap(),
        "--op-id",
        &operation,
    ]);
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["kind"], "outcome_unknown");
    assert_eq!(result["status"], 400);
    assert_eq!(result["op_id"], operation);
    assert!(String::from_utf8_lossy(&output.stderr).contains("address or device port"));

    let journal = JournalStore::open(&fixture.0.join("state"), "fixture", Uuid::nil()).unwrap();
    let saved = journal.operation(key).unwrap().unwrap();
    assert_eq!(saved.state, OperationState::Unknown);
    let staged = saved.uploads[0].staged_path.as_ref().unwrap();
    assert_eq!(fs::read(staged).unwrap(), bytes);
    fs::remove_file(file).unwrap();

    let retry = fixture.run(&["retry", "--op-id", &operation]);
    assert!(retry.status.success(), "{retry:?}");
    assert_eq!(
        journal.operation(key).unwrap().unwrap().state,
        OperationState::Committed
    );
    assert!(!staged.exists());
    let requests = server.requests();
    assert_eq!(requests[0].line.trim_end(), "GET /v2/me HTTP/1.1");
    for request in &requests[1..] {
        assert_eq!(request.line.trim_end(), "POST /v2/files HTTP/1.1");
        assert!(
            request
                .headers
                .contains(&("idempotency-key".into(), operation.clone()))
        );
        assert_eq!(request.body, bytes);
    }
}

#[test]
fn wrong_port_http_error_is_a_read_failure_in_json_and_sse() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        wrong_port_response(),
        wrong_port_response(),
        FixtureResponse::json(json!({"rev":5,"changes":[],"more":false})),
        wrong_port_response(),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let session = Uuid::new_v4().to_string();
    for args in [
        &["messages"][..],
        &["read", "--all-sessions", "--after", "5"][..],
        &["listen", "--next", "--session", &session, "--after", "5"][..],
    ] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("address or device port"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("refused"));
    }
    assert!(
        server.requests()[3]
            .headers
            .contains(&("accept".into(), "text/event-stream".into()))
    );
}

fn wrong_port_response() -> test_tls::FixtureResponse {
    test_tls::FixtureResponse {
        status: "400 Bad Request",
        headers: &[("Content-Type", "text/plain")],
        body: "Bad Request".into(),
        truncate: false,
    }
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
fn single_shot_internal_feed_errors_keep_their_exit_codes_and_cursor() {
    use harklight_agent::journal::JournalStore;
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        internal_error_response(),
        FixtureResponse::json(json!({"rev":10,"changes":[],"more":false})),
        FixtureResponse {
            status: "200 OK",
            headers: &[
                ("Content-Type", "text/event-stream"),
                ("Request-Id", "stream-fault"),
            ],
            body: concat!("event: closing\ndata: {\"reason\":\"internal_error\"}\n\n").into(),
            truncate: false,
        },
        FixtureResponse::json(json!({"rev":13,"changes":[],"more":false})),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let journal = JournalStore::open(&fixture.0.join("state"), "fixture", Uuid::nil()).unwrap();
    journal.record_v2_feed("", None, 10, &[], false).unwrap();

    let batch = fixture.run(&["read", "--all-sessions"]);
    assert_eq!(batch.status.code(), Some(1), "{batch:?}");
    assert!(batch.stdout.is_empty());
    let diagnostic = String::from_utf8(batch.stderr).unwrap();
    for detail in ["read", "internal_error", "boundary-fault"] {
        assert!(diagnostic.contains(detail), "{diagnostic}");
    }
    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(10));

    let stream = fixture.run(&["listen", "--next", "--all-sessions"]);
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
    assert_eq!(rows, [json!({"kind":"caught_up","rev":10})]);
    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(10));
    assert!(fixture.run(&["read", "--all-sessions"]).status.success());
    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(13));

    let requests = server.requests();
    assert!(requests[0].line.contains("after=10"));
    assert!(requests[1].line.contains("after=10"));
    assert!(
        requests[2]
            .headers
            .contains(&("last-event-id".into(), "10".into()))
    );
    assert!(requests[3].line.contains("after=10"));
}

#[test]
fn follow_retries_bad_responses_from_its_last_good_position() {
    use harklight_agent::journal::JournalStore;
    use test_tls::FixtureResponse;

    let batch = || FixtureResponse::json(json!({"rev":10,"changes":[],"more":false}));
    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse {
            status: "400 Bad Request",
            headers: &[("Content-Type", "text/plain")],
            body: "Bad Request".into(),
            truncate: false,
        },
        internal_error_response(),
        FixtureResponse {
            headers: &[("Request-Id", "another-fault")],
            ..internal_error_response()
        },
        batch(),
        FixtureResponse {
            status: "200 OK",
            headers: &[("Content-Type", "text/event-stream")],
            body: concat!(
                "id: 12\nevent: entry\ndata: {\"entry_id\":\"one\",\"item_id\":\"item\",\"rev\":12}\n\n",
                "id: 99\nevent: entry\ndata: {\"unexpected\":true}\n\n",
            ).into(),
            truncate: false,
        },
        FixtureResponse {
            status: "401 Unauthorized",
            headers: &[],
            body: r#"{"error":"unauthorized","retryable":false,"request_id":"stop"}"#.into(),
            truncate: false,
        },
    ]);
    let fixture = CliFixture::new(&server, identity);
    let journal = JournalStore::open(&fixture.0.join("state"), "fixture", Uuid::nil()).unwrap();
    journal.record_v2_feed("", None, 10, &[], false).unwrap();

    let result = fixture.run(&["listen", "--follow", "--all-sessions"]);
    assert_eq!(result.status.code(), Some(2), "{result:?}");
    let diagnostic = String::from_utf8(result.stderr).unwrap();
    for detail in [
        "HTTP 400",
        "internal_error",
        "boundary-fault",
        "live updates resumed",
        "unauthorized",
    ] {
        assert_eq!(diagnostic.matches(detail).count(), 1, "{diagnostic}");
    }
    assert!(!diagnostic.contains("another-fault"), "{diagnostic}");
    let rows: Vec<Value> = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["data"]["entry_id"], "one");
    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(12));

    let requests = server.requests();
    assert!(
        requests[..4]
            .iter()
            .all(|request| request.line.contains("after=10"))
    );
    assert!(
        requests[4]
            .headers
            .contains(&("last-event-id".into(), "10".into()))
    );
    assert!(requests[5].line.contains("after=12"));
}

#[test]
fn follow_saves_receipt_only_revisions_without_reprinting_the_reply() {
    use harklight_agent::journal::JournalStore;
    use test_tls::FixtureResponse;

    let first = json!({
        "entry_id":"one","item_id":"item","kind":"reply","text":"hello","rev":11,
        "delivery":{"delivered":false,"acted":false},
    });
    let mut delivered = first.clone();
    delivered["rev"] = 12.into();
    delivered["delivery"]["delivered"] = true.into();
    let mut acted = delivered.clone();
    acted["rev"] = 13.into();
    acted["delivery"]["acted"] = true.into();
    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse::json(json!({
            "rev":11,"changes":[{"rev":11,"kind":"entry","data":first}],"more":false,
        })),
        FixtureResponse {
            status: "200 OK",
            headers: &[("Content-Type", "text/event-stream")],
            body: format!(
                "id: 12\nevent: entry\ndata: {delivered}\n\n\
                 id: 13\nevent: entry\ndata: {acted}\n\n\
                 event: closing\ndata: {{\"reason\":\"session_closed\"}}\n\n"
            ),
            truncate: false,
        },
    ]);
    let fixture = CliFixture::new(&server, identity);
    let journal = JournalStore::open(&fixture.0.join("state"), "fixture", Uuid::nil()).unwrap();
    journal.record_v2_feed("", None, 10, &[], false).unwrap();

    let result = fixture.run(&["listen", "--follow", "--all-sessions"]);
    assert_eq!(result.status.code(), Some(2), "{result:?}");
    let rows: Vec<Value> = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|row: &Value| row["kind"] == "entry")
        .collect();
    assert_eq!(
        rows,
        [
            json!({"kind":"entry","rev":11,"data":first}),
            json!({"kind":"entry","rev":13,"data":acted}),
        ]
    );
    assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(13));
    let requests = server.requests();
    assert!(
        requests[1]
            .headers
            .contains(&("last-event-id".into(), "11".into()))
    );
}

#[test]
fn follow_stops_on_definite_api_refusals() {
    use test_tls::FixtureResponse;

    for (status, token) in [
        ("401 Unauthorized", "unauthorized"),
        ("403 Forbidden", "forbidden"),
        ("403 Forbidden", "account_paused"),
        ("404 Not Found", "not_found"),
    ] {
        let (server, identity) = SlowServer::mutual_tls_sequence(vec![FixtureResponse {
            status,
            headers: &[],
            body: json!({"error":token,"retryable":false,"request_id":"refusal"}).to_string(),
            truncate: false,
        }]);
        let fixture = CliFixture::new(&server, identity);
        let session = Uuid::new_v4().to_string();
        let result = fixture.run(&["listen", "--follow", "--session", &session, "--after", "10"]);
        assert_eq!(result.status.code(), Some(2), "{result:?}");
        assert!(String::from_utf8_lossy(&result.stderr).contains(token));
        assert_eq!(server.requests().len(), 1);
    }
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

#[test]
fn enroll_accepts_v2_invitation_and_saves_its_device_endpoint() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use test_tls::FixtureResponse;
    let server = SlowServer::sequence(vec![
        FixtureResponse {
            status: "200 OK",
            headers: &[],
            body: String::new(),
            truncate: true,
        },
        FixtureResponse {
            status: "503 Service Unavailable",
            headers: &[],
            body: r#"{"error":"capacity"}"#.into(),
            truncate: false,
        },
        FixtureResponse::json(json!({"state":"pending","approval_code":"ABCDEFG234"})),
        FixtureResponse::json(
            json!({"state":"approved","person_id":"person","device_id":"device"}),
        ),
    ]);
    let device_port = 24680;
    let mut invitation = join_fixture(server.port, 2, &format!("https://localhost:{device_port}"));
    let der = invitation_ca_der(&server.ca_pem);
    invitation["ca_pem"] = server.ca_pem.clone().into();
    invitation["ca_sha256"] = hex::encode(Sha256::digest(der)).into();
    let root = std::env::temp_dir().join(format!("agent-invitation-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let fixture = CliFixture(root);
    let identity = fixture.0.join("new-identity");
    let path = fixture.0.join("invite");
    fs::write(
        &path,
        format!(
            "harklight://join#{}",
            URL_SAFE_NO_PAD.encode(invitation.to_string())
        ),
    )
    .unwrap();
    let start = enroll_invitation_cli(
        &identity,
        &["start", "--invitation", path.to_str().unwrap()],
    );
    assert_eq!(start.status.code(), Some(7), "{start:?}");
    fs::remove_file(&path).unwrap();
    let unavailable = enroll_invitation_cli(&identity, &["retry"]);
    assert!(
        identity.join("invitation-enrollment.json").is_file(),
        "{unavailable:?}"
    );
    assert_eq!(unavailable.status.code(), Some(7), "{unavailable:?}");
    let retry = enroll_invitation_cli(&identity, &["retry"]);
    assert!(retry.status.success(), "{retry:?}");
    let saved: Value =
        serde_json::from_slice(&fs::read(identity.join("server.json")).unwrap()).unwrap();
    assert_eq!(saved["port"], server.port);
    assert_eq!(saved["device_port"], device_port);
    assert_eq!(
        saved["device_url"],
        format!("https://localhost:{device_port}")
    );
    let approved = enroll_invitation_cli(&identity, &["status"]);
    assert!(approved.status.success(), "{approved:?}");
    let enrolled = harklight_agent::config::EnrolledIdentity::load(
        &identity,
        &identity.join("invitation-ca.pem"),
    )
    .unwrap();
    assert_eq!(enrolled.port, device_port);
    assert_eq!(
        enrolled.device_url("/v2/me").unwrap().as_str(),
        format!("https://localhost:{device_port}/v2/me")
    );
    let requests = server.requests();
    assert_eq!(requests[0].line.trim_end(), "POST /enroll HTTP/1.1");
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(first["invite_token"], "synthetic-invite");
    assert!(
        first["public_certificate"]
            .as_str()
            .unwrap()
            .contains("BEGIN CERTIFICATE")
    );
    assert_eq!(first["status_secret"].as_str().unwrap().len(), 64);
    assert_eq!(requests[0].line, requests[1].line);
    assert_eq!(requests[0].body, requests[1].body);
    assert_eq!(requests[0].line, requests[2].line);
    assert_eq!(requests[0].body, requests[2].body);
    assert_eq!(requests[3].line.trim_end(), "POST /enroll/status HTTP/1.1");
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[3].body).unwrap(),
        json!({"status_secret":first["status_secret"]})
    );
}

#[test]
fn enroll_refuses_v1_and_malformed_device_urls_before_creating_identity() {
    let root = std::env::temp_dir().join(format!("agent-invalid-invitation-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let fixture = CliFixture(root);
    let identity = fixture.0.join("new-identity");
    let path = fixture.0.join("invite");
    let mut version_one = join_fixture(12345, 1, "https://localhost:12346");
    version_one.as_object_mut().unwrap().remove("device_url");
    version_one["relay_url"] = "wss://localhost:12346".into();
    let mut cases = vec![(version_one, "ask for a new one")];
    for device_url in [
        "http://localhost:12346",
        "https://localhost",
        "https://localhost:12346/v2",
        "https://localhost:0",
    ] {
        cases.push((join_fixture(12345, 2, device_url), "device_url"));
    }
    for (document, expected) in cases {
        fs::write(&path, document.to_string()).unwrap();
        let result = enroll_invitation_cli(
            &identity,
            &["start", "--invitation", path.to_str().unwrap()],
        );
        assert!(!result.status.success(), "{result:?}");
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(expected),
            "{result:?}"
        );
        assert!(!identity.exists());
    }
}

#[test]
fn enroll_removes_unaccepted_identity_after_definite_invitation_refusal() {
    use test_tls::FixtureResponse;

    let root = std::env::temp_dir().join(format!("agent-spent-invitation-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let fixture = CliFixture(root);
    let identity = fixture.0.join("new-identity");
    let path = fixture.0.join("invite");
    for (status, error, diagnostic) in [
        ("409 Conflict", "used", "invitation already used"),
        ("404 Not Found", "refused", "ask for a new one"),
    ] {
        let server = SlowServer::sequence(vec![FixtureResponse {
            status,
            headers: &[],
            body: json!({"error": error}).to_string(),
            truncate: false,
        }]);
        let mut invitation = join_fixture(server.port, 2, "https://localhost:24680");
        invitation["ca_pem"] = server.ca_pem.clone().into();
        invitation["ca_sha256"] =
            hex::encode(Sha256::digest(invitation_ca_der(&server.ca_pem))).into();
        fs::write(&path, invitation.to_string()).unwrap();
        let result = enroll_invitation_cli(
            &identity,
            &["start", "--invitation", path.to_str().unwrap()],
        );
        assert_eq!(result.status.code(), Some(2), "{result:?}");
        assert!(result.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(diagnostic),
            "{result:?}"
        );
        assert!(!identity.exists());
        assert_eq!(server.requests().len(), 1);
    }
}

fn join_fixture(enrollment_port: u16, version: u32, device_url: &str) -> Value {
    json!({"v":version,"person_id":"person","server_name":"Fixture",
        "enrollment_url":format!("https://localhost:{enrollment_port}/enroll"),"device_url":device_url,
        "ca_pem":"","ca_sha256":"","invite_token":"synthetic-invite","expires_at":null})
}

fn invitation_ca_der(pem: &str) -> Vec<u8> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    STANDARD.decode(body).unwrap()
}

fn enroll_invitation_cli(identity: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_harklight-agent"))
        .env_remove("HARKLIGHT_CA_FILE")
        .env_remove("HARKLIGHT_DEVICE_PORT")
        .arg("--identity-dir")
        .arg(identity)
        .arg("enroll")
        .args(args)
        .output()
        .unwrap()
}
