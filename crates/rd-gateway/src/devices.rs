//! Devices added from the portal (*Add device*): agent machines whose tokens the gateway issued
//! itself, kept in `RD_STATE_DIR/devices.json`.
//!
//! Only the SHA-256 of a device's token is stored. The token itself is handed out once, inside
//! a setup code ([`rd_proto::setup_code`]), when the device is added or its token is replaced.
//! Devices live next to the read-only `RD_AGENT_TOKENS_DIR` (tokens provisioned from outside,
//! e.g. a Kubernetes secret); a machine id belongs to one of the two, and adding a device whose
//! id has a provisioned token is refused.
//!
//! Every change is written to a temporary file and renamed over `devices.json`, so a crash never
//! leaves a half-written store; a change that cannot be saved is undone. Without a state
//! directory, dev mode keeps devices in memory (lost on restart) and production turns the
//! feature off.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use rd_proto::setup_code::{TOKEN_PREFIX, valid_machine_id};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::util::unix_ms;

const FILE: &str = "devices.json";
const FILE_VERSION: u32 = 1;
/// Longest display name accepted, in characters.
pub const NAME_MAX: usize = 64;

/// One device as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub machine: String,
    /// Display name for the portal; the machine id when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Hex SHA-256 of the current token.
    pub token_sha256: String,
    pub created_ms: u64,
    /// Viewer email that added the device.
    pub created_by: String,
    /// When the current token was issued (= `created_ms` until it is replaced).
    pub token_ms: u64,
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    devices: Vec<Device>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceError {
    #[error("adding devices is turned off (no RD_STATE_DIR)")]
    Disabled,
    #[error("machine ids are 1-64 characters of A-Z, a-z, 0-9, - and _")]
    InvalidMachine,
    #[error("names are at most {NAME_MAX} characters, without control characters")]
    InvalidName,
    #[error("a device with this machine id already exists")]
    Exists,
    #[error("this machine id has a provisioned token (RD_AGENT_TOKENS_DIR); pick another id")]
    Provisioned,
    #[error("no device with this machine id")]
    NotFound,
    #[error("the device store could not be saved: {0}")]
    Save(String),
}

enum Mode {
    Disabled,
    Memory,
    File(PathBuf),
}

/// The device store; cheap to query, every change saved before it is visible.
pub struct DeviceStore {
    mode: Mode,
    devices: Mutex<Vec<Device>>,
}

/// A new token for `machine`: `rd1.<machine>.<32 random bytes, base64url>`.
fn new_token(machine: &str) -> String {
    let secret = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    format!("{TOKEN_PREFIX}.{machine}.{secret}")
}

fn hash_hex(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Trimmed name, `None` when empty.
fn clean_name(name: Option<&str>) -> Result<Option<String>, DeviceError> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if name.chars().count() > NAME_MAX || name.chars().any(char::is_control) {
        return Err(DeviceError::InvalidName);
    }
    Ok(Some(name.to_owned()))
}

impl DeviceStore {
    /// Adding devices is off.
    pub fn disabled() -> Self {
        DeviceStore {
            mode: Mode::Disabled,
            devices: Mutex::new(Vec::new()),
        }
    }

    /// Devices are kept in memory only (dev mode without a state directory).
    pub fn in_memory() -> Self {
        DeviceStore {
            mode: Mode::Memory,
            devices: Mutex::new(Vec::new()),
        }
    }

    /// Loads (or starts) `dir/devices.json`. A store that cannot be read fails start-up rather
    /// than silently dropping devices.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating RD_STATE_DIR {}", dir.display()))?;
        let path = dir.join(FILE);
        let devices = match std::fs::read(&path) {
            Ok(bytes) => {
                let file: StoreFile = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?;
                if file.version != FILE_VERSION {
                    bail!(
                        "{} has version {}, this gateway reads version {FILE_VERSION}",
                        path.display(),
                        file.version
                    );
                }
                for d in &file.devices {
                    let mut hash = [0u8; 32];
                    if !valid_machine_id(&d.machine)
                        || hex::decode_to_slice(&d.token_sha256, &mut hash).is_err()
                    {
                        bail!(
                            "{} has an invalid entry for {:?}",
                            path.display(),
                            d.machine
                        );
                    }
                }
                file.devices
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(DeviceStore {
            mode: Mode::File(path),
            devices: Mutex::new(devices),
        })
    }

    pub fn enabled(&self) -> bool {
        !matches!(self.mode, Mode::Disabled)
    }

    /// For logs: where devices are kept.
    pub fn describe(&self) -> String {
        match &self.mode {
            Mode::Disabled => "off".into(),
            Mode::Memory => "in memory".into(),
            Mode::File(path) => path.display().to_string(),
        }
    }

    /// SHA-256 of `machine`'s current token, if it is a device.
    pub fn token_hash(&self, machine: &str) -> Option<[u8; 32]> {
        let devices = self.devices.lock();
        let d = devices.iter().find(|d| d.machine == machine)?;
        let mut hash = [0u8; 32];
        hex::decode_to_slice(&d.token_sha256, &mut hash).ok()?;
        Some(hash)
    }

    pub fn list(&self) -> Vec<Device> {
        self.devices.lock().clone()
    }

    /// Adds a device and returns its token. `provisioned` says whether a machine id already
    /// has a token in `RD_AGENT_TOKENS_DIR`.
    pub fn add(
        &self,
        machine: &str,
        name: Option<&str>,
        by: &str,
        provisioned: impl FnOnce(&str) -> bool,
    ) -> Result<(Device, String), DeviceError> {
        if !self.enabled() {
            return Err(DeviceError::Disabled);
        }
        if !valid_machine_id(machine) {
            return Err(DeviceError::InvalidMachine);
        }
        let name = clean_name(name)?;
        let mut devices = self.devices.lock();
        if devices.iter().any(|d| d.machine == machine) {
            return Err(DeviceError::Exists);
        }
        if provisioned(machine) {
            return Err(DeviceError::Provisioned);
        }
        let token = new_token(machine);
        let now = unix_ms();
        let device = Device {
            machine: machine.to_owned(),
            name,
            token_sha256: hash_hex(&token),
            created_ms: now,
            created_by: by.to_owned(),
            token_ms: now,
        };
        devices.push(device.clone());
        if let Err(e) = self.save(&devices) {
            devices.pop();
            return Err(e);
        }
        Ok((device, token))
    }

    /// Issues a new token for `machine` (the old one stops working) and returns it.
    pub fn replace_token(&self, machine: &str) -> Result<(Device, String), DeviceError> {
        if !self.enabled() {
            return Err(DeviceError::Disabled);
        }
        let mut devices = self.devices.lock();
        let i = devices
            .iter()
            .position(|d| d.machine == machine)
            .ok_or(DeviceError::NotFound)?;
        let token = new_token(machine);
        let old = devices[i].clone();
        devices[i].token_sha256 = hash_hex(&token);
        devices[i].token_ms = unix_ms();
        if let Err(e) = self.save(&devices) {
            devices[i] = old;
            return Err(e);
        }
        Ok((devices[i].clone(), token))
    }

    pub fn remove(&self, machine: &str) -> Result<Device, DeviceError> {
        if !self.enabled() {
            return Err(DeviceError::Disabled);
        }
        let mut devices = self.devices.lock();
        let i = devices
            .iter()
            .position(|d| d.machine == machine)
            .ok_or(DeviceError::NotFound)?;
        let removed = devices.remove(i);
        if let Err(e) = self.save(&devices) {
            devices.insert(i, removed);
            return Err(e);
        }
        Ok(removed)
    }

    /// Writes `devices` to the store file (no-op in memory). Called with the lock held, so
    /// saves never interleave.
    fn save(&self, devices: &[Device]) -> Result<(), DeviceError> {
        let Mode::File(path) = &self.mode else {
            return Ok(());
        };
        let file = StoreFile {
            version: FILE_VERSION,
            devices: devices.to_vec(),
        };
        let json = serde_json::to_vec_pretty(&file).expect("device store serializes");
        let tmp = path.with_extension("json.tmp");
        let write = || -> std::io::Result<()> {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            DeviceError::Save(e.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use rd_proto::setup_code::token_machine;

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rd-gw-devices-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn add_replace_remove_persist() {
        let dir = temp_dir("persist");
        let store = DeviceStore::open(&dir).unwrap();
        let (device, token) = store
            .add("studio", Some(" Mac Studio "), "me@example.test", none)
            .unwrap();
        assert_eq!(device.name.as_deref(), Some("Mac Studio"));
        assert_eq!(token_machine(&token), Some("studio"));
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        assert_eq!(store.token_hash("studio"), Some(hash));
        assert_eq!(store.token_hash("other"), None);

        // Survives a restart; only the hash is on disk.
        let on_disk = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert!(!on_disk.contains(&token));
        let reopened = DeviceStore::open(&dir).unwrap();
        assert_eq!(reopened.list(), store.list());

        let (_, new_token) = reopened.replace_token("studio").unwrap();
        assert_ne!(new_token, token);
        let new_hash: [u8; 32] = Sha256::digest(new_token.as_bytes()).into();
        assert_eq!(reopened.token_hash("studio"), Some(new_hash));
        assert_eq!(
            reopened.replace_token("nope").unwrap_err(),
            DeviceError::NotFound
        );

        reopened.remove("studio").unwrap();
        assert!(DeviceStore::open(&dir).unwrap().list().is_empty());
        assert_eq!(
            reopened.remove("studio").unwrap_err(),
            DeviceError::NotFound
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_bad_input_duplicates_and_provisioned_ids() {
        let store = DeviceStore::in_memory();
        assert_eq!(
            store.add("a.b", None, "me", none).unwrap_err(),
            DeviceError::InvalidMachine
        );
        assert_eq!(
            store.add("pc", Some("bad\nname"), "me", none).unwrap_err(),
            DeviceError::InvalidName
        );
        assert_eq!(
            store
                .add("pc", Some(&"x".repeat(NAME_MAX + 1)), "me", none)
                .unwrap_err(),
            DeviceError::InvalidName
        );
        assert_eq!(
            store.add("mbp", None, "me", |m| m == "mbp").unwrap_err(),
            DeviceError::Provisioned
        );
        let (device, _) = store.add("pc", Some("  "), "me", none).unwrap();
        assert_eq!(device.name, None);
        assert_eq!(
            store.add("pc", None, "me", none).unwrap_err(),
            DeviceError::Exists
        );
        assert_eq!(
            DeviceStore::disabled()
                .add("pc", None, "me", none)
                .unwrap_err(),
            DeviceError::Disabled
        );
    }

    #[test]
    fn a_failed_save_leaves_the_store_unchanged() {
        let dir = temp_dir("readonly");
        let store = DeviceStore::open(&dir).unwrap();
        store.add("pc", None, "me", none).unwrap();
        // A directory where the temporary file should go makes every save fail.
        std::fs::create_dir(dir.join(format!("{FILE}.tmp"))).unwrap();
        assert!(matches!(
            store.add("other", None, "me", none),
            Err(DeviceError::Save(_))
        ));
        assert!(matches!(store.remove("pc"), Err(DeviceError::Save(_))));
        assert_eq!(store.list().len(), 1);
        assert!(store.token_hash("pc").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn refuses_a_corrupt_store() {
        let dir = temp_dir("corrupt");
        std::fs::write(dir.join(FILE), b"{not json").unwrap();
        assert!(DeviceStore::open(&dir).is_err());
        std::fs::write(
            dir.join(FILE),
            br#"{"version":1,"devices":[{"machine":"a/b","token_sha256":"00","created_ms":0,"created_by":"x","token_ms":0}]}"#,
        )
        .unwrap();
        assert!(DeviceStore::open(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
