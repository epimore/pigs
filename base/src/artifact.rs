use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    error::Error,
    fmt::{Display, Formatter},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct ArtifactDownloadPolicy {
    pub staging_root: PathBuf,
    pub allowed_hosts: HashSet<String>,
    pub max_bytes: u64,
    pub total_timeout: Duration,
    pub allow_loopback_http: bool,
}

#[derive(Debug, Clone)]
pub struct VerifiedArtifactRequest {
    pub url: String,
    pub output_name: String,
    pub expected_size: u64,
    pub expected_sha256: String,
    pub signature: Vec<u8>,
    pub public_key: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedArtifact {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactErrorKind {
    InvalidConfiguration,
    Denied,
    Timeout,
    TooLarge,
    Integrity,
    Signature,
    Io,
    Network,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactError {
    kind: ArtifactErrorKind,
    message: String,
}

impl ArtifactError {
    fn new(kind: ArtifactErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn kind(&self) -> ArtifactErrorKind {
        self.kind
    }
}

impl Display for ArtifactError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for ArtifactError {}

pub fn invalid_artifact_configuration(message: impl Into<String>) -> ArtifactError {
    ArtifactError::new(ArtifactErrorKind::InvalidConfiguration, message)
}

pub async fn download_verify_and_stage(
    request: &VerifiedArtifactRequest,
    policy: &ArtifactDownloadPolicy,
    cancel: CancellationToken,
) -> Result<StagedArtifact, ArtifactError> {
    validate_request(request, policy)?;
    std::fs::create_dir_all(&policy.staging_root)
        .map_err(|error| io_error("create staging root", error))?;
    let root = policy
        .staging_root
        .canonicalize()
        .map_err(|error| io_error("resolve staging root", error))?;
    let destination = root.join(&request.output_name);
    if destination.exists() {
        return verify_existing(&destination, request).await;
    }
    let temporary = root.join(format!(
        ".{}.{}.partial",
        request.output_name,
        random_suffix()
    ));
    let result = match tokio::time::timeout(
        policy.total_timeout,
        download_to_temporary(request, policy, &temporary, cancel),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            remove_partial(&temporary).await;
            return Err(ArtifactError::new(
                ArtifactErrorKind::Timeout,
                "artifact download timed out",
            ));
        }
    };
    let staged = match result {
        Ok(staged) => staged,
        Err(error) => {
            remove_partial(&temporary).await;
            return Err(error);
        }
    };
    tokio::fs::rename(&temporary, &destination)
        .await
        .map_err(|error| io_error("atomically stage artifact", error))?;
    Ok(StagedArtifact {
        path: destination,
        ..staged
    })
}

async fn verify_existing(
    destination: &Path,
    request: &VerifiedArtifactRequest,
) -> Result<StagedArtifact, ArtifactError> {
    let bytes = tokio::fs::read(destination)
        .await
        .map_err(|error| io_error("read existing staged artifact", error))?;
    if bytes.len() as u64 != request.expected_size {
        return Err(ArtifactError::new(
            ArtifactErrorKind::Integrity,
            "existing artifact size does not match manifest",
        ));
    }
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if !digest.eq_ignore_ascii_case(&request.expected_sha256) {
        return Err(ArtifactError::new(
            ArtifactErrorKind::Integrity,
            "existing artifact SHA-256 does not match manifest",
        ));
    }
    verify_ed25519(&bytes, &request.public_key, &request.signature)?;
    Ok(StagedArtifact {
        path: destination.to_path_buf(),
        size: request.expected_size,
        sha256: digest,
    })
}

async fn download_to_temporary(
    request: &VerifiedArtifactRequest,
    policy: &ArtifactDownloadPolicy,
    temporary: &Path,
    cancel: CancellationToken,
) -> Result<StagedArtifact, ArtifactError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| network_error("build artifact client", error))?;
    let response = tokio::select! {
        _ = cancel.cancelled() => return Err(ArtifactError::new(ArtifactErrorKind::Cancelled, "artifact download cancelled")),
        response = client.get(&request.url).send() => response
            .map_err(|error| network_error("download artifact", error))?,
    };
    if !response.status().is_success() {
        return Err(ArtifactError::new(
            ArtifactErrorKind::Network,
            format!("artifact server returned status {}", response.status()),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length != request.expected_size || length > policy.max_bytes)
    {
        return Err(ArtifactError::new(
            ArtifactErrorKind::TooLarge,
            "artifact Content-Length is outside the expected bound",
        ));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(temporary)
        .await
        .map_err(|error| io_error("create artifact partial file", error))?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    while let Some(chunk) = tokio::select! {
        _ = cancel.cancelled() => return Err(ArtifactError::new(ArtifactErrorKind::Cancelled, "artifact download cancelled")),
        chunk = stream.next() => chunk,
    } {
        let chunk = chunk.map_err(|error| network_error("read artifact body", error))?;
        size = size.saturating_add(chunk.len() as u64);
        if size > policy.max_bytes || size > request.expected_size {
            return Err(ArtifactError::new(
                ArtifactErrorKind::TooLarge,
                "artifact body exceeded the expected bound",
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| io_error("write artifact partial file", error))?;
    }
    file.flush()
        .await
        .map_err(|error| io_error("flush artifact partial file", error))?;
    file.sync_all()
        .await
        .map_err(|error| io_error("sync artifact partial file", error))?;
    if size != request.expected_size {
        return Err(ArtifactError::new(
            ArtifactErrorKind::Integrity,
            "artifact size does not match manifest",
        ));
    }
    let digest = format!("{:x}", hasher.finalize());
    if !digest.eq_ignore_ascii_case(&request.expected_sha256) {
        return Err(ArtifactError::new(
            ArtifactErrorKind::Integrity,
            "artifact SHA-256 does not match manifest",
        ));
    }
    verify_file_signature(temporary, &request.public_key, &request.signature).await?;
    Ok(StagedArtifact {
        path: PathBuf::new(),
        size,
        sha256: digest,
    })
}

async fn verify_file_signature(
    path: &Path,
    public_key: &[u8],
    signature: &[u8],
) -> Result<(), ArtifactError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| io_error("read artifact for signature verification", error))?;
    verify_ed25519(&bytes, public_key, signature)
}

pub fn verify_ed25519(
    message: &[u8],
    public_key: &[u8],
    signature: &[u8],
) -> Result<(), ArtifactError> {
    let public_key: &[u8; 32] = public_key.try_into().map_err(|_| {
        ArtifactError::new(
            ArtifactErrorKind::InvalidConfiguration,
            "invalid Ed25519 public key",
        )
    })?;
    let signature = Signature::from_slice(signature).map_err(|_| {
        ArtifactError::new(
            ArtifactErrorKind::Signature,
            "invalid Ed25519 signature encoding",
        )
    })?;
    let verifying_key = VerifyingKey::from_bytes(public_key).map_err(|_| {
        ArtifactError::new(
            ArtifactErrorKind::InvalidConfiguration,
            "invalid Ed25519 public key",
        )
    })?;
    verifying_key.verify(message, &signature).map_err(|_| {
        ArtifactError::new(
            ArtifactErrorKind::Signature,
            "artifact signature verification failed",
        )
    })
}

fn validate_request(
    request: &VerifiedArtifactRequest,
    policy: &ArtifactDownloadPolicy,
) -> Result<(), ArtifactError> {
    if policy.max_bytes == 0 || policy.total_timeout.is_zero() || request.expected_size == 0 {
        return Err(ArtifactError::new(
            ArtifactErrorKind::InvalidConfiguration,
            "artifact size and timeout bounds must be positive",
        ));
    }
    if request.expected_size > policy.max_bytes
        || request.expected_sha256.len() != 64
        || request.output_name.is_empty()
        || request.output_name.len() > 180
        || !request
            .output_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ArtifactError::new(
            ArtifactErrorKind::InvalidConfiguration,
            "artifact request is outside configured bounds",
        ));
    }
    let url = reqwest::Url::parse(&request.url).map_err(|_| {
        ArtifactError::new(
            ArtifactErrorKind::InvalidConfiguration,
            "invalid artifact URL",
        )
    })?;
    let host = url.host_str().ok_or_else(|| {
        ArtifactError::new(
            ArtifactErrorKind::InvalidConfiguration,
            "artifact URL has no host",
        )
    })?;
    let loopback_http = policy.allow_loopback_http
        && url.scheme() == "http"
        && matches!(host, "127.0.0.1" | "::1" | "localhost");
    if url.scheme() != "https" && !loopback_http {
        return Err(ArtifactError::new(
            ArtifactErrorKind::Denied,
            "artifact URL must use HTTPS",
        ));
    }
    if !policy.allowed_hosts.contains(host) {
        return Err(ArtifactError::new(
            ArtifactErrorKind::Denied,
            "artifact host is not allowlisted",
        ));
    }
    Ok(())
}

fn random_suffix() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

async fn remove_partial(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            log::warn!("remove artifact partial file failed: {error}");
        }
    }
}

fn io_error(context: &str, error: std::io::Error) -> ArtifactError {
    ArtifactError::new(ArtifactErrorKind::Io, format!("{context}: {error}"))
}

fn network_error(context: &str, error: reqwest::Error) -> ArtifactError {
    ArtifactError::new(ArtifactErrorKind::Network, format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve_once(head: &'static str, body: &'static [u8], delay: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await;
            stream.write_all(head.as_bytes()).await.unwrap();
            if !body.is_empty() {
                stream.write_all(&body[..1]).await.unwrap();
                tokio::time::sleep(delay).await;
                if body.len() > 1 {
                    let _ = stream.write_all(&body[1..]).await;
                }
            }
        });
        format!("http://{address}/artifact")
    }

    fn loopback_policy(staging_root: PathBuf, timeout: Duration) -> ArtifactDownloadPolicy {
        ArtifactDownloadPolicy {
            staging_root,
            allowed_hosts: HashSet::from(["127.0.0.1".to_string()]),
            max_bytes: 1024,
            total_timeout: timeout,
            allow_loopback_http: true,
        }
    }

    fn request(url: String, expected_size: u64) -> VerifiedArtifactRequest {
        VerifiedArtifactRequest {
            url,
            output_name: "bundle.bin".to_string(),
            expected_size,
            expected_sha256: "0".repeat(64),
            signature: vec![0; 64],
            public_key: vec![0; 32],
        }
    }

    #[test]
    fn verifies_detached_ed25519_signature_and_rejects_mutation() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let message = b"artifact";
        let signature = key.sign(message);
        verify_ed25519(
            message,
            key.verifying_key().as_bytes(),
            &signature.to_bytes(),
        )
        .unwrap();
        assert_eq!(
            verify_ed25519(
                b"mutated",
                key.verifying_key().as_bytes(),
                &signature.to_bytes()
            )
            .unwrap_err()
            .kind(),
            ArtifactErrorKind::Signature
        );
    }

    #[test]
    fn rejects_output_path_escape_and_untrusted_host() {
        let policy = ArtifactDownloadPolicy {
            staging_root: std::env::temp_dir(),
            allowed_hosts: HashSet::from(["artifacts.example.com".to_string()]),
            max_bytes: 1024,
            total_timeout: Duration::from_secs(1),
            allow_loopback_http: false,
        };
        let request = VerifiedArtifactRequest {
            url: "https://evil.example.com/bundle".to_string(),
            output_name: "../bundle".to_string(),
            expected_size: 1,
            expected_sha256: "0".repeat(64),
            signature: vec![0; 64],
            public_key: vec![0; 32],
        };
        assert_eq!(
            validate_request(&request, &policy).unwrap_err().kind(),
            ArtifactErrorKind::InvalidConfiguration
        );
    }

    #[tokio::test]
    async fn rejects_redirect_without_following_it() {
        let root = std::env::temp_dir().join(format!("artifact-{}", random_suffix()));
        let url = serve_once(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            b"",
            Duration::ZERO,
        )
        .await;
        let error = download_verify_and_stage(
            &request(url, 1),
            &loopback_policy(root.clone(), Duration::from_secs(1)),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ArtifactErrorKind::Network);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(root).unwrap();
    }

    #[tokio::test]
    async fn timeout_removes_partial_file() {
        let root = std::env::temp_dir().join(format!("artifact-{}", random_suffix()));
        let url = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n",
            b"12345678",
            Duration::from_millis(200),
        )
        .await;
        let error = download_verify_and_stage(
            &request(url, 8),
            &loopback_policy(root.clone(), Duration::from_millis(30)),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ArtifactErrorKind::Timeout);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(root).unwrap();
    }
}
