use std::{fs::File, path::Path};

use reqwest::{
    blocking::{Body, Response},
    header::{ACCEPT, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE, ETAG},
};
use serde_json::Value;
use uuid::Uuid;

use super::{AgentClient, V2Error, header};

pub struct Download {
    pub response: Response,
    pub sha256: String,
    pub size: u64,
}

impl AgentClient {
    pub fn upload_file(
        &self,
        key: Uuid,
        path: &Path,
        name: &str,
        size: u64,
    ) -> Result<Value, V2Error> {
        let file = File::open(path).map_err(V2Error::LocalFile)?;
        if file.metadata().map_err(V2Error::LocalFile)?.len() != size {
            return Err(V2Error::LocalFile(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "staged file size changed",
            )));
        }
        let request = self
            .client
            .post(self.url(&["files"])?)
            .header("Idempotency-Key", key.to_string())
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/octet-stream")
            .header(CONTENT_LENGTH, size)
            .header(CONTENT_DISPOSITION, file_disposition(name))
            .body(Body::sized(file, size));
        self.read_json(request)
    }

    pub fn download_file(&self, file_id: &str) -> Result<Download, V2Error> {
        let response = self.send_checked(self.client.get(self.url(&["files", file_id])?))?;
        let invalid = || V2Error::InvalidResponse {
            request_id: header(&response, "Request-Id").map(str::to_owned),
        };
        let sha256 = response
            .headers()
            .get(ETAG)
            .and_then(|tag| tag.to_str().ok())
            .and_then(|tag| tag.strip_prefix('"')?.strip_suffix('"'))
            .filter(|digest| digest.len() == 64 && digest.bytes().all(is_lower_hex))
            .ok_or_else(invalid)?
            .to_owned();
        let size = response.content_length().ok_or_else(invalid)?;
        Ok(Download {
            response,
            sha256,
            size,
        })
    }
}

impl Download {
    pub fn save(&mut self, destination: &Path) -> Result<(), String> {
        crate::files::persist_download(destination, &mut self.response, self.size, &self.sha256)
    }
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn file_disposition(name: &str) -> String {
    use std::fmt::Write;

    let mut header = "attachment; filename*=UTF-8''".to_owned();
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            header.push(char::from(byte));
        } else {
            write!(&mut header, "%{byte:02X}").unwrap();
        }
    }
    header
}

#[cfg(test)]
mod tests {
    use std::{io::Read, time::Duration};

    use crate::{
        config::EnrolledIdentity,
        test_tls::{SlowServer, TLS_NAME},
    };

    use super::*;

    fn client_for(server: &SlowServer) -> AgentClient {
        AgentClient::new(EnrolledIdentity::for_test(
            "127.0.0.1",
            TLS_NAME,
            server.port,
            server.ca_pem.as_bytes(),
        ))
        .unwrap()
    }

    #[test]
    fn upload_sends_exact_bytes_with_filename_and_operation_key() {
        let path = std::env::temp_dir().join(format!("harklight-v2-wire-{}", Uuid::new_v4()));
        let bytes = b"\x00\x01\xff\n";
        std::fs::write(&path, bytes).unwrap();
        let key = Uuid::new_v4();
        let server =
            SlowServer::respond(Duration::ZERO, "201 Created", &[], r#"{"file_id":"one"}"#);
        let result =
            client_for(&server).upload_file(key, &path, "f + résumé.jpg", bytes.len() as u64);
        std::fs::remove_file(path).unwrap();
        assert_eq!(result.unwrap()["file_id"], "one");
        let request = server.request();
        assert_eq!(request.line.trim_end(), "POST /v2/files HTTP/1.1");
        assert_eq!(request.body, bytes);
        assert!(
            request
                .headers
                .contains(&("idempotency-key".into(), key.to_string()))
        );
        assert!(request.headers.contains(&(
            "content-disposition".into(),
            "attachment; filename*=UTF-8''f%20%2B%20r%C3%A9sum%C3%A9.jpg".into()
        )));
    }

    #[test]
    fn download_returns_the_byte_stream_and_validated_length_and_digest_metadata() {
        let server = SlowServer::respond(
            Duration::ZERO,
            "200 OK",
            &[(
                "ETag",
                "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            )],
            "content",
        );
        let mut download = client_for(&server).download_file("opaque/file").unwrap();
        assert_eq!(download.size, 7);
        assert_eq!(download.sha256, "a".repeat(64));
        let mut got = Vec::new();
        download.response.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"content");
        assert_eq!(
            server.request_line().trim_end(),
            "GET /v2/files/opaque%2Ffile HTTP/1.1"
        );
    }
    #[test]
    fn invalid_download_does_not_publish_partial_bytes() {
        let server = SlowServer::respond(
            Duration::ZERO,
            "200 OK",
            &[(
                "ETag",
                "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
            )],
            "content",
        );
        let path = std::env::temp_dir().join(format!("harklight-v2-download-{}", Uuid::new_v4()));
        let mut download = client_for(&server).download_file("one").unwrap();
        assert!(download.save(&path).unwrap_err().contains("SHA-256"));
        assert!(!path.exists());
        server.request();
    }
}
