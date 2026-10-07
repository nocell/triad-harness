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
    #[serde(default)]
    pub ultracode: bool,
    #[serde(default)]
    pub fast_mode: bool,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: None,
            model: None,
            reasoning_effort: None,
            ultracode: false,
            fast_mode: false,
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
                    ProviderKind::Zcode => {
                        config.model = Some("GLM-5.3".into());
                        config.reasoning_effort = Some("max".into());
                    }
                    ProviderKind::ZcodeFlash => {
                        config.model = Some("GLM-5.3-Flash".into());
                        config.reasoning_effort = Some("max".into());
                    }
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
                ProviderKind::Zcode,
                ProviderKind::ZcodeFlash,
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
    /// Apply a run-local preset without persisting over the user's settings.
    pub fn with_modes(self, easy_mode: bool, ultra_mode: bool) -> Result<Self> {
        anyhow::ensure!(
            !(easy_mode && ultra_mode),
            "--easy-mode and --ultra-mode cannot be used together"
        );
        let mut config = self.with_easy_mode(easy_mode);
        if ultra_mode {
            let claude = config.providers.entry("claude".into()).or_default();
            claude.model = Some("claude-opus-5-5".into());
            claude.reasoning_effort = Some("xhigh".into());
            claude.ultracode = true;

            let codex = config.providers.entry("codex".into()).or_default();
            codex.model = Some("gpt-6-astra".into());
            codex.reasoning_effort = Some("ultra".into());
            codex.fast_mode = true;
        }
        Ok(config)
    }

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
        Self::from_toml(&body).with_context(|| format!("parse {}", path.display()))
    }

    fn from_toml(body: &str) -> Result<Self> {
        let mut config: Self = toml::from_str(body)?;
        // Extend only the former built-in order; an explicit custom (or empty)
        // order is the user's choice and must not gain new fallback leaders.
        if config.leader_order
            == [
                ProviderKind::Codex,
                ProviderKind::Claude,
                ProviderKind::Cursor,
                ProviderKind::Kimi,
            ]
        {
            config
                .leader_order
                .extend([ProviderKind::Zcode, ProviderKind::ZcodeFlash]);
        }
        Ok(config)
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
            ProviderKind::Zcode | ProviderKind::ZcodeFlash => {
                config.model.get_or_insert_with(|| {
                    if provider == ProviderKind::Zcode {
                        "GLM-5.3".into()
                    } else {
                        "GLM-5.3-Flash".into()
                    }
                });
                config.reasoning_effort.get_or_insert_with(|| "max".into());
            }
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_default_leader_order_adds_zcode_once_without_changing_custom_orders() {
        let mut legacy = Config {
            leader_order: vec![
                ProviderKind::Codex,
                ProviderKind::Claude,
                ProviderKind::Cursor,
                ProviderKind::Kimi,
            ],
            ..Config::default()
        };
        legacy.providers.remove("zcode");
        legacy.providers.remove("zcode_flash");
        let loaded = Config::from_toml(&toml::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(loaded.leader_order, Config::default().leader_order);
        let reloaded = Config::from_toml(&toml::to_string(&loaded).unwrap()).unwrap();
        assert_eq!(reloaded.leader_order, loaded.leader_order);
        for (kind, model) in [
            (ProviderKind::Zcode, "GLM-5.3"),
            (ProviderKind::ZcodeFlash, "GLM-5.3-Flash"),
        ] {
            let provider = loaded.provider(kind);
            assert!(provider.enabled);
            assert_eq!(provider.model.as_deref(), Some(model));
            assert_eq!(provider.reasoning_effort.as_deref(), Some("max"));
        }

        for order in [
            vec![],
            vec![ProviderKind::Codex],
            vec![
                ProviderKind::Claude,
                ProviderKind::Codex,
                ProviderKind::Cursor,
                ProviderKind::Kimi,
            ],
            vec![ProviderKind::ZcodeFlash, ProviderKind::Zcode],
        ] {
            legacy.leader_order = order.clone();
            let loaded = Config::from_toml(&toml::to_string(&legacy).unwrap()).unwrap();
            assert_eq!(loaded.leader_order, order);
        }
    }

    #[test]
    fn zcode_defaults_and_partial_configs_preserve_native_models_and_settings() {
        let mut config = Config::default();
        for (kind, model) in [
            (ProviderKind::Zcode, "GLM-5.3"),
            (ProviderKind::ZcodeFlash, "GLM-5.3-Flash"),
        ] {
            let provider = config.provider(kind);
            assert!(provider.enabled);
            assert_eq!(provider.model.as_deref(), Some(model));
            assert_eq!(provider.reasoning_effort.as_deref(), Some("max"));

            config.providers.insert(
                kind.as_str().into(),
                ProviderConfig {
                    enabled: false,
                    binary: Some("/custom/zcode".into()),
                    ..ProviderConfig::default()
                },
            );
            let provider = config.provider(kind);
            assert!(!provider.enabled);
            assert_eq!(provider.binary, Some("/custom/zcode".into()));
            assert_eq!(provider.model.as_deref(), Some(model));
            assert_eq!(provider.reasoning_effort.as_deref(), Some("max"));
        }
    }

    #[test]
    fn zcode_settings_are_unchanged_by_easy_and_ultra_modes() {
        let mut original = Config::default();
        for kind in [ProviderKind::Zcode, ProviderKind::ZcodeFlash] {
            let provider = original.providers.get_mut(kind.as_str()).unwrap();
            provider.enabled = false;
            provider.binary = Some("/custom/zcode".into());
            provider.model = Some(format!("custom-{kind}"));
            provider.reasoning_effort = Some("high".into());
        }
        for (easy, ultra) in [(false, false), (true, false), (false, true)] {
            let preset = original.clone().with_modes(easy, ultra).unwrap();
            for kind in [ProviderKind::Zcode, ProviderKind::ZcodeFlash] {
                assert_eq!(
                    serde_json::to_value(preset.provider(kind)).unwrap(),
                    serde_json::to_value(original.provider(kind)).unwrap()
                );
            }
        }
    }

    #[test]
    fn ultra_mode_is_run_local_and_preserves_other_provider_settings() {
        let mut original = Config::default();
        for name in ["claude", "codex"] {
            let provider = original.providers.get_mut(name).unwrap();
            provider.model = Some("custom-model".into());
            provider.reasoning_effort = Some("high".into());
            provider.binary = Some(format!("/custom/{name}").into());
            provider.enabled = false;
        }
        let before = serde_json::to_value(&original).unwrap();
        let ultra = original.clone().with_modes(false, true).unwrap();
        let claude = ultra.provider(ProviderKind::Claude);
        assert_eq!(claude.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(claude.reasoning_effort.as_deref(), Some("xhigh"));
        assert!(claude.ultracode);
        assert!(!claude.fast_mode);
        let codex = ultra.provider(ProviderKind::Codex);
        assert_eq!(codex.model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(codex.reasoning_effort.as_deref(), Some("ultra"));
        assert!(codex.fast_mode);
        assert!(!codex.ultracode);
        for kind in [ProviderKind::Claude, ProviderKind::Codex] {
            let provider = ultra.provider(kind);
            assert_eq!(provider.binary, Some(format!("/custom/{kind}").into()));
            assert!(!provider.enabled);
        }
        for kind in [ProviderKind::Kimi, ProviderKind::Cursor] {
            assert_eq!(
                serde_json::to_value(ultra.provider(kind)).unwrap(),
                serde_json::to_value(original.provider(kind)).unwrap()
            );
        }
        assert_eq!(serde_json::to_value(original).unwrap(), before);
    }

    #[test]
    fn ultra_mode_handles_partial_configs_and_rejects_combined_presets() {
        let mut config = Config::default();
        config.providers.clear();
        let ultra = config.with_modes(false, true).unwrap();
        assert!(ultra.provider(ProviderKind::Claude).ultracode);
        assert!(ultra.provider(ProviderKind::Codex).fast_mode);
        assert_eq!(
            ultra
                .provider(ProviderKind::Codex)
                .reasoning_effort
                .as_deref(),
            Some("ultra")
        );
        assert!(
            Config::default()
                .with_modes(true, true)
                .unwrap_err()
                .to_string()
                .contains("cannot be used together")
        );
    }

    #[test]
    fn old_configs_and_non_ultra_presets_keep_speed_features_disabled() {
        let legacy: ProviderConfig = toml::from_str("enabled = true").unwrap();
        assert!(!legacy.ultracode);
        assert!(!legacy.fast_mode);
        for easy in [false, true] {
            let config = Config::default().with_modes(easy, false).unwrap();
            for kind in ProviderKind::ALL {
                let provider = config.provider(kind);
                assert!(!provider.ultracode);
                assert!(!provider.fast_mode);
            }
            assert_eq!(
                config
                    .provider(ProviderKind::Codex)
                    .reasoning_effort
                    .as_deref(),
                Some("max")
            );
        }
    }

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
                ..ProviderConfig::default()
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
                    ..ProviderConfig::default()
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
