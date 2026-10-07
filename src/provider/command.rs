use crate::model::ProviderKind;
use chrono::{DateTime, Utc};
use regex::Regex;
use std::{
    fmt,
    path::{Path, PathBuf},
};
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
    if provider.is_zcode() && (message.contains("凭据已经失效") || message.contains("缺少请求凭据"))
    {
        return ProviderFailure::auth(provider, redact(message));
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
    let mut redacted = redact_log(message);
    const MAX: usize = 2_000;
    if redacted.len() > MAX {
        let mut boundary = MAX;
        while !redacted.is_char_boundary(boundary) {
            boundary -= 1;
        }
        redacted.truncate(boundary);
        redacted.push('…');
    }
    redacted
}

/// Redact secrets without truncating the provider's diagnostic artifacts.
/// In particular, JSONL logs must retain complete events and their final result.
pub fn redact_log(message: &str) -> String {
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
    redacted
}

/// Keep earlier attempt evidence, but never let a new call consume stale results.
/// A shared suffix associates the prior role's result and logs without requiring
/// a provider-specific session ID (which may not exist after a failed launch).
pub fn archive_previous_outputs(paths: &[&Path]) -> std::io::Result<()> {
    let attempt = uuid::Uuid::now_v7();
    for path in paths {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => {}
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("provider output is not a regular file: {}", path.display()),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        }
        let mut filename = path
            .file_name()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "provider output has no filename",
                )
            })?
            .to_os_string();
        filename.push(format!(".attempt-{attempt}"));
        std::fs::rename(path, path.with_file_name(filename))?;
    }
    Ok(())
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
    fn kimi_weekly_auth_error_is_a_quota_failure() {
        let failure = classify_failure(
            ProviderKind::Kimi,
            "provider.auth_error: 403 You've reached your weekly (7-day) usage limit. Your quota will reset when the current 7-day window ends.",
        );
        assert_eq!(failure.kind, ProviderFailureKind::Quota);
        assert!(failure.retry_at.is_none());
    }

    #[test]
    fn native_zcode_missing_or_expired_credentials_are_auth_failures() {
        for message in [
            "Standalone Account Provider 凭据已经失效",
            "Standalone Account Provider 缺少请求凭据",
        ] {
            assert_eq!(
                classify_failure(ProviderKind::Zcode, message).kind,
                ProviderFailureKind::Authentication
            );
        }
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

    #[test]
    fn full_log_redaction_preserves_long_jsonl_and_late_secrets() {
        let token = ["sk", "-ant-", "abcdefghijklmnop"].concat();
        let log = format!(
            "{}\n{}\n",
            serde_json::json!({"type": "progress", "text": "context ".repeat(500)}),
            serde_json::json!({
                "type": "result",
                "text": format!("Bearer abcdefghijklmnopqrstuvwxyz {token}"),
                "result": {"findings": []}
            })
        );
        let output = redact_log(&log);
        assert!(output.len() > 2_000);
        assert!(!output.contains(&token));
        assert!(!output.contains("abcdefghijklmnopqrstuvwxyz"));
        let events: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1]["type"], "result");
        assert_eq!(events[1]["result"]["findings"], serde_json::json!([]));
        assert!(output.ends_with('\n'));
    }

    #[test]
    fn diagnostic_redaction_is_bounded_at_a_utf8_boundary() {
        for unicode in ["é", "界", "🦀"] {
            let message = format!("{}{}", "a".repeat(1_999), unicode.repeat(8));
            let output = redact(&message);
            assert_eq!(output, format!("{}…", "a".repeat(1_999)));
            assert!(output.len() <= 2_003);
            assert_eq!(redact_log(&message), message);
        }
    }

    #[test]
    fn retry_archives_prior_outputs_together_without_replaying_or_losing_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let result = directory.path().join("reducer.final.txt");
        let log = directory.path().join("reducer.stdout.jsonl");
        let absent = directory.path().join("reducer.failure.json");
        std::fs::write(&result, "old result").unwrap();
        std::fs::write(&log, "old log\n").unwrap();

        archive_previous_outputs(&[&result, &log, &absent]).unwrap();
        assert!(!result.exists());
        assert!(!log.exists());
        assert!(!absent.exists());
        let mut entries: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        assert_eq!(entries.len(), 2);
        assert_eq!(std::fs::read_to_string(&entries[0]).unwrap(), "old result");
        assert_eq!(std::fs::read_to_string(&entries[1]).unwrap(), "old log\n");
        let suffix = |path: &Path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .split_once(".attempt-")
                .unwrap()
                .1
                .to_string()
        };
        assert_eq!(suffix(&entries[0]), suffix(&entries[1]));

        std::fs::write(&result, "new result").unwrap();
        archive_previous_outputs(&[&result, &log, &absent]).unwrap();
        assert!(!result.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 3);
        assert_eq!(std::fs::read_to_string(&entries[0]).unwrap(), "old result");
    }
}
