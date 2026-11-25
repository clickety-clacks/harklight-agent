use std::collections::BTreeSet;

use serde_json::{Value, json};
use uuid::Uuid;

use super::{AgentClient, V2Error, header};
use reqwest::header::ACCEPT;

impl AgentClient {
    pub fn account_search(
        &self,
        session: Option<Uuid>,
        sender: Option<&str>,
        message_id: Option<&str>,
        state: Option<&str>,
        page: Option<&str>,
    ) -> Result<Value, V2Error> {
        let session = session.map(|session| session.to_string());
        let query: Vec<_> = [
            ("session", session.as_deref()),
            ("sender", sender),
            ("message_id", message_id),
            ("state", state),
            ("page", page),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key, value)))
        .collect();
        let mut url = self.url(&["search"])?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(&query);
        }
        let response = self.send(self.client.get(url).header(ACCEPT, "application/json"))?;
        let account_capable = header(&response, "harklight-account-search") == Some("1");
        let response = match Self::checked_response(response) {
            Ok(response) if account_capable => response,
            Ok(_) => return Err(V2Error::AccountSearchUnsupported),
            Err(error) if !account_capable && error.is_not_found() => {
                return Err(V2Error::AccountSearchUnsupported);
            }
            Err(error) => return Err(error),
        };
        let (response, request_id) = Self::decode_json(response)?;
        let revision = response["rev"]
            .as_u64()
            .ok_or_else(|| invalid_response(&request_id))?;
        let items = response["items"]
            .as_array()
            .ok_or_else(|| invalid_response(&request_id))?;
        if items.iter().any(|item| {
            !nonempty_string(&item["item_id"])
                || !positive_at_most(&item["rev"], revision)
                || sender.is_some_and(|sender| item["sender"] != sender)
                || message_id.is_some_and(|id| item["message"]["message_id"] != id)
        }) || response
            .get("next_page")
            .is_some_and(|next| !nonempty_string(next))
        {
            return Err(invalid_response(&request_id));
        }
        Ok(response)
    }

    pub fn account_item_id(
        &self,
        message_id: &str,
        sender: Option<&str>,
    ) -> Result<String, V2Error> {
        let mut page = None;
        let mut seen_pages = BTreeSet::new();
        let mut item_id = None;
        loop {
            let response =
                self.account_search(None, sender, Some(message_id), None, page.as_deref())?;
            for item in response["items"]
                .as_array()
                .expect("validated search response")
            {
                let id = item["item_id"].as_str().expect("validated item ID");
                match &item_id {
                    Some(found) if found != id => return Err(V2Error::AmbiguousMessageId),
                    Some(_) => {}
                    None => item_id = Some(id.to_owned()),
                }
            }
            match response["next_page"].as_str() {
                Some(next) if seen_pages.insert(next.to_owned()) => page = Some(next.to_owned()),
                Some(_) => return Err(invalid_response(&None)),
                None => break,
            }
        }
        item_id.ok_or_else(|| V2Error::Http {
            status: 404,
            token: Some("not_found".into()),
            field: None,
            request_id: None,
            retry_after: None,
        })
    }

    pub fn account_message(&self, item_id: &str, revision: Option<u64>) -> Result<Value, V2Error> {
        let mut response = match revision {
            Some(revision) => {
                self.read(&["items", item_id, "revisions", &revision.to_string()], &[])?
            }
            None => self.read(&["items", item_id], &[])?,
        };
        response["view"] = json!("account");
        Ok(response)
    }

    pub fn account_entries(
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
        let mut response = self.read(&["items", item_id, "entries"], &query)?;
        response["view"] = json!("account");
        Ok(response)
    }

    pub fn messages(&self, session: Option<Uuid>, page: Option<&str>) -> Result<Value, V2Error> {
        let session = session.map(|session| session.to_string());
        let query: Vec<_> = [("session", session.as_deref()), ("page", page)]
            .into_iter()
            .filter_map(|(key, value)| value.map(|value| (key, value)))
            .collect();
        let (response, request_id) = self.read_correlated(&["agent", "messages"], &query)?;
        let (_, messages) = message_page(&response, page, &request_id)?;
        if let Some(session) = session {
            let session = session.to_string();
            if messages.iter().any(|message| message["session"] != session) {
                return Err(invalid_response(&request_id));
            }
        }
        Ok(response)
    }

    pub fn message(&self, item_id: &str) -> Result<Value, V2Error> {
        let (response, request_id) = self.read_correlated(&["agent", "messages", item_id], &[])?;
        let position = response["rev"]
            .as_u64()
            .ok_or_else(|| invalid_response(&request_id))?;
        let message = &response["message"];
        if message["item_id"] != item_id || !valid_sent_message(message, position) {
            return Err(invalid_response(&request_id));
        }
        Ok(response)
    }

    pub fn message_revision(&self, item_id: &str, revision: u64) -> Result<Value, V2Error> {
        let (response, request_id) = self.read_correlated(
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
            return Err(invalid_response(&request_id));
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
        let (response, request_id) =
            self.read_correlated(&["agent", "messages", item_id, "entries"], &query)?;
        let rev = response["rev"]
            .as_u64()
            .ok_or_else(|| invalid_response(&request_id))?;
        let entries = response["entries"]
            .as_array()
            .ok_or_else(|| invalid_response(&request_id))?;
        let older = response["older"]
            .as_bool()
            .ok_or_else(|| invalid_response(&request_id))?;
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
            return Err(invalid_response(&request_id));
        }
        Ok(response)
    }

    /// The journal also stores modern operation aliases, so a cached item
    /// can satisfy a legacy selector only if the Server confirms both fields.
    pub fn resolve_legacy_message(
        &self,
        sender: &str,
        legacy_message_id: &str,
        cached_item_id: Option<&str>,
    ) -> Result<Value, V2Error> {
        if let Some(item_id) = cached_item_id {
            match self.message(item_id) {
                Ok(response) if matches_legacy(&response["message"], sender, legacy_message_id) => {
                    return Ok(response);
                }
                Ok(_) | Err(V2Error::Http { status: 404, .. }) => {}
                Err(error) => return Err(error),
            }
        }
        self.legacy_message(sender, legacy_message_id)?
            .ok_or_else(|| V2Error::Http {
                status: 404,
                token: Some("not_found".into()),
                field: None,
                request_id: None,
                retry_after: None,
            })
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
            let (response, request_id) = self.read_correlated(&["agent", "messages"], &query)?;
            let (position, messages) = message_page(&response, token.as_deref(), &request_id)?;
            for message in messages {
                if !matches_legacy(message, sender, legacy_message_id) {
                    return Err(invalid_response(&request_id));
                }
                if let Some(prior) = &matched {
                    if prior["message"]["item_id"] != message["item_id"] {
                        return Err(invalid_response(&request_id));
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
                _ => return Err(invalid_response(&request_id)),
            }
        }
    }
}

fn matches_legacy(message: &Value, sender: &str, legacy_message_id: &str) -> bool {
    message["sender"] == sender && message["legacy_message_id"] == legacy_message_id
}

fn invalid_response(request_id: &Option<String>) -> V2Error {
    V2Error::InvalidResponse {
        request_id: request_id.clone(),
    }
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
    request_id: &Option<String>,
) -> Result<(u64, &'a [Value]), V2Error> {
    let position = response["rev"]
        .as_u64()
        .ok_or_else(|| invalid_response(request_id))?;
    let messages = response["messages"]
        .as_array()
        .ok_or_else(|| invalid_response(request_id))?;
    let next = &response["next_page"];
    if (!next.is_null()
        && next
            .as_str()
            .is_none_or(|next| next.is_empty() || requested_page == Some(next)))
        || messages
            .iter()
            .any(|message| !valid_sent_message(message, position))
    {
        return Err(invalid_response(request_id));
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
    fn legacy_resolution_checks_a_cached_alias_against_the_server() {
        use crate::test_tls::FixtureResponse;

        let mut message = json!({"item_id":"cached","sender":"app:test",
            "rev":4,"revision":1,"message":{},"seen":false,"files":[]});
        let modern = FixtureResponse::json(json!({"rev":4,"message":message}));
        message["item_id"] = "retained".into();
        message["legacy_message_id"] = "local".into();
        let server = SlowServer::sequence(vec![
            modern,
            FixtureResponse::json(json!({"rev":4,"messages":[message]})),
        ]);
        let result = client_for(&server)
            .resolve_legacy_message("app:test", "local", Some("cached"))
            .unwrap();
        assert_eq!(result["message"]["item_id"], "retained");
        let requests = server.requests();
        assert_eq!(
            requests[0].line.trim_end(),
            "GET /v2/agent/messages/cached HTTP/1.1"
        );
        assert_eq!(
            requests[1].line.trim_end(),
            "GET /v2/agent/messages?sender=app%3Atest&legacy_message_id=local HTTP/1.1",
        );
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
