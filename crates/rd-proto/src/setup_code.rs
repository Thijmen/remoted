//! Setup codes: everything an agent needs to join a gateway, as one string to paste.
//!
//! The portal's *Add device* creates a device and shows its setup code once; `rd-agent setup`
//! (and the Windows installer's finish page) accepts it instead of the gateway URL, machine id
//! and token. Format: `rds1_` followed by unpadded base64url of the JSON object
//! `{"gateway":"wss://…","token":"rd1.<machine>.<secret>"}`. The machine id is the token's.
//! A setup code is as secret as the token it carries.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

/// Prefix of every setup code (version 1).
pub const PREFIX: &str = "rds1_";

/// Agent token prefix: tokens are `rd1.<machine>.<secret>`.
pub const TOKEN_PREFIX: &str = "rd1";

/// Machine ids appear in URLs, token files and tokens, so they are restricted to
/// `[A-Za-z0-9_-]{1,64}` (no dots, no path separators).
pub fn valid_machine_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The machine of an `rd1.<machine>.<secret>` token (validated), if it has that form.
pub fn token_machine(token: &str) -> Option<&str> {
    let mut parts = token.splitn(3, '.');
    let (prefix, machine, secret) = (parts.next()?, parts.next()?, parts.next()?);
    (prefix == TOKEN_PREFIX && !secret.is_empty() && valid_machine_id(machine)).then_some(machine)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupCode {
    /// Where agents connect, e.g. `wss://rd-agent.example.com`.
    pub gateway: String,
    /// `rd1.<machine>.<secret>`.
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetupCodeError {
    #[error("not a setup code (it starts with {PREFIX})")]
    Prefix,
    #[error("the setup code is damaged (copy it again)")]
    Encoding,
    #[error("the setup code's gateway URL must start with ws:// or wss://")]
    Gateway,
    #[error("the setup code's token is not of the form rd1.<machine>.<secret>")]
    Token,
}

impl SetupCode {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("setup code serializes");
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(json))
    }

    /// Parses a pasted code; surrounding whitespace (and line breaks a terminal may add) is
    /// ignored.
    pub fn decode(code: &str) -> Result<Self, SetupCodeError> {
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
        let body = code.strip_prefix(PREFIX).ok_or(SetupCodeError::Prefix)?;
        let json = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| SetupCodeError::Encoding)?;
        let parsed: SetupCode =
            serde_json::from_slice(&json).map_err(|_| SetupCodeError::Encoding)?;
        let scheme_ok = parsed
            .gateway
            .split_once("://")
            .is_some_and(|(scheme, host)| matches!(scheme, "ws" | "wss") && !host.is_empty());
        if !scheme_ok {
            return Err(SetupCodeError::Gateway);
        }
        if token_machine(&parsed.token).is_none() {
            return Err(SetupCodeError::Token);
        }
        Ok(parsed)
    }

    /// The machine id (from the token).
    pub fn machine(&self) -> &str {
        token_machine(&self.token).expect("validated in decode or built from a valid token")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> SetupCode {
        SetupCode {
            gateway: "wss://rd-agent.example.com".into(),
            token: "rd1.studio.test".into(),
        }
    }

    #[test]
    fn round_trips_and_names_the_machine() {
        let encoded = code().encode();
        assert!(encoded.starts_with(PREFIX));
        assert!(
            encoded[PREFIX.len()..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{encoded}"
        );
        let decoded = SetupCode::decode(&encoded).unwrap();
        assert_eq!(decoded, code());
        assert_eq!(decoded.machine(), "studio");
    }

    #[test]
    fn ignores_whitespace_from_copy_and_paste() {
        let encoded = code().encode();
        let (a, b) = encoded.split_at(12);
        let pasted = format!("  {a}\r\n{b} \n");
        assert_eq!(SetupCode::decode(&pasted).unwrap(), code());
    }

    #[test]
    fn rejects_damaged_or_foreign_codes() {
        assert_eq!(
            SetupCode::decode("rd1.studio.x"),
            Err(SetupCodeError::Prefix)
        );
        assert_eq!(SetupCode::decode("rds1_!!!"), Err(SetupCodeError::Encoding));
        let mut truncated = code().encode();
        truncated.truncate(truncated.len() - 3);
        assert_eq!(SetupCode::decode(&truncated), Err(SetupCodeError::Encoding));
        let https = SetupCode {
            gateway: "https://rd.example.com".into(),
            ..code()
        };
        assert_eq!(
            SetupCode::decode(&https.encode()),
            Err(SetupCodeError::Gateway)
        );
        let bad_token = SetupCode {
            token: "rd1.no/slash.x".into(),
            ..code()
        };
        assert_eq!(
            SetupCode::decode(&bad_token.encode()),
            Err(SetupCodeError::Token)
        );
    }

    #[test]
    fn machine_ids_and_tokens() {
        assert!(valid_machine_id("desktop-thijmen_1"));
        assert!(!valid_machine_id(""));
        assert!(!valid_machine_id("a.b"));
        assert!(!valid_machine_id(&"x".repeat(65)));
        assert_eq!(token_machine("rd1.mbp.s3cret"), Some("mbp"));
        assert_eq!(token_machine("rd1.mbp.with.dots"), Some("mbp"));
        assert_eq!(token_machine("rd1.mbp."), None);
        assert_eq!(token_machine("rd2.mbp.x"), None);
        assert_eq!(token_machine("garbage"), None);
    }
}
