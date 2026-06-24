//! CLI configuration loading from `~/.recalld/config.toml`.
//!
//! [`CliConfig`] provides the base configuration for the CLI binary.
//! All fields have defaults so the CLI works without a config file.
//! The precedence hierarchy is:
//!
//! 1. CLI flags (`--server`, `--format`) — highest priority
//! 2. Environment variables (`$RECALLD_URL`) — applied by clap
//! 3. Config file (`~/.recalld/config.toml`)
//! 4. Compiled defaults — lowest priority

use std::path::PathBuf;

use serde::Deserialize;

use crate::cli::commands::OutputFormat;

/// CLI configuration loaded from `~/.recalld/config.toml`.
///
/// All fields have defaults so the CLI works without a config file.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CliConfig {
    /// Base URL of the Recalld API server.
    pub server_url: String,

    /// Default namespace for commands that accept --namespace.
    pub default_namespace: String,

    /// Default output format (json or human).
    #[serde(deserialize_with = "deserialize_output_format")]
    pub default_format: OutputFormat,
}

impl Default for CliConfig {
    fn default() -> Self {
        Self {
            server_url: "http://localhost:7878".to_string(),
            default_namespace: "default".to_string(),
            default_format: OutputFormat::Json,
        }
    }
}

impl CliConfig {
    /// Load config from disk with the following precedence:
    ///
    /// 1. CLI flags (`--server`, `--format`) — highest priority, applied
    ///    after this function returns
    /// 2. Environment variables (`$RECALLD_URL`) — applied by clap
    ///    via `env` attribute
    /// 3. Config file (`~/.recalld/config.toml`)
    /// 4. Defaults — lowest priority
    ///
    /// If the config file does not exist, returns defaults silently.
    /// If the config file exists but is malformed, returns an error.
    pub fn load() -> crate::cli::Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = std::fs::read_to_string(&path).map_err(|e| {
            crate::cli::CliError::Config(format!("failed to read {}: {e}", path.display()))
        })?;

        let config: Self = toml::from_str(&contents).map_err(|e| {
            crate::cli::CliError::Config(format!("failed to parse {}: {e}", path.display()))
        })?;

        Ok(config)
    }

    /// Resolve the config file path: `~/.recalld/config.toml`.
    pub fn config_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".recalld")
            .join("config.toml")
    }

    /// Apply CLI flag overrides. Called after clap parsing.
    pub fn apply_overrides(&mut self, cli: &crate::cli::commands::Cli) {
        if let Some(ref server) = cli.server {
            self.server_url = server.clone();
        }
        // format override is handled in the dispatch layer (cli.format
        // takes precedence over config.default_format).
    }
}

/// Custom deserializer for [`OutputFormat`] from a TOML string.
fn deserialize_output_format<'de, D>(deserializer: D) -> std::result::Result<OutputFormat, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    match s.to_lowercase().as_str() {
        "json" => Ok(OutputFormat::Json),
        "human" => Ok(OutputFormat::Human),
        other => Err(serde::de::Error::custom(format!(
            "unknown output format '{other}', expected 'json' or 'human'"
        ))),
    }
}
