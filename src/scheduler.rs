use crate::{
    cli::{DoctorArgs, ProviderArgs, ProviderCommand, ProvidersArgs},
    config::Config,
    model::{ProviderKind, ProviderLedgerEntry, ProviderStatus, UsageState},
    provider::{self, ProviderAdapter, ProviderFailure, ProviderFailureKind},
    storage,
};
use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
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
    let statuses = inspect_all(args.refresh).await?;
    print_statuses(&statuses, args.json)?;
    Ok(0)
}

pub async fn doctor_command(args: DoctorArgs) -> Result<i32> {
    let statuses = inspect_all(args.refresh).await?;
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
    entry.enabled = enabled;
    entry.usage = if enabled {
        UsageState::Unknown
    } else {
        UsageState::Disabled
    };
    entry.usage_source = "config".into();
    ledger.save()?;
    println!(
        "{provider}: {}",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(0)
}

async fn login(value: &str) -> Result<i32> {
    let provider = ProviderKind::from_str(value)?;
    let config = Config::load()?;
    let adapter = provider::discover(&config, provider)
        .with_context(|| format!("{provider} CLI is not installed"))?;
    let args: &[&str] = match provider {
        ProviderKind::Claude => &["auth", "login"],
        ProviderKind::Codex => &["login"],
        ProviderKind::Kimi => &["login"],
        ProviderKind::Cursor => &["login"],
    };
    let mut command = Command::new(&adapter.binary);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    sanitize_command(&mut command);
    let status = command.status().await?;
    Ok(if status.success() { 0 } else { 3 })
}

async fn install(value: &str, yes: bool) -> Result<i32> {
    let provider = ProviderKind::from_str(value)?;
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
    let ledger = ProviderLedger::load()?;
    let inspections = ProviderKind::ALL
        .into_iter()
        .map(|kind| provider::inspect(&config, kind));
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
    provider_arg: &str,
    require_all: bool,
) -> Result<(Vec<ProviderAdapter>, Vec<ProviderStatus>)> {
    let config = Config::load()?;
    let statuses = inspect_all(false).await?;
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
            if let Some(mut adapter) = provider::discover(&config, *kind) {
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
}

pub fn record_failure(
    ledger: &mut ProviderLedger,
    failure: &ProviderFailure,
    cooldown_minutes: i64,
) {
    let Some(provider) = failure.provider else {
        return;
    };
    let entry = ledger.entry_mut(provider);
    entry.last_error = Some(failure.message.clone());
    match failure.kind {
        ProviderFailureKind::Quota => {
            if let Some(retry_at) = failure.retry_at {
                entry.usage = UsageState::ExhaustedUntil;
                entry.retry_at = Some(retry_at);
                entry.usage_source = "reported".into();
            } else {
                entry.usage = UsageState::Cooldown;
                entry.retry_at = Some(Utc::now() + ChronoDuration::minutes(cooldown_minutes));
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
        "OPENAI_API_KEY",
        "MOONSHOT_API_KEY",
        "KIMI_API_KEY",
        "CURSOR_API_KEY",
        "CURSOR_AUTH_TOKEN",
    ] {
        command.env_remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
