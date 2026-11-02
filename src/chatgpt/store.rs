//! The encrypted Sign in with ChatGPT credential file.
//!
//! One file holds this host's `ext_agent_host_id` and at most one ChatGPT
//! account registration (issued client ID, verified identity, granted scopes
//! and tokens), following the record in OpenAI's "Store credentials in a
//! local file". The whole record is sealed with XChaCha20-Poly1305 under
//! `CHATGPT_CREDENTIALS_KEY`, so a copy of the volume alone reveals nothing.
//! Writes are atomic (temp file + rename) with owner-only permissions.
//!
//! Nothing here ever formats a token: the `Debug` impls redact them, and
//! errors describe what failed, never the content.

use std::{
    fmt, fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    XChaCha20Poly1305, XNonce,
};
use serde::{Deserialize, Serialize};

use crate::config::CredentialsKey;

const FORMAT: &str = "opengate-chatgpt-credentials-v1";
/// Binds the ciphertext to its purpose, so it cannot be swapped with another
/// blob sealed under the same key.
const AAD: &[u8] = b"opengate chatgpt credentials v1";
/// Refresh tokens last 30 days from issue (Token reference: lifetimes).
pub const REFRESH_TOKEN_LIFETIME_SECS: u64 = 30 * 24 * 3600;

#[derive(Clone, Serialize, Deserialize)]
pub struct StoreFile {
    /// Stable, opaque identifier of this host (`urn:uuid:…`). Not a secret.
    pub ext_agent_host_id: String,
    #[serde(default)]
    pub account: Option<Account>,
}

/// One ChatGPT account registration, bound to its issued `client_id`.
#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub issuer: String,
    /// Validated ID-token `sub`.
    pub subject: String,
    pub email: Option<String>,
    /// Issued by dynamic registration (`oaiapp_…`), never `dynamic_agent_client`.
    pub client_id: String,
    /// Retained for a later reauthorization; cleared on sign-out.
    pub id_token: Option<String>,
    /// Granted scopes from the token response.
    pub scopes: Vec<String>,
    /// `None` after sign-out or once the refresh token became unusable.
    pub tokens: Option<Tokens>,
    /// Why `tokens` is `None`, for status reporting.
    #[serde(default)]
    pub ended: Option<Ended>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ended {
    SignedOut,
    /// The refresh token expired (unused for 30 days).
    Expired,
    /// The session was revoked or disconnected in ChatGPT settings, or the
    /// plan permission was no longer accepted.
    Revoked,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u64,
    /// Unix seconds when this token response was received.
    pub saved_at: u64,
    /// When the refresh token was issued (each refresh rotates it).
    pub refresh_issued_at: u64,
    /// Unix seconds before which a proactive refresh is not attempted.
    #[serde(default)]
    pub earliest_refresh_at: Option<u64>,
}

pub const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";

impl Account {
    /// The grant includes ChatGPT plan inference. A valid ID token alone
    /// does not authorize plan usage.
    pub fn plan_usage_granted(&self) -> bool {
        self.scopes.iter().any(|s| s == PLAN_SCOPE)
    }
}

impl Tokens {
    pub fn expires_at(&self) -> u64 {
        self.saved_at.saturating_add(self.expires_in)
    }

    pub fn refresh_expires_at(&self) -> u64 {
        self.refresh_issued_at
            .saturating_add(REFRESH_TOKEN_LIFETIME_SECS)
    }
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at())
            .finish()
    }
}

impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("client_id", &self.client_id)
            .field("scopes", &self.scopes)
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .field("tokens", &self.tokens)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    format: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    /// Could not read or write the file (permissions, missing volume, …).
    Io(String),
    /// Wrong `CHATGPT_CREDENTIALS_KEY`, or the file was modified.
    Decrypt,
    /// Not an OpenGate credential file.
    Format,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "credential file I/O failed: {e}"),
            StoreError::Decrypt => f.write_str(
                "credential file cannot be decrypted (wrong CHATGPT_CREDENTIALS_KEY or modified file)",
            ),
            StoreError::Format => f.write_str("credential file is not in OpenGate's format"),
        }
    }
}

/// Identifies one version of the file on disk; every save renames a new
/// inode into place, so any change by another process alters the stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    ino: u64,
    mtime_ns: i128,
    len: u64,
}

pub struct Store {
    path: PathBuf,
    cipher: XChaCha20Poly1305,
}

impl Store {
    pub fn new(path: PathBuf, key: &CredentialsKey) -> Self {
        Store {
            path,
            cipher: XChaCha20Poly1305::new((&key.0).into()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `None` if the file does not exist.
    pub fn stamp(&self) -> Option<Stamp> {
        let m = fs::metadata(&self.path).ok()?;
        Some(Stamp {
            ino: m.ino(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            len: m.len(),
        })
    }

    /// `Ok(None)` if the file does not exist yet.
    pub fn load(&self) -> Result<Option<(StoreFile, Stamp)>, StoreError> {
        let stamp = self.stamp();
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(StoreError::Io(e.kind().to_string())),
        };
        let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|_| StoreError::Format)?;
        if envelope.format != FORMAT {
            return Err(StoreError::Format);
        }
        let nonce = STANDARD
            .decode(envelope.nonce)
            .ok()
            .filter(|n| n.len() == 24)
            .ok_or(StoreError::Format)?;
        let ciphertext = STANDARD
            .decode(envelope.ciphertext)
            .map_err(|_| StoreError::Format)?;
        let plain = self
            .cipher
            .decrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: AAD,
                },
            )
            .map_err(|_| StoreError::Decrypt)?;
        let file = serde_json::from_slice(&plain).map_err(|_| StoreError::Format)?;
        // Stamp taken before the read: if the file changed in between, the
        // next comparison simply triggers one more reload.
        Ok(Some((file, stamp.ok_or(StoreError::Format)?)))
    }

    /// The existing file, or a new one with a fresh host ID (not yet saved).
    pub fn load_or_new(&self) -> Result<StoreFile, StoreError> {
        Ok(match self.load()? {
            Some((file, _)) => file,
            None => StoreFile {
                ext_agent_host_id: new_host_id(),
                account: None,
            },
        })
    }

    pub fn save(&self, file: &StoreFile) -> Result<Stamp, StoreError> {
        let io_err = |e: io::Error| StoreError::Io(e.kind().to_string());
        let dir = self
            .path
            .parent()
            .ok_or_else(|| StoreError::Io("no parent directory".into()))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(io_err)?;

        let plain = serde_json::to_vec(file).expect("serializable");
        let nonce = random_bytes::<24>();
        let ciphertext = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: &plain,
                    aad: AAD,
                },
            )
            .expect("encryption cannot fail for in-memory data");
        let envelope = serde_json::to_vec_pretty(&Envelope {
            format: FORMAT.into(),
            nonce: STANDARD.encode(nonce),
            ciphertext: STANDARD.encode(ciphertext),
        })
        .expect("serializable");

        let name = self.path.file_name().expect("validated in config");
        let tmp = dir.join(format!(
            ".{}.tmp-{}",
            name.to_string_lossy(),
            hex(&random_bytes::<6>())
        ));
        let result = (|| {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&envelope)?;
            f.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            // Make the rename itself durable.
            fs::File::open(dir)?.sync_all()
        })();
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(io_err(e));
        }
        self.stamp()
            .ok_or_else(|| StoreError::Io("file vanished after save".into()))
    }
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("the OS random number generator is available");
    buf
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A UUIDv4 host identifier in the `urn:uuid:` form the docs accept.
pub fn new_host_id() -> String {
    let mut b = random_bytes::<16>();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn key(byte: u8) -> CredentialsKey {
        CredentialsKey([byte; 32])
    }

    fn tmpdir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("opengate-store-{}", hex(&random_bytes::<8>())));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> StoreFile {
        StoreFile {
            ext_agent_host_id: new_host_id(),
            account: Some(Account {
                issuer: "https://auth.openai.com".into(),
                subject: "user-1".into(),
                email: Some("a@example.com".into()),
                client_id: "oaiapp_1".into(),
                id_token: Some("id-token-secret".into()),
                scopes: vec![PLAN_SCOPE.into()],
                tokens: Some(Tokens {
                    access_token: "access-secret".into(),
                    refresh_token: "refresh-secret".into(),
                    token_type: "Bearer".into(),
                    expires_in: 3600,
                    saved_at: 1,
                    refresh_issued_at: 1,
                    earliest_refresh_at: None,
                }),
                ended: None,
            }),
        }
    }

    #[test]
    fn round_trip_is_encrypted_and_private() {
        let path = tmpdir().join("sub").join("credentials.json");
        let store = Store::new(path.clone(), &key(7));
        assert_eq!(store.load().unwrap().map(|_| ()), None);
        let stamp = store.save(&sample()).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        for secret in [
            "access-secret",
            "refresh-secret",
            "id-token-secret",
            "a@example.com",
        ] {
            assert!(!raw.contains(secret), "file leaks {secret}");
        }
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);

        let (file, loaded_stamp) = store.load().unwrap().unwrap();
        assert_eq!(loaded_stamp, stamp);
        let tokens = file.account.unwrap().tokens.unwrap();
        assert_eq!(tokens.access_token, "access-secret");

        // A later save produces a different stamp.
        assert_ne!(store.save(&sample()).unwrap(), stamp);
    }

    #[test]
    fn wrong_key_or_tampering_is_detected() {
        let path = tmpdir().join("credentials.json");
        Store::new(path.clone(), &key(1)).save(&sample()).unwrap();
        assert_eq!(
            Store::new(path.clone(), &key(2)).load().err(),
            Some(StoreError::Decrypt)
        );

        let mut env: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let mut ct = STANDARD
            .decode(env["ciphertext"].as_str().unwrap())
            .unwrap();
        ct[0] ^= 1;
        env["ciphertext"] = STANDARD.encode(ct).into();
        fs::write(&path, env.to_string()).unwrap();
        assert_eq!(
            Store::new(path.clone(), &key(1)).load().err(),
            Some(StoreError::Decrypt)
        );
        fs::write(&path, "{}").unwrap();
        assert_eq!(
            Store::new(path, &key(1)).load().err(),
            Some(StoreError::Format)
        );
    }

    #[test]
    fn debug_never_prints_tokens() {
        let printed = format!("{:?}", sample().account.unwrap());
        assert!(!printed.contains("secret"), "{printed}");
    }

    #[test]
    fn host_ids_are_uuid_urns() {
        let id = new_host_id();
        assert!(id.starts_with("urn:uuid:") && id.len() == 45, "{id}");
        assert_eq!(&id[23..24], "4", "version 4");
        assert_ne!(id, new_host_id());
    }
}
