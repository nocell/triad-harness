use crate::{
    cli::{DoctorArgs, ProviderArgs, ProviderCommand, ProvidersArgs},
    config::Config,
    model::{ProviderKind, ProviderLedgerEntry, ProviderStatus, UsageState},
    provider::{self, ProviderAdapter, ProviderFailure, ProviderFailureKind},
    storage,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal},
    path::PathBuf,
    process::Stdio,
    str::FromStr,
};
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderLedger {
    pub providers: BTreeMap<String, ProviderLedgerEntry>,
}

impl ProviderLedger {
    pub fn load() -> Result<Self> {
        let path = ledger_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        storage::read_json(&path)
    }

    pub fn save(&self) -> Result<()> {
        storage::write_json(&ledger_path()?, self)
    }

    pub fn entry(&self, provider: ProviderKind) -> ProviderLedgerEntry {
        self.providers
            .get(provider.as_str())
            .cloned()
            .unwrap_or_default()
    }

    pub fn entry_mut(&mut self, provider: ProviderKind) -> &mut ProviderLedgerEntry {
        self.providers.entry(provider.as_str().into()).or_default()
    }
}

fn ledger_path() -> Result<PathBuf> {
    Ok(storage::data_root()?.join("providers.json"))
}

pub async fn providers_command(args: ProvidersArgs) -> Result<i32> {
    let config = Config::load()?.with_modes(args.easy_mode, args.ultra_mode)?;
    let statuses = inspect_with_config(&config).await?;
    print_statuses(&statuses, args.json)?;
    Ok(0)
}

pub async fn doctor_command(args: DoctorArgs) -> Result<i32> {
    let config = Config::load()?.with_modes(args.easy_mode, args.ultra_mode)?;
    let statuses = inspect_with_config(&config).await?;
    print_statuses(&statuses, args.json)?;
    let missing: Vec<_> = statuses
        .iter()
        .filter(|status| status.binary.is_none())
        .map(|status| status.provider)
        .collect();
    if !args.json && !missing.is_empty() {
        println!("\nMissing providers:");
        for provider in missing {
            if provider == ProviderKind::Cursor {
                println!(
                    "  cursor: triad provider install cursor --yes, then triad provider login cursor"
                );
            } else if provider.is_zcode() {
                println!(
                    "  {provider}: install official ZCode (https://zcode.z.ai), ensure Node.js is available, then sign in to its Z.ai Coding Plan account"
                );
            } else {
                println!("  {provider}: install its official CLI and rerun triad doctor --refresh");
            }
        }
    }
    Ok(
        if statuses.iter().any(|status| status.runnable(Utc::now())) {
            0
        } else {
            2
        },
    )
}

pub async fn provider_command(args: ProviderArgs) -> Result<i32> {
    match args.command {
        ProviderCommand::Enable { provider } => set_enabled(&provider, true),
        ProviderCommand::Disable { provider } => set_enabled(&provider, false),
        ProviderCommand::Login { provider } => login(&provider).await,
        ProviderCommand::Install { provider, yes } => install(&provider, yes).await,
    }
}

fn set_enabled(value: &str, enabled: bool) -> Result<i32> {
    let provider = ProviderKind::from_str(value)?;
    let mut config = Config::load()?;
    config
        .providers
        .entry(provider.as_str().into())
        .or_default()
        .enabled = enabled;
    config.save()?;
    let mut ledger = ProviderLedger::load()?;
    let entry = ledger.entry_mut(provider);
    set_entry_enabled(entry, enabled);
    ledger.save()?;
    println!(
        "{provider}: {}",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(0)
}

fn set_entry_enabled(entry: &mut ProviderLedgerEntry, enabled: bool) {
    entry.enabled = enabled;
    entry.usage = if enabled {
        UsageState::Unknown
    } else {
        UsageState::Disabled
    };
    entry.usage_source = "config".into();
    if enabled {
        entry.consecutive_quota_failures = 0;
        entry.retry_at = None;
        entry.last_error = None;
    }
}

async fn login(value: &str) -> Result<i32> {
    let provider = ProviderKind::from_str(value)?;
    let config = Config::load()?;
    let adapter = provider::discover(&config, provider)
        .await
        .with_context(|| format!("{provider} CLI is not installed"))?;
    let args: &[&str] = match provider {
        ProviderKind::Claude => &["auth", "login"],
        ProviderKind::Codex => &["login"],
        ProviderKind::Kimi => &["login"],
        ProviderKind::Cursor => &["login"],
        ProviderKind::Zcode | ProviderKind::ZcodeFlash => &["login", "zai"],
    };
    let mut command = Command::new(&adapter.binary);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    sanitize_command(&mut command);
    let status = command.status().await?;
    if status.success() && provider.is_zcode() {
        let mut ledger = ProviderLedger::load()?;
        reset_zcode_auth_failures(&mut ledger);
        ledger.save()?;
    }
    Ok(if status.success() { 0 } else { 3 })
}

fn reset_zcode_auth_failures(ledger: &mut ProviderLedger) {
    for kind in [ProviderKind::Zcode, ProviderKind::ZcodeFlash] {
        let entry = ledger.entry_mut(kind);
        if entry.usage == UsageState::Unavailable && entry.usage_source == "auth" {
            entry.usage = if entry.enabled {
                UsageState::Unknown
            } else {
                UsageState::Disabled
            };
            entry.usage_source = "login".into();
            entry.last_error = None;
            entry.retry_at = None;
        }
    }
}

async fn install(value: &str, yes: bool) -> Result<i32> {
    let provider = ProviderKind::from_str(value)?;
    if provider.is_zcode() {
        println!(
            "ZCode official source: https://zcode.z.ai\nInstall ZCode and sign in to its Z.ai Coding Plan account. Triad does not install ZCode automatically, even with --yes."
        );
        return Ok(2);
    }
    if provider != ProviderKind::Cursor {
        println!(
            "Automatic installation is currently supported only for Cursor. Install {provider} from its official documentation."
        );
        return Ok(2);
    }
    println!("Cursor official installer: https://cursor.com/install");
    if !yes {
        println!(
            "No changes made. Re-run with --yes to download and execute the official installer."
        );
        return Ok(2);
    }
    if !io::stdin().is_terminal() {
        eprintln!("warning: installing Cursor non-interactively because --yes was supplied");
    }
    let temp = tempfile::NamedTempFile::new()?;
    let download = Command::new("curl")
        .args(["-fsSL", "https://cursor.com/install", "-o"])
        .arg(temp.path())
        .status()
        .await?;
    if !download.success() {
        anyhow::bail!("failed to download Cursor installer");
    }
    let status = Command::new("sh")
        .arg(temp.path())
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await?;
    Ok(if status.success() { 0 } else { 3 })
}

pub async fn inspect_all(_refresh: bool) -> Result<Vec<ProviderStatus>> {
    let config = Config::load()?;
    inspect_with_config(&config).await
}

async fn inspect_with_config(config: &Config) -> Result<Vec<ProviderStatus>> {
    let ledger = ProviderLedger::load()?;
    let inspections = ProviderKind::ALL
        .into_iter()
        .map(|kind| provider::inspect(config, kind));
    let mut statuses = join_all(inspections).await;
    for status in &mut statuses {
        let entry = ledger.entry(status.provider);
        *status = provider::default_status_from_ledger(status.clone(), &entry);
    }
    Ok(statuses)
}

fn print_statuses(statuses: &[ProviderStatus], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(statuses)?);
        return Ok(());
    }
    println!(
        "{:<9} {:<16} {:<18} {:<12} {:<16} VERSION",
        "PROVIDER", "AUTH", "USAGE", "SOURCE", "MODEL"
    );
    for status in statuses {
        let cli = status
            .version
            .as_deref()
            .unwrap_or(if status.binary.is_some() {
                "found"
            } else {
                "missing"
            });
        let usage = if let Some(retry_at) = status.retry_at {
            format!("{:?}@{}", status.usage, retry_at.format("%H:%M"))
        } else {
            format!("{:?}", status.usage)
        };
        println!(
            "{:<9} {:<16} {:<18.18} {:<12} {:<16} {}",
            status.provider,
            format!("{:?}", status.auth),
            usage,
            status.usage_source,
            status.model.as_deref().unwrap_or("vendor-default"),
            cli
        );
    }
    Ok(())
}

pub async fn select(
    config: &Config,
    provider_arg: &str,
    require_all: bool,
) -> Result<(Vec<ProviderAdapter>, Vec<ProviderStatus>)> {
    let statuses = inspect_with_config(config).await?;
    let requested: Vec<ProviderKind> = if provider_arg == "auto" {
        ProviderKind::ALL.to_vec()
    } else {
        provider_arg
            .split(',')
            .map(|value| ProviderKind::from_str(value.trim()))
            .collect::<Result<_>>()?
    };
    let mut selected = Vec::new();
    for kind in &requested {
        let status = statuses
            .iter()
            .find(|status| status.provider == *kind)
            .context("provider status missing")?;
        if status.runnable(Utc::now()) {
            if let Some(mut adapter) = provider::discover(config, *kind).await {
                adapter.version = status.version.clone();
                selected.push(adapter);
            }
        } else if require_all {
            anyhow::bail!(
                "required provider {kind} is not runnable: auth={:?}, usage={:?}",
                status.auth,
                status.usage
            );
        }
    }
    if selected.is_empty() {
        anyhow::bail!("no providers with a valid subscription login and usable/unknown quota");
    }
    Ok((selected, statuses))
}

pub fn choose_leader(
    requested: &str,
    successful: &[ProviderKind],
    config: &Config,
) -> Result<ProviderKind> {
    if requested != "auto" {
        let provider = ProviderKind::from_str(requested)?;
        if successful.contains(&provider) {
            return Ok(provider);
        }
        anyhow::bail!(
            "pinned leader {provider} is unavailable or did not complete its reviewer run"
        );
    }
    config
        .leader_order
        .iter()
        .copied()
        .find(|provider| successful.contains(provider))
        .context("no successful provider is available as reducer")
}

pub fn record_success(ledger: &mut ProviderLedger, provider: ProviderKind) {
    let entry = ledger.entry_mut(provider);
    entry.usage = UsageState::Available;
    entry.usage_source = "observed".into();
    entry.last_success_at = Some(Utc::now());
    entry.last_error = None;
    entry.retry_at = None;
    entry.consecutive_quota_failures = 0;
}

pub fn record_failure(
    ledger: &mut ProviderLedger,
    failure: &ProviderFailure,
    cooldown_minutes: i64,
) {
    record_failure_at(ledger, failure, cooldown_minutes, Utc::now());
}

fn record_failure_at(
    ledger: &mut ProviderLedger,
    failure: &ProviderFailure,
    cooldown_minutes: i64,
    now: DateTime<Utc>,
) {
    let Some(provider) = failure.provider else {
        return;
    };
    let entry = ledger.entry_mut(provider);
    entry.last_error = Some(failure.message.clone());
    match failure.kind {
        ProviderFailureKind::Quota => {
            entry.consecutive_quota_failures = entry.consecutive_quota_failures.saturating_add(1);
            if let Some(retry_at) = failure.retry_at {
                entry.usage = UsageState::ExhaustedUntil;
                entry.retry_at = Some(retry_at);
                entry.usage_source = "reported".into();
            } else {
                entry.usage = UsageState::Cooldown;
                let multiplier = 2_i64.saturating_pow(entry.consecutive_quota_failures - 1);
                let delay_minutes = cooldown_minutes
                    .clamp(1, 360)
                    .saturating_mul(multiplier)
                    .min(360);
                entry.retry_at = Some(now + ChronoDuration::minutes(delay_minutes));
                entry.usage_source = "observed".into();
            }
        }
        ProviderFailureKind::Authentication => {
            entry.usage = UsageState::Unavailable;
            entry.usage_source = "auth".into();
        }
        _ => {
            entry.usage = UsageState::Unknown;
            entry.usage_source = "error".into();
        }
    }
}

pub fn sanitize_command(command: &mut Command) {
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "MOONSHOT_API_KEY",
        "KIMI_API_KEY",
        "CURSOR_API_KEY",
        "CURSOR_AUTH_TOKEN",
        "ZAI_API_KEY",
        "ZCODE_API_KEY",
        "ZHIPU_API_KEY",
        "ZHIPUAI_API_KEY",
        "GLM_API_KEY",
        "OPENROUTER_API_KEY",
        "NODE_OPTIONS",
        "NODE_PATH",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
    ] {
        command.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_ledgers_default_to_initial_quota_backoff() {
        let ledger: ProviderLedger = serde_json::from_value(serde_json::json!({
            "providers": {
                "kimi": {
                    "enabled": true,
                    "usage": "cooldown",
                    "usage_source": "observed",
                    "last_success_at": null,
                    "last_error": "weekly quota exhausted",
                    "retry_at": null
                }
            }
        }))
        .unwrap();
        assert_eq!(
            ledger.entry(ProviderKind::Kimi).consecutive_quota_failures,
            0
        );
    }

    #[test]
    fn unknown_quota_reset_uses_bounded_exponential_cooldown() {
        let mut ledger = ProviderLedger::default();
        let failure = ProviderFailure::quota(ProviderKind::Kimi, "weekly quota exhausted", None);
        let now = Utc::now();
        for (index, delay) in [15, 30, 60, 120, 240, 360, 360].into_iter().enumerate() {
            record_failure_at(&mut ledger, &failure, 15, now);
            let entry = ledger.entry(ProviderKind::Kimi);
            assert_eq!(entry.consecutive_quota_failures, index as u32 + 1);
            assert_eq!(entry.usage, UsageState::Cooldown);
            assert_eq!(entry.usage_source, "observed");
            assert_eq!(entry.retry_at, Some(now + ChronoDuration::minutes(delay)));
        }
        let restored: ProviderLedger =
            serde_json::from_str(&serde_json::to_string(&ledger).unwrap()).unwrap();
        assert_eq!(
            restored
                .entry(ProviderKind::Kimi)
                .consecutive_quota_failures,
            7
        );
    }

    #[test]
    fn reported_quota_reset_wins_over_backoff_cap() {
        let mut ledger = ProviderLedger::default();
        ledger
            .entry_mut(ProviderKind::Kimi)
            .consecutive_quota_failures = 12;
        let now = Utc::now();
        let reset = now + ChronoDuration::days(3);
        let failure =
            ProviderFailure::quota(ProviderKind::Kimi, "weekly quota exhausted", Some(reset));
        record_failure_at(&mut ledger, &failure, 15, now);
        let entry = ledger.entry(ProviderKind::Kimi);
        assert_eq!(entry.usage, UsageState::ExhaustedUntil);
        assert_eq!(entry.usage_source, "reported");
        assert_eq!(entry.retry_at, Some(reset));
    }

    #[test]
    fn success_and_manual_enable_restart_quota_backoff() {
        let mut ledger = ProviderLedger::default();
        let failure = ProviderFailure::quota(ProviderKind::Kimi, "weekly quota exhausted", None);
        let now = Utc::now();
        for _ in 0..3 {
            record_failure_at(&mut ledger, &failure, 15, now);
        }
        record_success(&mut ledger, ProviderKind::Kimi);
        let entry = ledger.entry(ProviderKind::Kimi);
        assert_eq!(entry.consecutive_quota_failures, 0);
        assert!(entry.retry_at.is_none());
        assert!(entry.last_error.is_none());
        record_failure_at(&mut ledger, &failure, 15, now);
        assert_eq!(
            ledger.entry(ProviderKind::Kimi).retry_at,
            Some(now + ChronoDuration::minutes(15))
        );
        set_entry_enabled(ledger.entry_mut(ProviderKind::Kimi), false);
        set_entry_enabled(ledger.entry_mut(ProviderKind::Kimi), true);
        let entry = ledger.entry(ProviderKind::Kimi);
        assert_eq!(entry.consecutive_quota_failures, 0);
        assert_eq!(entry.usage, UsageState::Unknown);
        assert!(entry.retry_at.is_none());
        assert!(entry.last_error.is_none());
    }

    #[test]
    fn cooldown_backoff_clamps_invalid_config_and_saturates_counter() {
        let failure = ProviderFailure::quota(ProviderKind::Kimi, "quota exhausted", None);
        let now = Utc::now();
        for (configured, expected) in [(0, 1), (-15, 1), (i64::MAX, 360)] {
            let mut ledger = ProviderLedger::default();
            record_failure_at(&mut ledger, &failure, configured, now);
            assert_eq!(
                ledger.entry(ProviderKind::Kimi).retry_at,
                Some(now + ChronoDuration::minutes(expected))
            );
        }
        let mut ledger = ProviderLedger::default();
        ledger
            .entry_mut(ProviderKind::Kimi)
            .consecutive_quota_failures = u32::MAX;
        record_failure_at(&mut ledger, &failure, 15, now);
        let entry = ledger.entry(ProviderKind::Kimi);
        assert_eq!(entry.consecutive_quota_failures, u32::MAX);
        assert_eq!(entry.retry_at, Some(now + ChronoDuration::hours(6)));
    }

    #[test]
    fn auto_leader_uses_priority_order() {
        let config = Config::default();
        assert_eq!(
            choose_leader(
                "auto",
                &[ProviderKind::Claude, ProviderKind::Cursor],
                &config
            )
            .unwrap(),
            ProviderKind::Claude
        );
        assert_eq!(
            choose_leader("auto", &[ProviderKind::Kimi, ProviderKind::Codex], &config).unwrap(),
            ProviderKind::Codex
        );
    }

    #[test]
    fn pinned_leader_fails_closed() {
        assert!(choose_leader("cursor", &[ProviderKind::Codex], &Config::default()).is_err());
    }

    #[test]
    fn glm_slots_have_independent_enablement_and_observed_quota() {
        // ZCode has model-specific allowances as well as plan-level limits.
        // An unscoped error must not guess that the other model is exhausted.
        let mut ledger = ProviderLedger::default();
        set_entry_enabled(ledger.entry_mut(ProviderKind::ZcodeFlash), false);
        assert!(ledger.entry(ProviderKind::Zcode).enabled);
        set_entry_enabled(ledger.entry_mut(ProviderKind::ZcodeFlash), true);
        let failure =
            ProviderFailure::quota(ProviderKind::ZcodeFlash, "model quota exhausted", None);
        record_failure(&mut ledger, &failure, 15);
        let reset = ledger.entry(ProviderKind::ZcodeFlash).retry_at;
        record_success(&mut ledger, ProviderKind::Zcode);
        assert_eq!(
            ledger.entry(ProviderKind::Zcode).usage,
            UsageState::Available
        );
        assert_eq!(
            ledger.entry(ProviderKind::ZcodeFlash).usage,
            UsageState::Cooldown
        );
        assert_eq!(ledger.entry(ProviderKind::ZcodeFlash).retry_at, reset);
        record_failure(&mut ledger, &failure, 15);
        assert_eq!(
            ledger.entry(ProviderKind::Zcode).usage,
            UsageState::Available
        );
    }

    #[test]
    fn glm_only_runs_have_a_leader_and_pinned_model_does_not_fallback() {
        let config = Config::default();
        assert_eq!(
            choose_leader(
                "auto",
                &[ProviderKind::ZcodeFlash, ProviderKind::Zcode],
                &config
            )
            .unwrap(),
            ProviderKind::Zcode
        );
        assert_eq!(
            choose_leader("auto", &[ProviderKind::ZcodeFlash], &config).unwrap(),
            ProviderKind::ZcodeFlash
        );
        assert!(choose_leader("zcode", &[ProviderKind::ZcodeFlash], &config).is_err());
    }

    #[test]
    fn explicit_native_login_clears_auth_failures_but_not_model_quota() {
        let mut ledger = ProviderLedger::default();
        record_failure(
            &mut ledger,
            &ProviderFailure::auth(ProviderKind::Zcode, "login required"),
            15,
        );
        record_failure(
            &mut ledger,
            &ProviderFailure::quota(ProviderKind::ZcodeFlash, "quota", None),
            15,
        );
        let reset = ledger.entry(ProviderKind::ZcodeFlash).retry_at;
        reset_zcode_auth_failures(&mut ledger);
        assert_eq!(ledger.entry(ProviderKind::Zcode).usage, UsageState::Unknown);
        assert_eq!(
            ledger.entry(ProviderKind::ZcodeFlash).usage,
            UsageState::Cooldown
        );
        assert_eq!(ledger.entry(ProviderKind::ZcodeFlash).retry_at, reset);
        let entry = ledger.entry_mut(ProviderKind::Zcode);
        entry.enabled = false;
        entry.usage = UsageState::Unavailable;
        entry.usage_source = "auth".into();
        reset_zcode_auth_failures(&mut ledger);
        assert!(!ledger.entry(ProviderKind::Zcode).enabled);
        assert_eq!(
            ledger.entry(ProviderKind::Zcode).usage,
            UsageState::Disabled
        );
    }
}
