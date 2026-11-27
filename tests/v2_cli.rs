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

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_harklight-agent"));
        command
            .env("HOME", &self.0)
            .env_remove("CODEX_HOME")
            .env_remove("HARKLIGHT_DEVICE_PORT")
            .arg("--identity-dir")
            .arg(self.0.join("identity"))
            .arg("--ca-file")
            .arg(self.0.join("ca.pem"))
            .arg("--state-dir")
            .arg(self.0.join("state"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
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

fn account_search_response(body: Value) -> test_tls::FixtureResponse {
    test_tls::FixtureResponse {
        headers: &[("Harklight-Account-Search", "1")],
        ..test_tls::FixtureResponse::json(body)
    }
}

#[test]
fn binary_searches_the_account_on_the_device_listener() {
    let session = Uuid::new_v4().to_string();
    let (server, identity) = SlowServer::mutual_tls_sequence(vec![account_search_response(
        json!({"rev":3,"items":[],"next_page":"opaque-page"}),
    )]);
    let fixture = CliFixture::new(&server, identity);
    let output = fixture.run(&[
        "search",
        "--session",
        &session,
        "--sender",
        "app:second",
        "--message-id",
        "legacy-id",
        "--state",
        "retracted",
    ]);

    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["next_page"], "opaque-page");

    let line = server.request_line();
    assert!(line.starts_with("GET /v2/search?"), "{line}");
    for filter in [
        format!("session={session}"),
        "sender=app%3Asecond".into(),
        "message_id=legacy-id".into(),
        "state=retracted".into(),
    ] {
        assert!(line.contains(&filter), "{line}");
    }
}

#[test]
fn sender_qualified_message_get_uses_account_search_and_reads_the_item() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        account_search_response(json!({
            "rev":4,
            "items":[{"item_id":"i23","rev":4,"sender":"app:second",
                      "message":{"message_id":"reference"}}]
        })),
        FixtureResponse::json(json!({"rev":4,"item":{"item_id":"i23"}})),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let output = fixture.run(&[
        "message-get",
        "--sender-id",
        "app:second",
        "--message-id",
        "reference",
    ]);

    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["view"], "account");
    assert_eq!(result["item"]["item_id"], "i23");
    let requests = server.requests();
    assert_eq!(
        requests[0].line.trim_end(),
        "GET /v2/search?sender=app%3Asecond&message_id=reference HTTP/1.1"
    );
    assert_eq!(requests[1].line.trim_end(), "GET /v2/items/i23 HTTP/1.1");
}

#[test]
fn binary_marks_foreign_message_get_as_an_account_view() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse {
            status: "404 Not Found",
            ..FixtureResponse::json(json!({
                "error":"not_found", "retryable":false, "request_id":"fixture-404"
            }))
        },
        FixtureResponse::json(json!({
            "rev":4,
            "item":{
                "item_id":"i23", "rev":4, "sender":"app:second",
                "request_revision":1, "message":{"title":"Other sender"},
                "seen":false, "files":[], "terminal":null
            }
        })),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let output = fixture.run(&["message-get", "--item-id", "i23"]);

    assert!(output.status.success(), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["view"], "account");
    assert_eq!(result["item"]["item_id"], "i23");
    let requests = server.requests();
    assert_eq!(
        requests[0].line.trim_end(),
        "GET /v2/agent/messages/i23 HTTP/1.1"
    );
    assert_eq!(requests[1].line.trim_end(), "GET /v2/items/i23 HTTP/1.1");
}

#[test]
fn binary_refuses_to_retract_an_id_with_matches_across_pages() {
    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        account_search_response(json!({
            "rev":4, "next_page":"another",
            "items":[{"item_id":"i4", "rev":4, "message":{"message_id":"shared"}}]
        })),
        account_search_response(json!({
            "rev":4,
            "items":[{"item_id":"i2", "rev":2, "message":{"message_id":"shared"}}]
        })),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let output = fixture.run(&["retract", "--message-id", "shared"]);

    assert_eq!(output.status.code(), Some(2));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"], "ambiguous_message_id");
    assert!(String::from_utf8_lossy(&output.stderr).contains("matches multiple items"));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .line
            .starts_with("GET /v2/search?message_id=shared ")
    );
    assert!(
        requests[1]
            .line
            .starts_with("GET /v2/search?message_id=shared&page=another ")
    );
}

#[test]
fn account_search_refuses_old_servers_instead_of_accepting_ignored_filters() {
    use test_tls::FixtureResponse;

    for command in [
        &["search", "--message-id", "legacy"][..],
        &["message-get", "--message-id", "legacy"][..],
        &["message-entries", "--message-id", "legacy"][..],
        &["retract", "--message-id", "legacy"][..],
        &[
            "search",
            "--session",
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        ][..],
    ] {
        let response = if command[1] == "--session" {
            FixtureResponse::json(json!({"rev":3,"items":[]}))
        } else {
            FixtureResponse {
                status: "404 Not Found",
                ..FixtureResponse::json(json!({
                    "error":"not_found", "retryable":false, "request_id":"old-server"
                }))
            }
        };
        let (server, identity) = SlowServer::mutual_tls_sequence(vec![response]);
        let fixture = CliFixture::new(&server, identity);
        let output = fixture.run(command);
        assert_eq!(output.status.code(), Some(5), "{output:?}");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["error"], "unsupported");
        assert_eq!(server.requests().len(), 1);
    }
}

#[test]
fn retract_lookup_reports_read_failures_without_sending_a_write() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![FixtureResponse {
        status: "503 Service Unavailable",
        ..account_search_response(json!({
            "error":"retry_later", "retryable":true, "request_id":"lookup-busy"
        }))
    }]);
    let fixture = CliFixture::new(&server, identity);
    let output = fixture.run(&["retract", "--message-id", "legacy"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["kind"], "read_failed");
    assert_eq!(result["error"], "retry_later");
    assert_eq!(result["status"], 503);
    assert_eq!(result["request_id"], "lookup-busy");
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn granted_agent_reads_fall_back_on_a_forbidden_own_view() {
    use test_tls::FixtureResponse;

    for (command, suffix, body) in [
        ("message-get", "", json!({"rev":4,"item":{"item_id":"i23"}})),
        (
            "message-entries",
            "/entries",
            json!({"rev":4,"entries":[],"older":false}),
        ),
    ] {
        let (server, identity) = SlowServer::mutual_tls_sequence(vec![
            FixtureResponse {
                status: "403 Forbidden",
                ..FixtureResponse::json(json!({
                    "error":"forbidden", "retryable":false, "request_id":"granted"
                }))
            },
            FixtureResponse::json(body),
        ]);
        let fixture = CliFixture::new(&server, identity);
        let output = fixture.run(&[command, "--item-id", "i23"]);
        assert!(output.status.success(), "{output:?}");
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["view"], "account");
        assert_eq!(
            server.requests()[1].line.trim_end(),
            format!("GET /v2/items/i23{suffix} HTTP/1.1")
        );
    }
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
            "seen":false,"recipient_reach":"none","files":[{"file_id":"read-only-id","name":"a","size":1,"sha256":"a".repeat(64)}],
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
    assert!(output.stderr.is_empty(), "{output:?}");
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
fn binary_requires_an_explicit_device_port_when_only_the_enrollment_port_is_saved() {
    let (server, identity) = SlowServer::mutual_tls(r#"{"rev":0,"messages":[]}"#);
    let fixture = CliFixture::new(&server, identity);
    let path = fixture.0.join("identity/server.json");
    let mut server_config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    server_config.as_object_mut().unwrap().remove("device_port");
    fs::write(path, server_config.to_string()).unwrap();

    let missing_port = fixture.run(&["messages"]);
    assert_eq!(missing_port.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&missing_port.stderr).contains("provide --device-port"));

    let output = fixture
        .command()
        .env("HARKLIGHT_DEVICE_PORT", server.port.to_string())
        .arg("messages")
        .output()
        .unwrap();
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
fn follow_prints_all_delivered_revisions_in_batch_and_stream() {
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
    for stream_receipt in [false, true] {
        let mut batch = vec![json!({"rev":11,"kind":"entry","data":first})];
        if !stream_receipt {
            batch.push(json!({"rev":12,"kind":"entry","data":delivered}));
        }
        let batch_rev = if stream_receipt { 11 } else { 12 };
        let stream = if stream_receipt {
            format!("id: 12\nevent: entry\ndata: {delivered}\n\n")
        } else {
            String::new()
        };
        let (server, identity) = SlowServer::mutual_tls_sequence(vec![
            FixtureResponse::json(json!({"rev":batch_rev,"changes":batch,"more":false})),
            FixtureResponse {
                status: "200 OK",
                headers: &[("Content-Type", "text/event-stream")],
                body: format!(
                    "{stream}id: 13\nevent: entry\ndata: {acted}\n\n\
                     event: closing\ndata: {{\"reason\":\"revoked\"}}\n\n"
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
                json!({"kind":"entry","rev":12,"data":delivered}),
                json!({"kind":"entry","rev":13,"data":acted}),
            ]
        );
        assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(13));
        let requests = server.requests();
        assert!(
            requests[1]
                .headers
                .contains(&("last-event-id".into(), batch_rev.to_string()))
        );
    }
}

#[test]
fn follow_reconnects_after_an_unrecognized_closing_reason() {
    use harklight_agent::journal::JournalStore;
    use test_tls::FixtureResponse;

    let batch = || FixtureResponse::json(json!({"rev":10,"changes":[],"more":false}));
    let stream = |body: &str| FixtureResponse {
        status: "200 OK",
        headers: &[("Content-Type", "text/event-stream")],
        body: body.into(),
        truncate: false,
    };
    for terminal in ["revoked", "account_paused"] {
        let (server, identity) = SlowServer::mutual_tls_sequence(vec![
            batch(),
            stream("event: closing\ndata: {\"reason\":\"upstream_maintenance\"}\n\n"),
            batch(),
            stream(&format!(
                "id: 11\nevent: entry\ndata: {{\"entry_id\":\"one\",\"rev\":11}}\n\n\
                 event: closing\ndata: {{\"reason\":\"{terminal}\"}}\n\n"
            )),
        ]);
        let fixture = CliFixture::new(&server, identity);
        let journal = JournalStore::open(&fixture.0.join("state"), "fixture", Uuid::nil()).unwrap();
        journal.record_v2_feed("", None, 10, &[], false).unwrap();

        let result = fixture.run(&["listen", "--follow", "--all-sessions"]);
        assert_eq!(result.status.code(), Some(2), "{result:?}");
        let diagnostic = String::from_utf8(result.stderr).unwrap();
        for detail in ["upstream_maintenance", "live updates resumed", terminal] {
            assert_eq!(diagnostic.matches(detail).count(), 1, "{diagnostic}");
        }
        assert_eq!(journal.v2_feed_cursor("").unwrap(), Some(11));
        let requests = server.requests();
        assert_eq!(requests.len(), 4);
        for request in requests.iter().step_by(2) {
            assert!(request.line.contains("after=10"), "{}", request.line);
        }
        for request in requests.iter().skip(1).step_by(2) {
            assert!(
                request
                    .headers
                    .contains(&("last-event-id".into(), "10".into()))
            );
        }
    }
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
        .env("HOME", identity.parent().unwrap())
        .env_remove("CODEX_HOME")
        .env_remove("HARKLIGHT_CA_FILE")
        .env_remove("HARKLIGHT_DEVICE_PORT")
        .arg("--identity-dir")
        .arg(identity)
        .arg("enroll")
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn caller_pending_prepares_and_replays_without_agent_state() {
    let session = Uuid::new_v4().to_string();
    let changes = json!([
        {"kind":"entry","rev":11,"data":{"entry_id":"chat","item_id":"closed","rev":11,
            "author":"person","kind":"reply","delivery":{"acted":true}}},
        {"kind":"message","rev":12,"data":{"item_id":"closed","session":session,"rev":12,
            "terminal":{"state":"answered"}}},
    ]);
    let page = |i: usize| {
        test_tls::FixtureResponse::json(if i == 0 {
            json!({"rev":15,"changes":[changes[i]],"next_page":"page 2"})
        } else {
            json!({"rev":15,"changes":[changes[i]]})
        })
    };
    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        test_tls::FixtureResponse::json(json!({"rev":15,"next_page":"token +/"})),
        page(0),
        page(1),
        page(0),
        page(1),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let read = |flag, token| {
        let mut args = vec!["read", "--session", &session, "--pending", flag];
        args.extend(token);
        let result = fixture.run(&args);
        assert!(result.status.success(), "{result:?}");
        assert!(!fixture.0.join("state").exists());
        String::from_utf8(result.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        read("--prepare", None),
        [json!({"kind":"baseline_prepared","session":session,"rev":15,"token":"token +/"})]
    );
    let first = read("--resume", Some("token +/"));
    assert_eq!(first, read("--resume", Some("token +/")));
    assert_eq!(first[0]["kind"], "baseline");
    assert_eq!(first[1]["pending"], false);
    assert_eq!(first[1]["data"], changes[0]["data"]);
    assert_eq!(first[2], changes[1]);
    assert_eq!(first[3], json!({"kind":"baseline_complete","rev":15}));
    let requests = server.requests();
    assert_eq!(requests.len(), 5);
    assert!(!requests[0].line.contains("page="));
    assert!(requests[1].line.contains("page=token+%2B%2F"));
    assert!(requests[2].line.contains("page=page+2"));
    assert_eq!(requests[1].line, requests[3].line);
}

#[test]
fn caller_pending_token_or_missing_route_never_starts_fresh() {
    let session = Uuid::new_v4().to_string();
    for (status, body, exit, kind) in [
        (
            "400 Bad Request",
            r#"{"error":"invalid_field","field":"page","retryable":false,"request_id":"invalid"}"#,
            2,
            "token_invalid",
        ),
        (
            "404 Not Found",
            r#"{"error":"not_found","retryable":false,"request_id":"old-server"}"#,
            5,
            "unsupported",
        ),
    ] {
        let (server, identity) = SlowServer::mutual_tls_sequence(vec![test_tls::FixtureResponse {
            status,
            headers: &[],
            body: body.into(),
            truncate: false,
        }]);
        let fixture = CliFixture::new(&server, identity);
        let result = fixture.run(&[
            "read",
            "--session",
            &session,
            "--pending",
            "--resume",
            "bad",
        ]);
        assert_eq!(result.status.code(), Some(exit), "{result:?}");
        let row: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(row["kind"], kind);
        if exit == 2 {
            assert_eq!(row["reason"], "invalid_field");
            assert_eq!(row["field"], "page");
        }
        assert!(!fixture.0.join("state").exists());
        assert!(server.request_line().contains("page=bad"));
    }
}

#[test]
fn caller_follow_exits_on_expiry_without_rebaselining() {
    let session = Uuid::new_v4().to_string();
    for stream in [false, true] {
        let responses = if stream {
            vec![
                test_tls::FixtureResponse::json(json!({"rev":11,"changes":[],"more":false})),
                test_tls::FixtureResponse {
                    status: "200 OK",
                    headers: &[("Content-Type", "text/event-stream")],
                    body: "event: closing\ndata: {\"reason\":\"cursor_expired\"}\n\n".into(),
                    truncate: false,
                },
            ]
        } else {
            vec![test_tls::FixtureResponse {
                status: "410 Gone",
                headers: &[],
                body: r#"{"error":"cursor_expired","retryable":false,"request_id":"expired"}"#
                    .into(),
                truncate: false,
            }]
        };
        let (server, identity) = SlowServer::mutual_tls_sequence(responses);
        let fixture = CliFixture::new(&server, identity);
        let journal = harklight_agent::journal::JournalStore::open(
            &fixture.0.join("state"),
            "fixture",
            Uuid::nil(),
        )
        .unwrap();
        journal
            .record_v2_feed(&session, None, 7, &[], false)
            .unwrap();
        let result = fixture.run(&[
            "listen",
            "--session",
            &session,
            "--follow",
            "--after",
            "10",
            "--on-expired",
            "exit",
        ]);
        assert_eq!(result.status.code(), Some(2), "{result:?}");
        let rows: Vec<Value> = String::from_utf8(result.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let rev = if stream { 11 } else { 10 };
        assert_eq!(
            rows.last(),
            Some(&json!({"kind":"cursor_expired","rev":rev}))
        );
        assert_eq!(rows.len(), if stream { 2 } else { 1 });
        assert_eq!(journal.v2_feed_cursor(&session).unwrap(), Some(7));
        assert!(
            server
                .requests()
                .iter()
                .all(|request| request.line.contains("/agent/changes"))
        );
    }
}

#[test]
fn caller_pending_refuses_missing_or_conflicting_scope_before_serving() {
    let session = Uuid::new_v4().to_string();
    let chat = json!({
        "kind":"entry","rev":11,"data":{"entry_id":"waiting","item_id":"item","rev":11,
            "author":"person","kind":"reply","text":"still waiting"},
    });
    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        test_tls::FixtureResponse::json(json!({"rev":15,"next_page":"original"})),
        test_tls::FixtureResponse::json(json!({"rev":15,"next_page":"fresh"})),
        test_tls::FixtureResponse::json(json!({"rev":15,"changes":[chat]})),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let prepared = fixture.run(&["read", "--session", &session, "--pending", "--prepare"]);
    assert!(prepared.status.success(), "{prepared:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&prepared.stdout).unwrap()["token"],
        "original"
    );

    for action in [&["--prepare"][..], &["--resume", "original"][..]] {
        for scope in [
            &[][..],
            &["--all-sessions"][..],
            &["--session", &session, "--all-sessions"][..],
        ] {
            let mut args = vec!["read", "--pending"];
            args.extend(scope);
            args.extend(action);
            let rejected = fixture.run(&args);
            assert_eq!(rejected.status.code(), Some(2), "{args:?}: {rejected:?}");
            assert!(rejected.stdout.is_empty(), "{args:?}: {rejected:?}");
            let diagnostic = String::from_utf8_lossy(&rejected.stderr);
            assert!(diagnostic.contains("--session"), "{args:?}: {diagnostic}");
            assert!(!fixture.0.join("state").exists());
        }
    }

    let prepared = fixture.run(&["read", "--session", &session, "--pending", "--prepare"]);
    assert!(prepared.status.success(), "{prepared:?}");
    assert_eq!(
        serde_json::from_slice::<Value>(&prepared.stdout).unwrap()["token"],
        "fresh"
    );
    let resumed = fixture.run(&[
        "read",
        "--session",
        &session,
        "--pending",
        "--resume",
        "fresh",
    ]);
    assert!(resumed.status.success(), "{resumed:?}");
    let output = String::from_utf8(resumed.stdout).unwrap();
    let rows: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows[1], chat);
    assert!(!fixture.0.join("state").exists());

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].line.contains("page="));
    assert!(!requests[1].line.contains("page="));
    assert!(requests[2].line.contains("page=fresh"));
}

#[test]
fn caller_pending_prepare_preserves_only_a_string_answer_capability() {
    let session = Uuid::new_v4().to_string();
    for capability in [None, Some(json!("all")), Some(json!(true))] {
        let mut reply = json!({"rev":15,"next_page":"token"});
        if let Some(value) = &capability {
            reply["answers"] = value.clone();
        }
        let (server, identity) =
            SlowServer::mutual_tls_sequence(vec![test_tls::FixtureResponse::json(reply)]);
        let fixture = CliFixture::new(&server, identity);
        let result = fixture.run(&["read", "--session", &session, "--pending", "--prepare"]);
        assert!(result.status.success(), "{result:?}");
        let mut expected =
            json!({"kind":"baseline_prepared","session":session,"rev":15,"token":"token"});
        if let Some(Value::String(value)) = capability {
            expected["answers"] = json!(value);
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stdout).unwrap(),
            expected
        );
        assert!(!fixture.0.join("state").exists());
        assert!(!server.request_line().contains("page="));
    }
}

#[test]
fn setup_resumes_approval_and_records_the_test_answer() {
    use std::sync::Mutex;
    use test_tls::FixtureResponse;

    let message = Mutex::new(Value::Null);
    let server = SlowServer::respond_to(move |request| {
        let resource = request.line.split_whitespace().nth(1).unwrap();
        let (path, _) = resource.split_once('?').unwrap_or((resource, ""));
        let mut message = message.lock().unwrap();
        let body = || serde_json::from_slice::<Value>(&request.body).unwrap();
        let response = match path {
            "/enroll" => json!({"state":"pending","approval_code":"ABCDEFG234"}),
            "/enroll/status" => {
                assert!(body()["status_secret"].is_string());
                json!({"state":"approved","person_id":"person","device_id":"device"})
            }
            "/v2/me" => json!({"sender_names":["app:agent/"]}),
            "/v2/agent/messages" => {
                let sent = body();
                assert_eq!(sent["sender"], "app:agent/setup");
                assert_eq!(sent["message"]["title"], "Harklight is connected");
                assert_eq!(sent["message"]["answer_ui"]["actions"][0]["id"], "reply");
                *message = json!({
                    "rev":1,"item_id":"test-item","sender":sent["sender"],
                    "session":sent["session"],"revision":1,"message":sent["message"],
                    "seen":false,"files":[],"terminal":null
                });
                json!({"rev":1,"message":*message})
            }
            "/v2/agent/changes" => {
                assert!(resource.contains("after=1"));
                assert!(resource.contains(message["session"].as_str().unwrap()));
                assert!(
                    request
                        .headers
                        .iter()
                        .any(|(name, value)| { name == "harklight-ack2" && value == "true" })
                );
                message["rev"] = 2.into();
                message["terminal"] = json!({"state":"answered","answer":"Ready"});
                json!({"rev":2,"more":false,
                    "changes":[{"kind":"message","rev":2,"data":*message}]})
            }
            "/v2/agent/messages/test-item/receipt" => {
                assert_eq!(body()["outcome"], "handed");
                json!({"rev":3,"status":"recorded","stage_outcome":"handed","message":*message})
            }
            "/v2/agent/messages/test-item/result" => {
                assert_eq!(body()["status"], "applied");
                json!({"rev":4,"message":*message,"entry":{
                    "entry_id":"result","item_id":"test-item","rev":4,"position":1,
                    "request_revision":1,"kind":"result","author":"origin"
                }})
            }
            "/stop-test" => return None,
            _ => panic!("unexpected request to {resource}"),
        };
        Some(FixtureResponse::json(response))
    });
    let (fixture, link) = setup_fixture(&server);
    let claude_home = fixture.0.join(".claude");
    let codex_home = fixture.0.join("custom-codex-home");
    fs::create_dir(&claude_home).unwrap();
    fs::create_dir(&codex_home).unwrap();
    let claude_skill = claude_home.join("skills/harklight/SKILL.md");
    let codex_skill = fixture.0.join(".agents/skills/harklight/SKILL.md");
    let identity = fixture.0.join("identity");
    let invitation = fixture.0.join("invitation");
    fs::write(&invitation, link).unwrap();
    let start = enroll_invitation_cli(
        &identity,
        &["start", "--invitation", invitation.to_str().unwrap()],
    );
    assert!(start.status.success(), "{start:?}");
    assert!(!claude_skill.exists());
    assert!(!codex_skill.exists());
    let setup = || {
        let mut command = setup_cli(&fixture);
        command.env("CODEX_HOME", &codex_home);
        command
    };
    let output = setup().output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("ABCDEFG234"), "{stdout}");
    assert!(stdout.contains("Sender IDs:      app:agent/"), "{stdout}");
    assert!(stdout.contains("Answer: Ready\nConnected."), "{stdout}");

    let vendored = include_str!("../vendor/harklight/SKILL.md");
    for skill in [&claude_skill, &codex_skill] {
        assert!(stdout.contains(skill.to_str().unwrap()), "{stdout}");
        assert_eq!(fs::read_to_string(skill).unwrap(), vendored);
        fs::write(skill, "earlier skill version").unwrap();
    }
    let refresh = setup().output().unwrap();
    assert!(refresh.status.success(), "{refresh:?}");
    for skill in [&claude_skill, &codex_skill] {
        assert_eq!(fs::read_to_string(skill).unwrap(), vendored);
    }
    let stdout = String::from_utf8(refresh.stdout).unwrap();
    assert!(stdout.contains("Account: person"), "{stdout}");
    assert!(!stdout.contains("Sent a test question"), "{stdout}");

    let ca = reqwest::Certificate::from_pem(server.ca_pem.as_bytes()).unwrap();
    let _ = reqwest::blocking::Client::builder()
        .tls_certs_only([ca])
        .build()
        .unwrap()
        .get(format!("https://localhost:{}/stop-test", server.port))
        .send();
    let requests = server.requests();
    assert_eq!(requests.len(), 8);
}

#[test]
fn setup_explains_short_links_and_sets_aside_an_identity_without_grants() {
    use test_tls::FixtureResponse;

    let server = SlowServer::sequence(vec![
        FixtureResponse::json(json!({
            "state":"approved","person_id":"person","device_id":"device"
        })),
        FixtureResponse::json(json!({"sender_names":[]})),
    ]);
    let (fixture, link) = setup_fixture(&server);
    let short = setup_cli(&fixture)
        .arg("https://example.test/join#short")
        .output()
        .unwrap();
    assert!(!short.status.success(), "{short:?}");
    assert!(String::from_utf8_lossy(&short.stderr).contains("Copy full link"));
    let identity = fixture.0.join("identity");
    assert!(!identity.exists());

    let declined = setup_cli(&fixture)
        .args([link.as_str(), "--label", "Work computer"])
        .output()
        .unwrap();
    assert_eq!(declined.status.code(), Some(2), "{declined:?}");
    assert!(String::from_utf8_lossy(&declined.stderr).contains("Remove it in Devices"));
    assert!(!identity.exists());
    assert!(fs::read_dir(&fixture.0).unwrap().any(|entry| {
        let entry = entry.unwrap();
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("identity.no-sender-grant-")
            && entry.path().join("enrollment.json").is_file()
    }));
    let requests = server.requests();
    let enrollment: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(enrollment["label"], "Work computer");
}

fn setup_fixture(server: &SlowServer) -> (CliFixture, String) {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

    let root = std::env::temp_dir().join(format!("harklight-setup-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let mut invitation = join_fixture(
        server.port,
        2,
        &format!("https://localhost:{}", server.port),
    );
    invitation["ca_sha256"] = hex::encode(Sha256::digest(invitation_ca_der(&server.ca_pem))).into();
    invitation["ca_pem"] = server.ca_pem.clone().into();
    let link = format!(
        "harklight://join#{}",
        URL_SAFE_NO_PAD.encode(invitation.to_string())
    );
    (CliFixture(root), link)
}

fn setup_cli(fixture: &CliFixture) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_harklight-agent"));
    command
        .env("HOME", &fixture.0)
        .env_remove("CODEX_HOME")
        .env_remove("HARKLIGHT_CA_FILE")
        .env_remove("HARKLIGHT_DEVICE_PORT")
        .env("HARKLIGHT_IDENTITY_DIR", fixture.0.join("identity"))
        .env("HARKLIGHT_AGENT_STATE_DIR", fixture.0.join("state"))
        .arg("setup");
    command
}

#[test]
fn identity_defaults_yield_to_the_existing_environment_and_flag() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(
        (0..3)
            .map(|_| FixtureResponse::json(json!({"sender_names":["app:agent/"]})))
            .collect(),
    );
    let fixture = CliFixture::new(&server, identity);
    let explicit = fixture.0.join("explicit");
    let env = fixture.0.join("identity");
    let home_data = fixture.0.join("data");
    #[cfg(target_os = "macos")]
    let default = fixture
        .0
        .join("Library/Application Support/Harklight/Agent/identity");
    #[cfg(target_os = "linux")]
    let default = home_data.join("harklight-agent/identity");

    fs::create_dir_all(default.parent().unwrap()).unwrap();
    for (directory, device) in [(&explicit, "explicit"), (&default, "default")] {
        fs::create_dir(directory).unwrap();
        for file in ["identity.key", "identity.pem", "server.json"] {
            fs::copy(env.join(file), directory.join(file)).unwrap();
        }
        fs::write(
            directory.join("enrollment.json"),
            json!({"device_id":device}).to_string(),
        )
        .unwrap();
    }
    let env_setup = || {
        let mut command = setup_cli(&fixture);
        command
            .env("HARKLIGHT_CA_FILE", fixture.0.join("ca.pem"))
            .env("XDG_DATA_HOME", &home_data);
        command
    };
    for (output, device) in [
        (env_setup().output().unwrap(), "fixture"),
        (
            env_setup()
                .arg("--identity-dir")
                .arg(explicit)
                .output()
                .unwrap(),
            "explicit",
        ),
        (
            env_setup()
                .env_remove("HARKLIGHT_IDENTITY_DIR")
                .output()
                .unwrap(),
            "default",
        ),
    ] {
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains(&format!("Device: {device}")));
    }
    assert!(!fixture.0.join(".claude").exists());
    assert!(!fixture.0.join(".agents").exists());
    assert_eq!(server.requests().len(), 3);
}

#[test]
fn commands_other_than_setup_leave_skill_directories_untouched() {
    let (server, identity) = SlowServer::mutual_tls(r#"{"rev":1,"messages":[]}"#);
    let fixture = CliFixture::new(&server, identity);
    fs::create_dir(fixture.0.join(".codex")).unwrap();
    let existing = fixture.0.join(".claude/skills/harklight/SKILL.md");
    fs::create_dir_all(existing.parent().unwrap()).unwrap();
    fs::write(&existing, "existing user skill").unwrap();

    let op_id = Uuid::new_v4().to_string();
    let prepare = [
        "send",
        "--prepare",
        "--op-id",
        &op_id,
        "--sender-id",
        "app:agent/test",
        "--type",
        "fyi",
        "--title",
        "Test",
    ];
    for args in [
        &["journal-id"][..],
        &["messages"][..],
        &["--version"][..],
        &prepare,
    ] {
        let output = fixture.run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert_eq!(
            fs::read_to_string(&existing).unwrap(),
            "existing user skill"
        );
        assert!(!fixture.0.join(".agents").exists());
    }
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn group_send_preserves_selector_and_replays_with_both_capabilities() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use test_tls::FixtureResponse;

    for (flag, value, selector_field) in [
        ("--group-id", "group/opaque", "group_id"),
        ("--group-name", "Release crew", "name"),
    ] {
        let attempts = AtomicUsize::new(0);
        let (server, identity) = SlowServer::mutual_tls_respond_to(move |request| {
            if request.line.starts_with("GET /v2/me ") {
                assert!(
                    !request
                        .headers
                        .iter()
                        .any(|(key, _)| key == "harklight-features")
                );
                return Some(FixtureResponse::json(json!({
                    "features":{"shared_messages":true,"groups":true}
                })));
            }
            assert_eq!(request.line.trim_end(), "POST /v2/agent/messages HTTP/1.1");
            assert!(
                request
                    .headers
                    .contains(&("harklight-features".into(), "shared_messages,groups".into()))
            );
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(
                body["recipients"],
                json!({"kind":"group",selector_field:value})
            );
            assert_eq!(
                body["answer_policy"],
                json!({"kind":"named","people":["decider"]})
            );
            let mut response = FixtureResponse::json(json!({
                "rev":3,"recipients":{"kind":"group","group_id":"group/opaque"},"message":{
                    "rev":3,"revision":1,"item_id":"item/opaque","seen":false,"files":[],
                    "sender":body["sender"],"message":body["message"],
                    "shared":{"shared_id":"item/opaque","group":{"id":"group/opaque","name":"Release crew"}}
                }
            }));
            response.truncate = attempts.fetch_add(1, Ordering::Relaxed) == 0;
            Some(response)
        });
        let fixture = CliFixture::new(&server, identity);
        let key = Uuid::new_v4().to_string();
        let prepared = fixture.run(&[
            "send",
            "--sender-id",
            "app:test",
            "--type",
            "request",
            "--reply",
            "--title",
            "Notice",
            flag,
            value,
            "--decider-id",
            "decider",
            "--prepare",
            "--op-id",
            &key,
        ]);
        assert!(prepared.status.success(), "{prepared:?}");
        let output: Value = serde_json::from_slice(&prepared.stdout).unwrap();
        assert_eq!(
            output["prepared"]["recipients"],
            json!({"kind":"group",selector_field:value})
        );
        let first = fixture.run(&["send", "--execute", "--op-id", &key]);
        assert_eq!(first.status.code(), Some(7), "{first:?}");
        let journal: Value = serde_json::from_slice(&fixture.run(&["journal-id"]).stdout).unwrap();
        let replay = fixture.run(&[
            "retry",
            "--op-id",
            &key,
            "--journal-id",
            journal["journal_id"].as_str().unwrap(),
        ]);
        assert!(replay.status.success(), "{replay:?}");
        let output: Value = serde_json::from_slice(&replay.stdout).unwrap();
        assert_eq!(output["item_id"], "item/opaque");
        assert_eq!(
            output["response"]["recipients"],
            json!({"kind":"group","group_id":"group/opaque"})
        );
    }
}

#[test]
fn new_group_prepare_checks_discovery_without_saving_a_refused_operation() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse::json(json!({"features":{"shared_messages":true}})),
        FixtureResponse::json(json!({"features":{"shared_messages":true,"groups":true}})),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let attachment = fixture.0.join("draft.txt");
    fs::write(&attachment, "Group attachment").unwrap();
    let key = Uuid::new_v4().to_string();
    let args = [
        "send",
        "--sender-id",
        "app:test",
        "--type",
        "fyi",
        "--title",
        "Notice",
        "--group-name",
        "Planning",
        "--file",
        attachment.to_str().unwrap(),
        "--prepare",
        "--op-id",
        &key,
    ];

    let refusal = fixture.run(&args);
    assert_eq!(refusal.status.code(), Some(5), "{refusal:?}");
    let refusal: Value = serde_json::from_slice(&refusal.stdout).unwrap();
    assert_eq!(
        refusal,
        json!({
            "kind":"refused","operation":"send","op_id":key,"status":400,"error":"unsupported"
        })
    );
    let journal: Value = serde_json::from_slice(&fixture.run(&["journal-id"]).stdout).unwrap();
    let pinned = fixture.run(&[
        "retry",
        "--op-id",
        &key,
        "--journal-id",
        journal["journal_id"].as_str().unwrap(),
    ]);
    assert_eq!(pinned.status.code(), Some(1), "{pinned:?}");
    let absent: Value = serde_json::from_slice(&pinned.stdout).unwrap();
    assert_eq!(absent["kind"], "operation_absent");
    assert!(!fixture.0.join("state/upload-staging").exists());

    let prepared = fixture.run(&args);
    assert!(prepared.status.success(), "{prepared:?}");
    let repeated = fixture.run(&args);
    assert!(repeated.status.success(), "{repeated:?}");
    assert_eq!(prepared.stdout, repeated.stdout);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.line.starts_with("GET /v2/me "))
    );
}

#[test]
fn group_capability_refuses_before_any_group_write() {
    use test_tls::FixtureResponse;
    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse::json(json!({"features":{"shared_messages":true}})),
        FixtureResponse::json(json!({"features":{"shared_messages":true}})),
    ]);
    let fixture = CliFixture::new(&server, identity);
    for args in [
        &[
            "send",
            "--sender-id",
            "app:test",
            "--type",
            "fyi",
            "--title",
            "Notice",
            "--group-id",
            "g",
        ][..],
        &["reply", "--groups", "--item-id", "item", "--body", "Notice"][..],
    ] {
        let refusal = fixture.run(args);
        assert_eq!(refusal.status.code(), Some(5), "{refusal:?}");
        let output: Value = serde_json::from_slice(&refusal.stdout).unwrap();
        assert_eq!(output["kind"], "refused");
        assert_eq!(output["error"], "unsupported");
    }
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.line.starts_with("GET /v2/me "))
    );
}

#[test]
fn alarm_prepare_refuses_an_older_server_without_saving_the_operation() {
    use test_tls::FixtureResponse;
    let (server, identity) =
        SlowServer::mutual_tls_sequence(vec![FixtureResponse::json(json!({"features":{}}))]);
    let fixture = CliFixture::new(&server, identity);
    let key = Uuid::new_v4().to_string();
    let prepared = fixture.run(&[
        "send",
        "--sender-id",
        "app:test",
        "--type",
        "fyi",
        "--title",
        "Alarm",
        "--importance",
        "alarm",
        "--prepare",
        "--op-id",
        &key,
    ]);
    assert_eq!(prepared.status.code(), Some(5), "{prepared:?}");
    let output: Value = serde_json::from_slice(&prepared.stdout).unwrap();
    assert_eq!(output["error"], "unsupported");
    assert_eq!(output["status"], 400);
    let journal: Value = serde_json::from_slice(&fixture.run(&["journal-id"]).stdout).unwrap();
    let retry = fixture.run(&[
        "retry",
        "--op-id",
        &key,
        "--journal-id",
        journal["journal_id"].as_str().unwrap(),
    ]);
    let absent: Value = serde_json::from_slice(&retry.stdout).unwrap();
    assert_eq!(absent["kind"], "operation_absent");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].line.trim_end(), "GET /v2/me HTTP/1.1");
}

#[test]
fn alarm_send_reuses_exact_preparation_and_keeps_unknown_when_capability_is_lost() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use test_tls::FixtureResponse;

    let discoveries = AtomicUsize::new(0);
    let sends = AtomicUsize::new(0);
    let first_body = std::sync::Mutex::new(None);
    let (server, identity) = SlowServer::mutual_tls_respond_to(move |request| {
        if request.line.starts_with("GET ") {
            let attempt = discoveries.fetch_add(1, Ordering::Relaxed);
            assert_eq!(request.line.trim_end(), "GET /v2/me HTTP/1.1");
            return Some(FixtureResponse::json(
                json!({"features":{"alarm":attempt != 2}}),
            ));
        }
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["message"]["alarm"], true);
        assert!(body["message"].get("urgency").is_none());
        assert!(
            !request
                .headers
                .iter()
                .any(|(name, _)| name == "harklight-features")
        );
        let mut first_body = first_body.lock().unwrap();
        if let Some(first) = &*first_body {
            assert_eq!(&request.body, first);
        } else {
            *first_body = Some(request.body.clone());
        }
        let mut response = FixtureResponse::json(json!({"rev":1,"message":{
            "rev":1,"item_id":"item","sender":body["sender"],"revision":1,
            "message":body["message"],"seen":false,"files":[],
        }}));
        response.truncate = sends.fetch_add(1, Ordering::Relaxed) == 0;
        Some(response)
    });
    let fixture = CliFixture::new(&server, identity);
    let key = Uuid::new_v4().to_string();
    let args = [
        "send",
        "--sender-id",
        "app:test",
        "--type",
        "fyi",
        "--title",
        "Alarm",
        "--importance",
        "alarm",
        "--prepare",
        "--op-id",
        &key,
    ];
    let prepared = fixture.run(&args);
    assert!(prepared.status.success(), "{prepared:?}");
    let repeated = fixture.run(&args);
    assert_eq!(prepared.stdout, repeated.stdout);
    let lost = fixture.run(&["send", "--execute", "--op-id", &key]);
    assert_eq!(lost.status.code(), Some(7), "{lost:?}");
    let disabled = fixture.run(&["retry", "--op-id", &key]);
    let output: Value = serde_json::from_slice(&disabled.stdout).unwrap();
    assert_eq!(disabled.status.code(), Some(7), "{disabled:?}");
    assert_eq!(output["kind"], "outcome_unknown");
    let recovered = fixture.run(&["retry", "--op-id", &key]);
    assert!(recovered.status.success(), "{recovered:?}");
    assert_eq!(
        fixture.run(&["retry", "--op-id", &key]).stdout,
        recovered.stdout
    );
}

#[test]
fn explicit_alarm_refuses_shared_writes_before_contacting_the_server() {
    let (server, identity) = SlowServer::mutual_tls_respond_to(|request| {
        panic!("shared Alarm operation unexpectedly sent {}", request.line);
    });
    let fixture = CliFixture::new(&server, identity);
    let input = fixture.0.join("revision.json");
    fs::write(&input, r#"{"title":"Alarm","created_at":1,"alarm":false}"#).unwrap();
    for args in [
        &[
            "send",
            "--sender-id",
            "app:test",
            "--type",
            "fyi",
            "--title",
            "Alarm",
            "--importance",
            "alarm",
            "--person-id",
            "person",
        ][..],
        &[
            "send",
            "--sender-id",
            "app:test",
            "--type",
            "fyi",
            "--title",
            "Alarm",
            "--importance",
            "alarm",
            "--group-id",
            "group",
        ][..],
        &[
            "revise",
            "--shared",
            "--item-id",
            "item",
            "--base-revision",
            "1",
            "--message-json",
            input.to_str().unwrap(),
        ][..],
        &[
            "revise",
            "--groups",
            "--item-id",
            "item",
            "--base-revision",
            "1",
            "--message-json",
            input.to_str().unwrap(),
        ][..],
    ] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(5), "{output:?}");
        let output: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(output["kind"], "refused");
        assert_eq!(output["status"], 400);
        assert_eq!(output["error"], "unsupported");
    }
}

#[test]
fn alarm_revision_preserves_ordinary_urgency_and_can_clear_or_omit_marker() {
    use test_tls::FixtureResponse;

    for (flags, expected) in [
        (&["--importance", "alarm"][..], Some(true)),
        (&["--clear-alarm"][..], Some(false)),
        (&[][..], None),
    ] {
        let (server, identity) = SlowServer::mutual_tls_respond_to(move |request| {
            if request.line.starts_with("GET /v2/me ") {
                assert!(expected.is_some());
                return Some(FixtureResponse::json(json!({"features":{"alarm":true}})));
            }
            let response = |rev, content| {
                FixtureResponse::json(json!({"rev":rev,"message":{
                    "rev":rev,"item_id":"item","sender":"app:test","revision":rev,
                    "message":content,"seen":false,"files":[],
                }}))
            };
            if request.line.starts_with("GET ") {
                assert_eq!(
                    request.line.trim_end(),
                    "GET /v2/agent/messages/item HTTP/1.1"
                );
                return Some(response(
                    1,
                    json!({"title":"Before","created_at":1,"urgency":"high","alarm":true}),
                ));
            }
            assert_eq!(
                request.line.trim_end(),
                "PUT /v2/agent/messages/item HTTP/1.1"
            );
            let mut body: Value =
                serde_json::from_slice::<Value>(&request.body).unwrap()["message"].clone();
            assert_eq!(body["urgency"], "high");
            assert_eq!(body.get("alarm").and_then(Value::as_bool), expected);
            if expected == Some(false) {
                body.as_object_mut().unwrap().remove("alarm");
            } else {
                body["alarm"] = true.into();
            }
            Some(response(2, body))
        });
        let fixture = CliFixture::new(&server, identity);
        let mut args = vec![
            "revise",
            "--item-id",
            "item",
            "--base-revision",
            "1",
            "--title",
            "After",
        ];
        args.extend_from_slice(flags);
        let revised = fixture.run(&args);
        assert!(revised.status.success(), "{revised:?}");
    }
}

#[test]
fn recipient_reach_notice_preserves_send_and_message_get_json() {
    use test_tls::FixtureResponse;

    for reach in [Some("none"), Some("reachable"), None, Some("future-value")] {
        let (server, identity) = SlowServer::mutual_tls_respond_to(move |request| {
            let content = if request.line.starts_with("POST ") {
                serde_json::from_slice::<Value>(&request.body).unwrap()["message"].clone()
            } else {
                assert_eq!(
                    request.line.trim_end(),
                    "GET /v2/agent/messages/item HTTP/1.1"
                );
                json!({"title":"Notice","created_at":10})
            };
            let mut message = json!({
                "rev":1,"item_id":"item","sender":"app:test","revision":1,
                "message":content,"seen":false,"files":[],
            });
            if let Some(reach) = reach {
                message["recipient_reach"] = reach.into();
            }
            Some(FixtureResponse::json(json!({"rev":1,"message":message})))
        });
        let fixture = CliFixture::new(&server, identity);
        for args in [
            &[
                "send",
                "--sender-id",
                "app:test",
                "--type",
                "fyi",
                "--title",
                "Notice",
            ][..],
            &["message-get", "--item-id", "item"][..],
        ] {
            let result = fixture.run(args);
            assert_eq!(result.status.code(), Some(0), "{result:?}");
            assert_eq!(
                result.stderr,
                if reach == Some("none") {
                    &b"No one will get this\n"[..]
                } else {
                    &b""[..]
                }
            );
            let output: Value = serde_json::from_slice(&result.stdout).unwrap();
            let response = if args[0] == "send" {
                assert_eq!(output["kind"], "committed");
                &output["response"]
            } else {
                &output
            };
            assert_eq!(
                response["message"]
                    .get("recipient_reach")
                    .and_then(Value::as_str),
                reach
            );
            assert_eq!(response["message"]["message"]["title"], "Notice");
        }
    }
}

#[test]
fn unresolved_send_replay_reports_current_recipient_reach() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use test_tls::FixtureResponse;

    let requests = AtomicUsize::new(0);
    let key = Uuid::new_v4().to_string();
    let expected_key = key.clone();
    let (server, identity) = SlowServer::mutual_tls_respond_to(move |request| {
        assert_eq!(request.line.trim_end(), "POST /v2/agent/messages HTTP/1.1");
        assert!(
            request
                .headers
                .contains(&("idempotency-key".into(), expected_key.clone()))
        );
        let sent: Value = serde_json::from_slice(&request.body).unwrap();
        let is_replay = requests.fetch_add(1, Ordering::Relaxed) > 0;
        let mut response = FixtureResponse::json(json!({"rev":1,"message":{
            "rev":1,"item_id":"item","sender":sent["sender"],"revision":1,
            "message":sent["message"],"seen":false,"files":[],
            "recipient_reach": if is_replay { "none" } else { "reachable" },
        }}));
        response.truncate = !is_replay;
        if is_replay {
            response.headers = &[("Idempotent-Replayed", "true")];
        }
        Some(response)
    });
    let fixture = CliFixture::new(&server, identity);
    let lost = fixture.run(&[
        "send",
        "--sender-id",
        "app:test",
        "--type",
        "fyi",
        "--title",
        "Notice",
        "--op-id",
        &key,
    ]);
    assert_eq!(lost.status.code(), Some(7), "{lost:?}");
    let replay = fixture.run(&["retry", "--op-id", &key]);
    assert_eq!(replay.status.code(), Some(0), "{replay:?}");
    assert_eq!(replay.stderr, b"No one will get this\n");
    let output: Value = serde_json::from_slice(&replay.stdout).unwrap();
    assert_eq!(output["kind"], "committed");
    assert_eq!(output["response"]["message"]["recipient_reach"], "none");
}

#[test]
fn forbidden_send_reports_current_grants_without_changing_machine_output() {
    use test_tls::FixtureResponse;

    let (server, identity) = SlowServer::mutual_tls_sequence(vec![
        FixtureResponse {
            status: "403 Forbidden",
            headers: &[],
            body: r#"{"error":"forbidden","retryable":false,"request_id":"send"}"#.into(),
            truncate: false,
        },
        FixtureResponse::json(json!({"sender_names":["app:agent/","app:build"]})),
    ]);
    let fixture = CliFixture::new(&server, identity);
    let refused = fixture.run(&[
        "send",
        "--sender-id",
        "app:other",
        "--type",
        "fyi",
        "--title",
        "Notice",
    ]);
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    let stderr = String::from_utf8(refused.stderr).unwrap();
    assert!(stderr.contains("app:other"), "{stderr}");
    assert!(
        stderr.contains("Granted sender IDs: app:agent/, app:build"),
        "{stderr}"
    );
    let output: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(output["error"], "forbidden");
    assert!(output.get("sender_names").is_none());
    let requests = server.requests();
    assert_eq!(requests[1].line.trim_end(), "GET /v2/me HTTP/1.1");
}
