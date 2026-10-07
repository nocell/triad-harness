use assert_cmd::Command;
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command as StdCommand, Output},
    thread,
    time::{Duration, Instant},
};

fn git(repo: &Path, args: &[&str]) -> String {
    let output = StdCommand::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

struct Fixture {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    config: PathBuf,
    data: PathBuf,
    control: PathBuf,
    base: String,
    head: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let config = temp.path().join("config");
        let data = temp.path().join("data");
        let control = temp.path().join("control");
        for path in [&repo, &config, &control] {
            fs::create_dir_all(path).unwrap();
        }
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "triad@test.invalid"]);
        git(&repo, &["config", "user.name", "Triad Test"]);
        fs::write(repo.join("file.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "base"]);
        let base = git(&repo, &["rev-parse", "HEAD"]);
        fs::write(repo.join("file.txt"), "base\nreviewed change\n").unwrap();
        git(&repo, &["commit", "-am", "review target"]);
        let head = git(&repo, &["rev-parse", "HEAD"]);

        let binary = temp.path().join("fake-codex");
        let script = r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo 'codex-cli custom-test-build'; exit 0; fi
if [ "$1 $2" = "exec --help" ]; then
  echo '--json --ignore-user-config --strict-config --disable --config --output-schema --output-last-message --sandbox --ignore-rules --cd --model'
  exit 0
fi
if [ "$1" = "login" ]; then echo 'Logged in using ChatGPT'; exit 0; fi
control='@CONTROL@'
all="$*"
final=''
while [ $# -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then final="$2"; shift 2; else shift; fi
done
case "$all" in
  *'Triad fixer'*)
    printf '%s\n' fixer >> "$control/calls"
    printf 'proposed isolated fix\n' >> file.txt
    printf '%s' '{"summary":"Proposed fix needs more work","tests":[{"command":"existing unit test","status":"failed"}]}' > "$final"
    ;;
  *'Triad reducer'*)
    printf '%s\n' reducer >> "$control/calls"
    git rev-parse HEAD >> "$control/reducer-heads"
    if [ "$(cat "$control/mode")" = fail ]; then echo 'synthetic reducer connection failure' >&2; exit 1; fi
    if [ "$(cat "$control/mode")" = slow ]; then sleep 12; fi
    cat "$control/reducer.json" > "$final"
    ;;
  *)
    printf '%s\n' reviewer >> "$control/calls"
    git rev-parse HEAD >> "$control/reviewer-heads"
    printf '%s' '{"findings":[{"title":"Concrete regression","severity":"high","confidence":"high","category":"correctness","file":"file.txt","line":2,"claim":"bug","evidence":"changed line","trigger":"read","impact":"failure","suggested_fix":"fix line"}]}' > "$final"
    ;;
esac
printf '%s\n' '{"type":"thread.started","thread_id":"fake-recovery-session"}'
"#;
        fs::write(
            &binary,
            script.replace("@CONTROL@", &control.to_string_lossy()),
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            config.join("config.toml"),
            format!(
                r#"leader_order = ["codex"]
reviewer_timeout_minutes = 1
reducer_timeout_minutes = 1
fixer_timeout_minutes = 1
cooldown_minutes = 15

[providers.codex]
enabled = true
binary = "{}"

[providers.claude]
enabled = false
[providers.kimi]
enabled = false
[providers.cursor]
enabled = false
[providers.zcode]
enabled = false
[providers.zcode_flash]
enabled = false
"#,
                binary.display()
            ),
        )
        .unwrap();
        fs::write(control.join("mode"), "success").unwrap();
        fs::write(control.join("reducer.json"), r#"{"findings":[]}"#).unwrap();
        Self {
            _temp: temp,
            repo,
            config,
            data,
            control,
            base,
            head,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::cargo_bin("triad").unwrap();
        command
            .current_dir(&self.repo)
            .env("TRIAD_CONFIG_HOME", &self.config)
            .env("TRIAD_DATA_HOME", &self.data);
        command
    }

    fn review(&self) -> Output {
        self.command()
            .args([
                "review",
                "--base",
                &self.base,
                "--providers",
                "codex",
                "--leader",
                "codex",
                "--dry-run",
                "--json",
            ])
            .output()
            .unwrap()
    }

    fn run_dir(&self) -> PathBuf {
        let runs: Vec<_> = fs::read_dir(self.data.join("runs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(runs.len(), 1);
        runs[0].clone()
    }

    fn manifest(&self) -> Value {
        serde_json::from_slice(&fs::read(self.run_dir().join("manifest.json")).unwrap()).unwrap()
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.control.join("calls"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn assert_incomplete(&self, output: &Output) {
        assert_eq!(
            output.status.code(),
            Some(3),
            "stdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let manifest = self.manifest();
        assert_eq!(manifest["state"], "failed", "{manifest}");
        assert!(
            manifest["error"]
                .as_str()
                .is_some_and(|s| s.contains("reducer")),
            "{manifest}"
        );
        let report = fs::read_to_string(self.run_dir().join("report.md"))
            .unwrap_or_else(|error| panic!("missing incomplete report: {error}; {manifest}"));
        assert!(report.starts_with("# Incomplete Triad review"), "{report}");
        assert!(report.contains("not a completed review"), "{report}");
        assert!(!report.contains("## Accepted\n\nNone."), "{report}");
    }
}

#[test]
fn reducer_retry_reuses_map_and_exact_target_after_source_head_advances() {
    let fixture = Fixture::new();
    fs::write(fixture.control.join("mode"), "fail").unwrap();
    fixture.assert_incomplete(&fixture.review());
    assert_eq!(fixture.calls(), ["reviewer", "reducer"]);
    let manifest = fixture.manifest();
    let run_id = manifest["id"].as_str().unwrap();
    let target = manifest["target"].clone();
    assert_eq!(target["base_sha"], fixture.base);
    assert_eq!(target["head_sha"], fixture.head);
    let checkpoint_path = fixture.run_dir().join("provider-results.json");
    let checkpoint = fs::read(&checkpoint_path).unwrap();
    let results: Value = serde_json::from_slice(&checkpoint).unwrap();
    assert!(results["codex"].is_string(), "{results}");

    fs::write(fixture.repo.join("file.txt"), "unrelated later commit\n").unwrap();
    git(
        &fixture.repo,
        &["commit", "-am", "advance source after failed reduce"],
    );
    let new_head = git(&fixture.repo, &["rev-parse", "HEAD"]);
    assert_ne!(new_head, fixture.head);
    fs::write(fixture.control.join("mode"), "success").unwrap();

    let output = fixture
        .command()
        .args(["resume", run_id, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["state"], "completed");
    assert_eq!(fixture.calls(), ["reviewer", "reducer", "reducer"]);
    assert_eq!(fs::read(checkpoint_path).unwrap(), checkpoint);
    let manifest = fixture.manifest();
    assert_eq!(manifest["state"], "completed");
    assert_eq!(manifest["target"], target);
    assert_eq!(manifest["error"], Value::Null);
    assert_eq!(
        fs::read_to_string(fixture.control.join("reducer-heads")).unwrap(),
        format!("{}\n{}\n", fixture.head, fixture.head)
    );
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"]), new_head);
    assert!(git(&fixture.repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn malformed_reducer_roots_fail_instead_of_looking_like_clean_reviews() {
    for malformed in [
        "{}",
        r#"{"issues":[{"status":"accepted","title":"Dropped finding"}]}"#,
        r#"{"findings":[{"status":"accepted","title":"Missing verdict"}]}"#,
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.control.join("reducer.json"), malformed).unwrap();
        fixture.assert_incomplete(&fixture.review());
        assert_eq!(fixture.calls(), ["reviewer", "reducer"]);
    }
}

#[test]
fn failed_fixer_tests_keep_patch_but_do_not_claim_completion() {
    let fixture = Fixture::new();
    fs::write(
        fixture.control.join("reducer.json"),
        r#"{"findings":[{"id":"TRIAD-001","verdict":"accepted","title":"Concrete regression","severity":"high","file":"file.txt","line":2,"rationale":"verified","evidence":"changed line","trigger":"read","impact":"failure","suggested_fix":"fix line","sources":["codex"]}]}"#,
    )
    .unwrap();
    fixture
        .command()
        .args([
            "review",
            "--base",
            &fixture.base,
            "--providers",
            "codex",
            "--leader",
            "codex",
        ])
        .assert()
        .code(2);
    let manifest = fixture.manifest();
    assert_eq!(manifest["state"], "awaiting_approval");
    let run_id = manifest["id"].as_str().unwrap();
    fixture.command().args(["fix", run_id]).assert().code(3);
    let manifest = fixture.manifest();
    assert_eq!(manifest["state"], "fix_incomplete", "{manifest}");
    assert!(manifest["error"].as_str().is_some_and(|s| !s.is_empty()));
    let patch = fs::read_to_string(fixture.run_dir().join("fix.patch")).unwrap();
    assert!(patch.contains("+proposed isolated fix"), "{patch}");
    assert_eq!(fixture.calls(), ["reviewer", "reducer", "fixer"]);
    assert_eq!(
        fs::read_to_string(fixture.repo.join("file.txt")).unwrap(),
        "base\nreviewed change\n"
    );
    assert_eq!(git(&fixture.repo, &["rev-parse", "HEAD"]), fixture.head);
    assert!(git(&fixture.repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn detached_json_is_machine_readable_and_slow_reducer_updates_heartbeat() {
    let fixture = Fixture::new();
    fs::write(fixture.control.join("mode"), "slow").unwrap();
    let output = fixture
        .command()
        .args([
            "review",
            "--base",
            &fixture.base,
            "--providers",
            "codex",
            "--leader",
            "codex",
            "--detach",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let started: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(started["detached"], true);
    assert!(started["pid"].as_u64().is_some());
    let run_id = started["run_id"].as_str().unwrap();
    assert_eq!(fixture.manifest()["id"], run_id);

    let deadline = Instant::now() + Duration::from_secs(25);
    let mut first_reducing_heartbeat = None;
    let mut advanced_while_reducing = false;
    let mut checked_active_resume = false;
    loop {
        let manifest = fixture.manifest();
        if manifest["state"] == "reducing" {
            if !checked_active_resume {
                let rejected = fixture
                    .command()
                    .args(["resume", run_id, "--detach", "--json"])
                    .output()
                    .unwrap();
                assert!(!rejected.status.success());
                assert!(String::from_utf8_lossy(&rejected.stderr).contains("already active"));
                assert_eq!(fixture.manifest()["pid"], manifest["pid"]);
                checked_active_resume = true;
            }
            let heartbeat =
                chrono::DateTime::parse_from_rfc3339(manifest["heartbeat_at"].as_str().unwrap())
                    .unwrap();
            let first = *first_reducing_heartbeat.get_or_insert(heartbeat);
            advanced_while_reducing |= (heartbeat - first).num_seconds() >= 5;
        }
        if manifest["state"] == "awaiting_approval" {
            break;
        }
        assert_ne!(manifest["state"], "failed", "{manifest}");
        assert!(
            Instant::now() < deadline,
            "detached worker timed out: {manifest}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    assert!(
        advanced_while_reducing,
        "heartbeat did not advance during the blocked reducer"
    );
    assert_eq!(fixture.calls(), ["reviewer", "reducer"]);
    assert!(git(&fixture.repo, &["status", "--porcelain"]).is_empty());
}
