use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

use rcgen::{
    CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, PKCS_RSA_SHA256, RsaKeySize,
};
use reqwest::{
    Certificate, Url,
    blocking::Client,
    header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::connect::{ConnectAddressResolver, resolve_configured_address};

const EXIT_LOCAL: i32 = 1;
const EXIT_REFUSED: i32 = 2;
const EXIT_UNSUPPORTED: i32 = 5;
const EXIT_UNKNOWN: i32 = 7;
const PENDING_LIFETIME_MS: u64 = 86_400_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct EnrollmentError {
    pub message: String,
    pub exit_code: i32,
}

impl EnrollmentError {
    fn local(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_LOCAL,
        }
    }

    fn refused(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_REFUSED,
        }
    }

    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_UNSUPPORTED,
        }
    }

    fn unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_UNKNOWN,
        }
    }
}

#[derive(Debug, Clone)]
struct Endpoint {
    address: String,
    tls_name: String,
    port: u16,
}

impl Endpoint {
    fn url(&self, path: &str) -> Result<Url, EnrollmentError> {
        Url::parse(&format!("https://{}:{}{path}", self.tls_name, self.port))
            .map_err(|error| EnrollmentError::local(format!("invalid TLS name: {error}")))
    }

    fn display(&self) -> String {
        format!(
            "{}:{} (TLS name {})",
            self.address, self.port, self.tls_name
        )
    }
}

#[derive(Debug)]
struct EnrollmentContext {
    identity_dir: PathBuf,
    ca_pem: Vec<u8>,
    ca_sha256: String,
    endpoint: Endpoint,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    address: String,
    name: String,
    port: u16,
    ca_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingFile {
    approval_code: String,
    status_secret: String,
    expires_at: u64,
    status_expires_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentFile {
    person_id: String,
    device_id: String,
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

#[derive(Debug)]
enum RequestFailure {
    Connect(String),
    Unknown(String),
    Local(String),
}

trait EnrollmentTransport {
    fn post(&self, path: &str, payload: &Value) -> Result<HttpResponse, RequestFailure>;
}

struct HttpsTransport {
    client: Client,
    endpoint: Endpoint,
}

impl HttpsTransport {
    fn new(endpoint: Endpoint, ca_pem: &[u8]) -> Result<Self, EnrollmentError> {
        let certificate = Certificate::from_pem(ca_pem).map_err(|error| {
            EnrollmentError::local(format!(
                "could not load the configured CA certificate: {error}"
            ))
        })?;
        let client = Client::builder()
            .tls_backend_rustls()
            .tls_certs_only([certificate])
            .dns_resolver(Arc::new(ConnectAddressResolver::new(
                endpoint.address.clone(),
                endpoint.tls_name.clone(),
            )))
            .tls_version_min(reqwest::tls::Version::TLS_1_3)
            .tls_version_max(reqwest::tls::Version::TLS_1_3)
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .https_only(true)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| EnrollmentError::local(format!("could not configure TLS: {error}")))?;
        Ok(Self { client, endpoint })
    }
}

impl EnrollmentTransport for HttpsTransport {
    fn post(&self, path: &str, payload: &Value) -> Result<HttpResponse, RequestFailure> {
        let body = serde_json::to_vec(payload)
            .map_err(|error| RequestFailure::Local(format!("could not encode request: {error}")))?;
        let url = self
            .endpoint
            .url(path)
            .map_err(|error| RequestFailure::Local(error.message))?;
        let response = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header(CONTENT_LENGTH, body.len().to_string())
            .header(CONNECTION, "close")
            .body(body)
            .send()
            .map_err(|error| {
                if error.is_connect() {
                    RequestFailure::Connect(error.to_string())
                } else {
                    RequestFailure::Unknown(error.to_string())
                }
            })?;
        let status = response.status().as_u16();
        let body = response
            .bytes()
            .map_err(|error| RequestFailure::Unknown(error.to_string()))?
            .to_vec();
        Ok(HttpResponse { status, body })
    }
}

pub fn start(
    identity_dir: Option<&Path>,
    ca_file: Option<&Path>,
    connect: Option<&str>,
    tls_name: Option<&str>,
    port: Option<&str>,
    label: Option<&str>,
    origin_port: Option<u16>,
) -> Result<(), EnrollmentError> {
    let label = required(label, "--label")?;
    validate_label(label)?;
    if origin_port == Some(0) {
        return Err(EnrollmentError::local("--origin-port must be nonzero"));
    }
    let context = make_context(identity_dir, ca_file, connect, tls_name, port)?;

    resolve_before_mutation(&context.endpoint)?;
    let transport = HttpsTransport::new(context.endpoint.clone(), &context.ca_pem)?;
    create_identity_directory(&context.identity_dir)?;
    let server = ServerFile {
        address: context.endpoint.address.clone(),
        name: context.endpoint.tls_name.clone(),
        port: context.endpoint.port,
        ca_sha256: context.ca_sha256.clone(),
        origin_port,
    };
    write_json_private(&context.identity_dir, "server.json", &server)?;

    let (certificate_pem, key_pem) = generate_identity()?;
    atomic_write(&context.identity_dir, "identity.key", key_pem.as_bytes())?;
    atomic_write(
        &context.identity_dir,
        "identity.pem",
        certificate_pem.as_bytes(),
    )?;

    let pending = submit_existing(
        &context.identity_dir,
        label,
        &transport,
        &context.endpoint,
        current_time_ms(),
    )?;
    write_json_private(&context.identity_dir, "pending.json", &pending)?;
    print_pending(&pending)
}

pub fn retry(
    identity_dir: Option<&Path>,
    ca_file: Option<&Path>,
    connect: Option<&str>,
    tls_name: Option<&str>,
    port: Option<&str>,
    label: Option<&str>,
) -> Result<(), EnrollmentError> {
    let label = required(label, "--label")?;
    validate_label(label)?;
    let context = make_context(identity_dir, ca_file, connect, tls_name, port)?;
    verify_existing_server(&context)?;
    resolve_before_mutation(&context.endpoint)?;
    let transport = HttpsTransport::new(context.endpoint.clone(), &context.ca_pem)?;
    let pending_path = context.identity_dir.join("pending.json");

    if let Some(pending) = read_optional_pending(&context.identity_dir)? {
        let state = query_status(
            &context.identity_dir,
            &pending,
            &transport,
            &context.endpoint,
        )?;
        let now = current_time_ms();
        if matches!(state, StatusOutcome::Expired)
            || (matches!(state, StatusOutcome::NotFound) && now >= pending.expires_at)
        {
            archive_pending(&context.identity_dir, &pending)?;
            let next = submit_existing(
                &context.identity_dir,
                label,
                &transport,
                &context.endpoint,
                now,
            )?;
            write_json_private(&context.identity_dir, "pending.json", &next)?;
            return print_pending(&next);
        }
        return print_status_outcome(&context.identity_dir, &pending, state);
    }

    if pending_path.exists() {
        return Err(EnrollmentError::local(
            "pending.json exists but could not be read as a pending enrollment",
        ));
    }
    let pending = submit_existing(
        &context.identity_dir,
        label,
        &transport,
        &context.endpoint,
        current_time_ms(),
    )?;
    write_json_private(&context.identity_dir, "pending.json", &pending)?;
    print_pending(&pending)
}

pub fn status(
    identity_dir: Option<&Path>,
    ca_file: Option<&Path>,
    connect: Option<&str>,
    tls_name: Option<&str>,
    port: Option<&str>,
) -> Result<(), EnrollmentError> {
    let context = make_context(identity_dir, ca_file, connect, tls_name, port)?;
    verify_existing_server(&context)?;
    resolve_before_mutation(&context.endpoint)?;
    let transport = HttpsTransport::new(context.endpoint.clone(), &context.ca_pem)?;
    let pending = read_optional_pending(&context.identity_dir)?.ok_or_else(|| {
        EnrollmentError::local(
            "pending.json is missing; this identity directory has no request to check",
        )
    })?;
    let state = query_status(
        &context.identity_dir,
        &pending,
        &transport,
        &context.endpoint,
    )?;
    print_status_outcome(&context.identity_dir, &pending, state)
}

fn make_context(
    identity_dir: Option<&Path>,
    ca_file: Option<&Path>,
    connect: Option<&str>,
    tls_name: Option<&str>,
    port: Option<&str>,
) -> Result<EnrollmentContext, EnrollmentError> {
    let identity_dir = identity_dir
        .map(Path::to_path_buf)
        .or_else(|| env_path("HARKLIGHT_IDENTITY_DIR"))
        .ok_or_else(|| {
            EnrollmentError::local("provide --identity-dir or HARKLIGHT_IDENTITY_DIR")
        })?;
    let ca_file = ca_file
        .map(Path::to_path_buf)
        .or_else(|| env_path("HARKLIGHT_CA_FILE"))
        .ok_or_else(|| EnrollmentError::local("provide --ca-file or HARKLIGHT_CA_FILE"))?;
    let endpoint = parse_endpoint(connect, tls_name, port)?;
    let ca_pem = fs::read(&ca_file).map_err(|error| {
        EnrollmentError::local(format!(
            "could not read CA file {}: {error}",
            ca_file.display()
        ))
    })?;
    Certificate::from_pem(&ca_pem).map_err(|error| {
        EnrollmentError::local(format!(
            "could not load CA file {}: {error}",
            ca_file.display()
        ))
    })?;
    let ca_sha256 = hex::encode(Sha256::digest(&ca_pem));
    Ok(EnrollmentContext {
        identity_dir,
        ca_pem,
        ca_sha256,
        endpoint,
    })
}

fn parse_endpoint(
    connect: Option<&str>,
    tls_name: Option<&str>,
    port: Option<&str>,
) -> Result<Endpoint, EnrollmentError> {
    let address = required(connect, "--connect")?;
    if address.trim().is_empty() || address.trim() != address {
        return Err(EnrollmentError::local(
            "--connect must be an IP address or DNS name without surrounding whitespace",
        ));
    }
    let tls_name = required(tls_name, "--name")?;
    if tls_name.trim().is_empty() || tls_name.trim() != tls_name {
        return Err(EnrollmentError::local("--name must be a TLS DNS name"));
    }
    let port = required(port, "--port")?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| EnrollmentError::local("--port must be an integer from 1 to 65535"))?;
    let endpoint = Endpoint {
        address: address.to_owned(),
        tls_name: tls_name.to_owned(),
        port,
    };
    let url = endpoint.url("/enroll")?;
    if url.host_str().is_none() || url.path() != "/enroll" || url.port() != Some(port) {
        return Err(EnrollmentError::local("--name must be a TLS DNS name"));
    }
    Ok(endpoint)
}

fn required<'a>(value: Option<&'a str>, option: &str) -> Result<&'a str, EnrollmentError> {
    value.ok_or_else(|| EnrollmentError::local(format!("provide required {option} input")))
}

fn validate_label(label: &str) -> Result<(), EnrollmentError> {
    if !(1..=256).contains(&label.len()) {
        return Err(EnrollmentError::local(
            "--label must contain between 1 and 256 UTF-8 bytes",
        ));
    }
    Ok(())
}

fn resolve_before_mutation(endpoint: &Endpoint) -> Result<Vec<SocketAddr>, EnrollmentError> {
    resolve_configured_address(&endpoint.address).map_err(|error| {
        EnrollmentError::local(format!(
            "could not resolve connect address {}: {error}",
            endpoint.address
        ))
    })
}

fn verify_existing_server(context: &EnrollmentContext) -> Result<(), EnrollmentError> {
    check_private_directory(&context.identity_dir)?;
    let server_path = context.identity_dir.join("server.json");
    let bytes = fs::read(&server_path).map_err(|error| {
        EnrollmentError::local(format!("could not read {}: {error}", server_path.display()))
    })?;
    let server: ServerFile = serde_json::from_slice(&bytes).map_err(|error| {
        EnrollmentError::local(format!(
            "invalid JSON in {}: {error}",
            server_path.display()
        ))
    })?;
    if server.address != context.endpoint.address
        || server.name != context.endpoint.tls_name
        || server.port != context.endpoint.port
        || server.ca_sha256 != context.ca_sha256
    {
        return Err(EnrollmentError::local(
            "this identity directory belongs to a different server",
        ));
    }
    Ok(())
}

fn create_identity_directory(path: &Path) -> Result<(), EnrollmentError> {
    if path.exists() {
        return Err(EnrollmentError::local(
            "identity directory already exists; use enroll retry or enroll status, or choose a new directory",
        ));
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            EnrollmentError::local(format!(
                "could not create identity directory parent: {error}"
            ))
        })?;
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(EnrollmentError::local(
                "identity directory already exists; use enroll retry or enroll status, or choose a new directory",
            ));
        }
        Err(error) => {
            return Err(EnrollmentError::local(format!(
                "could not create identity directory {}: {error}",
                path.display()
            )));
        }
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        EnrollmentError::local(format!("could not protect identity directory: {error}"))
    })?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                EnrollmentError::local(format!("could not persist identity directory: {error}"))
            })?;
    }
    Ok(())
}

fn generate_identity() -> Result<(String, String), EnrollmentError> {
    let key = KeyPair::generate_rsa_for(&PKCS_RSA_SHA256, RsaKeySize::_2048).map_err(|error| {
        EnrollmentError::local(format!("could not generate identity key: {error}"))
    })?;
    let now = time::OffsetDateTime::now_utc();
    let mut params = CertificateParams::default();
    params.not_before = now;
    params.not_after = now + time::Duration::days(365);
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "attention-device");
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let certificate = params.self_signed(&key).map_err(|error| {
        EnrollmentError::local(format!("could not create identity certificate: {error}"))
    })?;
    Ok((certificate.pem(), key.serialize_pem()))
}

fn submit_existing(
    identity_dir: &Path,
    label: &str,
    transport: &impl EnrollmentTransport,
    endpoint: &Endpoint,
    now_ms: u64,
) -> Result<PendingFile, EnrollmentError> {
    let certificate_path = identity_dir.join("identity.pem");
    check_private_file(&certificate_path)?;
    let certificate = fs::read_to_string(&certificate_path).map_err(|error| {
        EnrollmentError::local(format!("could not read identity certificate: {error}"))
    })?;
    let response = transport
        .post(
            "/enroll",
            &json!({"public_certificate": certificate, "label": label}),
        )
        .map_err(|error| request_error(error, endpoint))?;
    let value = parse_response(response, endpoint)?;
    if let Some(error) = refusal_response(&value, endpoint)? {
        return Err(error);
    }
    if value.get("status") != Some(&Value::from(200)) {
        return Err(EnrollmentError::unsupported(
            "the Server returned an unsupported enrollment response",
        ));
    }
    let pending: PendingFile = serde_json::from_value(value["body"].clone()).map_err(|error| {
        EnrollmentError::unsupported(format!(
            "the Server returned an invalid enrollment response: {error}"
        ))
    })?;
    let expected_deadline = pending
        .expires_at
        .checked_add(PENDING_LIFETIME_MS)
        .ok_or_else(|| {
            EnrollmentError::unsupported("the Server returned an invalid enrollment deadline")
        })?;
    if !valid_code(&pending.approval_code)
        || !valid_secret(&pending.status_secret)
        || pending.expires_at <= now_ms
        || pending.status_expires_at != expected_deadline
    {
        return Err(EnrollmentError::unsupported(
            "the Server returned an invalid enrollment response",
        ));
    }
    Ok(pending)
}

fn query_status(
    identity_dir: &Path,
    pending: &PendingFile,
    transport: &impl EnrollmentTransport,
    endpoint: &Endpoint,
) -> Result<StatusOutcome, EnrollmentError> {
    let response = transport
        .post(
            "/enroll/status",
            &json!({"status_secret": pending.status_secret}),
        )
        .map_err(|error| request_error(error, endpoint))?;
    let value = parse_response(response, endpoint)?;
    if value["status"] == 404 && value["body"]["error"] == "not_found" {
        return Ok(StatusOutcome::NotFound);
    }
    if let Some(error) = refusal_response(&value, endpoint)? {
        return Err(error);
    }
    if value.get("status") != Some(&Value::from(200)) {
        return Err(EnrollmentError::unsupported(
            "the Server returned an unsupported enrollment status response",
        ));
    }
    let body = value.get("body").ok_or_else(|| {
        EnrollmentError::unsupported("the Server returned an invalid enrollment status response")
    })?;
    match body.get("state").and_then(Value::as_str) {
        Some("approved") => {
            let person_id = nonempty_string(body, "person_id")?;
            let device_id = nonempty_string(body, "device_id")?;
            let enrollment = EnrollmentFile {
                person_id: person_id.to_owned(),
                device_id: device_id.to_owned(),
            };
            write_json_private(identity_dir, "enrollment.json", &enrollment)?;
            Ok(StatusOutcome::Approved {
                person_id: enrollment.person_id,
                device_id: enrollment.device_id,
            })
        }
        Some("pending") => Ok(StatusOutcome::State("pending")),
        Some("expired") => Ok(StatusOutcome::Expired),
        Some("revoked") => Ok(StatusOutcome::State("revoked")),
        Some("declined") => Ok(StatusOutcome::State("declined")),
        Some("cancelled") => Ok(StatusOutcome::State("cancelled")),
        _ => Err(EnrollmentError::unsupported(
            "the Server returned an invalid enrollment status response",
        )),
    }
}

fn nonempty_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, EnrollmentError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            EnrollmentError::unsupported(format!(
                "the Server returned an invalid enrollment status response: missing {field}"
            ))
        })
}

#[derive(Debug)]
enum StatusOutcome {
    Approved {
        person_id: String,
        device_id: String,
    },
    State(&'static str),
    Expired,
    NotFound,
}

fn print_status_outcome(
    identity_dir: &Path,
    pending: &PendingFile,
    outcome: StatusOutcome,
) -> Result<(), EnrollmentError> {
    match outcome {
        StatusOutcome::Approved {
            person_id,
            device_id,
        } => emit_json(&json!({
            "state":"approved",
            "person_id":person_id,
            "device_id":device_id
        })),
        StatusOutcome::State(state) => {
            emit_json(&json!({"state":state,"approval_code":pending.approval_code}))?;
            match state {
                "pending" => print_approval_guidance(),
                "revoked" | "declined" | "cancelled" => eprintln!(
                    "This enrollment request will not be approved; use a new identity directory to start another request."
                ),
                _ => {}
            }
            Ok(())
        }
        StatusOutcome::Expired => {
            emit_json(&json!({"state":"expired","approval_code":pending.approval_code}))?;
            eprintln!(
                "This enrollment request has expired; run `harklight-agent enroll retry` to submit the same key again."
            );
            Ok(())
        }
        StatusOutcome::NotFound => {
            let enrollment_path = identity_dir.join("enrollment.json");
            if enrollment_path.exists() {
                let enrollment = read_enrollment(identity_dir)?;
                emit_json(&json!({
                    "state":"approved",
                    "person_id":enrollment.person_id,
                    "device_id":enrollment.device_id,
                    "readback":"ended"
                }))
            } else {
                Err(EnrollmentError::refused(
                    "this enrollment request is unknown or past its readback deadline; run `harklight-agent enroll retry`",
                ))
            }
        }
    }
}

fn print_pending(pending: &PendingFile) -> Result<(), EnrollmentError> {
    emit_json(&json!({
        "state":"pending",
        "approval_code":pending.approval_code,
        "expires_at":pending.expires_at,
        "status_expires_at":pending.status_expires_at
    }))?;
    print_approval_guidance();
    Ok(())
}

fn print_approval_guidance() {
    eprintln!(
        "Approve this code in your Harklight dashboard and enter the sender ID this agent will use, then run `harklight-agent enroll status`."
    );
}

fn request_error(error: RequestFailure, endpoint: &Endpoint) -> EnrollmentError {
    match error {
        RequestFailure::Connect(reason) => EnrollmentError::local(format!(
            "could not connect to {}: {reason}",
            endpoint.display()
        )),
        RequestFailure::Unknown(reason) => EnrollmentError::unknown(format!(
            "the outcome of the request to {} is unknown; the Server may have accepted it. Keep this identity directory and run `harklight-agent enroll retry`. {reason}",
            endpoint.display()
        )),
        RequestFailure::Local(reason) => EnrollmentError::local(reason),
    }
}

fn parse_response(response: HttpResponse, endpoint: &Endpoint) -> Result<Value, EnrollmentError> {
    let body: Value = serde_json::from_slice(&response.body).map_err(|error| {
        EnrollmentError::unsupported(format!(
            "the Server returned an unsupported response from {}: {error}",
            endpoint.display()
        ))
    })?;
    Ok(json!({"status":response.status,"body":body}))
}

fn refusal_response(
    response: &Value,
    endpoint: &Endpoint,
) -> Result<Option<EnrollmentError>, EnrollmentError> {
    let status = response
        .get("status")
        .and_then(Value::as_u64)
        .unwrap_or_default() as u16;
    if status == 200 {
        return Ok(None);
    }
    let error = response
        .get("body")
        .and_then(|body| body.get("error"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = match (status, error) {
        (409, "key_pending") => Some((
            EXIT_REFUSED,
            "The Server already holds a request for this key, usually because an earlier reply was lost, and its approval code cannot be recovered. Keep this identity directory and wait up to 24 hours for the earlier request to expire, then run `harklight-agent enroll retry`, or start over with `enroll start` in a new identity directory.",
        )),
        (409, "key_in_use") => Some((
            EXIT_REFUSED,
            "This key is already enrolled; use `harklight-agent enroll status`, or start over in a new identity directory.",
        )),
        (400, "invalid_request") => Some((
            EXIT_REFUSED,
            "The Server refused the request as malformed; check the label and certificate.",
        )),
        (404, "not_found") => Some((
            EXIT_REFUSED,
            "not_found: this enrollment request is unknown or past its readback deadline; run `harklight-agent enroll retry`.",
        )),
        (503, "capacity" | "unavailable") => Some((
            EXIT_REFUSED,
            "The Server cannot take enrollments right now; nothing was accepted. Run `harklight-agent enroll retry` later.",
        )),
        _ => None,
    };
    match message {
        Some((exit_code, message)) => Ok(Some(EnrollmentError {
            message: message.to_owned(),
            exit_code,
        })),
        None => Err(EnrollmentError::unsupported(format!(
            "the Server returned unsupported HTTP {status} from {}",
            endpoint.display()
        ))),
    }
}

fn read_optional_pending(identity_dir: &Path) -> Result<Option<PendingFile>, EnrollmentError> {
    let path = identity_dir.join("pending.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(EnrollmentError::local(format!(
                "could not read {}: {error}",
                path.display()
            )));
        }
    };
    check_private_file(&path)?;
    let pending: PendingFile = serde_json::from_slice(&bytes).map_err(|error| {
        EnrollmentError::local(format!("invalid JSON in {}: {error}", path.display()))
    })?;
    if !valid_code(&pending.approval_code)
        || !valid_secret(&pending.status_secret)
        || pending.expires_at.checked_add(PENDING_LIFETIME_MS) != Some(pending.status_expires_at)
    {
        return Err(EnrollmentError::local(
            "pending.json contains an invalid enrollment record",
        ));
    }
    Ok(Some(pending))
}

fn read_enrollment(identity_dir: &Path) -> Result<EnrollmentFile, EnrollmentError> {
    let path = identity_dir.join("enrollment.json");
    check_private_file(&path)?;
    let bytes = fs::read(&path).map_err(|error| {
        EnrollmentError::local(format!("could not read {}: {error}", path.display()))
    })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        EnrollmentError::local(format!("invalid JSON in {}: {error}", path.display()))
    })
}

fn archive_pending(identity_dir: &Path, pending: &PendingFile) -> Result<(), EnrollmentError> {
    let source = identity_dir.join("pending.json");
    let destination = identity_dir.join(format!("pending-{}.json", pending.approval_code));
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(EnrollmentError::local(format!(
            "cannot archive the expired request because {} already exists; no files were changed",
            destination
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        )));
    }
    fs::rename(&source, &destination).map_err(|error| {
        EnrollmentError::local(format!(
            "could not archive expired pending request: {error}"
        ))
    })?;
    sync_directory(identity_dir)?;
    Ok(())
}

fn write_json_private<T: Serialize>(
    directory: &Path,
    name: &str,
    value: &T,
) -> Result<(), EnrollmentError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| EnrollmentError::local(format!("could not encode {name}: {error}")))?;
    atomic_write(directory, name, &bytes)
}

fn atomic_write(directory: &Path, name: &str, bytes: &[u8]) -> Result<(), EnrollmentError> {
    let temporary = directory.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary).map_err(|error| {
        EnrollmentError::local(format!("could not create temporary {name}: {error}"))
    })?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(EnrollmentError::local(format!(
            "could not write {name}: {error}"
        )));
    }
    #[cfg(unix)]
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        EnrollmentError::local(format!("could not protect {name}: {error}"))
    })?;
    if let Err(error) = fs::rename(&temporary, directory.join(name)) {
        let _ = fs::remove_file(&temporary);
        return Err(EnrollmentError::local(format!(
            "could not publish {name}: {error}"
        )));
    }
    sync_directory(directory)
}

fn sync_directory(directory: &Path) -> Result<(), EnrollmentError> {
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            EnrollmentError::local(format!("could not persist directory changes: {error}"))
        })
}

fn check_private_directory(directory: &Path) -> Result<(), EnrollmentError> {
    let metadata = fs::metadata(directory).map_err(|error| {
        EnrollmentError::local(format!("could not inspect identity directory: {error}"))
    })?;
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(EnrollmentError::local(format!(
            "identity directory must be private (mode 0700): {}",
            directory.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(EnrollmentError::local("identity path is not a directory"));
    }
    Ok(())
}

fn check_private_file(path: &Path) -> Result<(), EnrollmentError> {
    let metadata = fs::metadata(path).map_err(|error| {
        EnrollmentError::local(format!("could not inspect {}: {error}", path.display()))
    })?;
    #[cfg(unix)]
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(EnrollmentError::local(format!(
            "identity file must be private (mode 0600): {}",
            path.display()
        )));
    }
    Ok(())
}

fn valid_code(code: &str) -> bool {
    code.len() == 10
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || (b'2'..=b'7').contains(&byte))
}

fn valid_secret(secret: &str) -> bool {
    secret.len() == 64
        && secret
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn emit_json(value: &Value) -> Result<(), EnrollmentError> {
    let mut stdout = io::stdout().lock();
    let bytes = serde_json::to_vec(value)
        .map_err(|error| EnrollmentError::local(format!("could not encode output: {error}")))?;
    stdout
        .write_all(&bytes)
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .map_err(|error| EnrollmentError::local(format!("stdout write failed: {error}")))
}

#[cfg(test)]
fn print_pending_for_test(pending: &PendingFile) -> Value {
    json!({
        "state":"pending",
        "approval_code":pending.approval_code,
        "expires_at":pending.expires_at,
        "status_expires_at":pending.status_expires_at
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        path::{Path, PathBuf},
    };

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::{
        EnrollmentError, EnrollmentTransport, HttpResponse, PendingFile, RequestFailure,
        StatusOutcome, archive_pending, generate_identity, parse_endpoint, print_pending_for_test,
        query_status, start, submit_existing, write_json_private,
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("harklight-enroll-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            #[cfg(unix)]
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Default)]
    struct FakeTransport {
        responses: VecDeque<Result<HttpResponse, RequestFailure>>,
        calls: Vec<(String, serde_json::Value)>,
    }

    impl FakeTransport {
        fn with(responses: impl IntoIterator<Item = Result<HttpResponse, RequestFailure>>) -> Self {
            Self {
                responses: responses.into_iter().collect(),
                calls: Vec::new(),
            }
        }
    }

    impl EnrollmentTransport for FakeTransport {
        fn post(
            &self,
            _path: &str,
            _payload: &serde_json::Value,
        ) -> Result<HttpResponse, RequestFailure> {
            unreachable!("mutable fake is used by post_mut")
        }
    }

    impl FakeTransport {
        fn post_mut(
            &mut self,
            path: &str,
            payload: &serde_json::Value,
        ) -> Result<HttpResponse, RequestFailure> {
            self.calls.push((path.to_owned(), payload.clone()));
            self.responses
                .pop_front()
                .expect("test response configured")
        }
    }

    struct FakeTransportRef<'a>(&'a std::cell::RefCell<FakeTransport>);

    impl EnrollmentTransport for FakeTransportRef<'_> {
        fn post(
            &self,
            path: &str,
            payload: &serde_json::Value,
        ) -> Result<HttpResponse, RequestFailure> {
            self.0.borrow_mut().post_mut(path, payload)
        }
    }

    fn reply(status: u16, body: serde_json::Value) -> Result<HttpResponse, RequestFailure> {
        Ok(HttpResponse {
            status,
            body: serde_json::to_vec(&body).unwrap(),
        })
    }

    fn pending(code: &str, expires_at: u64) -> PendingFile {
        PendingFile {
            approval_code: code.to_owned(),
            status_secret: "a".repeat(64),
            expires_at,
            status_expires_at: expires_at + 86_400_000,
        }
    }

    fn setup_identity(directory: &Path) {
        fs::write(directory.join("identity.pem"), "certificate pem\n").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            directory.join("identity.pem"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        fs::write(directory.join("identity.key"), "private key pem\n").unwrap();
        #[cfg(unix)]
        fs::set_permissions(
            directory.join("identity.key"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    #[test]
    fn enrollment_payload_is_exact_and_uses_the_existing_key_after_lost_reply() {
        let directory = TestDirectory::new();
        setup_identity(directory.path());
        let key_before = fs::read(directory.path().join("identity.key")).unwrap();
        let certificate_before = fs::read(directory.path().join("identity.pem")).unwrap();
        let initial_files = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        let fake = std::cell::RefCell::new(FakeTransport::with([
            Err(RequestFailure::Unknown("reply lost".into())),
            reply(409, json!({"error":"key_pending"})),
            reply(
                200,
                json!({
                    "approval_code":"ABCDEFG234",
                    "status_secret":"b".repeat(64),
                    "expires_at":86_600_000,
                    "status_expires_at":173_000_000
                }),
            ),
        ]));
        let transport = FakeTransportRef(&fake);
        let endpoint = parse_endpoint(
            Some("service.example"),
            Some("harklight.example"),
            Some("18444"),
        )
        .unwrap();

        assert!(matches!(
            submit_existing(
                directory.path(),
                "Work laptop",
                &transport,
                &endpoint,
                100_000
            ),
            Err(EnrollmentError { exit_code: 7, .. })
        ));
        assert!(!directory.path().join("pending.json").exists());
        assert_eq!(
            submit_existing(
                directory.path(),
                "Work laptop",
                &transport,
                &endpoint,
                100_000
            )
            .unwrap_err()
            .exit_code,
            2
        );
        assert_eq!(
            fs::read(directory.path().join("identity.key")).unwrap(),
            key_before
        );
        assert_eq!(
            fs::read(directory.path().join("identity.pem")).unwrap(),
            certificate_before
        );
        assert_eq!(
            fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>(),
            initial_files
        );
        let calls_before_expiry = fake.borrow().calls.clone();
        assert_eq!(calls_before_expiry[1].0, "/enroll");
        assert_eq!(
            calls_before_expiry[1].1,
            json!({"public_certificate":"certificate pem\n","label":"Work laptop"})
        );
        let accepted = submit_existing(
            directory.path(),
            "Work laptop",
            &transport,
            &endpoint,
            86_500_001,
        )
        .unwrap();
        assert_eq!(accepted.approval_code, "ABCDEFG234");
        assert_eq!(
            fs::read(directory.path().join("identity.key")).unwrap(),
            key_before
        );
        assert_eq!(fake.borrow().calls[2].0, "/enroll");
        assert_eq!(fake.borrow().calls[2].1.as_object().unwrap().len(), 2);
    }

    #[test]
    fn status_uses_only_the_status_secret_and_approval_writes_identity_result() {
        let directory = TestDirectory::new();
        setup_identity(directory.path());
        let pending = pending("ABCDEFG234", 200_000);
        write_json_private(directory.path(), "pending.json", &pending).unwrap();
        let fake = std::cell::RefCell::new(FakeTransport::with([reply(
            200,
            json!({"state":"approved","person_id":"person-1","device_id":"device-1"}),
        )]));
        let endpoint =
            parse_endpoint(Some("192.0.2.7"), Some("harklight.example"), Some("18444")).unwrap();
        let state = query_status(
            directory.path(),
            &pending,
            &FakeTransportRef(&fake),
            &endpoint,
        )
        .unwrap();
        assert!(matches!(state, StatusOutcome::Approved { .. }));
        let enrollment: serde_json::Value =
            serde_json::from_slice(&fs::read(directory.path().join("enrollment.json")).unwrap())
                .unwrap();
        assert_eq!(
            enrollment,
            json!({"person_id":"person-1","device_id":"device-1"})
        );
        assert_eq!(
            fake.borrow().calls[0].1,
            json!({"status_secret":"a".repeat(64)})
        );
    }

    #[test]
    fn expired_pending_is_archived_without_changing_the_private_key() {
        let directory = TestDirectory::new();
        setup_identity(directory.path());
        let pending = pending("ABCDEFG234", 200_000);
        write_json_private(directory.path(), "pending.json", &pending).unwrap();
        let key_before = fs::read(directory.path().join("identity.key")).unwrap();

        archive_pending(directory.path(), &pending).unwrap();

        assert!(!directory.path().join("pending.json").exists());
        assert!(directory.path().join("pending-ABCDEFG234.json").exists());
        assert_eq!(
            fs::read(directory.path().join("identity.key")).unwrap(),
            key_before
        );
    }

    #[test]
    fn endpoint_rejects_missing_port_as_a_local_input_error() {
        assert_eq!(
            parse_endpoint(Some("192.0.2.7"), Some("harklight.example"), None)
                .unwrap_err()
                .exit_code,
            1
        );
    }

    #[test]
    fn generated_certificate_and_private_key_form_a_loadable_client_identity() {
        let (certificate, key) = generate_identity().unwrap();
        assert!(certificate.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(key.starts_with("-----BEGIN PRIVATE KEY-----\n"));
        let mut identity = certificate.into_bytes();
        if !identity.ends_with(b"\n") {
            identity.push(b'\n');
        }
        identity.extend_from_slice(key.as_bytes());
        reqwest::Identity::from_pem(&identity).unwrap();
    }

    #[test]
    fn missing_port_fails_before_creating_the_identity_directory() {
        let parent = TestDirectory::new();
        let identity = parent.path().join("new-identity");
        let ca = parent.path().join("not-read-because-port-is-missing.pem");

        let error = start(
            Some(&identity),
            Some(&ca),
            Some("192.0.2.7"),
            Some("harklight.example"),
            None,
            Some("Work laptop"),
            None,
        )
        .unwrap_err();

        assert_eq!(error.exit_code, 1);
        assert!(error.message.contains("--port"));
        assert!(!identity.exists());
    }

    #[test]
    fn pending_output_keeps_the_server_expiry_and_readback_deadline() {
        let output = print_pending_for_test(&pending("ABCDEFG234", 200_000));
        assert_eq!(output["expires_at"], 200_000);
        assert_eq!(output["status_expires_at"], 86_600_000);
    }
}
