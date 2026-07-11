//! Ed25519 company identity: the [`LocalSigner`].
//!
//! A company's tiny.place identity is a single Ed25519 keypair. The 32-byte
//! seed persists at `keys/agent.ed25519` in the company bundle, hex-encoded,
//! restricted to `0600` on unix, and excluded from bundle exports (see
//! [`crate::store::Bundle::EXPORT_EXCLUDES`]). The `agentId` surfaced to
//! tiny.place is the base58 (Solana-style) encoding of the 32-byte public key.
//!
//! Everything here is offline: keys are generated from `OsRng`, and signing and
//! verification never touch the network.

use ed25519_dalek::{Signer as _, SigningKey, VerifyingKey};

use crate::error::OpenCompanyError;
use crate::store::Bundle;
use crate::store::paths::restrict_file;
use crate::{Result, ports::types::CompanyId};

/// A company's local Ed25519 signer.
///
/// Wraps a [`SigningKey`]; the public key doubles as the tiny.place `agentId`
/// via its base58 encoding.
pub struct LocalSigner {
    keypair: SigningKey,
}

impl LocalSigner {
    /// Builds a signer from a raw 32-byte Ed25519 seed.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            keypair: SigningKey::from_bytes(seed),
        }
    }

    /// Generates a fresh signer from the operating system's CSPRNG.
    pub fn generate() -> Self {
        use rand_core::RngCore as _;

        let mut seed = [0u8; 32];
        rand_core::OsRng.fill_bytes(&mut seed);
        Self::from_seed(&seed)
    }

    /// The base58 (Solana-style) address of the 32-byte public key. This is the
    /// company's tiny.place `agentId`.
    pub fn agent_id(&self) -> String {
        bs58::encode(self.public_key_bytes()).into_string()
    }

    /// The raw 32-byte Ed25519 public key.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.keypair.verifying_key().to_bytes()
    }

    /// The raw 32-byte seed. Kept crate-private so it is never serialized into
    /// an exportable surface.
    pub(crate) fn seed_bytes(&self) -> [u8; 32] {
        self.keypair.to_bytes()
    }

    /// Signs `msg`, returning the 64-byte Ed25519 signature.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        self.keypair.sign(msg).to_bytes()
    }

    /// Signs `msg` and returns the signature base58-encoded (the wire form used
    /// in SIWX and x402 authorizations).
    pub fn sign_b58(&self, msg: &[u8]) -> String {
        bs58::encode(self.sign(msg)).into_string()
    }
}

/// Verifies a base58-encoded Ed25519 signature over `msg` by a given base58
/// `agent_id` (the signer's public key).
///
/// Returns `Ok(())` on a valid signature, or [`OpenCompanyError::InvalidRequest`]
/// when the id or signature is malformed or the signature does not verify.
pub fn verify_b58(agent_id: &str, msg: &[u8], signature_b58: &str) -> Result<()> {
    let pubkey_bytes = decode_32(agent_id).ok_or_else(|| {
        OpenCompanyError::InvalidRequest(format!(
            "agentId `{agent_id}` is not a 32-byte base58 key"
        ))
    })?;
    let verifying = VerifyingKey::from_bytes(&pubkey_bytes).map_err(|_| {
        OpenCompanyError::InvalidRequest("agentId is not a valid Ed25519 key".into())
    })?;

    let sig_bytes = decode_64(signature_b58).ok_or_else(|| {
        OpenCompanyError::InvalidRequest("signature is not 64 base58 bytes".into())
    })?;
    let signature = ed25519_dalek::Signature::from_bytes(&sig_bytes);

    use ed25519_dalek::Verifier as _;
    verifying
        .verify(msg, &signature)
        .map_err(|_| OpenCompanyError::InvalidRequest("signature does not verify".into()))
}

/// Loads the company's signer from `keys/agent.ed25519`, generating and
/// persisting a fresh key (hex seed, `0600`) if the file is absent.
pub async fn load_or_create_signer(bundle: &Bundle) -> Result<LocalSigner> {
    bundle.ensure_dirs().await?;
    let path = bundle.agent_key();

    match tokio::fs::read_to_string(&path).await {
        Ok(text) => {
            let seed = decode_hex_seed(text.trim()).ok_or_else(|| {
                OpenCompanyError::Store(format!(
                    "identity key {} is corrupt (expected 64 hex chars)",
                    path.display()
                ))
            })?;
            Ok(LocalSigner::from_seed(&seed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let signer = LocalSigner::generate();
            let hex = encode_hex(&signer.seed_bytes());
            tokio::fs::write(&path, hex.as_bytes())
                .await
                .map_err(|source| OpenCompanyError::StoreIo {
                    path: path.clone(),
                    source,
                })?;
            restrict_file(&path)?;
            Ok(signer)
        }
        Err(source) => Err(OpenCompanyError::StoreIo { path, source }),
    }
}

/// Resolves the signer for a [`CompanyId`] under an OpenCompany home root.
pub async fn signer_for(
    root: impl Into<std::path::PathBuf>,
    id: &CompanyId,
) -> Result<LocalSigner> {
    let bundle = Bundle::new(root, id);
    load_or_create_signer(&bundle).await
}

// ---------------------------------------------------------------------------
// Small dependency-free codecs (avoid pulling a `hex` crate).
// ---------------------------------------------------------------------------

fn encode_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn decode_hex_seed(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = text.as_bytes();
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn decode_32(b58: &str) -> Option<[u8; 32]> {
    let v = bs58::decode(b58).into_vec().ok()?;
    v.try_into().ok()
}

fn decode_64(b58: &str) -> Option<[u8; 64]> {
    let v = bs58::decode(b58).into_vec().ok()?;
    v.try_into().ok()
}

#[cfg(test)]
mod test {
    use super::*;

    fn tmp_bundle() -> (tempfile::TempDir, Bundle) {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = Bundle::new(dir.path().to_path_buf(), &CompanyId::new("acme"));
        (dir, bundle)
    }

    #[tokio::test]
    async fn keygen_persists_and_reloads_same_agent_id() {
        let (_dir, bundle) = tmp_bundle();
        let first = load_or_create_signer(&bundle).await.expect("create");
        assert!(bundle.agent_key().exists(), "seed file written");

        let again = load_or_create_signer(&bundle).await.expect("reload");
        assert_eq!(
            first.agent_id(),
            again.agent_id(),
            "reload yields identical identity"
        );
    }

    #[test]
    fn agent_id_is_base58_of_32_byte_pubkey() {
        let signer = LocalSigner::generate();
        let id = signer.agent_id();
        let decoded = bs58::decode(&id).into_vec().expect("base58");
        assert_eq!(decoded.len(), 32);
        assert_eq!(decoded, signer.public_key_bytes().to_vec());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn seed_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, bundle) = tmp_bundle();
        load_or_create_signer(&bundle).await.expect("create");
        let meta = std::fs::metadata(bundle.agent_key()).expect("metadata");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn sign_verify_round_trip_and_wrong_key_fails() {
        let signer = LocalSigner::generate();
        let other = LocalSigner::generate();
        let msg = b"canonical-payload";

        let sig = signer.sign_b58(msg);
        verify_b58(&signer.agent_id(), msg, &sig).expect("valid signature verifies");

        // Same signature attributed to a different agent must fail.
        assert!(verify_b58(&other.agent_id(), msg, &sig).is_err());
        // Tampered message must fail.
        assert!(verify_b58(&signer.agent_id(), b"tampered", &sig).is_err());
    }

    #[test]
    fn hex_codec_round_trips() {
        let seed = [
            0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 255,
        ];
        let hex = encode_hex(&seed);
        assert_eq!(hex.len(), 64);
        assert_eq!(decode_hex_seed(&hex), Some(seed));
        assert_eq!(decode_hex_seed("xyz"), None);
    }
}
