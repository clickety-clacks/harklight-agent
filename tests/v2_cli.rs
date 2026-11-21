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
                "port":server.port,
                "origin_port":1,
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
        r#"{"rev":12,"message":{"item_id":"opaque/item","rev":7,"sender":"app:fixture","revision":2,"message":{"title":"Old","created_at":10},"files":[],"delivered":true,"seen":false}}"#,
    );
    let fixture = CliFixture::new(&server, identity);
    let output = fixture.run(&["message-get", "--item-id", "opaque/item", "--revision", "2"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["message"]["revision"], 2);
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
