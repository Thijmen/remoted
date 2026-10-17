//! Agent configuration (`config.toml`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Gateway base URL for agents, e.g. `wss://rd-agent.example.com` or `ws://127.0.0.1:8081`.
    pub gateway_url: String,
    /// Machine id, e.g. "mbp". Must match the token's machine part.
    pub machine: String,
    /// Token `rd1.<machine>.<secret>`. Usually not in the file: loaded from the Keychain /
    /// Credential Manager, or `RD_AGENT_TOKEN` env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Defaults for new sessions (the viewer's `start` overrides).
    #[serde(default)]
    pub defaults: StreamDefaults,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamDefaults {
    pub max_kbps: u32,
    pub fps: u32,
}

impl Default for StreamDefaults {
    fn default() -> Self {
        StreamDefaults {
            max_kbps: 40_000,
            fps: 60,
        }
    }
}

impl AgentConfig {
    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&s)?)
    }
    pub fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}
