use anyhow::Context;

use crate::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub dns: Vec<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = dirs::config_dir().map(|p| p.join("ecrou.yaml"));

        match config_path {
            Some(path) if path.exists() => {
                let content =
                    std::fs::read_to_string(&path).context("Failed to read config file")?;

                let config: Config =
                    serde_yaml::from_str(&content).context("Failed to parse config file")?;

                // Set defaults
                let config = Config {
                    dns: if config.dns.is_empty() {
                        vec!["1.1.1.1".to_string()]
                    } else {
                        config.dns
                    },
                };

                Ok(config)
            }
            _ => {
                // No config file, use defaults
                Ok(Config {
                    dns: vec!["1.1.1.1".to_string()],
                })
            }
        }
    }
}
