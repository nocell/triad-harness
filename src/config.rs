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
                    ProviderKind::Cursor => config.model = Some("grok-4.7-fast".into()),
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
    /// A run-local model preset. Never saves over the user's provider settings.
    pub fn with_easy_mode(mut self, enabled: bool) -> Self {
        if enabled {
            for (provider, model) in [
                (ProviderKind::Claude, "claude-opus-5-5"),
                (ProviderKind::Codex, "gpt-6.1-sol"),
            ] {
                self.providers
                    .entry(provider.as_str().into())
                    .or_default()
                    .model = Some(model.into());
            }
        }
        self
    }

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
                config.model.get_or_insert_with(|| "grok-4.7-fast".into());
            }
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn easy_mode_only_overrides_claude_and_codex_models() {
        let mut original = Config::default();
        let codex = original.providers.get_mut("codex").unwrap();
        codex.model = Some("custom-model".into());
        codex.reasoning_effort = Some("high".into());
        codex.binary = Some("/custom/codex".into());
        codex.enabled = false;
        let easy = original.clone().with_easy_mode(true);
        assert_eq!(
            easy.provider(ProviderKind::Claude).model.as_deref(),
            Some("claude-opus-5-5")
        );
        let codex = easy.provider(ProviderKind::Codex);
        assert_eq!(codex.model.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(codex.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(codex.binary, Some("/custom/codex".into()));
        assert!(!codex.enabled);
        for kind in [ProviderKind::Kimi, ProviderKind::Cursor] {
            assert_eq!(easy.provider(kind).model, original.provider(kind).model);
        }
        assert_eq!(
            original
                .clone()
                .with_easy_mode(false)
                .provider(ProviderKind::Codex)
                .model
                .as_deref(),
            Some("custom-model")
        );
        assert_eq!(
            original.provider(ProviderKind::Claude).model.as_deref(),
            Some("claude-fable-5-1")
        );
        original.providers.clear();
        assert_eq!(
            original
                .with_easy_mode(true)
                .provider(ProviderKind::Codex)
                .reasoning_effort
                .as_deref(),
            Some("max")
        );
    }

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
        assert_eq!(
            config.provider(ProviderKind::Cursor).model.as_deref(),
            Some("grok-4.7-fast")
        );
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
            Some("grok-4.7-fast")
        );
    }
}
