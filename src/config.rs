use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub listen: String,
    pub upstream_url: String,
    pub pcr0: String,
    pub pcr1: String,
    pub pcr2: String,
    #[serde(default)]
    pub debug_mode: bool,
    #[serde(default)]
    pub enclave_host: Option<String>,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path.as_ref())?;
        let cfg: Self = toml::from_str(&s)?;
        Ok(cfg)
    }

    pub fn pcrs(&self) -> anyhow::Result<enclavia_protocol::attestation::Pcrs> {
        Ok(enclavia_protocol::attestation::Pcrs {
            pcr0: hex::decode(&self.pcr0)?,
            pcr1: hex::decode(&self.pcr1)?,
            pcr2: hex::decode(&self.pcr2)?,
        })
    }
}
