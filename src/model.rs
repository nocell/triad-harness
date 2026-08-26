use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{fmt, path::PathBuf, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Claude,
    Codex,
    Kimi,
    Cursor,
}

impl ProviderKind {
    pub const ALL: [Self; 4] = [Self::Claude, Self::Codex, Self::Kimi, Self::Cursor];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
            Self::Cursor => "cursor",
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProviderKind {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "claude" => Ok(Self::Claude),
            "codex" => Ok(Self::Codex),
            "kimi" => Ok(Self::Kimi),
            "cursor" | "cursor-agent" | "agent" => Ok(Self::Cursor),
            _ => anyhow::bail!("unknown provider '{value}'"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthState {
    Subscription,
    ApiKey,
    NotAuthenticated,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageState {
    Available,
    Unknown,
    ExhaustedUntil,
    Cooldown,
    Unavailable,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub provider: ProviderKind,
    pub enabled: bool,
    pub binary: Option<PathBuf>,
    pub version: Option<String>,
    pub auth: AuthState,
    pub auth_detail: Option<String>,
    pub usage: UsageState,
    pub usage_source: String,
    pub model: Option<String>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub retry_at: Option<DateTime<Utc>>,
}

impl ProviderStatus {
    pub fn runnable(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled || self.binary.is_none() || self.auth != AuthState::Subscription {
            return false;
        }
        match self.usage {
            UsageState::Available | UsageState::Unknown => true,
            UsageState::Cooldown | UsageState::ExhaustedUntil => {
                self.retry_at.is_some_and(|retry_at| retry_at <= now)
            }
            UsageState::Unavailable | UsageState::Disabled => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderLedgerEntry {
    pub enabled: bool,
    pub usage: UsageState,
    pub usage_source: String,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub retry_at: Option<DateTime<Utc>>,
}

impl Default for ProviderLedgerEntry {
    fn default() -> Self {
        Self {
            enabled: true,
            usage: UsageState::Unknown,
            usage_source: "unknown".into(),
            last_success_at: None,
            last_error: None,
            retry_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Discovering,
    Mapping,
    Reducing,
    AwaitingApproval,
    Fixing,
    Verifying,
    Completed,
    Failed,
    Cancelled,
    FixIncomplete,
}

impl RunState {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::AwaitingApproval
                | Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::FixIncomplete
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitTarget {
    pub source_repo: PathBuf,
    pub remote_url: Option<String>,
    pub base_sha: String,
    pub head_sha: String,
    pub title: String,
    pub uncommitted: bool,
    pub patch_path: Option<PathBuf>,
    pub untracked_files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRunRecord {
    pub provider: ProviderKind,
    pub selected: bool,
    pub skipped_reason: Option<String>,
    pub model: Option<String>,
    pub version: Option<String>,
    pub auth_source: String,
    pub usage_source: String,
    pub session_id: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub protocol_violation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunManifest {
    pub id: String,
    pub state: RunState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub pid: Option<u32>,
    pub request_path: PathBuf,
    pub target: Option<GitTarget>,
    pub providers: Vec<ProviderRunRecord>,
    pub leader: Option<ProviderKind>,
    pub degraded: bool,
    pub error: Option<String>,
    pub report_path: Option<PathBuf>,
    pub patch_path: Option<PathBuf>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawFinding {
    pub title: String,
    pub severity: String,
    #[serde(default)]
    pub confidence: String,
    #[serde(default)]
    pub category: String,
    pub file: String,
    #[serde(default)]
    pub line: Option<u32>,
    pub claim: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub trigger: String,
    #[serde(default)]
    pub impact: String,
    #[serde(default)]
    pub suggested_fix: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingsEnvelope {
    #[serde(default)]
    pub findings: Vec<RawFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReducedFinding {
    pub id: String,
    pub verdict: String,
    pub title: String,
    pub severity: String,
    pub file: String,
    pub line: Option<u32>,
    pub rationale: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub trigger: String,
    #[serde(default)]
    pub impact: String,
    #[serde(default)]
    pub suggested_fix: String,
    #[serde(default)]
    pub sources: Vec<ProviderKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReductionEnvelope {
    #[serde(default)]
    pub findings: Vec<ReducedFinding>,
}

#[derive(Debug, Clone, Copy)]
pub enum AgentRole {
    Reviewer,
    Reducer,
    Fixer,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_becomes_runnable_after_reset() {
        let now = Utc::now();
        let status = ProviderStatus {
            provider: ProviderKind::Claude,
            enabled: true,
            binary: Some("claude".into()),
            version: None,
            auth: AuthState::Subscription,
            auth_detail: None,
            usage: UsageState::Cooldown,
            usage_source: "observed".into(),
            model: None,
            last_success_at: None,
            last_error: None,
            retry_at: Some(now - chrono::Duration::seconds(1)),
        };
        assert!(status.runnable(now));
    }

    #[test]
    fn cooldown_is_runnable_at_exact_reset_boundary() {
        let now = Utc::now();
        let status = ProviderStatus {
            provider: ProviderKind::Codex,
            enabled: true,
            binary: Some("codex".into()),
            version: None,
            auth: AuthState::Subscription,
            auth_detail: None,
            usage: UsageState::ExhaustedUntil,
            usage_source: "reported".into(),
            model: None,
            last_success_at: None,
            last_error: Some("usage limit".into()),
            retry_at: Some(now),
        };
        assert!(status.runnable(now));
    }
}
