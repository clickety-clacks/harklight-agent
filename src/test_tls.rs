//! A one-request TLS 1.3 server for tests that need the real HTTPS clients to
//! wait on a slow Server answer.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
};
use rustls::{
    RootCertStore, ServerConfig, ServerConnection, StreamOwned,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
    server::WebPkiClientVerifier,
};

pub const TLS_NAME: &str = "harklight.test";

pub struct SlowServer {
    pub port: u16,
    pub ca_pem: String,
    handle: JoinHandle<Vec<RecordedRequest>>,
}

pub struct TestIdentity {
    pub certificate: Vec<u8>,
    pub key: Vec<u8>,
}

pub struct FixtureResponse {
    pub status: &'static str,
    pub headers: &'static [(&'static str, &'static str)],
    pub body: String,
    pub truncate: bool,
}

impl FixtureResponse {
    pub fn json(body: serde_json::Value) -> Self {
        Self {
            status: "200 OK",
            headers: &[],
            body: body.to_string(),
            truncate: false,
        }
    }
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
        Self::serve(
            delay,
            vec![FixtureResponse {
                status,
                headers: response_headers,
                body: body.to_owned(),
                truncate: false,
            }],
            false,
        )
        .0
    }

    /// Announce a longer body than the one sent, then close the connection.
    pub fn truncated(
        status: &'static str,
        headers: &'static [(&'static str, &'static str)],
        body: &'static str,
    ) -> Self {
        Self::serve(
            Duration::ZERO,
            vec![FixtureResponse {
                status,
                headers,
                body: body.to_owned(),
                truncate: true,
            }],
            false,
        )
        .0
    }

    pub fn mutual_tls(body: &'static str) -> (Self, TestIdentity) {
        let (server, identity) = Self::serve(
            Duration::ZERO,
            vec![FixtureResponse {
                status: "200 OK",
                headers: &[],
                body: body.to_owned(),
                truncate: false,
            }],
            true,
        );
        (server, identity.unwrap())
    }

    #[allow(dead_code)] // Used by the executable-level fixture crate.
    pub fn mutual_tls_sequence(responses: Vec<FixtureResponse>) -> (Self, TestIdentity) {
        let (server, identity) = Self::serve(Duration::ZERO, responses, true);
        (server, identity.unwrap())
    }

    pub fn sequence(responses: Vec<FixtureResponse>) -> Self {
        Self::serve(Duration::ZERO, responses, false).0
    }

    fn serve(
        delay: Duration,
        responses: Vec<FixtureResponse>,
        require_identity: bool,
    ) -> (Self, Option<TestIdentity>) {
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
        let builder = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap();
        let (builder, client_identity) = if require_identity {
            let mut roots = RootCertStore::empty();
            roots.add(ca.der().clone()).unwrap();
            let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .unwrap();
            let key = KeyPair::generate().unwrap();
            let mut params = CertificateParams::new(vec!["client".into()]).unwrap();
            params
                .extended_key_usages
                .push(ExtendedKeyUsagePurpose::ClientAuth);
            let cert = params.signed_by(&key, &ca).unwrap();
            (
                builder.with_client_cert_verifier(verifier),
                Some(TestIdentity {
                    certificate: cert.pem().into_bytes(),
                    key: key.serialize_pem().into_bytes(),
                }),
            )
        } else {
            (builder.with_no_client_auth(), None)
        };
        let config = builder
            .with_single_cert(
                vec![CertificateDer::from(server_cert.der().to_vec())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server_key.serialize_der())),
            )
            .unwrap();
        let config = Arc::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let mut recorded = Vec::with_capacity(responses.len());
            for response in responses {
                let (tcp, _) = listener.accept().unwrap();
                let connection = ServerConnection::new(Arc::clone(&config)).unwrap();
                let mut reader = BufReader::new(StreamOwned::new(connection, tcp));
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut request_content_length = 0usize;
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
                        request_content_length = value.parse().unwrap();
                    }
                    headers.push((name.to_ascii_lowercase(), value.to_owned()));
                }
                let mut request_body = vec![0; request_content_length];
                reader.read_exact(&mut request_body).unwrap();

                thread::sleep(delay);

                let mut stream = reader.into_inner();
                let FixtureResponse {
                    status,
                    headers: response_headers,
                    body,
                    truncate,
                } = response;
                let content_length = body.len() + if truncate { 10 } else { 0 };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {content_length}\r\nConnection: close\r\n",
                ).unwrap();
                if !response_headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                {
                    write!(stream, "Content-Type: application/json\r\n").unwrap();
                }
                for (name, value) in response_headers {
                    write!(stream, "{name}: {value}\r\n").unwrap();
                }
                write!(stream, "\r\n{body}").unwrap();
                stream.flush().unwrap();
                stream.conn.send_close_notify();
                let _ = stream.conn.complete_io(&mut stream.sock);
                recorded.push(RecordedRequest {
                    line: request_line,
                    headers,
                    body: request_body,
                });
            }
            recorded
        });

        (
            Self {
                port,
                ca_pem: ca.as_ref().pem(),
                handle,
            },
            client_identity,
        )
    }

    /// The request line the server received.
    pub fn request_line(self) -> String {
        self.request().line
    }

    pub fn request(self) -> RecordedRequest {
        let mut requests = self.requests();
        assert_eq!(requests.len(), 1);
        requests.pop().unwrap()
    }

    pub fn requests(self) -> Vec<RecordedRequest> {
        self.handle.join().unwrap()
    }
}
