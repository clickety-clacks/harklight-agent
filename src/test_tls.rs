//! A one-request TLS 1.3 server for tests that need the real HTTPS clients to
//! wait on a slow Server answer.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};

pub const TLS_NAME: &str = "harklight.test";

pub struct SlowServer {
    pub port: u16,
    pub ca_pem: String,
    handle: JoinHandle<RecordedRequest>,
}

pub struct RecordedRequest {
    pub line: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl SlowServer {
    /// Accept one HTTPS request, wait `delay`, then answer `200` with `body`.
    pub fn answer_after(delay: Duration, body: &'static str) -> Self {
        Self::respond(delay, "200 OK", &[], body)
    }

    pub fn respond(
        delay: Duration,
        status: &'static str,
        response_headers: &'static [(&'static str, &'static str)],
        body: &'static str,
    ) -> Self {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();

        let server_key = KeyPair::generate().unwrap();
        let server_cert = CertificateParams::new(vec![TLS_NAME.to_string()])
            .unwrap()
            .signed_by(&server_key, &ca)
            .unwrap();

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(server_cert.der().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
            )
            .unwrap();
        let config = Arc::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            let connection = ServerConnection::new(config).unwrap();
            let mut reader = BufReader::new(StreamOwned::new(connection, tcp));
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut content_length = 0usize;
            let mut headers = Vec::new();
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let (name, value) = header.split_once(':').unwrap();
                let value = value.trim();
                if name.eq_ignore_ascii_case("content-length") {
                    content_length = value.parse().unwrap();
                }
                headers.push((name.to_ascii_lowercase(), value.to_owned()));
            }
            let mut request_body = vec![0; content_length];
            reader.read_exact(&mut request_body).unwrap();

            thread::sleep(delay);

            let mut stream = reader.into_inner();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                body.len()
            )
            .unwrap();
            for (name, value) in response_headers {
                write!(stream, "{name}: {value}\r\n").unwrap();
            }
            write!(stream, "\r\n{body}").unwrap();
            stream.flush().unwrap();
            stream.conn.send_close_notify();
            let _ = stream.conn.complete_io(&mut stream.sock);
            RecordedRequest {
                line: request_line,
                headers,
                body: request_body,
            }
        });

        Self {
            port,
            ca_pem: ca.as_ref().pem(),
            handle,
        }
    }

    /// The request line the server received.
    pub fn request_line(self) -> String {
        self.request().line
    }

    pub fn request(self) -> RecordedRequest {
        self.handle.join().unwrap()
    }
}
