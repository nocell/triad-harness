use crate::model::ProviderKind;
use chrono::{DateTime, Utc};
use regex::Regex;
use std::{fmt, path::PathBuf};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct CommandSpec {
    pub binary: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub remove_env: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl CommandSpec {
    pub fn new(binary: PathBuf, cwd: PathBuf) -> Self {
        Self {
            binary,
            args: Vec::new(),
            cwd,
            remove_env: Vec::new(),
            env: Vec::new(),
        }
    }

    pub fn into_tokio_command(self) -> Command {
        let mut command = Command::new(self.binary);
        command.args(self.args).current_dir(self.cwd);
        for key in self.remove_env {
            command.env_remove(key);
        }
        for (key, value) in self.env {
            command.env(key, value);
        }
        command
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailureKind {
    Quota,
    Authentication,
    Timeout,
    Malformed,
    Spawn,
    Internal,
}

#[derive(Debug)]
pub struct ProviderFailure {
    pub provider: Option<ProviderKind>,
    pub kind: ProviderFailureKind,
    pub message: String,
    pub retry_at: Option<DateTime<Utc>>,
}

impl ProviderFailure {
    pub fn quota(
        provider: ProviderKind,
        message: impl Into<String>,
        retry_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            provider: Some(provider),
            kind: ProviderFailureKind::Quota,
            message: message.into(),
            retry_at,
        }
    }
    pub fn auth(provider: ProviderKind, message: impl Into<String>) -> Self {
        Self {
            provider: Some(provider),
            kind: ProviderFailureKind::Authentication,
            message: message.into(),
            retry_at: None,
        }
    }
    pub fn timeout(provider: ProviderKind) -> Self {
        Self {
            provider: Some(provider),
            kind: ProviderFailureKind::Timeout,
            message: "provider timed out".into(),
            retry_at: None,
        }
    }
    pub fn malformed(provider: ProviderKind, message: impl Into<String>) -> Self {
        Self {
            provider: Some(provider),
            kind: ProviderFailureKind::Malformed,
            message: message.into(),
            retry_at: None,
        }
    }
    pub fn spawn(provider: ProviderKind, error: impl fmt::Display) -> Self {
        Self {
            provider: Some(provider),
            kind: ProviderFailureKind::Spawn,
            message: error.to_string(),
            retry_at: None,
        }
    }
    pub fn internal(error: impl fmt::Display) -> Self {
        Self {
            provider: None,
            kind: ProviderFailureKind::Internal,
            message: error.to_string(),
            retry_at: None,
        }
    }
}

impl fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProviderFailure {}

pub fn classify_failure(provider: ProviderKind, message: &str) -> ProviderFailure {
    let lowered = message.to_ascii_lowercase();
    if [
        "rate limit",
        "usage limit",
        "quota",
        "too many requests",
        "429",
        "limit reached",
        "exhausted",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return ProviderFailure::quota(provider, redact(message), parse_retry_at(message));
    }
    if [
        "not authenticated",
        "login required",
        "unauthorized",
        "invalid token",
        "authentication",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return ProviderFailure::auth(provider, redact(message));
    }
    ProviderFailure {
        provider: Some(provider),
        kind: ProviderFailureKind::Spawn,
        message: redact(message),
        retry_at: None,
    }
}

fn parse_retry_at(message: &str) -> Option<DateTime<Utc>> {
    let regex = Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z").ok()?;
    regex
        .find(message)
        .and_then(|value| DateTime::parse_from_rfc3339(value.as_str()).ok())
        .map(|value| value.with_timezone(&Utc))
}

pub fn redact(message: &str) -> String {
    let patterns = [
        r"(?i)(sk-ant-[A-Za-z0-9_-]{12,})",
        r"(?i)(sk-[A-Za-z0-9_-]{12,})",
        r"(?i)(gh[opsu]_[A-Za-z0-9]{12,})",
        r"(?i)(bearer\s+)[A-Za-z0-9._~-]{12,}",
    ];
    let mut redacted = message.to_string();
    for pattern in patterns {
        if let Ok(regex) = Regex::new(pattern) {
            redacted = regex.replace_all(&redacted, "[REDACTED]").into_owned();
        }
    }
    const MAX: usize = 2_000;
    if redacted.len() > MAX {
        redacted.truncate(MAX);
        redacted.push('…');
    }
    redacted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_quota_and_reset() {
        let failure = classify_failure(
            ProviderKind::Cursor,
            "429 usage limit; resets at 2026-08-20T12:30:00Z",
        );
        assert_eq!(failure.kind, ProviderFailureKind::Quota);
        assert_eq!(
            failure.retry_at.unwrap().to_rfc3339(),
            "2026-08-20T12:30:00+00:00"
        );
    }

    #[test]
    fn redacts_common_tokens() {
        let anthropic_token = ["sk", "-ant-", "abcdefghijklmnop"].concat();
        let output = redact(&format!(
            "Bearer abcdefghijklmnopqrstuvwxyz {anthropic_token}"
        ));
        assert!(!output.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(!output.contains("sk-ant-"));
    }
}
