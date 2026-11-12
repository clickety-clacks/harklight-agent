use std::time::Duration;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::config::{ConfigError, EnrolledIdentity};

#[derive(Debug, Error)]
pub enum ApiError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("origin request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("origin request via configured address {address} failed: {source}")]
    TransportAddress {
        address: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("origin returned HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("origin response was not a valid v1 JSON envelope: {0}")]
    InvalidResponse(String),
    #[error("origin refused the request: {0}")]
    Refused(String),
    #[error("origin sent an invalid SSE change notice: {0}")]
    InvalidNotice(String),
    #[error("local file failed before the origin request: {0}")]
    Local(String),
    #[error("origin SSE stream ended")]
    StreamEnded,
    #[error("origin SSE read failed: {0}")]
    StreamIo(#[from] std::io::Error),
}

impl ApiError {
    /// A failed POST may have committed if no valid application envelope came
    /// back. Such calls must retain and retry the same operation key.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Transport(_) | Self::TransportAddress { .. } | Self::InvalidResponse(_) => true,
            Self::Http { status, .. } => *status >= 500,
            _ => false,
        }
    }

    /// The server's stated refusal reason, when the response carries a valid
    /// v1 error envelope.
    pub fn refusal_reason(&self) -> Option<String> {
        match self {
            Self::Refused(reason) => Some(reason.clone()),
            Self::Http { body, .. } => serde_json::from_str::<Value>(body)
                .ok()
                .filter(|value| value["v"] == 1 && value["type"] == "error")
                .and_then(|value| value["reason"].as_str().map(str::to_owned)),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct OriginClient {
    client: Client,
    identity: EnrolledIdentity,
    port: u16,
}

pub struct DownloadedFile {
    pub response: Response,
    pub sha256: Option<String>,
    pub content_length: Option<u64>,
}

impl OriginClient {
    pub fn new(identity: EnrolledIdentity, origin_port: Option<u16>) -> Result<Self, ApiError> {
        let port = identity.resolve_origin_port(origin_port)?;
        let client = identity.client_for_port(port)?;
        Ok(Self {
            client,
            identity,
            port,
        })
    }

    pub fn device_id(&self) -> &str {
        &self.identity.device_id
    }

    fn send(&self, request: RequestBuilder) -> Result<Response, ApiError> {
        request.send().map_err(|source| ApiError::TransportAddress {
            address: self.identity.address.clone(),
            source,
        })
    }

    pub fn post(&self, command: &Value) -> Result<Value, ApiError> {
        let url = self.identity.origin_url(self.port, "/origin/v1")?;
        let response = self.send(
            self.client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .timeout(Duration::from_secs(30))
                .json(command),
        )?;
        parse_post_response(response, &self.identity.address)
    }

    pub fn upload_file(
        &self,
        op_id: Uuid,
        sender_id: &str,
        message_id: &str,
        request_revision: u64,
        name: &str,
        path: &Path,
        size: u64,
    ) -> Result<Value, ApiError> {
        let url = self.identity.origin_url(self.port, "/origin/v1/upload")?;
        let file = File::open(path).map_err(|error| {
            ApiError::Local(format!("could not open staged upload {name}: {error}"))
        })?;
        let actual_size = file
            .metadata()
            .map_err(|error| {
                ApiError::Local(format!("could not inspect staged upload {name}: {error}"))
            })?
            .len();
        if actual_size != size {
            return Err(ApiError::Local(format!(
                "staged upload {name} changed size from {size} to {actual_size}"
            )));
        }
        let encoded_name = URL_SAFE_NO_PAD.encode(name.as_bytes());
        let response = self.send(
            self.client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .header(reqwest::header::CONTENT_LENGTH, size)
                .header("X-Harklight-Op-Id", op_id.to_string())
                .header("X-Harklight-Sender-Id", sender_id)
                .header("X-Harklight-Message-Id", message_id)
                .header("X-Harklight-Request-Revision", request_revision.to_string())
                .header("X-Harklight-File-Name", encoded_name)
                .body(reqwest::blocking::Body::new(file)),
        )?;
        parse_post_response(response, &self.identity.address)
    }

    pub fn download_file(&self, upload_id: Uuid) -> Result<DownloadedFile, ApiError> {
        let path = format!("/origin/v1/files/{upload_id}");
        let url = self.identity.origin_url(self.port, &path)?;
        let response = self.send(self.client.get(url))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().unwrap_or_default();
            return Err(ApiError::Http { status, body });
        }
        let sha256 = response
            .headers()
            .get("X-Harklight-Sha256")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let content_length = response.content_length();
        Ok(DownloadedFile {
            response,
            sha256,
            content_length,
        })
    }

    pub fn connect_stream(&self, session: Uuid) -> Result<SseStream, ApiError> {
        self.open_stream(session, false, false)
    }

    pub fn connect_advisory_stream(&self, session: Uuid) -> Result<SseStream, ApiError> {
        self.open_stream(session, false, true)
    }

    pub fn reconnect_stream(&self, session: Uuid) -> Result<SseStream, ApiError> {
        self.open_stream(session, true, false)
    }

    pub fn reconnect_advisory_stream(&self, session: Uuid) -> Result<SseStream, ApiError> {
        self.open_stream(session, true, true)
    }

    fn open_stream(
        &self,
        session: Uuid,
        reconnect: bool,
        advisory_v1: bool,
    ) -> Result<SseStream, ApiError> {
        let path = origin_stream_path(session, reconnect, advisory_v1);
        self.open_stream_path(&path)
    }

    pub fn connect_origin_stream(&self) -> Result<SseStream, ApiError> {
        self.open_stream_path("/origin/v1/stream?scope=origin&reconnect=1")
    }

    pub fn connect_focus_stream(&self) -> Result<SseStream, ApiError> {
        self.open_stream_path("/origin/v1/stream?scope=focus")
    }

    fn open_stream_path(&self, path: &str) -> Result<SseStream, ApiError> {
        let url = self.identity.origin_url(self.port, path)?;
        let response = self.send(self.client.get(url))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().unwrap_or_default();
            return Err(ApiError::Http { status, body });
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !content_type.starts_with("text/event-stream") {
            let body = response.text().unwrap_or_default();
            let envelope: Value = serde_json::from_str(&body)
                .map_err(|error| ApiError::InvalidResponse(error.to_string()))?;
            if envelope["type"] == "error" {
                return Err(ApiError::Refused(format!(
                    "{}",
                    envelope["reason"].as_str().unwrap_or("unspecified")
                )));
            }
            return Err(ApiError::InvalidResponse(format!(
                "expected event stream, received {content_type}"
            )));
        }
        Ok(SseStream::new(response))
    }
}

fn origin_stream_path(session: Uuid, reconnect: bool, advisory_v1: bool) -> String {
    let mut path = format!("/origin/v1/stream?session={session}");
    if advisory_v1 {
        path.push_str("&advisory_v1=1");
    }
    if reconnect {
        path.push_str("&reconnect=1");
    }
    path
}

fn parse_post_response(response: Response, address: &str) -> Result<Value, ApiError> {
    let status = response.status().as_u16();
    let body = response
        .bytes()
        .map_err(|source| ApiError::TransportAddress {
            address: address.to_owned(),
            source,
        })?;
    parse_post_response_body(status, &body)
}

fn parse_post_response_body(status: u16, body: impl AsRef<[u8]>) -> Result<Value, ApiError> {
    let body = body.as_ref();
    if !(200..300).contains(&status) {
        return Err(ApiError::Http {
            status,
            body: String::from_utf8_lossy(body).into_owned(),
        });
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| ApiError::InvalidResponse(error.to_string()))?;
    if value["v"] != 1 || !matches!(value["type"].as_str(), Some("ok" | "error")) {
        return Err(ApiError::InvalidResponse(
            "missing v:1 or type ok/error".into(),
        ));
    }
    Ok(value)
}

#[derive(Debug, PartialEq, Eq)]
pub enum OriginNotice {
    Change(u64),
    AdvisoryChange { session_handle: Uuid, cursor: u64 },
    AdvisoryUnavailable,
    Coverage,
    Replaced,
    FocusChange(Value),
}

pub struct SseStream {
    decoder: SseDecoder<BufReader<Response>>,
}

struct SseDecoder<R> {
    reader: R,
    event: String,
    data: Vec<String>,
}

impl SseStream {
    fn new(response: Response) -> Self {
        Self {
            decoder: SseDecoder::new(BufReader::new(response)),
        }
    }

    /// Read one cursor-only change notice. Keep-alives and unrelated comments
    /// are ignored and never cause a feed request.
    pub fn next_change(&mut self) -> Result<u64, ApiError> {
        self.decoder.next_change()
    }

    pub fn next_notice(&mut self) -> Result<OriginNotice, ApiError> {
        self.decoder.next_notice()
    }
}

impl<R: BufRead> SseDecoder<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            event: String::new(),
            data: Vec::new(),
        }
    }

    pub fn next_change(&mut self) -> Result<u64, ApiError> {
        loop {
            if let OriginNotice::Change(cursor) = self.next_notice()? {
                return Ok(cursor);
            }
        }
    }

    pub fn next_notice(&mut self) -> Result<OriginNotice, ApiError> {
        loop {
            let mut line = String::new();
            let read = self.reader.read_line(&mut line)?;
            if read == 0 {
                return Err(ApiError::StreamEnded);
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                let data = self.data.join("\n");
                let notice = match self.event.as_str() {
                    "change" if !self.data.is_empty() => {
                        let cursor = serde_json::from_str::<Value>(&data)
                            .ok()
                            .and_then(|value| value["cursor"].as_u64())
                            .ok_or_else(|| ApiError::InvalidNotice(data.clone()))?;
                        Some(OriginNotice::Change(cursor))
                    }
                    "advisory_change" => {
                        let notice = serde_json::from_str::<Value>(&data)
                            .ok()
                            .and_then(|value| {
                                let session_handle = value["session_handle"]
                                    .as_str()
                                    .and_then(|value| Uuid::parse_str(value).ok())
                                    .filter(|value| value.get_version_num() == 4)?;
                                let cursor = value["advisory_cursor"].as_u64()?;
                                Some(OriginNotice::AdvisoryChange {
                                    session_handle,
                                    cursor,
                                })
                            })
                            .unwrap_or(OriginNotice::AdvisoryUnavailable);
                        Some(notice)
                    }
                    "coverage" => Some(OriginNotice::Coverage),
                    "replaced" => Some(OriginNotice::Replaced),
                    "focus_change" if !self.data.is_empty() => {
                        let value = serde_json::from_str::<Value>(&data)
                            .map_err(|_| ApiError::InvalidNotice(data.clone()))?;
                        Some(OriginNotice::FocusChange(value))
                    }
                    _ => None,
                };
                self.event.clear();
                self.data.clear();
                if let Some(notice) = notice {
                    return Ok(notice);
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.event = value.to_owned(),
                "data" => self.data.push(value.to_owned()),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn advisory_stream_opt_in_preserves_session_and_reconnect_parameters() {
        let session = Uuid::new_v4();
        assert_eq!(
            origin_stream_path(session, false, true),
            format!("/origin/v1/stream?session={session}&advisory_v1=1")
        );
        assert_eq!(
            origin_stream_path(session, true, true),
            format!("/origin/v1/stream?session={session}&advisory_v1=1&reconnect=1")
        );
        assert_eq!(
            origin_stream_path(session, true, false),
            format!("/origin/v1/stream?session={session}&reconnect=1")
        );
    }

    #[test]
    fn origin_notices_preserve_control_events() {
        let session = Uuid::new_v4();
        let frames = format!(
            "event: coverage\ndata: {{}}\n\nevent: change\ndata: {{\"cursor\":7}}\n\nevent: advisory_change\ndata: {{\"session_handle\":\"{session}\",\"advisory_cursor\":3}}\n\nevent: replaced\ndata: {{}}\n\n"
        );
        let mut decoder = SseDecoder::new(Cursor::new(frames.as_bytes()));
        assert_eq!(decoder.next_notice().unwrap(), OriginNotice::Coverage);
        assert_eq!(decoder.next_notice().unwrap(), OriginNotice::Change(7));
        assert_eq!(
            decoder.next_notice().unwrap(),
            OriginNotice::AdvisoryChange {
                session_handle: session,
                cursor: 3
            }
        );
        assert_eq!(decoder.next_notice().unwrap(), OriginNotice::Replaced);
    }

    #[test]
    fn focus_change_notice_preserves_the_server_data_object() {
        let frames = concat!(
            "event: focus_change\n",
            "data: {\"clients\":[{\"client_id\":\"abc\",\"screen\":\"chat\",\"reported_at\":17}]}\n\n"
        );
        let mut decoder = SseDecoder::new(Cursor::new(frames.as_bytes()));
        assert_eq!(
            decoder.next_notice().unwrap(),
            OriginNotice::FocusChange(serde_json::json!({
                "clients":[{
                    "client_id":"abc",
                    "screen":"chat",
                    "reported_at":17
                }]
            }))
        );
    }

    #[test]
    fn malformed_focus_change_data_is_rejected() {
        let frames = "event: focus_change\ndata: {invalid}\n\n";
        let mut decoder = SseDecoder::new(Cursor::new(frames.as_bytes()));
        assert!(matches!(
            decoder.next_notice(),
            Err(ApiError::InvalidNotice(_))
        ));
    }

    #[test]
    fn malformed_advisory_change_is_unavailable_and_does_not_block_ordinary_changes() {
        let session = Uuid::new_v4();
        let frames = format!(
            "event: advisory_change\ndata: {{\"session_handle\":\"{session}\",\"advisory_cursor\":\"1\"}}\n\nevent: change\ndata: {{\"cursor\":41}}\n\n"
        );
        let mut decoder = SseDecoder::new(Cursor::new(frames.as_bytes()));
        assert_eq!(
            decoder.next_notice().unwrap(),
            OriginNotice::AdvisoryUnavailable
        );
        assert_eq!(decoder.next_notice().unwrap(), OriginNotice::Change(41));
    }

    #[test]
    fn one_cursor_subscription_reads_multiple_change_notices() {
        // One stream remains open across feed changes; channel labels belong to
        // the authoritative feed entries fetched for those cursor updates.
        let frames = concat!(
            ": keepalive\n\n",
            "event: message\n",
            "data: {\"text\":\"not a Harklight turn\"}\n\n",
            "event: change\n",
            "data: {\"cursor\":41}\n\n",
            "event: change\n",
            "data: {\"cursor\":42}\n\n"
        );
        let mut decoder = SseDecoder::new(Cursor::new(frames.as_bytes()));
        assert_eq!(decoder.next_change().unwrap(), 41);
        assert_eq!(decoder.next_change().unwrap(), 42);
    }

    #[test]
    fn sse_decoder_rejects_change_frames_without_an_integer_cursor() {
        let frames = "event: change\ndata: {\"cursor\":\"41\"}\n\n";
        let mut decoder = SseDecoder::new(Cursor::new(frames.as_bytes()));
        assert!(matches!(
            decoder.next_change(),
            Err(ApiError::InvalidNotice(_))
        ));
    }

    #[test]
    fn refusal_reason_reads_only_valid_v1_error_envelopes() {
        assert_eq!(
            ApiError::Refused("revoked".into())
                .refusal_reason()
                .as_deref(),
            Some("revoked")
        );
        assert_eq!(
            ApiError::Http {
                status: 403,
                body: r#"{"v":1,"type":"error","reason":"revoked"}"#.into(),
            }
            .refusal_reason()
            .as_deref(),
            Some("revoked")
        );
        for body in [
            "not json",
            r#"{"v":2,"type":"error","reason":"revoked"}"#,
            r#"{"v":1,"type":"ok","reason":"revoked"}"#,
            r#"{"v":1,"type":"error"}"#,
        ] {
            assert_eq!(
                ApiError::Http {
                    status: 403,
                    body: body.into(),
                }
                .refusal_reason(),
                None
            );
        }
        assert_eq!(
            ApiError::InvalidResponse("revoked".into()).refusal_reason(),
            None
        );
    }

    #[test]
    fn post_response_preserves_large_answer_integers_exactly() {
        let response = parse_post_response_body(
            200,
            r#"{"v":1,"type":"ok","answer":{"issue":18446744073709551617}}"#,
        )
        .unwrap();

        assert_eq!(
            response["answer"]["issue"].to_string(),
            "18446744073709551617"
        );
        assert!(
            serde_json::to_string(&response)
                .unwrap()
                .contains("18446744073709551617")
        );
    }
}
