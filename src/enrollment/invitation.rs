use super::*;
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};

const CA_FILE: &str = "invitation-ca.pem";
const INVITATION_STATE: &str = "invitation-enrollment.json";

#[derive(Deserialize)]
struct Invitation {
    person_id: String,
    enrollment_url: String,
    device_url: String,
    ca_pem: String,
    ca_sha256: String,
    invite_token: String,
}

#[derive(Deserialize, Serialize)]
struct InvitationEnrollment {
    person_id: String,
    label: Option<String>,
    invite_token: Option<String>,
    status_secret: String,
}

pub(super) struct SavedInvitation {
    directory: PathBuf,
    state: InvitationEnrollment,
}

pub fn start_invitation(
    identity_dir: Option<&Path>,
    path: &Path,
    label: Option<&str>,
) -> Result<(), EnrollmentError> {
    let invalid = || EnrollmentError::local("invalid Harklight join invitation");
    let input = fs::read_to_string(path).map_err(|error| {
        EnrollmentError::local(format!("could not read invitation file: {error}"))
    })?;
    let input = input.trim();
    let document: Value = if let Some(fragment) = input.strip_prefix("harklight://join#") {
        let decoded = URL_SAFE_NO_PAD.decode(fragment).map_err(|_| invalid())?;
        serde_json::from_slice(&decoded).map_err(|_| invalid())?
    } else {
        serde_json::from_str(input).map_err(|_| invalid())?
    };
    if document["v"] == 1 {
        return Err(EnrollmentError::local(
            "this invitation is from an older Server; ask for a new one",
        ));
    }
    if document["v"] != 2 {
        return Err(invalid());
    }
    let device = invitation_endpoint(document["device_url"].as_str(), "device_url", "/")?;
    let invitation: Invitation = serde_json::from_value(document).map_err(|_| invalid())?;
    let endpoint = invitation_endpoint(
        Some(&invitation.enrollment_url),
        "enrollment_url",
        "/enroll",
    )?;
    if invitation.person_id.is_empty() || invitation.invite_token.is_empty() {
        return Err(invalid());
    }
    let ca_pem = invitation.ca_pem.into_bytes();
    verify_ca_pin(&ca_pem, &invitation.ca_sha256)?;
    let directory = identity_path(identity_dir)?;
    let context = EnrollmentContext {
        identity_dir: directory.clone(),
        ca_sha256: hex::encode(Sha256::digest(&ca_pem)),
        ca_pem,
        endpoint,
    };
    let mut secret = [0; 32];
    getrandom::fill(&mut secret)
        .map_err(|_| EnrollmentError::local("could not generate an enrollment status secret"))?;
    let state = InvitationEnrollment {
        person_id: invitation.person_id,
        label: label.map(str::to_owned),
        invite_token: Some(invitation.invite_token),
        status_secret: hex::encode(secret),
    };
    resolve_before_mutation(&context.endpoint)?;
    let transport = HttpsTransport::new(context.endpoint.clone(), &context.ca_pem)?;
    initialize_identity(&context, device.port, Some(invitation.device_url))?;
    atomic_write(&directory, CA_FILE, &context.ca_pem)?;
    write_json_private(&directory, INVITATION_STATE, &state)?;
    SavedInvitation { directory, state }.redeem(&transport, &context.endpoint)
}

fn invitation_endpoint(
    value: Option<&str>,
    field: &str,
    path: &str,
) -> Result<Endpoint, EnrollmentError> {
    let invalid = || {
        let target = if path == "/" {
            "an HTTPS origin"
        } else {
            "an HTTPS /enroll URL"
        };
        EnrollmentError::local(format!(
            "invitation {field} must be {target} with an explicit port"
        ))
    };
    let value = value.ok_or_else(invalid)?;
    // Url normalizes an explicit default port away. Read the port from the
    // supplied authority, then validate the full URL separately.
    let authority = value.strip_prefix("https://").ok_or_else(invalid)?;
    let authority = if path == "/" {
        authority.strip_suffix('/').unwrap_or(authority)
    } else {
        authority.strip_suffix(path).ok_or_else(invalid)?
    };
    let (_, port) = authority.rsplit_once(':').ok_or_else(invalid)?;
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(invalid)?;
    let url = Url::parse(value).map_err(|_| invalid())?;
    let host = url
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or_else(invalid)?;
    if authority.contains(['/', '\\', '@', '?', '#'])
        || value.chars().any(char::is_whitespace)
        || url.path() != path
        || url.port_or_known_default() != Some(port)
    {
        return Err(invalid());
    }
    Ok(Endpoint {
        address: host.to_owned(),
        tls_name: host.to_owned(),
        port,
    })
}

fn verify_ca_pin(pem: &[u8], pin: &str) -> Result<(), EnrollmentError> {
    let invalid = || EnrollmentError::local("invalid invitation CA certificate or SHA-256 pin");
    let pem_text = std::str::from_utf8(pem).map_err(|_| invalid())?.trim();
    let body = pem_text
        .strip_prefix("-----BEGIN CERTIFICATE-----")
        .and_then(|body| body.strip_suffix("-----END CERTIFICATE-----"))
        .ok_or_else(invalid)?;
    let base64: String = body.split_ascii_whitespace().collect();
    let der = STANDARD.decode(base64).map_err(|_| invalid())?;
    Certificate::from_der(&der).map_err(|_| invalid())?;
    if hex::encode(Sha256::digest(&der)) != pin {
        return Err(invalid());
    }
    Ok(())
}

fn identity_path(identity_dir: Option<&Path>) -> Result<PathBuf, EnrollmentError> {
    identity_dir
        .map(Path::to_path_buf)
        .or_else(|| env_path("HARKLIGHT_IDENTITY_DIR"))
        .ok_or_else(|| EnrollmentError::local("provide --identity-dir or HARKLIGHT_IDENTITY_DIR"))
}

pub(super) fn saved(
    identity_dir: Option<&Path>,
) -> Result<Option<SavedInvitation>, EnrollmentError> {
    let directory = identity_path(identity_dir)?;
    let path = directory.join(INVITATION_STATE);
    match fs::read(&path) {
        Ok(bytes) => {
            ensure_private_file(&path)?;
            let state = serde_json::from_slice(&bytes)
                .map_err(|_| EnrollmentError::local("invalid saved invitation enrollment"))?;
            Ok(Some(SavedInvitation { directory, state }))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(EnrollmentError::local(format!(
            "cannot read saved invitation enrollment: {error}"
        ))),
    }
}

impl SavedInvitation {
    pub(super) fn finish(self, retry: bool) -> Result<(), EnrollmentError> {
        ensure_private_directory(&self.directory)?;
        let server_bytes = fs::read(self.directory.join("server.json")).map_err(|error| {
            EnrollmentError::local(format!("cannot read saved server: {error}"))
        })?;
        let server: ServerFile = serde_json::from_slice(&server_bytes)
            .map_err(|_| EnrollmentError::local("invalid saved server JSON"))?;
        let ca_pem = fs::read(self.directory.join(CA_FILE)).map_err(|error| {
            EnrollmentError::local(format!("cannot read saved invitation CA: {error}"))
        })?;
        if hex::encode(Sha256::digest(&ca_pem)) != server.ca_sha256 {
            return Err(EnrollmentError::local(
                "saved invitation CA does not match the enrolled server",
            ));
        }
        let endpoint = Endpoint {
            address: server.address,
            tls_name: server.name,
            port: server.port,
        };
        let transport = HttpsTransport::new(endpoint.clone(), &ca_pem)?;
        match read_optional_pending(&self.directory)? {
            Some(pending) => {
                let state = query_bound_status(
                    &self.directory,
                    &pending.status_secret,
                    &transport,
                    &endpoint,
                    Some(&self.state.person_id),
                )?;
                print_status_outcome(&self.directory, &pending, state)
            }
            None if self.directory.join("enrollment.json").exists() => {
                match query_bound_status(
                    &self.directory,
                    &self.state.status_secret,
                    &transport,
                    &endpoint,
                    Some(&self.state.person_id),
                )? {
                    StatusOutcome::Approved {
                        person_id,
                        device_id,
                    } => emit_json(
                        &json!({"state":"approved","person_id":person_id,"device_id":device_id}),
                    ),
                    _ => Err(EnrollmentError::refused(
                        "the Server could not confirm this enrollment",
                    )),
                }
            }
            None if retry => self.redeem(&transport, &endpoint),
            None => Err(EnrollmentError::local(
                "no saved approval code; use enroll retry to recover this invitation",
            )),
        }
    }

    fn redeem(
        self,
        transport: &impl EnrollmentTransport,
        endpoint: &Endpoint,
    ) -> Result<(), EnrollmentError> {
        let directory = self.directory.clone();
        let result = self.submit(transport, endpoint);
        if let Err(error) = &result
            && error.exit_code == EXIT_REFUSED
        {
            // Remove only files created for this unaccepted enrollment.
            for name in [
                INVITATION_STATE,
                "identity.key",
                "identity.pem",
                CA_FILE,
                "server.json",
            ] {
                fs::remove_file(directory.join(name)).map_err(|cleanup| {
                    EnrollmentError::local(format!(
                        "{}; could not remove enrollment files: {cleanup}",
                        error.message
                    ))
                })?;
            }
            fs::remove_dir(directory).map_err(|cleanup| {
                EnrollmentError::local(format!(
                    "{}; could not remove enrollment directory: {cleanup}",
                    error.message
                ))
            })?;
        }
        result
    }

    fn submit(
        mut self,
        transport: &impl EnrollmentTransport,
        endpoint: &Endpoint,
    ) -> Result<(), EnrollmentError> {
        let token = self.state.invite_token.as_ref().ok_or_else(|| {
            EnrollmentError::local("no invitation remains to redeem; ask for a new one")
        })?;
        let certificate =
            fs::read_to_string(self.directory.join("identity.pem")).map_err(|error| {
                EnrollmentError::local(format!("cannot read identity certificate: {error}"))
            })?;
        let mut payload = json!({
            "invite_token": token,
            "public_certificate": certificate,
            "status_secret": self.state.status_secret,
        });
        if let Some(label) = &self.state.label {
            payload["label"] = label.as_str().into();
        }
        let response = transport
            .post("/enroll", &payload)
            .map_err(|error| request_error(error, endpoint))?;
        let response = parse_response(response, endpoint)?;
        match (
            response["status"].as_u64(),
            response["body"]["error"].as_str(),
        ) {
            (Some(409), Some("used")) => {
                return Err(EnrollmentError::refused(
                    "invitation already used; ask for a new one",
                ));
            }
            (Some(404), Some("refused")) | (Some(410), Some("expired")) => {
                return Err(EnrollmentError::refused(
                    "the Server refused this invitation; ask for a new one",
                ));
            }
            (Some(503), Some("capacity" | "unavailable")) => {
                return Err(EnrollmentError::refused(
                    "the Server cannot accept enrollment right now; try the invitation again later",
                ));
            }
            _ => {}
        }
        if let Some(error) = refusal_response(&response, endpoint)? {
            return Err(error);
        }
        let body = &response["body"];
        if body["state"] != "pending" {
            return match status_outcome(&self.directory, body, Some(&self.state.person_id))? {
                StatusOutcome::Approved {
                    person_id,
                    device_id,
                } => {
                    self.state.invite_token = None;
                    write_json_private(&self.directory, INVITATION_STATE, &self.state)?;
                    emit_json(
                        &json!({"state":"approved","person_id":person_id,"device_id":device_id}),
                    )
                }
                _ => Err(EnrollmentError::refused(
                    "this invitation can no longer enroll this device; ask for a new one",
                )),
            };
        }
        let code = nonempty_string(body, "approval_code")?;
        if !valid_code(code) {
            return Err(EnrollmentError::unsupported(
                "the Server returned an invalid approval code",
            ));
        }
        let pending = PendingFile {
            approval_code: code.to_owned(),
            status_secret: self.state.status_secret.clone(),
            expires_at: body["expires_at"].as_u64(),
            status_expires_at: body["status_expires_at"].as_u64(),
        };
        write_json_private(&self.directory, "pending.json", &pending)?;
        self.state.invite_token = None;
        write_json_private(&self.directory, INVITATION_STATE, &self.state)?;
        print_pending(&pending)
    }
}
