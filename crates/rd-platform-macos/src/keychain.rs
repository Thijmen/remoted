//! Agent token storage in the login Keychain (generic password, service
//! `dev.thijmen.rd.agent`, account `gateway-token`).

use anyhow::Context as _;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use security_framework_sys::base::errSecItemNotFound;

pub const SERVICE: &str = "dev.thijmen.rd.agent";
pub const ACCOUNT: &str = "gateway-token";

/// `errSecNoSuchKeychain`: there is no (default) keychain at all, e.g. outside a login
/// session. Not in `security-framework-sys`.
const ERR_SEC_NO_SUCH_KEYCHAIN: i32 = -25294;

/// The item does not exist (or there is no keychain to hold it).
fn absent(code: i32) -> bool {
    code == errSecItemNotFound || code == ERR_SEC_NO_SUCH_KEYCHAIN
}

/// The stored gateway token, or `None` if there is none (no item, or no keychain).
pub fn load_token() -> anyhow::Result<Option<String>> {
    load(SERVICE, ACCOUNT)
}

/// Store (or replace) the gateway token.
pub fn store_token(token: &str) -> anyhow::Result<()> {
    set_generic_password(SERVICE, ACCOUNT, token.as_bytes())
        .with_context(|| format!("storing Keychain item {SERVICE}/{ACCOUNT}"))
}

/// Remove the gateway token. Succeeds if there was none.
pub fn delete_token() -> anyhow::Result<()> {
    match delete_generic_password(SERVICE, ACCOUNT) {
        Err(e) if !absent(e.code()) => {
            Err(e).with_context(|| format!("deleting Keychain item {SERVICE}/{ACCOUNT}"))
        }
        _ => Ok(()),
    }
}

fn load(service: &str, account: &str) -> anyhow::Result<Option<String>> {
    match get_generic_password(service, account) {
        Ok(bytes) => {
            let token = String::from_utf8(bytes)
                .with_context(|| format!("Keychain item {service}/{account} is not UTF-8"))?;
            Ok(Some(token))
        }
        Err(e) if absent(e.code()) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading Keychain item {service}/{account}")),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn absent_codes() {
        assert!(super::absent(super::errSecItemNotFound));
        assert!(super::absent(-25294));
        // Denied or interaction-required is a real error, not "no token".
        assert!(!super::absent(-25293));
        assert!(!super::absent(-25308));
    }

    #[test]
    fn missing_item_is_none() {
        let service = format!("dev.thijmen.rd.agent.test-missing-{}", std::process::id());
        assert_eq!(super::load(&service, "nobody").unwrap(), None);
    }
}
