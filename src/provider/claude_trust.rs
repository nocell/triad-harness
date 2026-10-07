use super::ProviderContext;
use crate::{model::AgentRole, storage};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::{Instant, sleep};

// Claude 2.1.281+ requires a durable workspace trust decision for --background.
// Trust only the clone Triad prepared, never its source repo or a parent folder.
pub async fn trust_snapshot(context: &ProviderContext) -> Result<()> {
    let snapshot = managed_snapshot(context)?;
    let home = dirs::home_dir().context("cannot determine Claude config directory")?;
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let path = config_path(&home, config_dir.as_deref());
    grant_trust(&path, &snapshot).await
}

fn config_path(home: &Path, override_dir: Option<&Path>) -> PathBuf {
    let legacy = override_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
        .join(".config.json");
    if legacy.exists() {
        legacy
    } else {
        override_dir.unwrap_or(home).join(".claude.json")
    }
}

fn managed_snapshot(context: &ProviderContext) -> Result<PathBuf> {
    let run_dir = context.run_dir.canonicalize()?;
    ensure!(
        run_dir.parent() == Some(storage::runs_root()?.canonicalize()?.as_path()),
        "refusing to trust a snapshot outside Triad's runs directory"
    );
    let name = match context.role {
        AgentRole::Reviewer => "claude",
        AgentRole::Reducer => "reducer",
        AgentRole::Fixer => "fix",
    };
    let snapshot = context.snapshot.canonicalize()?;
    ensure!(
        snapshot == run_dir.join("snapshots").join(name) && snapshot.join(".git").is_dir(),
        "refusing to trust anything except the managed disposable clone"
    );
    Ok(snapshot)
}

struct ConfigLock(PathBuf);

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.0);
    }
}

async fn grant_trust(config_path: &Path, snapshot: &Path) -> Result<()> {
    fs::create_dir_all(config_path.parent().context("config path has no parent")?)?;
    // Match Claude's proper-lockfile mkdir lock. Do not break an existing lock.
    let lock_path = PathBuf::from(format!("{}.lock", config_path.display()));
    let started = Instant::now();
    let _lock = loop {
        match fs::create_dir(&lock_path) {
            Ok(()) => break ConfigLock(lock_path),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    && started.elapsed() < Duration::from_secs(5) =>
            {
                sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error).context("cannot lock Claude workspace trust config"),
        }
    };
    let mut config: Value = match fs::read(config_path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).context("invalid Claude config; left unchanged")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(error.into()),
    };
    let root = config
        .as_object_mut()
        .context("Claude config must be an object")?;
    let projects = root
        .entry("projects")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Claude projects must be an object")?;
    let project = projects
        .entry(snapshot.to_str().context("snapshot path is not UTF-8")?)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("Claude project must be an object")?;
    if project.get("hasTrustDialogAccepted") == Some(&Value::Bool(true)) {
        return Ok(());
    }
    project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
    storage::write_json(config_path, &config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trusts_only_exact_snapshot_and_preserves_config() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".claude.json");
        let snapshot = temp.path().join("runs/one/snapshots/claude");
        let before = json!({"oauthAccount": {"id": "keep"}, "projects": {
            "/existing": {"hasTrustDialogAccepted": false, "other": 1}
        }});
        storage::write_json(&config, &before).unwrap();
        grant_trust(&config, &snapshot).await.unwrap();
        grant_trust(&config, &snapshot).await.unwrap();
        let after: Value = storage::read_json(&config).unwrap();
        assert_eq!(after["oauthAccount"], before["oauthAccount"]);
        assert_eq!(
            after["projects"]["/existing"],
            before["projects"]["/existing"]
        );
        assert_eq!(after["projects"].as_object().unwrap().len(), 2);
        assert_eq!(
            after["projects"][snapshot.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
        assert!(!temp.path().join(".claude.json.lock").exists());
    }

    #[tokio::test]
    async fn refuses_unmanaged_checkout() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let context = ProviderContext {
            role: AgentRole::Reviewer,
            snapshot: temp.path().into(),
            run_dir: temp.path().into(),
            prompt: String::new(),
            schema_path: temp.path().join("schema.json"),
            timeout: Duration::from_secs(1),
        };
        assert!(managed_snapshot(&context).is_err());
    }

    #[test]
    fn respects_custom_and_legacy_config_locations() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let custom = home.join("custom");
        assert_eq!(config_path(home, None), home.join(".claude.json"));
        assert_eq!(
            config_path(home, Some(&custom)),
            custom.join(".claude.json")
        );
        fs::create_dir(&custom).unwrap();
        fs::write(custom.join(".config.json"), "{}").unwrap();
        assert_eq!(
            config_path(home, Some(&custom)),
            custom.join(".config.json")
        );
    }

    #[tokio::test]
    async fn corrupt_config_is_not_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".claude.json");
        fs::write(&config, "invalid").unwrap();
        assert!(grant_trust(&config, temp.path()).await.is_err());
        assert_eq!(fs::read_to_string(config).unwrap(), "invalid");
    }

    #[tokio::test]
    async fn concurrent_grants_preserve_both_projects() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join(".claude.json");
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        let (first, second) = tokio::join!(grant_trust(&config, &a), grant_trust(&config, &b));
        first.unwrap();
        second.unwrap();
        let after: Value = storage::read_json(&config).unwrap();
        assert_eq!(after["projects"].as_object().unwrap().len(), 2);
    }
}
