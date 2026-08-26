use super::{
    CommandSpec, ProviderAdapter, ProviderContext, ProviderFailure, ProviderFailureKind,
    ProviderOutput, apply_external_action_guards, classify_failure, redact,
};
use crate::model::{AgentRole, ProviderKind};
use regex::Regex;
use serde_json::json;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    process::Command,
    time::{Instant, sleep, timeout},
};

pub async fn run(
    adapter: &ProviderAdapter,
    context: &ProviderContext,
) -> Result<ProviderOutput, ProviderFailure> {
    let provider_dir = context.run_dir.join("providers/claude");
    std::fs::create_dir_all(&provider_dir).map_err(ProviderFailure::internal)?;
    let result_path =
        provider_dir.join(format!("{:?}.hook.json", context.role).to_ascii_lowercase());
    let failure_path =
        provider_dir.join(format!("{:?}.failure.json", context.role).to_ascii_lowercase());
    let stdout_path =
        provider_dir.join(format!("{:?}.launch.log", context.role).to_ascii_lowercase());
    let stderr_path =
        provider_dir.join(format!("{:?}.stderr.log", context.role).to_ascii_lowercase());
    let settings_path =
        provider_dir.join(format!("{:?}.settings.json", context.role).to_ascii_lowercase());
    let empty_gh_config = provider_dir.join("empty-gh-config");
    std::fs::create_dir_all(&empty_gh_config).map_err(ProviderFailure::internal)?;
    let executable = std::env::current_exe().map_err(ProviderFailure::internal)?;
    let settings = hook_settings(&executable, &result_path, &failure_path, context.role);
    std::fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&settings).map_err(ProviderFailure::internal)?,
    )
    .map_err(ProviderFailure::internal)?;

    let mut spec = CommandSpec::new(adapter.binary.clone(), context.snapshot.clone());
    spec.remove_env.extend(
        [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "MOONSHOT_API_KEY",
            "KIMI_API_KEY",
            "CURSOR_API_KEY",
            "CURSOR_AUTH_TOKEN",
        ]
        .into_iter()
        .map(str::to_string),
    );
    apply_external_action_guards(&mut spec, &empty_gh_config);
    spec.args.extend([
        "--name".into(),
        "triad-reviewer".into(),
        "--agent".into(),
        "triad-reviewer".into(),
        "--background".into(),
        "--settings".into(),
        settings_path.display().to_string(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        "{\"mcpServers\":{}}".into(),
        "--disable-slash-commands".into(),
        "--no-chrome".into(),
    ]);
    if matches!(context.role, AgentRole::Fixer) {
        spec.args
            .extend(["--permission-mode".into(), "acceptEdits".into()]);
    } else {
        // Background plan mode writes plan files and can wait forever on Bash
        // approval. A reviewer gets only read tools and a non-interactive deny
        // policy, while the Stop hook still captures its final JSON.
        spec.args
            .extend(["--permission-mode".into(), "dontAsk".into()]);
    }
    if let Some(model) = &adapter.model {
        spec.args.extend(["--model".into(), model.clone()]);
    }
    spec.args.push(context.prompt.clone());

    let mut command = spec.into_tokio_command();
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let launch = timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| ProviderFailure::timeout(ProviderKind::Claude))?
        .map_err(|error| ProviderFailure::spawn(ProviderKind::Claude, error))?;
    let launch_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&launch.stdout),
        String::from_utf8_lossy(&launch.stderr)
    );
    std::fs::write(
        &stdout_path,
        redact(&String::from_utf8_lossy(&launch.stdout)),
    )
    .map_err(ProviderFailure::internal)?;
    std::fs::write(
        &stderr_path,
        redact(&String::from_utf8_lossy(&launch.stderr)),
    )
    .map_err(ProviderFailure::internal)?;
    if !launch.status.success() {
        return Err(classify_failure(ProviderKind::Claude, &launch_text));
    }
    let session_id = parse_session_id(&launch_text).ok_or_else(|| {
        ProviderFailure::malformed(
            ProviderKind::Claude,
            format!("cannot parse background session id from: {launch_text}"),
        )
    })?;
    std::fs::write(provider_dir.join("session-id"), &session_id)
        .map_err(ProviderFailure::internal)?;

    let started = Instant::now();
    while started.elapsed() < context.timeout {
        if result_path.exists() {
            let hook: serde_json::Value = serde_json::from_slice(
                &std::fs::read(&result_path).map_err(ProviderFailure::internal)?,
            )
            .map_err(ProviderFailure::internal)?;
            let text = hook
                .get("last_assistant_message")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            if text.trim().is_empty() {
                return Err(ProviderFailure::malformed(
                    ProviderKind::Claude,
                    "Stop hook did not contain last_assistant_message",
                ));
            }
            return Ok(ProviderOutput {
                provider: ProviderKind::Claude,
                text,
                model: adapter.model.clone(),
                session_id: Some(session_id),
                stdout_path,
                stderr_path,
            });
        }
        if failure_path.exists() {
            let body = std::fs::read_to_string(&failure_path).map_err(ProviderFailure::internal)?;
            return Err(classify_failure(ProviderKind::Claude, &body));
        }
        if started.elapsed() > Duration::from_secs(10)
            && background_session_active(&adapter.binary, &session_id).await == Some(false)
        {
            return Err(ProviderFailure::malformed(
                ProviderKind::Claude,
                "Claude background session ended without a Stop hook result",
            ));
        }
        sleep(Duration::from_secs(2)).await;
    }

    let _ = Command::new(&adapter.binary)
        .args(["stop", &session_id])
        .output()
        .await;
    Err(ProviderFailure {
        provider: Some(ProviderKind::Claude),
        kind: ProviderFailureKind::Timeout,
        message: "Claude background session timed out".into(),
        retry_at: None,
    })
}

async fn background_session_active(binary: &Path, session_id: &str) -> Option<bool> {
    let mut command = Command::new(binary);
    command.args(["agents", "--json"]);
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
    let output = timeout(Duration::from_secs(10), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sessions: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    Some(sessions.as_array()?.iter().any(|session| {
        session
            .get("id")
            .or_else(|| session.get("sessionId"))
            .and_then(|value| value.as_str())
            .is_some_and(|value| value.starts_with(session_id))
    }))
}

fn hook_settings(
    executable: &Path,
    output: &Path,
    failure: &Path,
    role: AgentRole,
) -> serde_json::Value {
    let success_command = format!(
        "{} internal claude-hook --output {}",
        quote(executable),
        quote(output)
    );
    let failure_command = format!(
        "{} internal claude-hook --output {} --failure",
        quote(executable),
        quote(failure)
    );
    let deny = if matches!(role, AgentRole::Fixer) {
        json!(["WebFetch", "WebSearch"])
    } else {
        json!(["WebFetch", "WebSearch", "Edit", "Write", "NotebookEdit"])
    };
    json!({
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": success_command, "timeout": 30}]}],
            "StopFailure": [{"hooks": [{"type": "command", "command": failure_command, "timeout": 30}]}]
        },
        "permissions": {
            "deny": deny
        }
    })
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn parse_session_id(value: &str) -> Option<String> {
    let regex = Regex::new(r"(?i)(?:backgrounded\s*[·:\-]?\s*|session\s+)([0-9a-f]{8,36})").ok()?;
    regex
        .captures(value)
        .and_then(|capture| capture.get(1))
        .map(|value| value.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_session_id;

    #[test]
    fn parses_background_id() {
        assert_eq!(
            parse_session_id("backgrounded · 7c5dcf5d"),
            Some("7c5dcf5d".into())
        );
    }
}
