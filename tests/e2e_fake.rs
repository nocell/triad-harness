use assert_cmd::Command;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command as StdCommand,
    thread,
    time::{Duration, Instant},
};

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

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

#[test]
fn four_provider_review_reduce_and_fix_leave_source_untouched() {
    four_provider_pipeline("default", true);
}

#[test]
fn easy_mode_survives_detach_resume_reduce_fix_and_dry_run() {
    four_provider_pipeline("easy", true);
}

#[test]
fn ultra_mode_survives_detach_resume_reduce_fix_and_dry_run() {
    four_provider_pipeline("ultra", true);
}

#[test]
fn ultra_without_claude_workflow_evidence_degrades_instead_of_accepting_fallback() {
    four_provider_pipeline("ultra", false);
}

fn four_provider_pipeline(mode: &str, attest_claude: bool) {
    let easy_mode = mode == "easy";
    let ultra_mode = mode == "ultra";
    let mode_args: &[&str] = match mode {
        "easy" => &["--easy-mode"],
        "ultra" => &["--ultra-mode"],
        _ => &[],
    };
    let claude_model = if easy_mode || ultra_mode {
        "claude-opus-5-5"
    } else {
        "claude-fable-5-1"
    };
    let codex_model = if easy_mode {
        "gpt-6.1-sol"
    } else {
        "gpt-6-astra"
    };
    let codex_effort = if ultra_mode { "ultra" } else { "max" };
    let fast_flag = if ultra_mode { "--enable" } else { "--disable" };
    let service_tier = if ultra_mode { "fast" } else { "default" };
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let bin = temp.path().join("bin");
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    let kimi_home = temp.path().join("kimi-home");
    let claude_home = temp.path().join("claude-home");
    fs::create_dir_all(kimi_home.join("credentials")).unwrap();
    // Discovery checks file presence only; fake providers never use real credentials.
    fs::write(kimi_home.join("credentials/kimi.json"), "{}").unwrap();
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&config).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "triad@test.invalid"]);
    git(&repo, &["config", "user.name", "Triad Test"]);
    fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("file.txt"), "base\nbug\n").unwrap();
    git(&repo, &["commit", "-am", "change"]);

    let deny_keys = r#"
for key in ANTHROPIC_API_KEY OPENAI_API_KEY MOONSHOT_API_KEY KIMI_API_KEY CURSOR_API_KEY CURSOR_AUTH_TOKEN GH_TOKEN GITHUB_TOKEN GITLAB_TOKEN SSH_AUTH_SOCK; do
  eval "value=\${$key:-}"
  if [ -n "$value" ]; then echo "secret env leaked: $key" >&2; exit 90; fi
done
"#;
    let guard_checks = r#"
if [ "${TRIAD_SIDE_EFFECT_POLICY:-}" != "read_only_no_external_actions" ]; then echo 'missing side-effect policy' >&2; exit 91; fi
if [ "${GIT_CONFIG_GLOBAL:-}" != "/dev/null" ] || [ "${GIT_CONFIG_SYSTEM:-}" != "/dev/null" ]; then echo 'git credentials not isolated' >&2; exit 92; fi
if [ -z "${GH_CONFIG_DIR:-}" ] || [ -e "$GH_CONFIG_DIR/hosts.yml" ]; then echo 'GitHub auth not isolated' >&2; exit 93; fi
if git remote get-url origin >/dev/null 2>&1; then echo 'snapshot retained push remote' >&2; exit 94; fi
"#;
    executable(
        &bin.join("claude"),
        &format!(
            r#"#!/bin/sh
{deny_keys}
if [ "$1" = "--version" ]; then echo '2.1.fake (Claude Code)'; exit 0; fi
if [ "$1" = "auth" ]; then echo '{{"loggedIn":true,"subscriptionType":"max","authMethod":"claude.ai"}}'; exit 0; fi
{guard_checks}
all="$*"
case " $all " in *" --model {claude_model} "*) ;; *) echo 'Claude model did not match the run preset' >&2; exit 95 ;; esac
settings=''; agents=''; sources='unset'; effort=''; tools=''
# Like Claude --background, the fake daemon owns its conversation UUID.
session="$(python3 -c 'import uuid; print(uuid.uuid4())')"
while [ $# -gt 0 ]; do
  case "$1" in
    --settings) settings="$2"; shift 2 ;;
    --agents) agents="$2"; shift 2 ;;
    --setting-sources) sources="$2"; shift 2 ;;
    --session-id) echo 'Background mode ignores caller-supplied session IDs' >&2; exit 103 ;;
    --effort) effort="$2"; shift 2 ;;
    --tools) tools="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ "$sources" = "" ] || {{ echo 'Claude inherited settings sources' >&2; exit 96; }}
python3 - "$agents" "$settings" "$session" "$effort" "$tools" <<'PY'
import json,os,shlex,subprocess,sys
from pathlib import Path
agents,settings,session,effort,tools=sys.argv[1:]
root=Path(os.environ["CLAUDE_CONFIG_DIR"])
trust=json.loads((root/".claude.json").read_text())
assert trust["projects"][str(Path.cwd().resolve())]["hasTrustDialogAccepted"] is True
a=json.loads(agents)
s=json.loads(Path(settings).read_text())
ultra="{ultra_mode}" == "true"
expected=["Read","Glob","Grep"] + (["Workflow"] if ultra else [])
assert a["triad-reviewer"]["tools"] == expected
packet=Path('.triad-review')
index=json.loads((packet/'changed-files.json').read_text())
assert index['files'], 'review packet omitted changed files'
assert '-base' not in (packet/'review.diff').read_text()
assert '+bug' in (packet/'review.diff').read_text()
entry=next(item for item in index['files'] if item['after']['path']=='file.txt')
assert (packet/entry['before']['asset']).read_text() == 'base\n'
assert (packet/entry['after']['asset']).read_text() == 'base\nbug\n'
hook={{"last_assistant_message":json.dumps({{"review_status":"complete","limitations":[],"findings":[]}})}}
if ultra:
    assert effort == "ultracode"
    assert s["ultracode"] is True and s["enableWorkflows"] is True and s["fastMode"] is False
    assert "Workflow" in tools and "Bash" not in tools
    assert "Workflow" in s["permissions"]["allow"]
    for denied in ["Edit","Write","Bash","NotebookEdit","Agent","WebFetch","WebSearch"]:
        assert denied in s["permissions"]["deny"], denied
    transcript=root/"projects"/"fake-ultra"/(session+".jsonl")
    transcript.parent.mkdir(parents=True,exist_ok=True)
    events=[
        {{"type":"attachment","isSidechain":False,"sessionId":session,"attachment":{{"type":"ultra_effort_enter","reminderType":"full"}}}},
        {{"type":"assistant","sessionId":session,"message":{{"role":"assistant","content":[{{"type":"tool_use","id":"workflow-1","name":"Workflow","input":{{"script":"await agent('read-only code check')"}}}}]}}}},
        {{"type":"user","sessionId":session,"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"workflow-1","content":"Workflow launched in background. Task ID: workflow-1","is_error":False}}]}}}},
    ]
    transcript.write_text("\n".join(json.dumps(event) for event in events)+"\n")
    hook.update(session_id=session,transcript_path=str(transcript),background_tasks=[])
    if "{attest_claude}" == "false":
        transcript.write_text("")
else:
    assert effort == ""
    assert not s.get("ultracode",False)
command=shlex.split(s["hooks"]["Stop"][0]["hooks"][0]["command"])
output=Path(command[command.index("--output")+1])
if ultra and "{attest_claude}" == "true":
    pending=dict(hook)
    pending.update(background_tasks=[{{"task_id":"workflow-1","status":"running"}}],last_assistant_message="Waiting for workflow checks")
    output.write_text(json.dumps(pending))
    # A real background session fires an interim Stop before the workflow's
    # final Stop; leave stdio detached so the launcher can exit immediately.
    finish='import os,sys,time; from pathlib import Path; time.sleep(1); p=Path(sys.argv[1]); t=p.with_suffix(".pending"); t.write_text(sys.argv[2]); os.replace(t,p)'
    subprocess.Popen([sys.executable,"-c",finish,str(output),json.dumps(hook)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)
else:
    output.write_text(json.dumps(hook))
PY
[ $? -eq 0 ] || exit 97
echo "backgrounded · ${{session%%-*}}"
"#
        ),
    );
    executable(
        &bin.join("codex"),
        &format!(
            r#"#!/bin/sh
{deny_keys}
if [ "$1" = "--version" ]; then echo 'codex-cli 0.1.0'; exit 0; fi
if [ "$1 $2" = "exec --help" ]; then echo '--json --ignore-user-config --strict-config --disable --enable --config --output-schema --output-last-message --sandbox --ignore-rules --cd --model'; exit 0; fi
if [ "$1" = "login" ]; then echo 'Logged in using ChatGPT'; exit 0; fi
{guard_checks}
all="$*"; final=''
case " $all " in *" --model {codex_model} "*) ;; *) echo 'Codex model did not match the run preset' >&2; exit 95 ;; esac
case " $all " in *" --config model_reasoning_effort={codex_effort} "*) ;; *) echo 'Codex reasoning did not match the run preset' >&2; exit 96 ;; esac
case " $all " in *" --ignore-user-config "*) ;; *) echo 'Codex inherited unsafe user config' >&2; exit 97 ;; esac
case " $all " in *" --disable hooks "*) ;; *) echo 'Codex hooks were not disabled' >&2; exit 98 ;; esac
case " $all " in *" {fast_flag} fast_mode "*) ;; *) echo 'Codex Fast feature did not match the run preset' >&2; exit 99 ;; esac
case " $all " in *' --config service_tier="{service_tier}" '*) ;; *) echo 'Codex service tier did not match the run preset' >&2; exit 100 ;; esac
while [ $# -gt 0 ]; do if [ "$1" = "--output-last-message" ]; then final="$2"; shift 2; else shift; fi; done
case "$all" in
  *"Triad reducer"*|*"Triad fixer"*) ;;
  *)
    python3 - "$final" <<'PY'
import json,sys,time
from pathlib import Path
manifest = Path(sys.argv[1]).parents[2] / "manifest.json"
deadline = time.monotonic() + 3
while time.monotonic() < deadline:
    records = json.loads(manifest.read_text())["providers"]
    if any(p["provider"] == "kimi" and (not p["selected"] or p["status"] == "completed") for p in records):
        break
    time.sleep(.02)
else:
    sys.exit("provider completion was hidden until all reviewers finished")
PY
    [ $? -eq 0 ] || exit 102 ;;
esac
case "$all" in
  *"Triad reducer"*) result='{{"review_status":"complete","limitations":[],"findings":[{{"id":"TRIAD-001","verdict":"accepted","title":"Concrete bug","severity":"high","file":"file.txt","line":2,"rationale":"verified","evidence":"bug line","trigger":"read file","impact":"failure","suggested_fix":"replace bug","sources":["codex"]}}]}}' ;;
  *"Triad fixer"*) printf 'fixed\n' >> file.txt; result='{{"summary":"fixed","tests":[{{"command":"true","status":"passed"}}]}}' ;;
  *) result='{{"review_status":"complete","limitations":[],"findings":[{{"title":"Concrete bug","severity":"high","confidence":"high","category":"correctness","file":"file.txt","line":2,"claim":"bug","evidence":"bug line","trigger":"read file","impact":"failure","suggested_fix":"replace bug"}}]}}' ;;
esac
printf '%s' "$result" > "$final"
printf '{{"type":"item.completed","item":{{"type":"agent_message","text":%s}}}}\n' "$(printf '%s' "$result" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')"
"#
        ),
    );
    executable(
        &bin.join("kimi"),
        &format!(
            r#"#!/bin/sh
{deny_keys}
if [ "$1" = "--version" ]; then echo '0.fake'; exit 0; fi
if [ "$1" = "doctor" ]; then echo 'Kimi doctor: membership authenticated'; exit 0; fi
{guard_checks}
all="$*"
case " $all " in *" --model kimi-code/k3 "*) ;; *) echo 'Kimi model was not pinned to K3' >&2; exit 95 ;; esac
echo '{{"type":"result","result":"{{\\"review_status\\":\\"complete\\",\\"limitations\\":[],\\"findings\\":[]}}","session_id":"kimi-1"}}'
"#
        ),
    );
    executable(
        &bin.join("cursor-agent"),
        &format!(
            r#"#!/bin/sh
{deny_keys}
if [ "$1" = "--version" ]; then echo 'cursor fake'; exit 0; fi
if [ "$1" = "status" ]; then echo 'Authenticated via browser login'; exit 0; fi
if [ "$1" = "mcp" ] && [ "$2" = "disable" ]; then exit 0; fi
{guard_checks}
all="$*"
case " $all " in *" --trust "*) ;; *) echo 'Workspace Trust Required' >&2; exit 95 ;; esac
case " $all " in *" --sandbox enabled "*) ;; *) echo 'Cursor sandbox was not enabled' >&2; exit 96 ;; esac
case " $all " in *" --mode ask "*) ;; *) echo 'Cursor reviewer was not read-only' >&2; exit 97 ;; esac
case " $all " in *" --model grok-4.7-high-fast "*) ;; *) echo 'Cursor model was not resolved to the installed Grok 4.7 High Fast ID' >&2; exit 98 ;; esac
case " $all " in *" --force "*|*" --yolo "*|*" -f "*) echo 'Cursor unsafe force flag was passed' >&2; exit 99 ;; esac
for flag in --single-turn --disable-indexing --disable-codebase-ref; do
  case " $all " in *" $flag "*) ;; *) echo "Cursor isolation flag missing: $flag" >&2; exit 100 ;; esac
done
if [ -z "$CURSOR_CONFIG_DIR" ] || [ ! -f "$CURSOR_CONFIG_DIR/mcp.json" ]; then echo 'Cursor inherited global config or MCPs' >&2; exit 100; fi
if [ ! -f "$(dirname "$CURSOR_CONFIG_DIR")/disabled-mcps.json" ]; then echo 'Cursor MCP audit file missing' >&2; exit 101; fi
echo '{{"type":"system","subtype":"init","apiKeySource":"login","model":"Grok 4.7 High Fast","session_id":"cursor-1"}}'
echo '{{"type":"result","result":"{{\\"review_status\\":\\"complete\\",\\"limitations\\":[],\\"findings\\":[]}}","session_id":"cursor-1"}}'
"#
        ),
    );

    let config_body = format!(
        r#"
leader_order = ["codex", "claude", "cursor", "kimi"]
reviewer_timeout_minutes = 1
reducer_timeout_minutes = 1
fixer_timeout_minutes = 1
cooldown_minutes = 15

[providers.claude]
enabled = true
binary = "{}"
model = "claude-fable-5-1"

[providers.codex]
enabled = true
binary = "{}"
model = "gpt-6-astra"

[providers.kimi]
enabled = true
binary = "{}"

[providers.cursor]
enabled = true
binary = "{}"
model = "grok-4.7-fast"

[providers.zcode]
enabled = false
[providers.zcode_flash]
enabled = false
"#,
        bin.join("claude").display(),
        bin.join("codex").display(),
        bin.join("kimi").display(),
        bin.join("cursor-agent").display()
    );
    fs::write(config.join("config.toml"), &config_body).unwrap();

    let mut review = Command::cargo_bin("triad").unwrap();
    let output = review
        .current_dir(&repo)
        .env("KIMI_CODE_HOME", &kimi_home)
        .env("CLAUDE_CONFIG_DIR", &claude_home)
        .env("TRIAD_CONFIG_HOME", &config)
        .env("TRIAD_DATA_HOME", &data)
        .env("ANTHROPIC_API_KEY", "must-not-leak")
        .env("OPENAI_API_KEY", "must-not-leak")
        .env("MOONSHOT_API_KEY", "must-not-leak")
        .env("CURSOR_API_KEY", "must-not-leak")
        .args(["review", "--base", &base, "--detach"])
        .args(mode_args)
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success(),
        "stdout={stdout}\nstderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run_id = stdout.trim().to_string();
    let run_dir = data.join("runs").join(&run_id);
    let started = Instant::now();
    loop {
        if let Ok(body) = fs::read_to_string(run_dir.join("manifest.json")) {
            let manifest: serde_json::Value = serde_json::from_str(&body).unwrap();
            let state = manifest
                .get("state")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            if state == "awaiting_approval" {
                break;
            }
            if matches!(state, "failed" | "cancelled") {
                let stderr =
                    fs::read_to_string(run_dir.join("worker.stderr.log")).unwrap_or_default();
                panic!("detached review ended as {state}: {body}\n{stderr}");
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "detached review timed out"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let report = fs::read_to_string(data.join("runs").join(&run_id).join("report.md")).unwrap();
    assert!(report.contains("TRIAD-001"));
    assert!(report.contains("**cursor:** completed"));

    let manifest_path = run_dir.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["easy_mode"], easy_mode);
    assert_eq!(manifest["ultra_mode"], ultra_mode);
    // This legacy four-provider fixture deliberately disables both GLM slots.
    assert_eq!(manifest["degraded"], true, "{manifest}");
    assert!(
        manifest["providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|provider| {
                if matches!(provider["provider"].as_str(), Some("zcode" | "zcode_flash")) {
                    return provider["selected"] == false;
                }
                provider["selected"] == true
                    && provider["status"]
                        == if provider["provider"] == "claude" && !attest_claude {
                            "failed"
                        } else {
                            "completed"
                        }
            }),
        "{manifest}"
    );
    for (provider, model) in [("claude", claude_model), ("codex", codex_model)] {
        let record = manifest["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["provider"] == provider)
            .unwrap();
        assert_eq!(record["model"], model);
        if provider == "claude" && !attest_claude {
            assert!(
                record["error"]
                    .as_str()
                    .unwrap()
                    .contains("Ultra unavailable or unverified"),
                "{record}"
            );
        }
    }
    if easy_mode || ultra_mode {
        // Simulate a stopped worker, then replay its saved request without a new mode flag.
        manifest["state"] = "failed".into();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        Command::cargo_bin("triad")
            .unwrap()
            .current_dir(&repo)
            .env("TRIAD_CONFIG_HOME", &config)
            .env("TRIAD_DATA_HOME", &data)
            .args(["resume", &run_id])
            .env("KIMI_CODE_HOME", &kimi_home)
            .env("CLAUDE_CONFIG_DIR", &claude_home)
            .assert()
            .code(2);
        manifest = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["easy_mode"], easy_mode);
        assert_eq!(manifest["ultra_mode"], ultra_mode);
        assert_eq!(manifest["state"], "awaiting_approval");
        assert_eq!(manifest["degraded"], true, "{manifest}");
    }
    manifest["error"] = serde_json::Value::String("stale retry error".into());
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let mut fix = Command::cargo_bin("triad").unwrap();
    let fix_output = fix
        .current_dir(&repo)
        .env("KIMI_CODE_HOME", &kimi_home)
        .env("TRIAD_CONFIG_HOME", &config)
        .env("TRIAD_DATA_HOME", &data)
        .env("OPENAI_API_KEY", "must-not-leak")
        .args(["fix", &run_id])
        .output()
        .unwrap();
    assert!(
        fix_output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&fix_output.stdout),
        String::from_utf8_lossy(&fix_output.stderr)
    );
    let patch = fs::read_to_string(data.join("runs").join(&run_id).join("fix.patch")).unwrap();
    assert!(patch.contains("+fixed"));
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["state"], "completed");
    assert_eq!(manifest["error"], serde_json::Value::Null);
    assert_eq!(manifest["easy_mode"], easy_mode);
    assert_eq!(manifest["ultra_mode"], ultra_mode);
    assert_eq!(
        fs::read_to_string(config.join("config.toml")).unwrap(),
        config_body
    );
    assert_eq!(
        fs::read_to_string(repo.join("file.txt")).unwrap(),
        "base\nbug\n"
    );
    assert!(git(&repo, &["status", "--porcelain"]).is_empty());

    let dry_data = temp.path().join("data-dry");
    let dry_run = Command::cargo_bin("triad")
        .unwrap()
        .env("KIMI_CODE_HOME", &kimi_home)
        .current_dir(&repo)
        .env("TRIAD_CONFIG_HOME", &config)
        .env("TRIAD_DATA_HOME", &dry_data)
        .args([
            "review",
            "--base",
            &base,
            "--providers",
            "codex",
            "--leader",
            "codex",
            "--dry-run",
            "--json",
        ])
        .args(mode_args)
        .output()
        .unwrap();
    assert_eq!(
        dry_run.status.code(),
        Some(2),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&dry_run.stdout),
        String::from_utf8_lossy(&dry_run.stderr)
    );
    let summary: serde_json::Value = serde_json::from_slice(&dry_run.stdout).unwrap();
    assert_eq!(summary["state"], "completed");
    assert_eq!(summary["dry_run"], true);
    assert_eq!(summary["blocking_findings"], 1);
    let dry_run_dir = dry_data
        .join("runs")
        .join(summary["run_id"].as_str().unwrap());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(dry_run_dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["state"], "completed");
    assert_eq!(manifest["dry_run"], true);
    assert_eq!(manifest["easy_mode"], easy_mode);
    assert_eq!(manifest["ultra_mode"], ultra_mode);
    assert!(!dry_run_dir.join("fix.patch").exists());
}

#[test]
fn reviewer_file_mutation_is_discarded_as_protocol_violation() {
    reviewer_mutation_is_discarded(false);
}

#[test]
fn reviewer_reediting_already_dirty_file_is_discarded_as_protocol_violation() {
    reviewer_mutation_is_discarded(true);
}

fn reviewer_mutation_is_discarded(uncommitted: bool) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let bin = temp.path().join("bin");
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&config).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "triad@test.invalid"]);
    git(&repo, &["config", "user.name", "Triad Test"]);
    fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("file.txt"), "base\nhead\n").unwrap();
    git(&repo, &["commit", "-am", "head"]);
    if uncommitted {
        fs::write(
            repo.join("file.txt"),
            "base\nhead\nuser-owned pending change\n",
        )
        .unwrap();
    }
    let source = fs::read_to_string(repo.join("file.txt")).unwrap();
    let source_status = git(&repo, &["status", "--porcelain"]);

    executable(
        &bin.join("codex"),
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo 'codex-cli custom-build'; exit 0; fi
if [ "$1 $2" = "exec --help" ]; then echo '--json --ignore-user-config --strict-config --disable --config --output-schema --output-last-message --sandbox --ignore-rules --cd --model'; exit 0; fi
if [ "$1" = "login" ]; then echo 'Logged in using ChatGPT'; exit 0; fi
printf 'agent mutation\n' >> file.txt
final=''
while [ $# -gt 0 ]; do if [ "$1" = "--output-last-message" ]; then final="$2"; shift 2; else shift; fi; done
result='{"review_status":"complete","limitations":[],"findings":[{"title":"tempting result","severity":"high","confidence":"high","category":"correctness","file":"file.txt","line":2,"claim":"bug","evidence":"line","trigger":"read","impact":"failure","suggested_fix":"fix"}]}'
printf '%s' "$result" > "$final"
printf '{"type":"item.completed","item":{"type":"agent_message","text":%s}}\n' "$(printf '%s' "$result" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))')"
"#,
    );
    fs::write(
        config.join("config.toml"),
        format!(
            r#"
leader_order = ["codex"]
reviewer_timeout_minutes = 1
reducer_timeout_minutes = 1
fixer_timeout_minutes = 1
cooldown_minutes = 15

[providers.codex]
enabled = true
binary = "{}"
"#,
            bin.join("codex").display()
        ),
    )
    .unwrap();

    let mut command = Command::cargo_bin("triad").unwrap();
    command
        .current_dir(&repo)
        .env("TRIAD_CONFIG_HOME", &config)
        .env("TRIAD_DATA_HOME", &data)
        .arg("review");
    if uncommitted {
        command.arg("--uncommitted");
    } else {
        command.args(["--base", &base]);
    }
    let output = command
        .args([
            "--providers",
            "codex",
            "--leader",
            "codex",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(fs::read_to_string(repo.join("file.txt")).unwrap(), source);
    assert_eq!(git(&repo, &["status", "--porcelain"]), source_status);

    let run_dir = fs::read_dir(data.join("runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(run_dir.join("manifest.json")).unwrap()).unwrap();
    let codex = manifest["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|provider| provider["provider"] == "codex")
        .unwrap();
    assert_eq!(codex["status"], "protocol_violation");
    assert_eq!(codex["protocol_violation"], true);
}

#[test]
fn correct_change_produces_no_false_positive() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let bin = temp.path().join("bin");
    let config = temp.path().join("config");
    let data = temp.path().join("data");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&config).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["config", "user.email", "triad@test.invalid"]);
    git(&repo, &["config", "user.name", "Triad Test"]);
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"clean-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn identity(value: i32) -> i32 { value }\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(
        repo.join("src/lib.rs"),
        r#"pub fn identity(value: i32) -> i32 { value }

pub fn clamp_percent(value: i32) -> i32 {
    value.clamp(0, 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_percent_to_closed_range() {
        assert_eq!(clamp_percent(-1), 0);
        assert_eq!(clamp_percent(42), 42);
        assert_eq!(clamp_percent(101), 100);
    }
}
"#,
    )
    .unwrap();
    git(&repo, &["commit", "-am", "add correct clamp helper"]);

    executable(
        &bin.join("codex"),
        r#"#!/bin/sh
if [ "$1" = "--version" ]; then echo 'codex-cli custom-build'; exit 0; fi
if [ "$1 $2" = "exec --help" ]; then echo '--json --ignore-user-config --strict-config --disable --config --output-schema --output-last-message --sandbox --ignore-rules --cd --model'; exit 0; fi
if [ "$1" = "login" ]; then echo 'Logged in using ChatGPT'; exit 0; fi
final=''
while [ $# -gt 0 ]; do if [ "$1" = "--output-last-message" ]; then final="$2"; shift 2; else shift; fi; done
result='{"review_status":"complete","limitations":[],"findings":[]}'
printf '%s' "$result" > "$final"
printf '{"type":"thread.started","thread_id":"clean-session"}\n'
printf '{"type":"item.completed","item":{"type":"agent_message","text":"{\\"review_status\\":\\"complete\\",\\"limitations\\":[],\\"findings\\":[]}"}}\n'
"#,
    );
    fs::write(
        config.join("config.toml"),
        format!(
            r#"
leader_order = ["codex"]
reviewer_timeout_minutes = 1
reducer_timeout_minutes = 1
fixer_timeout_minutes = 1
cooldown_minutes = 15

[providers.codex]
enabled = true
binary = "{}"
"#,
            bin.join("codex").display()
        ),
    )
    .unwrap();

    let output = Command::cargo_bin("triad")
        .unwrap()
        .current_dir(&repo)
        .env("TRIAD_CONFIG_HOME", &config)
        .env("TRIAD_DATA_HOME", &data)
        .args([
            "review",
            "--base",
            &base,
            "--providers",
            "codex",
            "--leader",
            "codex",
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(summary["state"], "completed");
    assert_eq!(summary["dry_run"], true);
    assert_eq!(summary["blocking_findings"], 0);

    let run_dir = fs::read_dir(data.join("runs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let report = fs::read_to_string(run_dir.join("report.md")).unwrap();
    let findings: serde_json::Value =
        serde_json::from_slice(&fs::read(run_dir.join("findings.json")).unwrap()).unwrap();
    assert!(report.contains("## Accepted\n\nNone."));
    assert!(report.contains("## Needs human\n\nNone."));
    assert!(report.contains("## Rejected\n\nNone."));
    assert_eq!(findings["findings"], serde_json::json!([]));
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(run_dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["state"], "completed");
    assert_eq!(manifest["dry_run"], true);
    assert!(!run_dir.join("fix.patch").exists());

    let fix = Command::cargo_bin("triad")
        .unwrap()
        .current_dir(&repo)
        .env("TRIAD_CONFIG_HOME", &config)
        .env("TRIAD_DATA_HOME", &data)
        .args(["fix", summary["run_id"].as_str().unwrap()])
        .output()
        .unwrap();
    assert!(!fix.status.success());
    assert!(String::from_utf8_lossy(&fix.stderr).contains("current state is Completed"));
    assert!(git(&repo, &["status", "--porcelain"]).is_empty());
}
