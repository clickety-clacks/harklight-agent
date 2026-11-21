use serde_json::{Value, json};
use uuid::Uuid;

use super::{AgentClient, V2Error};

impl AgentClient {
    pub fn messages(&self, session: Option<Uuid>, page: Option<&str>) -> Result<Value, V2Error> {
        let session = session.map(|session| session.to_string());
        let query: Vec<_> = [("session", session.as_deref()), ("page", page)]
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect();
        self.read(&["agent", "messages"], &query)
    }

    pub fn message(&self, item_id: &str) -> Result<Value, V2Error> {
        self.read(&["agent", "messages", item_id], &[])
    }

    pub fn message_revision(&self, item_id: &str, revision: u64) -> Result<Value, V2Error> {
        self.read(
            &[
                "agent",
                "messages",
                item_id,
                "revisions",
                &revision.to_string(),
            ],
            &[],
        )
    }

    pub fn message_entries(
        &self,
        item_id: &str,
        before: Option<&str>,
        limit: Option<u64>,
    ) -> Result<Value, V2Error> {
        let limit = limit.map(|limit| limit.to_string());
        let query: Vec<_> = [("before", before), ("limit", limit.as_deref())]
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect();
        self.read(&["agent", "messages", item_id, "entries"], &query)
    }

    pub fn legacy_message(
        &self,
        sender: &str,
        legacy_message_id: &str,
    ) -> Result<Option<Value>, V2Error> {
        let snapshot = self.read(
            &["agent", "messages"],
            &[("sender", sender), ("legacy_message_id", legacy_message_id)],
        )?;
        let invalid = || V2Error::InvalidResponse { request_id: None };
        let revision = snapshot["rev"].as_u64().ok_or_else(invalid)?;
        match snapshot["messages"].as_array().map(Vec::as_slice) {
            Some([]) => Ok(None),
            Some([message])
                if message["sender"] == sender
                    && message["legacy_message_id"] == legacy_message_id
                    && message["item_id"].as_str().is_some_and(|id| !id.is_empty()) =>
            {
                Ok(Some(json!({"rev": revision, "message": message})))
            }
            _ => Err(invalid()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::config::EnrolledIdentity;
    use crate::test_tls::{SlowServer, TLS_NAME};

    use super::*;

    #[test]
    fn legacy_reference_is_resolved_with_the_exact_filtered_read() {
        let server = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"rev":24,"messages":[{"item_id":"server/opaque","sender":"app:build",
            "legacy_message_id":"progress +","rev":22,"revision":4,
            "message":{"title":"Build"},"files":[{"file_id":"held-file"}]}]}"#,
        );
        let client = client_for(&server);
        let result = client
            .legacy_message("app:build", "progress +")
            .unwrap()
            .unwrap();

        assert_eq!(result["rev"], 24);
        assert_eq!(result["message"]["item_id"], "server/opaque");
        assert_eq!(result["message"]["revision"], 4);
        assert_eq!(result["message"]["files"], json!([{"file_id":"held-file"}]));
        assert_eq!(
            server.request_line().trim_end(),
            "GET /v2/agent/messages?sender=app%3Abuild&legacy_message_id=progress+%2B HTTP/1.1",
        );
    }

    #[test]
    fn legacy_lookup_refuses_an_unrelated_item() {
        let server = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"rev":24,"messages":[{"item_id":"opaque","sender":"app:other","legacy_message_id":"old"}]}"#,
        );
        let result = client_for(&server).legacy_message("app:build", "old");
        assert!(matches!(result, Err(V2Error::InvalidResponse { .. })));
        server.request();
    }

    #[test]
    fn message_pages_preserve_the_opaque_page_and_session_filter() {
        let server = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"rev":24,"messages":[],"next_page":"opaque+next"}"#,
        );
        let session = Uuid::new_v4();
        let result = client_for(&server)
            .messages(Some(session), Some("opaque&start"))
            .unwrap();
        assert_eq!(result["next_page"], "opaque+next");
        assert_eq!(
            server.request_line().trim_end(),
            format!("GET /v2/agent/messages?session={session}&page=opaque%26start HTTP/1.1"),
        );
    }

    #[test]
    fn historical_revision_preserves_the_selected_revision_and_file_ids() {
        let server = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"rev":24,"message":{"item_id":"item/one","revision":2,"files":[{"file_id":"older"}]}}"#,
        );
        let result = client_for(&server).message_revision("item/one", 2).unwrap();
        assert_eq!(result["message"]["revision"], 2);
        assert_eq!(result["message"]["files"][0]["file_id"], "older");
        assert_eq!(
            server.request_line().trim_end(),
            "GET /v2/agent/messages/item%2Fone/revisions/2 HTTP/1.1",
        );
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
}
