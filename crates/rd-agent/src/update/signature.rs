//! Update signatures (docs/updates.md, "Signing"): minisign, in Tauri's format. The public key
//! is base64 of the minisign public-key file and the same as rd Desktop's
//! (`plugins.updater.pubkey` in clients/desktop/src-tauri/tauri.conf.json); a signature is
//! base64 of the `.sig` file `tauri signer sign` writes.

use std::io::Read as _;
use std::path::Path;

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use minisign_verify::{Error as MinisignError, PublicKey, Signature};

/// The update signing key. Without its private half no installed agent can be updated: see
/// docs/updates.md before changing it.
const PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEU1MjVGQUFEMUNFNEE1QUQKUldTdHBlUWNyZm9sNVZsYWNCOGpsQkVlYTloM0VHSng2U0JscjNOM1F1MHRGZG5JZW9rRTRCVnkK";

/// Debug builds only: another key, for tests with a throwaway one. Release builds ignore it.
#[cfg(debug_assertions)]
const PUBLIC_KEY_ENV: &str = "RD_AGENT_UPDATE_PUBKEY";

fn public_key() -> anyhow::Result<PublicKey> {
    #[cfg(debug_assertions)]
    if let Some(key) = std::env::var(PUBLIC_KEY_ENV).ok().filter(|k| !k.is_empty()) {
        tracing::warn!("using the update key from {PUBLIC_KEY_ENV} (debug build)");
        return decode_public_key(&key);
    }
    decode_public_key(PUBLIC_KEY)
}

fn decode_public_key(b64: &str) -> anyhow::Result<PublicKey> {
    let text = decode_text(b64).context("invalid update public key")?;
    PublicKey::decode(&text).map_err(|e| anyhow::anyhow!("invalid update public key: {e}"))
}

fn decode_text(b64: &str) -> anyhow::Result<String> {
    let bytes = STANDARD.decode(b64.trim())?;
    Ok(String::from_utf8(bytes)?)
}

/// Checks `file` against `signature` (from the manifest) with the built-in key. The signature
/// must also name `version` (`tauri signer sign --app-version`): its trusted comment, which
/// the signature covers, has a `version:X.Y.Z` field. So a tampered manifest cannot offer an
/// older signed release under a newer version number.
pub fn verify_file(file: &Path, signature: &str, version: &str) -> anyhow::Result<()> {
    verify_with(&public_key()?, file, signature, version)
}

fn verify_with(key: &PublicKey, file: &Path, signature: &str, version: &str) -> anyhow::Result<()> {
    let text = decode_text(signature).context("the update's signature is not valid base64")?;
    let signature = Signature::decode(&text)
        .map_err(|e| anyhow::anyhow!("the update's signature is malformed: {e}"))?;
    let mut f = std::fs::File::open(file).with_context(|| format!("opening {}", file.display()))?;
    let result = match key.verify_stream(&signature) {
        Ok(mut verifier) => {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                verifier.update(&buf[..n]);
            }
            verifier.finalize()
        }
        // Signatures of the whole file rather than its hash (old minisign): accepted, as
        // tauri-plugin-updater does.
        Err(MinisignError::UnsupportedLegacyMode) => {
            let mut data = Vec::new();
            f.read_to_end(&mut data)?;
            key.verify(&data, &signature, true)
        }
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => {}
        Err(MinisignError::UnexpectedKeyId) => {
            bail!("the update is signed with another key than this agent trusts")
        }
        Err(e) => bail!("the update's signature does not match: {e}"),
    }
    // Only now is the trusted comment known to be genuine.
    match signed_version(signature.trusted_comment()) {
        Some(signed) if signed == version => Ok(()),
        Some(signed) => bail!("the update is signed as version {signed}, not {version}"),
        None => bail!("the update's signature does not name its version"),
    }
}

/// The `version:` field of a trusted comment (`timestamp:…\tfile:…\tversion:1.6.0`).
fn signed_version(trusted_comment: &str) -> Option<&str> {
    trusted_comment
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_key_decodes() {
        let key = decode_public_key(PUBLIC_KEY).unwrap();
        assert_eq!(
            key.untrusted_comment(),
            Some("untrusted comment: minisign public key: E525FAAD1CE4A5AD")
        );
    }

    /// A throwaway key, and signatures of [`TEST_FILE`] made with `tauri signer sign`
    /// (`--app-version 1.6.0`, none, and `--app-version 1.5.0`).
    #[test]
    fn verifies_version_bound_tauri_signatures() {
        let dir = std::env::temp_dir().join(format!("rd-agent-sig-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("update.bin");
        std::fs::write(&file, TEST_FILE).unwrap();
        let key = decode_public_key(TEST_PUBLIC_KEY).unwrap();
        verify_with(&key, &file, SIGNED_1_6_0, "1.6.0").unwrap();
        let err = |sig: &str, version: &str| {
            verify_with(&key, &file, sig, version)
                .unwrap_err()
                .to_string()
        };

        // A release signed for another version, or without one: refused.
        assert_eq!(
            err(SIGNED_1_5_0, "1.6.0"),
            "the update is signed as version 1.5.0, not 1.6.0"
        );
        assert_eq!(
            err(SIGNED_1_6_0, "1.6.1"),
            "the update is signed as version 1.6.0, not 1.6.1"
        );
        assert_eq!(
            err(SIGNED_NO_VERSION, "1.6.0"),
            "the update's signature does not name its version"
        );

        // Another file, or the right file with another key: refused.
        std::fs::write(&file, b"something else").unwrap();
        assert!(err(SIGNED_1_6_0, "1.6.0").contains("does not match"));
        std::fs::write(&file, TEST_FILE).unwrap();
        let real = decode_public_key(PUBLIC_KEY).unwrap();
        let other = verify_with(&real, &file, SIGNED_1_6_0, "1.6.0").unwrap_err();
        assert!(other.to_string().contains("another key"), "{other:#}");
        assert!(verify_with(&key, &file, "not base64!", "1.6.0").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_the_signed_version() {
        assert_eq!(
            signed_version(
                "timestamp:1792443754\tfile:rd-agent-1.6.0-windows-x64-setup.exe\tversion:1.6.0"
            ),
            Some("1.6.0")
        );
        assert_eq!(signed_version("timestamp:1\tfile:x"), None);
        // A file name that merely contains "version:" is not the field.
        assert_eq!(signed_version("timestamp:1\tfile:a version:9.9.9"), None);
    }

    const TEST_FILE: &[u8] = b"rd-agent update test file\n";
    const TEST_PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEUzMTNEREYzRUIxRjhBMkEKUldRcWloL3I4OTBUNDc2TmVkY2NhWTRjWWM3KzRMUjJ2cjVOOXFOY215aFpzZWw4a0tsNWNwczEK";
    const SIGNED_1_6_0: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRcWloL3I4OTBUNHlVWjhJQWJBR2tUU09qVTlYU0VNMUJGenRPY3lCeXVIeFRLMWx6T1JFY3NaWHBSSUs4TURIcTNvQjhhaGUzVy9VZkdPVSt1bW80aHk3MGJTYWFpOVFFPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyNDQzODExCWZpbGU6dXBkYXRlLmJpbgl2ZXJzaW9uOjEuNi4wCkg1eEloSkllRlkrQVRBYzlsakJuellhWEV5L2JJWkR4M05ET1lVMm84UzNXdnpheVhXbWxPVVg4M3Z4bW1yeVlLa3AyT3NVeEVmaWwxNVJaT3dlREJnPT0K";
    const SIGNED_NO_VERSION: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRcWloL3I4OTBUNHc3TGpQNVNmdi9RaFBxN1NjelZaQktrZGJQdzhkN1lISFlucE0rdzFGT1l3TCtLelZGNis5VHZDUHNYRDV2c0Q3alBzUUhnSm1zalI0Nk1wb2tjNGdZPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyNDQzODExCWZpbGU6cGxhaW4uYmluCjhxY3hDYU04ZC8yQ3JZQ3B4RFhkTVd4WWN4c3NOVEhoRjZrejhZbDJ3RkV0akxrZHVTQlJGYm5kQjFQam5ycEtmd0ZnMEhLM0RDcjZ5bUlpeEhOMEF3PT0K";
    const SIGNED_1_5_0: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRcWloL3I4OTBUNHhpejBwcEN1a25VK1A3RkNCTk45Y1B1QzRlbkRndUNVaVpGZmJpWHBkQm5sZDZ3bG9iRVFXc0Nwd1hsQjVoeG80VFE1UTN3VnUvWitScEN1elhQckFzPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyNDQzODExCWZpbGU6b2xkLmJpbgl2ZXJzaW9uOjEuNS4wCmJSVHIzUmVSZjVYVnhEMFJzeE9NT01RZUxCWGN6M052QkNLdFJMaWx2RG9qWi9qLytUS3lzVFFTTFBRNDZ2alhXTFVKZGlVeTJ4elVRbGZhSGRQREFRPT0K";
}
