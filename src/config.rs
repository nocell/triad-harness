use crate::model::ProviderKind;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub enabled: bool,
    pub binary: Option<PathBuf>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: None,
            model: None,
            reasoning_effort: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub leader_order: Vec<ProviderKind>,
    pub reviewer_timeout_minutes: u64,
    pub reducer_timeout_minutes: u64,
    pub fixer_timeout_minutes: u64,
    pub cooldown_minutes: i64,
    pub providers: BTreeMap<String, ProviderConfig>,
}

impl Default for Config {
    fn default() -> Self {
        let providers = ProviderKind::ALL
            .into_iter()
            .map(|provider| {
                let mut config = ProviderConfig::default();
                match provider {
                    ProviderKind::Claude => {
                        config.model = Some("claude-fable-5-1".into());
                    }
                    ProviderKind::Codex => {
                        config.model = Some("gpt-6-astra".into());
                        config.reasoning_effort = Some("max".into());
                    }
                    ProviderKind::Kimi => {
                        config.model = Some("kimi-code/k3".into());
                    }
                    ProviderKind::Cursor => config.model = Some("grok-4.6-fast".into()),
                }
                (provider.as_str().to_string(), config)
            })
            .collect();
        Self {
            leader_order: vec![
                ProviderKind::Codex,
                ProviderKind::Claude,
                ProviderKind::Cursor,
                ProviderKind::Kimi,
            ],
            reviewer_timeout_minutes: 45,
            reducer_timeout_minutes: 30,
            fixer_timeout_minutes: 90,
            cooldown_minutes: 15,
            providers,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&body).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self)?;
        crate::storage::atomic_write(&path, body.as_bytes())
    }

    pub fn path() -> Result<PathBuf> {
        if let Some(root) = std::env::var_os("TRIAD_CONFIG_HOME") {
            return Ok(PathBuf::from(root).join("config.toml"));
        }
        let root = dirs::config_dir().context("cannot determine config directory")?;
        Ok(root.join("triad/config.toml"))
    }

    pub fn provider(&self, provider: ProviderKind) -> ProviderConfig {
        let mut config = self
            .providers
            .get(provider.as_str())
            .cloned()
            .unwrap_or_default();
        match provider {
            ProviderKind::Claude => {
                config
                    .model
                    .get_or_insert_with(|| "claude-fable-5-1".into());
            }
            ProviderKind::Codex => {
                config.model.get_or_insert_with(|| "gpt-6-astra".into());
                config.reasoning_effort.get_or_insert_with(|| "max".into());
            }
            ProviderKind::Kimi => {
                config.model.get_or_insert_with(|| "kimi-code/k3".into());
            }
            ProviderKind::Cursor => {
                config.model.get_or_insert_with(|| "grok-4.6-fast".into());
            }
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_defaults_to_astra_with_max_reasoning_even_in_partial_config() {
        let mut config = Config::default();
        let codex = config.provider(ProviderKind::Codex);
        assert_eq!(codex.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(codex.reasoning_effort.as_deref(), Some("max"));

        config.providers.insert(
            "codex".into(),
            ProviderConfig {
                enabled: true,
                binary: Some("codex".into()),
                model: None,
                reasoning_effort: None,
            },
        );

        let codex = config.provider(ProviderKind::Codex);
        assert_eq!(codex.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(codex.reasoning_effort.as_deref(), Some("max"));
    }

    #[test]
    fn subscription_providers_keep_pinned_models_in_partial_config() {
        let mut config = Config::default();
        for provider in [
            ProviderKind::Claude,
            ProviderKind::Kimi,
            ProviderKind::Cursor,
        ] {
            config.providers.insert(
                provider.as_str().into(),
                ProviderConfig {
                    enabled: true,
                    binary: Some(provider.as_str().into()),
                    model: None,
                    reasoning_effort: None,
                },
            );
        }

        assert_eq!(
            config.provider(ProviderKind::Claude).model.as_deref(),
            Some("claude-fable-5-1")
        );
        assert_eq!(
            config.provider(ProviderKind::Kimi).model.as_deref(),
            Some("kimi-code/k3")
        );
        assert_eq!(
            config.provider(ProviderKind::Cursor).model.as_deref(),
            Some("grok-4.6-fast")
        );
    }
}
