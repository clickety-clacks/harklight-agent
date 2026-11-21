use std::collections::BTreeSet;

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
        let response = self.read(&["agent", "messages"], &query)?;
        let (_, messages) = message_page(&response, page)?;
        if let Some(session) = session {
            let session = session.to_string();
            if messages.iter().any(|message| message["session"] != session) {
                return Err(invalid_response());
            }
        }
        Ok(response)
    }

    pub fn message(&self, item_id: &str) -> Result<Value, V2Error> {
        let response = self.read(&["agent", "messages", item_id], &[])?;
        let position = response["rev"].as_u64().ok_or_else(invalid_response)?;
        let message = &response["message"];
        if message["item_id"] != item_id || !valid_sent_message(message, position) {
            return Err(invalid_response());
        }
        Ok(response)
    }

    pub fn message_revision(&self, item_id: &str, revision: u64) -> Result<Value, V2Error> {
        let response = self.read(
            &[
                "agent",
                "messages",
                item_id,
                "revisions",
                &revision.to_string(),
            ],
            &[],
        )?;
        let observed = &response["revision"];
        if !response["rev"].is_u64()
            || observed["request_revision"] != revision
            || !observed["message"].is_object()
            || !valid_files(&observed["files"])
        {
            return Err(invalid_response());
        }
        Ok(response)
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
        let response = self.read(&["agent", "messages", item_id, "entries"], &query)?;
        let rev = response["rev"].as_u64().ok_or_else(invalid_response)?;
        let entries = response["entries"]
            .as_array()
            .ok_or_else(invalid_response)?;
        let older = response["older"].as_bool().ok_or_else(invalid_response)?;
        if (older && entries.is_empty())
            || entries.iter().any(|entry| {
                entry["item_id"] != item_id
                    || !nonempty_string(&entry["entry_id"])
                    || !positive_at_most(&entry["rev"], rev)
                    || !positive_at_most(&entry["position"], u64::MAX)
                    || before.is_some_and(|before| entry["entry_id"] == before)
            })
            || entries
                .windows(2)
                .any(|pair| pair[0]["position"].as_u64() >= pair[1]["position"].as_u64())
        {
            return Err(invalid_response());
        }
        Ok(response)
    }

    pub fn legacy_message(
        &self,
        sender: &str,
        legacy_message_id: &str,
    ) -> Result<Option<Value>, V2Error> {
        let mut token = None;
        let mut seen_tokens = BTreeSet::new();
        let mut matched: Option<Value> = None;
        loop {
            let mut query = vec![("sender", sender), ("legacy_message_id", legacy_message_id)];
            if let Some(page) = token.as_deref() {
                query.push(("page", page));
            }
            let response = self.read(&["agent", "messages"], &query)?;
            let (position, messages) = message_page(&response, token.as_deref())?;
            for message in messages {
                if message["sender"] != sender || message["legacy_message_id"] != legacy_message_id
                {
                    return Err(invalid_response());
                }
                if let Some(prior) = &matched {
                    if prior["message"]["item_id"] != message["item_id"] {
                        return Err(invalid_response());
                    }
                    if prior["message"]["rev"].as_u64() >= message["rev"].as_u64() {
                        continue;
                    }
                }
                matched = Some(json!({"rev":position,"message":message}));
            }
            match response["next_page"].as_str() {
                Some(next) if seen_tokens.insert(next.to_owned()) => token = Some(next.to_owned()),
                None => return Ok(matched),
                _ => return Err(invalid_response()),
            }
        }
    }
}

fn invalid_response() -> V2Error {
    V2Error::InvalidResponse { request_id: None }
}

fn nonempty_string(value: &Value) -> bool {
    value.as_str().is_some_and(|text| !text.is_empty())
}

fn positive_at_most(value: &Value, max: u64) -> bool {
    value
        .as_u64()
        .is_some_and(|number| number > 0 && number <= max)
}

fn valid_files(files: &Value) -> bool {
    files.as_array().is_some_and(|files| {
        files.iter().all(|file| {
            nonempty_string(&file["file_id"])
                && nonempty_string(&file["name"])
                && file["size"].is_u64()
                && file["sha256"].as_str().is_some_and(|digest| {
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
        })
    })
}

pub(super) fn valid_sent_message(message: &Value, position: u64) -> bool {
    nonempty_string(&message["item_id"])
        && nonempty_string(&message["sender"])
        && positive_at_most(&message["rev"], position)
        && positive_at_most(&message["revision"], u64::MAX)
        && message["message"].is_object()
        && message["seen"].is_boolean()
        && valid_files(&message["files"])
}

fn message_page<'a>(
    response: &'a Value,
    requested_page: Option<&str>,
) -> Result<(u64, &'a [Value]), V2Error> {
    let position = response["rev"].as_u64().ok_or_else(invalid_response)?;
    let messages = response["messages"]
        .as_array()
        .ok_or_else(invalid_response)?;
    let next = &response["next_page"];
    if (!next.is_null()
        && next
            .as_str()
            .is_none_or(|next| next.is_empty() || requested_page == Some(next)))
        || messages
            .iter()
            .any(|message| !valid_sent_message(message, position))
    {
        return Err(invalid_response());
    }
    Ok((position, messages))
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
            "message":{"title":"Build"},"seen":false,"files":[]}]}"#,
        );
        let client = client_for(&server);
        let result = client
            .legacy_message("app:build", "progress +")
            .unwrap()
            .unwrap();

        assert_eq!(result["rev"], 24);
        assert_eq!(result["message"]["item_id"], "server/opaque");
        assert_eq!(result["message"]["revision"], 4);
        assert_eq!(result["message"]["files"], json!([]));
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
            r#"{"rev":24,"revision":{"request_revision":2,"message":{"title":"Old"},"files":[]}}"#,
        );
        let result = client_for(&server).message_revision("item/one", 2).unwrap();
        assert_eq!(result["revision"]["request_revision"], 2);
        assert_eq!(result["revision"]["message"]["title"], "Old");
        assert_eq!(
            server.request_line().trim_end(),
            "GET /v2/agent/messages/item%2Fone/revisions/2 HTTP/1.1",
        );
    }

    #[test]
    fn legacy_lookup_paginates_before_answering_and_rejects_repeated_tokens() {
        use crate::test_tls::FixtureResponse;
        let message = json!({"item_id":"item-1","sender":"app:test","legacy_message_id":"ref",
            "rev":4,"revision":1,"message":{},"seen":false,"files":[]});
        let server = SlowServer::sequence(vec![
            FixtureResponse::json(json!({"rev":4,"messages":[],"next_page":"go"})),
            FixtureResponse::json(json!({"rev":4,"messages":[message]})),
        ]);
        let found = client_for(&server)
            .legacy_message("app:test", "ref")
            .unwrap()
            .unwrap();
        assert_eq!(found["message"]["item_id"], "item-1");
        assert!(server.requests()[1].line.contains("page=go"));

        let repeat = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"rev":4,"messages":[],"next_page":"go"}"#,
        );
        assert!(matches!(
            client_for(&repeat).messages(None, Some("go")),
            Err(V2Error::InvalidResponse { .. })
        ));
        repeat.request();
    }

    #[test]
    fn current_message_refuses_a_mismatched_id_even_with_a_valid_resource() {
        let server = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"rev":4,"message":{"item_id":"other","sender":"app:example",
            "rev":4,"revision":1,"message":{},"seen":false,"files":[]}}"#,
        );
        assert!(matches!(
            client_for(&server).message("wanted"),
            Err(V2Error::InvalidResponse { .. })
        ));
        assert_eq!(
            server.request_line().trim_end(),
            "GET /v2/agent/messages/wanted HTTP/1.1"
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
