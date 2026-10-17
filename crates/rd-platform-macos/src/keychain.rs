//! Agent token storage in the login Keychain (generic password, service `dev.thijmen.rd.agent`,
//! account `gateway-token`). TO BE IMPLEMENTED.

pub const SERVICE: &str = "dev.thijmen.rd.agent";
pub const ACCOUNT: &str = "gateway-token";

pub fn load_token() -> anyhow::Result<Option<String>> {
    todo!("rd-platform-macos: keychain::load_token")
}

pub fn store_token(token: &str) -> anyhow::Result<()> {
    let _ = token;
    todo!()
}
