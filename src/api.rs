use std::time::Duration;
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::blocking::{Client, Response};
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
            Self::Transport(_) | Self::InvalidResponse(_) => true,
            Self::Http { status, .. } => *status >= 500,
            _ => false,
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

    pub fn post(&self, command: &Value) -> Result<Value, ApiError> {
        let url = self.identity.origin_url(self.port, "/origin/v1")?;
        let response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .timeout(Duration::from_secs(30))
            .json(command)
            .send()?;
        parse_post_response(response)
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
        let response = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::CONTENT_LENGTH, size)
            .header("X-Harklight-Op-Id", op_id.to_string())
            .header("X-Harklight-Sender-Id", sender_id)
            .header("X-Harklight-Message-Id", message_id)
            .header("X-Harklight-Request-Revision", request_revision.to_string())
            .header("X-Harklight-File-Name", encoded_name)
            .body(reqwest::blocking::Body::new(file))
            .send()?;
        parse_post_response(response)
    }

    pub fn download_file(&self, upload_id: Uuid) -> Result<DownloadedFile, ApiError> {
        let path = format!("/origin/v1/files/{upload_id}");
        let url = self.identity.origin_url(self.port, &path)?;
        let response = self.client.get(url).send()?;
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
        let path = format!("/origin/v1/stream?session={session}");
        let url = self.identity.origin_url(self.port, &path)?;
        let response = self.client.get(url).send()?;
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

fn parse_post_response(response: Response) -> Result<Value, ApiError> {
    let status = response.status().as_u16();
    let body = response.text()?;
    if !(200..300).contains(&status) {
        return Err(ApiError::Http { status, body });
    }
    let value: Value = serde_json::from_str(&body)
        .map_err(|error| ApiError::InvalidResponse(error.to_string()))?;
    if value["v"] != 1 || !matches!(value["type"].as_str(), Some("ok" | "error")) {
        return Err(ApiError::InvalidResponse(
            "missing v:1 or type ok/error".into(),
        ));
    }
    Ok(value)
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
            let mut line = String::new();
            let read = self.reader.read_line(&mut line)?;
            if read == 0 {
                return Err(ApiError::StreamEnded);
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                if self.event == "change" && !self.data.is_empty() {
                    let data = self.data.join("\n");
                    let cursor = serde_json::from_str::<Value>(&data)
                        .ok()
                        .and_then(|value| value["cursor"].as_u64())
                        .ok_or_else(|| ApiError::InvalidNotice(data.clone()))?;
                    self.event.clear();
                    self.data.clear();
                    return Ok(cursor);
                }
                self.event.clear();
                self.data.clear();
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
    fn sse_decoder_ignores_keepalives_and_reads_cursor_only_change_notices() {
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
}
