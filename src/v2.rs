//! Transport for the agent API on the enrolled device listener.

use reqwest::{
    Method, Url,
    blocking::{Client, RequestBuilder, Response},
    header::{ACCEPT, CONTENT_TYPE, IF_MATCH},
};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::config::{ConfigError, EnrolledIdentity};

#[derive(Clone)]
pub struct AgentClient {
    client: Client,
    base: Url,
    address: String,
    server_binding: String,
    identity: EnrolledIdentity,
    ack2: bool,
}

impl AgentClient {
    pub fn new(identity: EnrolledIdentity) -> Result<Self, ConfigError> {
        let client = identity.device_client()?;
        let base = identity.device_url("/v2/")?;
        Ok(Self {
            client,
            base,
            server_binding: identity.server_binding(),
            address: identity.address.clone(),
            identity,
            ack2: false,
        })
    }

    pub(crate) fn server_binding(&self) -> &str {
        &self.server_binding
    }

    pub fn read(&self, path: &[&str], query: &[(&str, &str)]) -> Result<Value, V2Error> {
        self.read_correlated(path, query).map(|(value, _)| value)
    }

    fn read_correlated(
        &self,
        path: &[&str],
        query: &[(&str, &str)],
    ) -> Result<(Value, Option<String>), V2Error> {
        let mut url = self.url(path)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        self.read_json_correlated(self.client.get(url).header(ACCEPT, "application/json"))
    }

    /// The caller provides the journaled bytes and key for both first
    /// delivery and retries. This method expects a JSON response.
    pub fn write(
        &self,
        method: Method,
        path: &[&str],
        write: JsonWrite<'_>,
    ) -> Result<Value, V2Error> {
        self.write_correlated(method, path, write)
            .map(|(value, _)| value)
    }

    pub(crate) fn write_correlated(
        &self,
        method: Method,
        path: &[&str],
        write: JsonWrite<'_>,
    ) -> Result<(Value, Option<String>), V2Error> {
        let mut request = self
            .client
            .request(method, self.url(path)?)
            .header(ACCEPT, "application/json")
            .header("Idempotency-Key", write.key.to_string());
        if let Some(tag) = write.if_match {
            request = request.header(IF_MATCH, tag);
        }
        if !write.body.is_empty() {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(write.body.to_vec());
        }
        self.read_json_correlated(request)
    }

    pub fn delete(&self, path: &[&str], key: Uuid) -> Result<(), V2Error> {
        let response = self.send_checked(
            self.client
                .delete(self.url(path)?)
                .header("Idempotency-Key", key.to_string()),
        )?;
        if response.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(());
        }
        Err(V2Error::InvalidResponse {
            request_id: header(&response, "Request-Id").map(str::to_owned),
        })
    }

    fn url(&self, path: &[&str]) -> Result<Url, V2Error> {
        let mut url = self.base.clone();
        let mut segments = url
            .path_segments_mut()
            .expect("HTTPS supports path segments");
        segments.pop_if_empty();
        for segment in path {
            if matches!(*segment, "" | "." | "..") {
                return Err(V2Error::InvalidPath);
            }
            segments.push(segment);
        }
        drop(segments);
        Ok(url)
    }

    fn send(&self, request: RequestBuilder) -> Result<Response, V2Error> {
        request
            .send()
            .map_err(|source| self.transport_error(source))
    }

    fn send_checked(&self, request: RequestBuilder) -> Result<Response, V2Error> {
        let response = self.send(request)?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let request_id = header(&response, "Request-Id").map(str::to_owned);
        let retry_after = header(&response, "Retry-After").and_then(|seconds| seconds.parse().ok());
        let body = response
            .bytes()
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ErrorBody>(&bytes).ok());
        let request_id =
            request_id.or_else(|| body.as_ref().and_then(|body| body.request_id.clone()));
        if (400..500).contains(&status)
            && !body.as_ref().is_some_and(|body| {
                body.error.as_deref().and_then(error_status) == Some(status)
                    && body.retryable == Some(false)
                    && body
                        .request_id
                        .as_ref()
                        .is_some_and(|id| !id.is_empty() && id.chars().count() <= 128)
            })
        {
            return Err(V2Error::UnexpectedClientError { status, request_id });
        }
        let (token, field) = body
            .and_then(|body| {
                body.error
                    .filter(|token| error_status(token).is_some())
                    .map(|token| (token, body.field))
            })
            .map_or((None, None), |(token, field)| (Some(token), field));
        Err(V2Error::Http {
            status,
            request_id,
            token,
            field,
            retry_after,
        })
    }

    fn read_json(&self, request: RequestBuilder) -> Result<Value, V2Error> {
        self.read_json_correlated(request).map(|(value, _)| value)
    }

    fn read_json_correlated(
        &self,
        request: RequestBuilder,
    ) -> Result<(Value, Option<String>), V2Error> {
        let response = self.send_checked(request)?;
        let status = response.status().as_u16();
        let request_id = header(&response, "Request-Id").map(str::to_owned);
        let bytes = response.bytes().map_err(|source| V2Error::ResponseRead {
            status,
            request_id: request_id.clone(),
            source,
        })?;
        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
            .map(|value| (value, request_id.clone()))
            .ok_or(V2Error::InvalidResponse { request_id })
    }

    fn transport_error(&self, source: reqwest::Error) -> V2Error {
        V2Error::Transport {
            address: self.address.clone(),
            source,
        }
    }
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response.headers().get(name)?.to_str().ok()
}

pub struct JsonWrite<'a> {
    pub key: Uuid,
    pub body: &'a [u8],
    pub if_match: Option<&'a str>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
    retryable: Option<bool>,
    request_id: Option<String>,
    field: Option<String>,
}

fn error_status(token: &str) -> Option<u16> {
    Some(match token {
        "invalid_field" | "unsupported" => 400,
        "unauthorized" => 401,
        "forbidden" | "account_paused" => 403,
        "not_found" => 404,
        "stale_request"
        | "already_answered"
        | "already_closed"
        | "already_retracted"
        | "not_answered"
        | "idempotency_conflict" => 409,
        "cursor_expired" => 410,
        "precondition_failed" => 412,
        "payload_too_large" => 413,
        "internal_error" => 500,
        "retry_later" | "storage_unavailable" => 503,
        _ => return None,
    })
}

#[derive(Debug, Error)]
pub enum V2Error {
    #[error("server request via configured address {address} failed: {source}")]
    Transport {
        address: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("invalid server request path")]
    InvalidPath,
    #[error("local file failed before the request: {0}")]
    LocalFile(#[source] std::io::Error),
    #[error("server sent an unexpected response (request ID {request_id:?})")]
    InvalidResponse { request_id: Option<String> },
    #[error(
        "HTTP {status} response was outside the Harklight API. Check the configured address or device port."
    )]
    UnexpectedClientError {
        status: u16,
        request_id: Option<String>,
    },
    #[error("could not read HTTP {status} response (request ID {request_id:?}): {source}")]
    ResponseRead {
        status: u16,
        request_id: Option<String>,
        #[source]
        source: reqwest::Error,
    },
    #[error("server returned HTTP {status}: {token:?} (request ID {request_id:?})")]
    Http {
        status: u16,
        token: Option<String>,
        field: Option<String>,
        request_id: Option<String>,
        retry_after: Option<u64>,
    },
}

impl V2Error {
    pub fn is_internal_error(&self) -> bool {
        matches!(self, Self::Http { status: 500, token: Some(token), .. } if token == "internal_error")
    }

    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Transport { .. }
            | Self::InvalidResponse { .. }
            | Self::UnexpectedClientError { .. }
            | Self::ResponseRead { .. } => true,
            Self::Http { status, .. } => *status >= 500,
            Self::InvalidPath | Self::LocalFile(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;
    use crate::test_tls::{SlowServer, TLS_NAME};

    #[test]
    fn reads_on_the_enrolled_device_port_with_encoded_ids_and_queries() {
        let server = SlowServer::answer_after(Duration::ZERO, r#"{"rev":1,"entries":[]}"#);
        let identity = EnrolledIdentity::for_test(
            "127.0.0.1",
            TLS_NAME,
            server.port,
            server.ca_pem.as_bytes(),
        );
        let client = AgentClient::new(identity).unwrap();
        let response = client
            .read(
                &["agent", "messages", "item/one", "entries"],
                &[("before", "+&")],
            )
            .unwrap();

        assert_eq!(response, json!({"rev":1,"entries":[]}));
        assert_eq!(
            server.request_line().trim_end(),
            "GET /v2/agent/messages/item%2Fone/entries?before=%2B%26 HTTP/1.1"
        );
    }

    #[test]
    fn put_sends_the_saved_body_and_key_without_reserializing() {
        let server = SlowServer::answer_after(
            Duration::ZERO,
            r#"{"rev":2,"message":{"answer":18446744073709551617}}"#,
        );
        let client = client_for(&server);
        let key = Uuid::new_v4();
        let body = br#"{ "message" : { "title":"Progress" }, "file_ids" : [ "abc" ] }"#;
        let response = client
            .write(
                Method::PUT,
                &["agent", "messages", "one"],
                JsonWrite {
                    key,
                    body,
                    if_match: Some(r#""1""#),
                },
            )
            .unwrap();

        assert_eq!(
            response["message"]["answer"].to_string(),
            "18446744073709551617"
        );
        let request = server.request();
        assert_eq!(
            request.line.trim_end(),
            "PUT /v2/agent/messages/one HTTP/1.1"
        );
        assert!(
            request
                .headers
                .contains(&("idempotency-key".into(), key.to_string()))
        );
        assert!(
            request
                .headers
                .contains(&("if-match".into(), r#""1""#.into()))
        );
        assert_eq!(request.body, body);
    }

    #[test]
    fn service_refusal_preserves_retry_after_and_request_id() {
        let server = SlowServer::respond(
            Duration::ZERO,
            "503 Service Unavailable",
            &[("Retry-After", "2"), ("Request-Id", "req-1")],
            r#"{"error":"storage_unavailable","retryable":true,"request_id":"req-1"}"#,
        );
        let error = client_for(&server).read(&["me"], &[]).unwrap_err();

        assert!(error.outcome_unknown());
        assert!(matches!(
            error,
            V2Error::Http { status: 503, token: Some(token), request_id: Some(id), retry_after: Some(2), .. }
                if token == "storage_unavailable" && id == "req-1"
        ));
        assert_eq!(server.request_line().trim_end(), "GET /v2/me HTTP/1.1");
    }

    #[test]
    fn genuine_device_mutual_tls_requires_the_enrolled_certificate() {
        let (server, test_identity) = SlowServer::mutual_tls(r#"{"rev":0,"messages":[]}"#);
        let identity = EnrolledIdentity::for_test(
            "127.0.0.1",
            TLS_NAME,
            server.port,
            server.ca_pem.as_bytes(),
        )
        .with_test_identity(test_identity);
        let response = AgentClient::new(identity)
            .unwrap()
            .messages(None, None)
            .unwrap();
        assert_eq!(response["messages"], json!([]));
        assert_eq!(
            server.request_line().trim_end(),
            "GET /v2/agent/messages HTTP/1.1"
        );
    }

    #[test]
    fn truncated_503_keeps_status_and_response_headers() {
        let server = SlowServer::truncated(
            "503 Service Unavailable",
            &[("Request-Id", "header-id"), ("Retry-After", "9")],
            r#"{"error":"storage_unavailable""#,
        );
        let error = client_for(&server).read(&["me"], &[]).unwrap_err();
        assert!(error.outcome_unknown());
        assert!(matches!(
            error,
            V2Error::Http {
                status: 503, token: None, request_id: Some(id), retry_after: Some(9), ..
            } if id == "header-id"
        ));
        server.request();
    }

    #[test]
    fn closed_error_tokens_and_request_id_precedence() {
        let bad_token = SlowServer::respond(
            Duration::ZERO,
            "409 Conflict",
            &[("Request-Id", "header-id")],
            r#"{"error":"unexpected_detail","retryable":false,"request_id":"body-id"}"#,
        );
        let error = client_for(&bad_token).read(&["me"], &[]).unwrap_err();
        assert!(!error.to_string().contains("unexpected_detail"));
        assert!(matches!(
            error,
            V2Error::UnexpectedClientError { status: 409, request_id: Some(id) } if id == "header-id"
        ));
        bad_token.request();

        let no_body_id = SlowServer::respond(
            Duration::ZERO,
            "400 Bad Request",
            &[("Request-Id", "request-id")],
            r#"{"error":"invalid_field","retryable":false,"field":"sender"}"#,
        );
        let error = client_for(&no_body_id).read(&["me"], &[]).unwrap_err();
        assert!(matches!(
            error,
            V2Error::UnexpectedClientError { status: 400, request_id: Some(id) } if id == "request-id"
        ));
        no_body_id.request();

        let valid = SlowServer::respond(
            Duration::ZERO,
            "400 Bad Request",
            &[("Request-Id", "header-id")],
            r#"{"error":"invalid_field","retryable":false,"field":"sender","request_id":"body-id"}"#,
        );
        let error = client_for(&valid).read(&["me"], &[]).unwrap_err();
        assert!(matches!(
            error,
            V2Error::Http { status: 400, token: Some(token), field: Some(field), request_id: Some(id), .. }
                if token == "invalid_field" && field == "sender" && id == "header-id"
        ));
        valid.request();
    }

    #[test]
    fn deleting_an_upload_accepts_bodyless_success() {
        let server = SlowServer::respond(Duration::ZERO, "204 No Content", &[], "");
        let key = Uuid::new_v4();
        client_for(&server)
            .delete(&["files", "file/id"], key)
            .unwrap();
        let request = server.request();
        assert_eq!(
            request.line.trim_end(),
            "DELETE /v2/files/file%2Fid HTTP/1.1"
        );
        assert!(request.body.is_empty());
        assert!(
            request
                .headers
                .contains(&("idempotency-key".into(), key.to_string()))
        );
    }

    #[test]
    fn enrolled_https_calls_do_not_follow_redirects() {
        let server = SlowServer::respond(
            Duration::ZERO,
            "307 Temporary Redirect",
            &[("Location", "/other")],
            "",
        );
        let result = client_for(&server).write(
            Method::POST,
            &["agent", "messages"],
            JsonWrite {
                key: Uuid::new_v4(),
                body: b"{}",
                if_match: None,
            },
        );
        assert!(matches!(result, Err(V2Error::Http { status: 307, .. })));
        assert_eq!(
            server.request_line().trim_end(),
            "POST /v2/agent/messages HTTP/1.1"
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

mod reads;
pub mod writes;

mod files;
pub use files::Download;

pub mod sends;

pub mod mutations;

pub mod feed;
