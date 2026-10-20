use std::{
    fs,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use reqwest::{Certificate, Identity, Url, blocking::Client};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read enrolled identity: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("identity directory must be private (mode 0700): {0}")]
    DirectoryPermissions(PathBuf),
    #[error("identity private key must be private (mode 0600): {0}")]
    KeyPermissions(PathBuf),
    #[error("server configuration does not match the enrolled CA")]
    CaMismatch,
    #[error("enrollment or server configuration is incomplete: {0}")]
    Invalid(String),
    #[error("TLS configuration failed: {0}")]
    Tls(#[from] reqwest::Error),
}

#[derive(Debug, Deserialize)]
struct ServerFile {
    address: String,
    name: String,
    port: u16,
    origin_port: Option<u16>,
    ca_sha256: String,
}

#[derive(Debug, Deserialize)]
struct EnrollmentFile {
    person_id: Option<String>,
    device_id: String,
}

/// The files and identity created by the existing Harklight device enrollment.
/// The sender grant remains server-owned and is never copied into this config.
#[derive(Debug, Clone)]
pub struct EnrolledIdentity {
    pub directory: PathBuf,
    pub address: String,
    pub name: String,
    pub port: u16,
    pub origin_port: Option<u16>,
    pub person_id: Option<String>,
    pub device_id: String,
    pub ca_file: PathBuf,
    certificate_pem: Vec<u8>,
    key_pem: Vec<u8>,
    ca_pem: Vec<u8>,
}

impl EnrolledIdentity {
    pub fn load(directory: &Path, ca_file: &Path) -> Result<Self, ConfigError> {
        check_private_directory(directory)?;

        let server_path = directory.join("server.json");
        let server: ServerFile = read_json(&server_path)?;
        let enrollment_path = directory.join("enrollment.json");
        let enrollment: EnrollmentFile = read_json(&enrollment_path)?;

        if server.address.trim().is_empty() || server.name.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "server address or name is empty".into(),
            ));
        }
        if enrollment.device_id.trim().is_empty() {
            return Err(ConfigError::Invalid("device_id is empty".into()));
        }

        let certificate_path = directory.join("identity.pem");
        let key_path = directory.join("identity.key");
        let key_metadata = fs::metadata(&key_path)?;
        if key_metadata.permissions().mode() & 0o077 != 0 {
            return Err(ConfigError::KeyPermissions(key_path));
        }

        let certificate_pem = fs::read(&certificate_path)?;
        let key_pem = fs::read(&key_path)?;
        let ca_pem = fs::read(ca_file)?;
        let actual_ca_hash = hex::encode(Sha256::digest(&ca_pem));
        if !actual_ca_hash.eq_ignore_ascii_case(&server.ca_sha256) {
            return Err(ConfigError::CaMismatch);
        }

        Ok(Self {
            directory: directory.to_path_buf(),
            address: server.address,
            name: server.name,
            port: server.port,
            origin_port: server.origin_port,
            person_id: enrollment.person_id,
            device_id: enrollment.device_id,
            ca_file: ca_file.to_path_buf(),
            certificate_pem,
            key_pem,
            ca_pem,
        })
    }

    pub fn resolve_origin_port(&self, override_port: Option<u16>) -> Result<u16, ConfigError> {
        let port = override_port.or(self.origin_port).ok_or_else(|| {
            ConfigError::Invalid(
                "origin HTTPS port is not configured; provide --origin-port from the server endpoint configuration".into(),
            )
        })?;
        if port == 0 {
            return Err(ConfigError::Invalid(
                "origin HTTPS port must be nonzero".into(),
            ));
        }
        Ok(port)
    }

    /// Build a client that uses only the enrolled CA and device certificate.
    /// The endpoint route and port are supplied by the confirmed server wire.
    pub fn client_for_port(&self, port: u16) -> Result<Client, ConfigError> {
        let address = self.socket_addr(port)?;
        let mut identity_pem = self.certificate_pem.clone();
        if !identity_pem.ends_with(b"\n") {
            identity_pem.push(b'\n');
        }
        identity_pem.extend_from_slice(&self.key_pem);

        let identity = Identity::from_pem(&identity_pem)?;
        let ca = Certificate::from_pem(&self.ca_pem)?;
        Ok(Client::builder()
            .tls_backend_rustls()
            .tls_certs_only([ca])
            .identity(identity)
            .resolve_to_addrs(&self.name, &[address])
            .tls_version_min(reqwest::tls::Version::TLS_1_3)
            .tls_version_max(reqwest::tls::Version::TLS_1_3)
            .https_only(true)
            .build()?)
    }

    /// Create a URL using the enrolled certificate name while resolving it to
    /// the enrolled address. A server-provided port is explicit because the
    /// origin listener may differ from the relay listener in server.json.
    pub fn origin_url(&self, port: u16, path: &str) -> Result<Url, ConfigError> {
        if !path.starts_with('/') || path.starts_with("//") {
            return Err(ConfigError::Invalid("origin path must be absolute".into()));
        }
        let mut url = Url::parse(&format!("https://{}:{port}{path}", self.name))
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        url.set_fragment(None);
        Ok(url)
    }

    /// Parse the enrolled address for HTTP client's explicit resolver mapping.
    pub fn socket_addr(&self, port: u16) -> Result<SocketAddr, ConfigError> {
        let address: IpAddr = self
            .address
            .parse()
            .map_err(|error: std::net::AddrParseError| ConfigError::Invalid(error.to_string()))?;
        Ok(SocketAddr::new(address, port))
    }
}

pub fn default_state_root() -> Result<PathBuf, ConfigError> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| ConfigError::Invalid("HOME is not set".into()))?;
        return Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("Harklight")
            .join("Agent"));
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
            return Ok(PathBuf::from(state_home).join("harklight-agent"));
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| ConfigError::Invalid("HOME is not set".into()))?;
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("harklight-agent"));
    }

    #[allow(unreachable_code)]
    Err(ConfigError::Invalid("unsupported host platform".into()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, ConfigError> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|source| ConfigError::Json {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(unix)]
fn check_private_directory(directory: &Path) -> Result<(), ConfigError> {
    let metadata = fs::metadata(directory)?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ConfigError::DirectoryPermissions(directory.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_directory(_directory: &Path) -> Result<(), ConfigError> {
    Ok(())
}
