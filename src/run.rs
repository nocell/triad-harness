use crate::{
    cli::{
        FixArgs, FollowArgs, InstallSkillArgs, InternalArgs, InternalCommand, ResumeArgs,
        ReviewArgs, RunIdArgs, RunsArgs, SkillHost,
    },
    config::Config,
    git,
    model::{
        AgentRole, ProviderKind, ProviderRunRecord, ReducedFinding, ReductionEnvelope, RunManifest,
        RunState,
    },
    provider::{self, ProviderContext},
    report,
    scheduler::{self, ProviderLedger},
    storage,
};
use anyhow::{Context, Result};
use chrono::Utc;
use fs2::FileExt;
use futures::{StreamExt, stream::FuturesUnordered};
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    future::Future,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{process::Command, time::sleep};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WorkerRequest {
    Review {
        run_id: String,
        args: ReviewArgs,
    },
    Fix {
        run_id: String,
        only: Vec<String>,
        exclude: Vec<String>,
        leader: String,
    },
}

struct RunLock(File);

impl RunLock {
    fn acquire(run_dir: &Path) -> Result<Self> {
        let path = run_dir.join("run.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock_exclusive().context("run is already active")?;
        Ok(Self(file))
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

pub async fn review_command(mut args: ReviewArgs) -> Result<i32> {
    let run_id = args
        .run_id
        .take()
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let run_dir = storage::run_dir(&run_id)?;
    fs::create_dir_all(&run_dir)?;
    let detach = args.detach;
    args.detach = false;
    args.run_id = None;
    let request = WorkerRequest::Review {
        run_id: run_id.clone(),
        args: args.clone(),
    };
    let request_path = run_dir.join("request.json");
    storage::write_json(&request_path, &request)?;
    if !manifest_path(&run_id)?.exists() {
        save_manifest(&RunManifest {
            id: run_id.clone(),
            state: RunState::Queued,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            heartbeat_at: None,
            pid: None,
            request_path: request_path.clone(),
            target: None,
            providers: Vec::new(),
            leader: None,
            degraded: false,
            error: None,
            report_path: None,
            patch_path: None,
            dry_run: args.dry_run,
            easy_mode: args.easy_mode,
            ultra_mode: args.ultra_mode,
        })?;
    }
    if detach {
        create_detached(&request, &run_id, args.json).await
    } else {
        run_review_pipeline(&run_id, &args).await
    }
}

// Entry used by main before the args are normalized when detach is requested.
async fn create_detached(request: &WorkerRequest, run_id: &str, json: bool) -> Result<i32> {
    let run_dir = storage::run_dir(run_id)?;
    let launch_lock = RunLock::acquire(&run_dir)?;
    let request_path = match request {
        WorkerRequest::Review { .. } => run_dir.join("request.json"),
        WorkerRequest::Fix { .. } => run_dir.join("fix-request.json"),
    };
    storage::write_json(&request_path, request)?;
    let stdout = File::create(run_dir.join("worker.stdout.log"))?;
    let stderr = File::create(run_dir.join("worker.stderr.log"))?;
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .args([
            "internal",
            "worker",
            request_path.to_str().context("invalid request path")?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    command.process_group(0);
    // Publish launch metadata before spawning: the worker owns subsequent
    // manifest writes, so a fast worker cannot be overwritten by its parent.
    let mut manifest = load_manifest(run_id)?;
    manifest.pid = None;
    manifest.request_path = request_path;
    let launched_at = Utc::now();
    manifest.heartbeat_at = Some(launched_at);
    manifest.updated_at = launched_at;
    save_manifest(&manifest)?;
    drop(launch_lock);
    let mut child = command.spawn()?;
    let pid = child.id().context("worker pid unavailable")?;
    // Do not expose a cancellable run until the worker has published its PID.
    // A very fast worker may already have published a terminal result instead.
    let startup = Instant::now();
    loop {
        let current = load_manifest(run_id)?;
        if current.pid == Some(pid)
            || (current.state.terminal() && current.updated_at > launched_at)
        {
            break;
        }
        if child.try_wait()?.is_some() {
            mark_failed(
                run_id,
                &anyhow::anyhow!(
                    "detached worker exited before startup; inspect worker.stderr.log"
                ),
            );
            break;
        }
        if startup.elapsed() >= Duration::from_secs(10) {
            child.kill().await?;
            let error = anyhow::anyhow!("detached worker did not acknowledge startup");
            mark_failed(run_id, &error);
            return Err(error);
        }
        sleep(Duration::from_millis(20)).await;
    }
    if json {
        println!(
            "{}",
            serde_json::json!({"run_id": run_id, "pid": pid, "detached": true})
        );
    } else {
        println!("{run_id}");
    }
    Ok(0)
}

pub async fn status_command(args: RunIdArgs) -> Result<i32> {
    let manifest = load_manifest(&args.run_id)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    } else {
        print_manifest(&manifest);
    }
    terminal_exit_code(&manifest)
}

pub async fn follow_command(args: FollowArgs) -> Result<i32> {
    let mut last_state = None;
    loop {
        let manifest = load_manifest(&args.run_id)?;
        if args.json {
            println!("{}", serde_json::to_string(&manifest)?);
        } else if last_state != Some(manifest.state) {
            println!(
                "{}  {:?}{}",
                manifest.updated_at.to_rfc3339(),
                manifest.state,
                if manifest.degraded { " (degraded)" } else { "" }
            );
            last_state = Some(manifest.state);
        }
        if manifest.state.terminal() {
            return terminal_exit_code(&manifest);
        }
        sleep(Duration::from_secs(args.interval)).await;
    }
}

pub async fn report_command(args: RunIdArgs) -> Result<i32> {
    let manifest = load_manifest(&args.run_id)?;
    let path = manifest
        .report_path
        .clone()
        .context("run has no report yet")?;
    let report = fs::read_to_string(path)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"run_id": args.run_id, "report": report})
            )?
        );
    } else {
        print!("{report}");
    }
    terminal_exit_code(&manifest)
}

pub async fn cancel_command(args: RunIdArgs) -> Result<i32> {
    let mut manifest = load_manifest(&args.run_id)?;
    if let Some(pid) = manifest.pid {
        let pid = Pid::from_raw(pid as i32);
        if killpg(pid, Signal::SIGTERM).is_err() {
            let _ = kill(pid, Signal::SIGTERM);
        }
    }
    let claude_session = storage::run_dir(&args.run_id)?.join("providers/claude/session-id");
    if claude_session.exists()
        && let Ok(session_id) = fs::read_to_string(claude_session)
        && let Ok(config) = Config::load()
        && let Some(adapter) =
            crate::provider::discover(&config, crate::model::ProviderKind::Claude).await
    {
        let _ = Command::new(adapter.binary)
            .args(["stop", session_id.trim()])
            .output()
            .await;
    }
    manifest.state = RunState::Cancelled;
    manifest.pid = None;
    manifest.updated_at = Utc::now();
    manifest.error = Some("cancelled by user".into());
    save_manifest(&manifest)?;
    println!("{} cancelled", args.run_id);
    Ok(0)
}

pub async fn resume_command(args: ResumeArgs) -> Result<i32> {
    let manifest = load_manifest(&args.run_id)?;
    if matches!(
        manifest.state,
        RunState::AwaitingApproval | RunState::Completed
    ) {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        } else {
            print_manifest(&manifest);
        }
        return terminal_exit_code(&manifest);
    }
    let mut request: WorkerRequest = storage::read_json(&manifest.request_path)?;
    if let WorkerRequest::Review { args: review, .. } = &mut request {
        review.json |= args.json;
    }
    if args.detach {
        create_detached(&request, &args.run_id, args.json).await
    } else {
        match request {
            WorkerRequest::Review { run_id, args } => run_review_pipeline(&run_id, &args).await,
            WorkerRequest::Fix {
                run_id,
                only,
                exclude,
                leader,
            } => run_fix_pipeline(&run_id, &only, &exclude, &leader).await,
        }
    }
}

pub async fn runs_command(args: RunsArgs) -> Result<i32> {
    let mut manifests = Vec::new();
    for entry in fs::read_dir(storage::runs_root()?)? {
        let entry = entry?;
        let path = entry.path().join("manifest.json");
        if path.exists()
            && let Ok(manifest) = storage::read_json::<RunManifest>(&path)
        {
            manifests.push(manifest);
        }
    }
    manifests.sort_by_key(|manifest| std::cmp::Reverse(manifest.created_at));
    if args.json {
        println!("{}", serde_json::to_string_pretty(&manifests)?);
    } else {
        for manifest in manifests {
            println!(
                "{}  {:?}  {}",
                manifest.id,
                manifest.state,
                manifest
                    .target
                    .as_ref()
                    .map(|target| target.title.as_str())
                    .unwrap_or("pending")
            );
        }
    }
    Ok(0)
}

pub async fn fix_command(args: FixArgs) -> Result<i32> {
    let run_dir = storage::run_dir(&args.run_id)?;
    let mut manifest = load_manifest(&args.run_id)?;
    if !matches!(
        manifest.state,
        RunState::AwaitingApproval | RunState::FixIncomplete
    ) {
        anyhow::bail!(
            "run must be awaiting approval before fix; current state is {:?}",
            manifest.state
        );
    }
    let request = WorkerRequest::Fix {
        run_id: args.run_id.clone(),
        only: args.only.clone(),
        exclude: args.exclude.clone(),
        leader: args.leader.clone(),
    };
    let request_path = run_dir.join("fix-request.json");
    storage::write_json(&request_path, &request)?;
    manifest.request_path = request_path;
    save_manifest(&manifest)?;
    if args.detach {
        create_detached(&request, &args.run_id, args.json).await
    } else {
        run_fix_pipeline(&args.run_id, &args.only, &args.exclude, &args.leader).await
    }
}

pub async fn internal_command(args: InternalArgs) -> Result<i32> {
    match args.command {
        InternalCommand::Worker { request } => {
            let request: WorkerRequest = storage::read_json(&request)?;
            match request {
                WorkerRequest::Review { run_id, args } => run_review_pipeline(&run_id, &args).await,
                WorkerRequest::Fix {
                    run_id,
                    only,
                    exclude,
                    leader,
                } => run_fix_pipeline(&run_id, &only, &exclude, &leader).await,
            }
        }
        InternalCommand::ClaudeHook { output, failure: _ } => {
            let mut input = Vec::new();
            std::io::stdin().read_to_end(&mut input)?;
            storage::atomic_write(&output, &input)?;
            Ok(0)
        }
    }
}

pub async fn install_skill_command(args: InstallSkillArgs) -> Result<i32> {
    anyhow::ensure!(
        !(args.easy_mode && args.ultra_mode),
        "--easy-mode and --ultra-mode cannot be used together"
    );
    let home = dirs::home_dir().context("home directory unavailable")?;
    let skill_name = if args.easy_mode {
        "triad-easy"
    } else if args.ultra_mode {
        "triad-ultra"
    } else {
        "triad"
    };
    let mut targets = Vec::new();
    match args.host {
        SkillHost::Codex => targets.push((home.join(".codex/skills").join(skill_name), true)),
        SkillHost::Claude => targets.push((home.join(".claude/skills").join(skill_name), false)),
        SkillHost::Kimi => targets.push((home.join(".kimi-code/skills").join(skill_name), false)),
        SkillHost::All => {
            targets.push((home.join(".codex/skills").join(skill_name), true));
            targets.push((home.join(".claude/skills").join(skill_name), false));
            targets.push((home.join(".kimi-code/skills").join(skill_name), false));
        }
    }
    for (target, codex) in &targets {
        println!("would install {}", target.join("SKILL.md").display());
        if *codex {
            println!(
                "would install {}",
                target.join("agents/openai.yaml").display()
            );
        }
    }
    if !args.yes {
        println!("No changes made. Re-run with --yes.");
        return Ok(2);
    }
    let skill = r#"---
name: triad
description: Run subscription-backed frontier-model MapReduce reviews with Triad. Use for large or complex code and PR reviews, adversarial cross-model analysis, CI dry runs, run monitoring, or an explicitly approved isolated fix.
---

# Triad

Use the installed `triad` CLI from the target Git repository to make the most of the frontier-model intelligence available through existing subscriptions. Combine independent perspectives on one change with a separate verification pass; more models do not guarantee a correct conclusion.

## MapReduce contract

```text
Same Git change -> independent reviewers in parallel -> leader verification -> report
Report -> separate user approval -> isolated patch and test results
```

- Map: Let Triad launch at most one reviewer per runnable provider: up to six reviewers across five vendor subscriptions, including both ZCode models in parallel. Each gets the same complete change and its own disposable snapshot, with a different review focus. Do not split files between models or launch extra reviewers to accumulate votes.
- Reduce: Treat Map outputs as claims, not votes. The leader must inspect the code independently, validate each reachable trigger and consequence, deduplicate overlapping claims, and classify findings as `accepted`, `needs-human`, or `rejected`. Agreement between reviewers is supporting context, never proof.
- Act: Present the report before any fix. Only a separately approved `triad fix <run-id>` processes accepted findings in another disposable snapshot. Distinguish agent-reported test results from tests independently run and observed.

## Model team

Triad's built-in subscription model defaults are listed below. Respect explicit provider/model overrides and report the actual models from the run manifest; do not silently substitute API-backed or weaker models:

- Claude: `claude-fable-5-1` for architecture and data flow.
- Codex: `gpt-6-astra` with `max` effort and Standard processing (Fast mode disabled) for correctness and concurrency.
- Kimi: `kimi-code/k3` for regressions and API contracts.
- Cursor: `grok-4.7-fast`, resolved to `grok-4.7-high-fast`, for adversarial and cross-file analysis.
- ZCode (`zcode`): `GLM-5.3` for cross-file contracts and state transitions.
- ZCode Flash (`zcode_flash`): `GLM-5.3-Flash` for edge cases and simple regressions.

Both ZCode reviewers use the native Z.ai Coding Plan login and remain parallel reviewers in Default, Easy, and Ultra presets. On macOS, the official bundled CLI discovery candidate is `/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs`; discovery does not prove a live authenticated model call. `GLM-5.3-Flash` is the official model name: do not substitute FlashX or treat Instant as a separately verified model.

## Lazy-senior review policy

- Optimize for shipping safe, understandable code, not for ideal architecture.
- Focus on changes that materially affect users, correctness, security, reliability, code quality, or objective readability and maintainability.
- Do not demand broad refactors, redesigns, abstractions, deduplication, cleanup, renaming, formatting, or extra tests merely for elegance, personal preference, or textbook DRY. Small local duplication is often cheaper than a speculative abstraction.
- Respect the repository's current architecture and local conventions. Prefer the smallest local fix that resolves a proven impact.
- A readability finding must identify concrete obscured behavior or maintenance risk. If the code can safely ship as written, return no finding.
- Codex gets an additional YAGNI gate: hypothetical reuse, scale, consistency, flexibility, and pattern purity are not findings; prefer an existing path, a direct guard, deletion, small duplication, or no change.

## Review

- Select the target that matches the request: a PR number or URL, `--base REF`, `--commit SHA`, or `--uncommitted`.
- Use `--providers auto --leader auto` unless the user pins providers or a leader.
- Use `--require-all` only when the user explicitly requires every selected provider. Otherwise allow quota or availability failures to produce clearly reported degraded coverage.
- ZCode `subscription_pending` means its native Coding Plan-only route passed local checks, not that login is verified. Its real review task confirms login; never send `/model list` or another prompt as an authentication probe. ZCode is review/reduce-only; use another provider for separately approved fixes.
- Let Triad handle observed quota state, cooldowns, and reset times. An `unknown` balance can be runnable; do not invent remaining usage, make separate model-call probes, or bypass an exhausted pinned leader by silently choosing another provider.
- For a long interactive review, run `triad review ... --detach --json`, capture the run ID, monitor it with `triad status <run-id> --json` or `triad follow <run-id> --json`, and present `triad report <run-id>` once review reaches `awaiting_approval`. If the run fails or is cancelled, report that state and any available partial findings.
- For CI or a report-only check, run `triad review ... --dry-run --json`. Exit `0` means no accepted or needs-human findings, `2` means blocking findings, and `3` means a selected provider, reducer, or protocol failure. Add `--require-all` only when missing optional providers must fail CI.
- `--dry-run` still calls models, consumes subscription quota, and saves run artifacts. It cannot be combined with `--detach` or followed by `triad fix`; use a normal review when an approved fix may follow. For zero-model-call pipeline validation, run `cargo test --test e2e_fake` from a Triad source checkout instead.
- Use `triad doctor --refresh --json` when the user asks about authentication/availability or a provider fails discovery. It performs status checks, not model-call probes.
- In the final response, state the reviewed base/head revisions, participating and skipped providers with reasons, degraded coverage, actual leader/model, verdicts, and the report path or run ID. `awaiting_approval` means the review is ready for the user, not that a fix was made. Do not present an active or failed run as a completed review.

## Safety and approval

- Keep subscription login only. Never introduce vendor API keys, API billing, automatic overage, Claude `-p`, Agent SDK, or ultrareview.
- Never install a provider, start an interactive login, enable a disabled provider, or change account settings without explicit user approval.
- Reviewers and the reducer are passive: no edits, deletes, commits, pushes, branches, tags, GitHub comments or reviews, deployments, or external messages. They may inspect code and run existing local tests only inside disposable snapshots.
- Show the completed report before any fix. Call `triad fix <run-id>` only after a separate explicit user approval of the patch stage.
- A Triad fix only prepares an isolated patch and test results. Do not apply it to the source checkout, commit, push, or post externally unless the user separately asks for that action.
"#;
    let openai_yaml = r#"interface:
  display_name: "Triad"
  short_description: "Run frontier-model MapReduce code reviews"
  default_prompt: "Use $triad to run a high-signal lazy-senior MapReduce review with every available subscription-backed frontier model. Prefer safe minimal changes over refactors, and present the verified report without making changes."
policy:
  allow_implicit_invocation: true
"#;
    let (skill, openai_yaml) = if args.easy_mode {
        (
            r#"---
name: triad-easy
description: Run Triad Easy subscription-backed MapReduce reviews with Claude Opus 5.5 and GPT-6.1 Sol. Use when the user asks for Triad Easy or an easy-mode cross-model code review, provider check, or run follow-up.
---

# Triad Easy

Use the installed `triad` CLI in the target Git repository. This is a standalone skill; the regular `triad` skill is not required.

## Mode and models

- Always add `--easy-mode` to `review`, `providers`, and `doctor`. It selects Claude `claude-opus-5-5` and Codex `gpt-6.1-sol` for this run without changing saved configuration. Kimi, Cursor, and both ZCode reviewers keep their configured models and all providers keep their enabled/disabled settings.
- The model team has up to six parallel reviewers across five vendor subscriptions. ZCode uses native Z.ai Coding Plan login for `zcode` (`GLM-5.3`, cross-file contracts and state) and `zcode_flash` (`GLM-5.3-Flash`, edge cases and simple regressions). The macOS bundled CLI discovery candidate is `/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs`; discovery is not proof of a live authenticated call. Do not substitute FlashX or treat Instant as a separately verified model.
- Codex keeps its configured reasoning effort (default `max`) and Standard processing (Fast mode disabled). Easy mode is a model preset, not a promise of lower cost, latency, or reasoning effort.
- If the CLI does not support `--easy-mode`, report that Triad needs updating; never silently run the normal model preset instead.
- `resume`, `status`, `follow`, `report`, and `fix` take the run ID, not `--easy-mode`. Resume and fix inherit the saved mode. Report actual models from the manifest and disclose any mismatch or degraded coverage.

## MapReduce review

- Choose the requested PR number/URL, `--base REF`, `--commit SHA`, or `--uncommitted`. Default to `--providers auto --leader auto`; respect user-pinned providers and leaders. Use `--require-all` only if requested.
- Start a long review with `triad review <target/options> --easy-mode --providers auto --leader auto --detach --json`. Capture its run ID; monitor with `triad status <run-id> --json` or `triad follow <run-id> --json`.
- Each runnable provider gets the same full change in its own disposable snapshot. Do not split files among models or add extra reviewers for voting. The reducer independently verifies claims, deduplicates them, and classifies `accepted`, `needs-human`, and `rejected`; agreement is not proof.
- Follow a lazy-senior policy: report concrete correctness, security, user-impact, or objective maintainability problems. Prefer small local fixes; do not demand speculative refactors, abstractions, cleanup, or textbook DRY. Safe code may need no findings.
- Present `triad report <run-id>` when the run reaches `awaiting_approval`, including exact base/head revisions, participating/skipped providers and reasons, actual leader/models, verdicts, and report path. Do not present active or failed runs as completed reviews. Distinguish agent-reported tests from independently observed tests.
- CI/report-only: `triad review <target/options> --easy-mode --dry-run --json`. Exit 0 means no accepted/needs-human findings, 2 blocking findings, and 3 provider/reducer/protocol failure. Dry runs still call models and consume quota; they cannot use `--detach` or be followed by a fix. Zero-model-call validation uses `cargo test --test e2e_fake` from Triad source.
- For requested availability/auth checks, use `triad doctor --easy-mode --refresh --json` or `triad providers --easy-mode --json`. Do not make model-call probes or invent remaining usage. Let the scheduler enforce cooldowns and pinned-leader failures.

## Safety and approval

- Subscription login only: no vendor API keys, API billing, automatic overage, Claude `-p`, Agent SDK, or ultrareview. Do not install providers, start interactive logins, enable disabled providers, or change account settings without explicit approval.
- Reviewers and the reducer are passive: inspect code and run existing local tests only in disposable snapshots. No edits/deletes, commits, pushes, branches/tags, GitHub comments/reviews, deployments, or external messages.
- Stop after presenting the report. Run `triad fix <run-id>` only after separate explicit user approval. It prepares an isolated patch and test results; applying the patch, committing, pushing, or publishing needs separate authorization.
"#,
            r#"interface:
  display_name: "Triad Easy"
  short_description: "MapReduce reviews with Opus 5.5 and GPT-6.1 Sol"
  default_prompt: "Use $triad-easy to run a passive lazy-senior MapReduce review with --easy-mode, then present the verified report without making changes."
policy:
  allow_implicit_invocation: true
"#,
        )
    } else if args.ultra_mode {
        (
            r#"---
name: triad-ultra
description: Run Triad Ultra subscription-backed MapReduce reviews with Claude Opus 5.5 Ultracode and GPT-6 Astra ultra reasoning plus Fast. Use when the user asks for Triad Ultra, Triade Ultra, or an Ultra-mode cross-model review, provider check, or run follow-up.
---

# Triad Ultra

Use the installed `triad` CLI in the target Git repository. This skill is standalone; the regular and Easy skills are not required.

## Mode and models

- Always add `--ultra-mode` to `review`, `providers`, and `doctor`. Never combine it with `--easy-mode` or silently fall back to another preset. Check `triad review --help` if compatibility is uncertain; a missing flag means the local Triad CLI needs updating, not that a provider login is required.
- Claude: `claude-opus-5-5` with Ultracode (`--effort ultracode`, xhigh reasoning). Claude Fast remains disabled. Keep the subscription-backed interactive background adapter; never replace it with Claude `-p`, Agent SDK, or ultrareview.
- Codex: `gpt-6-astra`, reasoning `ultra`, and ordinary Fast processing (`service_tier="fast"`). Fast is not Ultrafast; do not substitute that service tier.
- This preset is run-local and does not overwrite saved configuration. Kimi, Cursor, both ZCode reviewers, and every provider's enabled/disabled settings remain unchanged. Up to six reviewer slots may run in parallel.
- `resume`, `status`, `follow`, `report`, and `fix` take the run ID, not `--ultra-mode`; they use the saved run's mode. Report actual models and degraded coverage from the manifest.
- Claude Ultra reviewers/reducer expose only Read, Glob, Grep, and Workflow: do not promise shell execution or unit tests from them. Ultracode attestation proves the mode and a successful Workflow launch, not independently verified success of every child check.

## MapReduce review

- Choose the requested PR number/URL, `--base REF`, `--commit SHA`, or `--uncommitted`. Default to `--providers auto --leader auto`, preserving explicit user selections. Use `--require-all` only when requested.
- Start a long review with `triad review <target/options> --ultra-mode --providers auto --leader auto --detach --json`. Capture the run ID; monitor `triad status <run-id> --json` or `triad follow <run-id> --json` until terminal.
- Every reviewer receives the same complete change in a separate disposable snapshot. Do not shard files or add reviewers for majority voting. The reducer independently checks evidence, reachability, and impact, deduplicates claims, and classifies `accepted`, `needs-human`, and `rejected`.
- Follow the lazy-senior policy: prioritize concrete user-impact, correctness, security, reliability, and objective maintainability problems. Prefer the smallest local fix; do not demand speculative abstractions, broad refactors, cleanup, or textbook DRY. Safe code may produce no findings.
- Present `triad report <run-id>` at `awaiting_approval`, including exact base/head revisions, participating and skipped providers, reasons for degraded coverage, actual leader/models, verdicts, and report path. Active/failed runs are not completed reviews. Distinguish agent-reported tests from independently observed tests.
- For CI/report-only checks, use `triad review <target/options> --ultra-mode --dry-run --json`. Exit 0 means no accepted/needs-human findings, 2 blocking findings, and 3 provider/reducer/protocol failure. Dry runs still call models and consume quota; they cannot use `--detach` or be followed by a fix. Use fake E2E tests from Triad source for zero-model-call validation.
- For requested availability checks, use `triad doctor --ultra-mode --refresh --json` or `triad providers --ultra-mode --json`. Never make separate model-call probes or invent remaining quota. Preserve cooldowns and fail-closed pinned-leader behavior.
- ZCode `subscription_pending` means only its Coding Plan-only route was validated locally; login is checked on a real task. Never use `/model list` as an auth probe. GLM supports review/reduce, not fixes; another provider must handle separately approved fixes.

## Safety and approval

- Subscription login only: no vendor API keys, API billing, or automatic overage. Do not install providers, start interactive logins, enable disabled providers, or change account settings without explicit approval.
- Reviewers and the reducer remain passive. No edits/deletes, commits, pushes, branches/tags, GitHub comments/reviews, deployments, or external messages. Inspect only disposable snapshots; existing local tests are allowed only where the adapter's permissions support them.
- Stop after the report. Run `triad fix <run-id>` only after separate explicit user approval. It prepares an isolated patch and test results; applying, committing, pushing, or publishing requires separate authorization.
"#,
            r#"interface:
  display_name: "Triad Ultra"
  short_description: "Opus Ultracode + Astra Ultra reasoning with Fast"
  default_prompt: "Use $triad-ultra to run a passive MapReduce review with --ultra-mode, then present the verified report without making changes."
policy:
  allow_implicit_invocation: true
"#,
        )
    } else {
        (skill, openai_yaml)
    };
    for (target, codex) in targets {
        fs::create_dir_all(&target)?;
        storage::atomic_write(&target.join("SKILL.md"), skill.as_bytes())?;
        if codex {
            let agents_dir = target.join("agents");
            fs::create_dir_all(&agents_dir)?;
            storage::atomic_write(&agents_dir.join("openai.yaml"), openai_yaml.as_bytes())?;
        }
    }
    Ok(0)
}

async fn run_review_pipeline(run_id: &str, args: &ReviewArgs) -> Result<i32> {
    match run_review_pipeline_inner(run_id, args).await {
        Ok(code) => Ok(code),
        Err(error) => {
            if !error.to_string().contains("already active") {
                mark_failed(run_id, &error);
            }
            Err(error)
        }
    }
}

fn reviewer_checkpoint(
    run_dir: &Path,
    manifest: &RunManifest,
) -> Result<Option<Vec<(ProviderKind, String)>>> {
    let path = run_dir.join("provider-results.json");
    if manifest.target.is_none() || !path.exists() {
        return Ok(None);
    }
    let saved: BTreeMap<String, String> = storage::read_json(&path)?;
    let mut outputs = Vec::new();
    for record in manifest.providers.iter().filter(|record| record.selected) {
        if record.status == "completed" {
            let text = saved.get(record.provider.as_str()).with_context(|| {
                format!(
                    "review checkpoint missing {} result; refusing to repeat completed reviewers",
                    record.provider
                )
            })?;
            outputs.push((record.provider, text.clone()));
        } else if matches!(record.status.as_str(), "queued" | "running") {
            anyhow::bail!(
                "review checkpoint is incomplete; start a new review instead of mixing attempts"
            );
        }
    }
    anyhow::ensure!(
        !outputs.is_empty(),
        "review checkpoint has no completed reviewers"
    );
    Ok(Some(outputs))
}

async fn with_heartbeat<T>(
    manifest: &mut RunManifest,
    future: impl Future<Output = T>,
) -> Result<T> {
    tokio::pin!(future);
    let mut interval = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            value = &mut future => return Ok(value),
            _ = interval.tick() => {
                manifest.heartbeat_at = Some(Utc::now());
                save_manifest(manifest)?;
            }
        }
    }
}

fn checkpoint_provider_arg(requested: &str, providers: &[ProviderRunRecord]) -> String {
    if requested != "auto" {
        return requested.to_string();
    }
    // Resume the original runnable cohort; providers added to the registry later
    // and records that were deliberately skipped must not become requirements.
    providers
        .iter()
        .filter(|record| record.selected)
        .map(|record| record.provider.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn checkpoint_coverage_degraded(providers: &[ProviderRunRecord]) -> bool {
    // The persisted records are the expected coverage for this historical run.
    // Comparing their length with today's registry changes old reports on resume.
    providers
        .iter()
        .any(|record| !record.selected || record.status != "completed")
}

async fn run_review_pipeline_inner(run_id: &str, args: &ReviewArgs) -> Result<i32> {
    let run_dir = storage::run_dir(run_id)?;
    let _lock = RunLock::acquire(&run_dir)?;
    let previous = load_manifest(run_id)?;
    let cached_outputs = reviewer_checkpoint(&run_dir, &previous)?;
    let resume_reduce = cached_outputs.is_some();
    update_state(run_id, RunState::Discovering, None)?;
    let config = Config::load()?.with_modes(args.easy_mode, args.ultra_mode)?;
    let provider_arg = if resume_reduce {
        checkpoint_provider_arg(&args.providers, &previous.providers)
    } else {
        args.providers.clone()
    };
    let (adapters, statuses) = scheduler::select(&config, &provider_arg, args.require_all).await?;
    let target = if resume_reduce {
        previous
            .target
            .clone()
            .context("checkpoint target missing")?
    } else {
        git::resolve_target(args, &run_dir).await?
    };
    let context_snapshot = run_dir.join("snapshots/context");
    if !resume_reduce || !context_snapshot.exists() {
        // A saved context is also the immutable source for an uncommitted
        // review. Do not silently recapture a changed checkout on resume.
        anyhow::ensure!(
            !resume_reduce || !target.uncommitted,
            "saved uncommitted review context is missing; start a new review"
        );
        git::create_snapshot(
            &target,
            &context_snapshot,
            "Triad is preparing review context.",
        )
        .await?;
    }
    let diff = git::diff_for_target(&context_snapshot, &target).await?;
    let diff_path = run_dir.join("review.diff");
    storage::atomic_write(&diff_path, &diff)?;
    let stat = git::diff_stat(&context_snapshot, &target)
        .await
        .unwrap_or_default();
    let context_markdown = format!(
        "# Triad review context\n\nTarget: {}\nBase: `{}`\nHead: `{}`\n\n## Diff stat\n\n```\n{}\n```\n\nThe complete diff is available through `git diff {} {}` in this disposable checkout.\n",
        target.title, target.base_sha, target.head_sha, stat, target.base_sha, target.head_sha
    );
    let reviewer_schema = run_dir.join("reviewer.schema.json");
    report::write_reviewer_schema(&reviewer_schema)?;

    let selected_set: HashSet<_> = adapters.iter().map(|adapter| adapter.kind).collect();
    let mut manifest = load_manifest(run_id)?;
    manifest.target = Some(target.clone());
    manifest.report_path = None;
    let mut outputs = cached_outputs.unwrap_or_default();
    let mut degraded = if resume_reduce {
        checkpoint_coverage_degraded(&previous.providers)
    } else {
        selected_set.len() < ProviderKind::ALL.len()
    };
    let mut provider_summaries;
    if resume_reduce {
        manifest.providers = previous.providers;
        provider_summaries = manifest
            .providers
            .iter()
            .map(|record| {
                (
                    record.provider,
                    record
                        .error
                        .clone()
                        .unwrap_or_else(|| record.status.clone()),
                )
            })
            .collect::<Vec<_>>();
    } else {
        // Fail a pinned, unavailable reducer before spending any reviewer quota.
        scheduler::choose_leader(
            &args.leader,
            &selected_set.iter().copied().collect::<Vec<_>>(),
            &config,
        )?;
        manifest.providers = statuses
            .iter()
            .map(|status| ProviderRunRecord {
                provider: status.provider,
                selected: selected_set.contains(&status.provider),
                skipped_reason: if selected_set.contains(&status.provider) {
                    None
                } else {
                    Some(format!("auth={:?}, usage={:?}", status.auth, status.usage))
                },
                model: status.model.clone(),
                version: status.version.clone(),
                auth_source: format!("{:?}", status.auth),
                usage_source: status.usage_source.clone(),
                session_id: None,
                status: if selected_set.contains(&status.provider) {
                    "queued".into()
                } else {
                    "skipped".into()
                },
                error: None,
                protocol_violation: false,
            })
            .collect();
        manifest.state = RunState::Mapping;
        manifest.updated_at = Utc::now();
        manifest.heartbeat_at = Some(Utc::now());
        save_manifest(&manifest)?;

        let mut jobs = FuturesUnordered::new();
        for adapter in adapters {
            let snapshot = run_dir.join("snapshots").join(adapter.kind.as_str());
            git::create_snapshot(&target, &snapshot, &context_markdown).await?;
            provider::prepare_snapshot(adapter.kind, AgentRole::Reviewer, &snapshot)?;
            let baseline = git::status_signature(&snapshot).await?;
            let context = ProviderContext {
                role: AgentRole::Reviewer,
                snapshot: snapshot.clone(),
                run_dir: run_dir.clone(),
                prompt: report::reviewer_prompt(
                    adapter.kind,
                    &target.base_sha,
                    &target.head_sha,
                    target.uncommitted,
                ),
                schema_path: reviewer_schema.clone(),
                timeout: Duration::from_secs(config.reviewer_timeout_minutes * 60),
            };
            jobs.push(async move {
                let result = adapter.run(&context).await;
                let after = git::status_signature(&snapshot).await;
                let violation = after
                    .as_ref()
                    .map(|after| after != &baseline)
                    .unwrap_or(true);
                (adapter, result, violation)
            });
        }
        for record in &mut manifest.providers {
            if record.selected {
                record.status = "running".into();
            }
        }
        manifest.updated_at = Utc::now();
        save_manifest(&manifest)?;
        provider_summaries = manifest
            .providers
            .iter()
            .filter(|record| !record.selected)
            .map(|record| {
                (
                    record.provider,
                    format!(
                        "skipped: {}",
                        record.skipped_reason.as_deref().unwrap_or("not selected")
                    ),
                )
            })
            .collect::<Vec<_>>();
        while let Some((adapter, result, violation)) =
            with_heartbeat(&mut manifest, jobs.next()).await?
        {
            let mut ledger = ProviderLedger::load()?;
            let record = manifest
                .providers
                .iter_mut()
                .find(|record| record.provider == adapter.kind)
                .unwrap();
            match result {
                Ok(output) if !violation => {
                    scheduler::record_success(&mut ledger, adapter.kind);
                    record.session_id = output.session_id.clone();
                    record.model = output.model.clone();
                    if adapter.kind.is_zcode() {
                        record.auth_source =
                            "subscription (native Coding Plan model attested)".into();
                    }
                    match report::parse_findings(&output.text) {
                        Ok(_) => {
                            record.status = "completed".into();
                            provider_summaries.push((adapter.kind, "completed".into()));
                            outputs.push((adapter.kind, output.text));
                        }
                        Err(error) => {
                            degraded = true;
                            record.status = "malformed".into();
                            let message = format!("malformed reviewer output: {error}");
                            record.error = Some(message.clone());
                            provider_summaries.push((adapter.kind, message));
                        }
                    }
                }
                Ok(_) => {
                    scheduler::record_success(&mut ledger, adapter.kind);
                    degraded = true;
                    record.status = "protocol_violation".into();
                    record.protocol_violation = true;
                    record.error =
                        Some("reviewer changed its disposable snapshot; result discarded".into());
                    provider_summaries.push((adapter.kind, "discarded: protocol violation".into()));
                }
                Err(error) => {
                    scheduler::record_failure(&mut ledger, &error, config.cooldown_minutes);
                    degraded = true;
                    record.status = "failed".into();
                    record.error = Some(error.message.clone());
                    provider_summaries.push((adapter.kind, format!("failed: {}", error.message)));
                }
            }
            manifest.updated_at = Utc::now();
            manifest.heartbeat_at = Some(Utc::now());
            manifest.degraded = degraded;
            save_manifest(&manifest)?;
            ledger.save()?;
        }
    }
    if outputs.is_empty() {
        manifest.state = RunState::Failed;
        manifest.pid = None;
        manifest.error = Some("all reviewers failed".into());
        manifest.degraded = true;
        save_manifest(&manifest)?;
        return Ok(3);
    }

    // Save successful Map results before any fallible reduce setup. Resume
    // reuses this checkpoint and the original SHAs, not the current PR/HEAD.
    let provider_results_path = run_dir.join("provider-results.json");
    report::write_provider_results(&provider_results_path, &outputs)?;
    let successful: Vec<_> = outputs
        .iter()
        .map(|(provider, _)| *provider)
        .filter(|provider| selected_set.contains(provider))
        .collect();
    let leader = scheduler::choose_leader(&args.leader, &successful, &config)?;
    manifest.leader = Some(leader);
    manifest.state = RunState::Reducing;
    manifest.degraded = degraded;
    manifest.updated_at = Utc::now();
    save_manifest(&manifest)?;
    let reducer_schema = run_dir.join("reducer.schema.json");
    report::write_reducer_schema(&reducer_schema)?;
    let reducer_snapshot = run_dir.join("snapshots/reducer");
    let mut reducer_target = target.clone();
    reducer_target.source_repo = context_snapshot;
    git::create_snapshot(&reducer_target, &reducer_snapshot, &context_markdown).await?;
    provider::prepare_snapshot(leader, AgentRole::Reducer, &reducer_snapshot)?;
    report::install_context(&reducer_snapshot, Some(&provider_results_path))?;
    let reducer_baseline = git::status_signature(&reducer_snapshot).await?;
    let reducer_adapter = provider::for_kind(&config, leader).await?;
    let reducer_context = ProviderContext {
        role: AgentRole::Reducer,
        snapshot: reducer_snapshot,
        run_dir: run_dir.clone(),
        prompt: report::reducer_prompt(
            leader,
            &target.base_sha,
            &target.head_sha,
            target.uncommitted,
        ),
        schema_path: reducer_schema,
        timeout: Duration::from_secs(config.reducer_timeout_minutes * 60),
    };
    let reducer_result =
        with_heartbeat(&mut manifest, reducer_adapter.run(&reducer_context)).await?;
    let mut ledger = ProviderLedger::load()?;
    let reduction = match reducer_result {
        Ok(output)
            if git::status_signature(&reducer_context.snapshot)
                .await
                .as_ref()
                .is_ok_and(|after| after == &reducer_baseline) =>
        {
            match report::parse_reduction(&output.text) {
                Ok(reduction) => {
                    scheduler::record_success(&mut ledger, leader);
                    reduction
                }
                Err(error) => {
                    degraded = true;
                    manifest.error = Some(format!("malformed reducer output: {error}"));
                    report::fallback_reduction(&outputs)
                }
            }
        }
        Ok(_) => {
            degraded = true;
            manifest.error =
                Some("reducer changed its read-only snapshot; output discarded".into());
            report::fallback_reduction(&outputs)
        }
        Err(error) => {
            scheduler::record_failure(&mut ledger, &error, config.cooldown_minutes);
            degraded = true;
            manifest.error = Some(format!("reducer failed: {}", error.message));
            report::fallback_reduction(&outputs)
        }
    };
    ledger.save()?;
    let findings_path = run_dir.join("findings.json");
    storage::write_json(&findings_path, &reduction)?;
    let report_body = report::render_report(
        run_id,
        &target.title,
        leader,
        degraded,
        &provider_summaries,
        manifest.error.as_deref(),
        &reduction,
    );
    let report_path = run_dir.join("report.md");
    storage::atomic_write(&report_path, report_body.as_bytes())?;
    let blocking_findings = reduction
        .findings
        .iter()
        .filter(|finding| matches!(finding.verdict.as_str(), "accepted" | "needs-human"))
        .count();
    manifest.state = if manifest.error.is_some() {
        RunState::Failed
    } else if args.dry_run {
        RunState::Completed
    } else {
        RunState::AwaitingApproval
    };
    manifest.pid = None;
    manifest.degraded = degraded;
    manifest.report_path = Some(report_path.clone());
    manifest.updated_at = Utc::now();
    manifest.heartbeat_at = Some(Utc::now());
    save_manifest(&manifest)?;
    if args.dry_run && args.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "run_id": run_id,
                "state": manifest.state,
                "dry_run": true,
                "blocking_findings": blocking_findings,
                "degraded": degraded,
                "report": report_path
            }))?
        );
    } else if args.dry_run {
        println!(
            "Dry run {:?}: {run_id}\nBlocking findings: {blocking_findings}\nReport: {}",
            manifest.state,
            report_path.display()
        );
    } else if args.json {
        println!(
            "{}",
            serde_json::to_string(
                &serde_json::json!({"run_id": run_id, "state": manifest.state, "degraded": degraded, "error": manifest.error, "report": report_path})
            )?
        );
    } else if manifest.error.is_some() {
        println!(
            "Review incomplete: {run_id}\nPartial report: {}\nRun `triad resume {run_id}` to retry reduction without repeating completed reviewers.",
            report_path.display()
        );
    } else {
        println!(
            "Review complete: {run_id}\nReport: {}\nRun `triad fix {run_id}` only after approving the accepted findings.",
            report_path.display()
        );
    }
    terminal_exit_code(&manifest)
}

async fn run_fix_pipeline(
    run_id: &str,
    only: &[String],
    exclude: &[String],
    requested_leader: &str,
) -> Result<i32> {
    let current = load_manifest(run_id)?;
    if !matches!(
        current.state,
        RunState::AwaitingApproval | RunState::FixIncomplete
    ) {
        anyhow::bail!(
            "run must be awaiting approval before fix; current state is {:?}",
            current.state
        );
    }
    match run_fix_pipeline_inner(run_id, only, exclude, requested_leader).await {
        Ok(code) => Ok(code),
        Err(error) => {
            if !error.to_string().contains("already active") {
                mark_fix_incomplete(run_id, &error);
            }
            Err(error)
        }
    }
}

async fn run_fix_pipeline_inner(
    run_id: &str,
    only: &[String],
    exclude: &[String],
    requested_leader: &str,
) -> Result<i32> {
    let run_dir = storage::run_dir(run_id)?;
    let _lock = RunLock::acquire(&run_dir)?;
    let mut manifest = load_manifest(run_id)?;
    if !matches!(
        manifest.state,
        RunState::AwaitingApproval | RunState::FixIncomplete
    ) {
        anyhow::bail!(
            "run must be awaiting approval before fix; current state is {:?}",
            manifest.state
        );
    }
    manifest.pid = Some(std::process::id());
    manifest.heartbeat_at = Some(Utc::now());
    save_manifest(&manifest)?;
    let target = manifest.target.clone().context("run target missing")?;
    let reduction: ReductionEnvelope = storage::read_json(&run_dir.join("findings.json"))?;
    let only: HashSet<_> = only.iter().cloned().collect();
    let exclude: HashSet<_> = exclude.iter().cloned().collect();
    let selected: Vec<ReducedFinding> = reduction
        .findings
        .into_iter()
        .filter(|finding| {
            finding.verdict == "accepted"
                && (only.is_empty() || only.contains(&finding.id))
                && !exclude.contains(&finding.id)
        })
        .collect();
    if selected.is_empty() {
        anyhow::bail!("no accepted findings selected for fixing");
    }
    let config = Config::load()?.with_modes(manifest.easy_mode, manifest.ultra_mode)?;
    if requested_leader != "auto" && requested_leader.parse::<ProviderKind>()?.is_zcode() {
        anyhow::bail!(
            "ZCode supports passive reviews and reduction, not fixes; choose another --leader"
        );
    }
    let (available, _) = scheduler::select(&config, "auto", false).await?;
    let available_kinds: Vec<_> = available
        .iter()
        .map(|adapter| adapter.kind)
        .filter(|kind| !kind.is_zcode())
        .collect();
    anyhow::ensure!(
        !available_kinds.is_empty(),
        "no provider capable of safe fixes is available; ZCode is review-only"
    );
    let leader = if requested_leader == "auto" {
        manifest
            .leader
            .filter(|leader| available_kinds.contains(leader))
            .unwrap_or(scheduler::choose_leader("auto", &available_kinds, &config)?)
    } else {
        scheduler::choose_leader(requested_leader, &available_kinds, &config)?
    };
    manifest.state = RunState::Fixing;
    manifest.pid = Some(std::process::id());
    manifest.leader = Some(leader);
    manifest.updated_at = Utc::now();
    manifest.heartbeat_at = Some(Utc::now());
    save_manifest(&manifest)?;
    let context_markdown = format!(
        "# Triad fix context\n\nTarget: {}\nBase: `{}`\nHead: `{}`\nOnly approved findings in the prompt may be changed.\n",
        target.title, target.base_sha, target.head_sha
    );
    let snapshot = run_dir.join("snapshots/fix");
    git::create_snapshot(&target, &snapshot, &context_markdown).await?;
    let schema = run_dir.join("fixer.schema.json");
    report::write_fixer_schema(&schema)?;
    let adapter = provider::for_kind(&config, leader).await?;
    provider::prepare_snapshot(leader, AgentRole::Fixer, &snapshot)?;
    let context = ProviderContext {
        role: AgentRole::Fixer,
        snapshot: snapshot.clone(),
        run_dir: run_dir.clone(),
        prompt: report::fixer_prompt(leader, &selected)?,
        schema_path: schema,
        timeout: Duration::from_secs(config.fixer_timeout_minutes * 60),
    };
    let mut ledger = ProviderLedger::load()?;
    let output = match with_heartbeat(&mut manifest, adapter.run(&context)).await? {
        Ok(output) => {
            scheduler::record_success(&mut ledger, leader);
            output
        }
        Err(error) => {
            scheduler::record_failure(&mut ledger, &error, config.cooldown_minutes);
            ledger.save()?;
            manifest.state = RunState::FixIncomplete;
            manifest.pid = None;
            manifest.error = Some(error.message);
            manifest.degraded = true;
            save_manifest(&manifest)?;
            return Ok(3);
        }
    };
    ledger.save()?;
    manifest.state = RunState::Verifying;
    manifest.updated_at = Utc::now();
    save_manifest(&manifest)?;
    let patch = git::working_patch(&snapshot).await?;
    let patch_path = run_dir.join("fix.patch");
    storage::atomic_write(&patch_path, &patch)?;
    let tests_path = run_dir.join("tests.json");
    let parsed_tests = report::parse_fixer(&output.text);
    let verification_error = match &parsed_tests {
        Err(error) => Some(format!("malformed fixer test report: {error}")),
        Ok(tests) if tests["tests"].as_array().is_some_and(|tests| tests.iter().any(|test| test["status"] == "failed")) => Some("fixer produced a patch but reported failed tests; inspect tests.json before applying".into()),
        _ => None,
    };
    let tests = parsed_tests.unwrap_or_else(|_| serde_json::json!({"raw": output.text}));
    storage::write_json(&tests_path, &tests)?;
    manifest.state = if patch.is_empty() || verification_error.is_some() {
        RunState::FixIncomplete
    } else {
        RunState::Completed
    };
    manifest.patch_path = Some(patch_path.clone());
    manifest.pid = None;
    manifest.updated_at = Utc::now();
    manifest.heartbeat_at = Some(Utc::now());
    if patch.is_empty() {
        manifest.error = Some("fixer produced no patch".into());
    } else {
        manifest.error = verification_error;
    }
    save_manifest(&manifest)?;
    println!(
        "Fix run {:?}: {}\nPatch: {}\nSnapshot: {}",
        manifest.state,
        run_id,
        patch_path.display(),
        snapshot.display()
    );
    Ok(if manifest.state == RunState::Completed {
        0
    } else {
        3
    })
}

fn print_manifest(manifest: &RunManifest) {
    println!(
        "mode: {}",
        if manifest.ultra_mode {
            "ultra"
        } else if manifest.easy_mode {
            "easy"
        } else {
            "default"
        }
    );
    println!(
        "run: {}\nstate: {:?}\ndegraded: {}\nleader: {}\ntarget: {}",
        manifest.id,
        manifest.state,
        manifest.degraded,
        manifest
            .leader
            .map(|value| value.to_string())
            .unwrap_or_else(|| "pending".into()),
        manifest
            .target
            .as_ref()
            .map(|target| target.title.as_str())
            .unwrap_or("pending")
    );
    if let Some(error) = &manifest.error {
        println!("error: {error}");
    }
    if let Some(report) = &manifest.report_path {
        println!("report: {}", report.display());
    }
    if let Some(patch) = &manifest.patch_path {
        println!("patch: {}", patch.display());
    }
}

fn manifest_path(run_id: &str) -> Result<PathBuf> {
    Ok(storage::run_dir(run_id)?.join("manifest.json"))
}
fn load_manifest(run_id: &str) -> Result<RunManifest> {
    storage::read_json(&manifest_path(run_id)?)
}

fn terminal_exit_code(manifest: &RunManifest) -> Result<i32> {
    if manifest.dry_run {
        if !manifest.state.terminal() {
            return Ok(0);
        }
        if matches!(
            manifest.state,
            RunState::Failed | RunState::Cancelled | RunState::FixIncomplete
        ) || manifest.error.is_some()
            || manifest
                .providers
                .iter()
                .any(|provider| provider.selected && provider.status != "completed")
        {
            return Ok(3);
        }
        let findings: ReductionEnvelope =
            storage::read_json(&storage::run_dir(&manifest.id)?.join("findings.json"))?;
        let blocking = findings
            .findings
            .iter()
            .any(|finding| matches!(finding.verdict.as_str(), "accepted" | "needs-human"));
        return Ok(if blocking { 2 } else { 0 });
    }
    Ok(
        if matches!(
            manifest.state,
            RunState::Failed | RunState::Cancelled | RunState::FixIncomplete
        ) {
            3
        } else if manifest.degraded {
            2
        } else {
            0
        },
    )
}
fn save_manifest(manifest: &RunManifest) -> Result<()> {
    storage::write_json(&manifest_path(&manifest.id)?, manifest)
}
fn update_state(run_id: &str, state: RunState, error: Option<String>) -> Result<()> {
    let mut manifest = load_manifest(run_id)?;
    manifest.state = state;
    manifest.error = error;
    manifest.updated_at = Utc::now();
    manifest.heartbeat_at = Some(Utc::now());
    manifest.pid = Some(std::process::id());
    save_manifest(&manifest)
}

fn mark_failed(run_id: &str, error: &anyhow::Error) {
    if let Ok(mut manifest) = load_manifest(run_id) {
        manifest.state = RunState::Failed;
        manifest.error = Some(format!("{error:#}"));
        manifest.degraded = true;
        manifest.pid = None;
        manifest.updated_at = Utc::now();
        manifest.heartbeat_at = Some(Utc::now());
        let _ = save_manifest(&manifest);
    }
}

fn mark_fix_incomplete(run_id: &str, error: &anyhow::Error) {
    if let Ok(mut manifest) = load_manifest(run_id) {
        manifest.state = RunState::FixIncomplete;
        manifest.error = Some(format!("{error:#}"));
        manifest.degraded = true;
        manifest.pid = None;
        manifest.updated_at = Utc::now();
        manifest.heartbeat_at = Some(Utc::now());
        let _ = save_manifest(&manifest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed_record(provider: ProviderKind) -> ProviderRunRecord {
        ProviderRunRecord {
            provider,
            selected: true,
            skipped_reason: None,
            model: None,
            version: None,
            auth_source: "Subscription".into(),
            usage_source: "observed".into(),
            session_id: None,
            status: "completed".into(),
            error: None,
            protocol_violation: false,
        }
    }

    #[test]
    fn legacy_checkpoint_keeps_original_cohort_after_registry_expands() {
        let records = [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Kimi,
            ProviderKind::Cursor,
        ]
        .map(completed_record);
        assert_eq!(
            checkpoint_provider_arg("auto", &records),
            "claude,codex,kimi,cursor"
        );
        assert!(!checkpoint_coverage_degraded(&records));
    }

    #[test]
    fn checkpoint_does_not_require_skipped_providers_or_expand_explicit_selection() {
        let mut records = [
            completed_record(ProviderKind::Codex),
            completed_record(ProviderKind::Kimi),
        ];
        records[1].selected = false;
        records[1].status = "skipped".into();
        assert_eq!(checkpoint_provider_arg("auto", &records), "codex");
        assert_eq!(checkpoint_provider_arg("codex", &records), "codex");
        assert!(checkpoint_coverage_degraded(&records));

        records[1].selected = true;
        records[1].status = "failed".into();
        assert_eq!(checkpoint_provider_arg("auto", &records), "codex,kimi");
        assert!(checkpoint_coverage_degraded(&records));
    }
}
