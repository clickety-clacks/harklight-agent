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
}

impl AgentClient {
    pub fn new(identity: EnrolledIdentity) -> Result<Self, ConfigError> {
        let client = identity.client_for_port(identity.port)?;
        let base = identity.origin_url(identity.port, "/v2/")?;
        Ok(Self {
            client,
            base,
            address: identity.address,
        })
    }

    pub fn read(&self, path: &[&str], query: &[(&str, &str)]) -> Result<Value, V2Error> {
        let mut url = self.url(path)?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        self.read_json(self.client.get(url).header(ACCEPT, "application/json"))
    }

    /// The caller provides the journaled bytes and key for both first
    /// delivery and retries. Only POST and PUT have JSON request bodies.
    pub fn write(
        &self,
        method: Method,
        path: &[&str],
        write: JsonWrite<'_>,
    ) -> Result<Value, V2Error> {
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
        self.read_json(request)
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

    fn read_json(&self, request: RequestBuilder) -> Result<Value, V2Error> {
        let response = self.send(request)?;
        let status = response.status().as_u16();
        let request_id = header(&response, "Request-Id").map(str::to_owned);
        let retry_after = header(&response, "Retry-After").and_then(|seconds| seconds.parse().ok());
        let bytes = response
            .bytes()
            .map_err(|source| self.transport_error(source))?;

        if !(200..300).contains(&status) {
            let body = serde_json::from_slice::<ErrorBody>(&bytes).ok();
            return Err(V2Error::Http {
                status,
                request_id: request_id
                    .or_else(|| body.as_ref().map(|body| body.request_id.clone())),
                token: body.map(|body| body.error),
                retry_after,
            });
        }

        serde_json::from_slice::<Value>(&bytes)
            .ok()
            .filter(Value::is_object)
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
    error: String,
    request_id: String,
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
    #[error("server response was not a JSON object (request ID {request_id:?})")]
    InvalidResponse { request_id: Option<String> },
    #[error("server returned HTTP {status}: {token:?} (request ID {request_id:?})")]
    Http {
        status: u16,
        token: Option<String>,
        request_id: Option<String>,
        retry_after: Option<u64>,
    },
}

impl V2Error {
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Transport { .. } | Self::InvalidResponse { .. } => true,
            Self::Http { status, .. } => *status >= 500,
            Self::InvalidPath => false,
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
        let mut identity = EnrolledIdentity::for_test(
            "127.0.0.1",
            TLS_NAME,
            server.port,
            server.ca_pem.as_bytes(),
        );
        identity.origin_port = Some(1);
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
            V2Error::Http { status: 503, token: Some(token), request_id: Some(id), retry_after: Some(2) }
                if token == "storage_unavailable" && id == "req-1"
        ));
        assert_eq!(server.request_line().trim_end(), "GET /v2/me HTTP/1.1");
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
