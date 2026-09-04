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
use futures::future::join_all;
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
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
        })?;
    }
    if detach {
        create_detached(&request, &run_id).await
    } else {
        run_review_pipeline(&run_id, &args).await
    }
}

// Entry used by main before the args are normalized when detach is requested.
async fn create_detached(request: &WorkerRequest, run_id: &str) -> Result<i32> {
    let run_dir = storage::run_dir(run_id)?;
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
    let child = command.spawn()?;
    let pid = child.id().context("worker pid unavailable")?;
    let mut manifest = load_manifest(run_id)?;
    manifest.pid = Some(pid);
    manifest.request_path = request_path;
    manifest.heartbeat_at = Some(Utc::now());
    save_manifest(&manifest)?;
    println!("{run_id}");
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
            crate::provider::discover(&config, crate::model::ProviderKind::Claude)
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
        print_manifest(&manifest);
        return terminal_exit_code(&manifest);
    }
    let request: WorkerRequest = storage::read_json(&manifest.request_path)?;
    if args.detach {
        create_detached(&request, &args.run_id).await
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
        create_detached(&request, &args.run_id).await
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
    let home = dirs::home_dir().context("home directory unavailable")?;
    let mut targets = Vec::new();
    match args.host {
        SkillHost::Codex => targets.push((home.join(".codex/skills/triad"), true)),
        SkillHost::Claude => targets.push((home.join(".claude/skills/triad"), false)),
        SkillHost::Kimi => targets.push((home.join(".kimi-code/skills/triad"), false)),
        SkillHost::All => {
            targets.push((home.join(".codex/skills/triad"), true));
            targets.push((home.join(".claude/skills/triad"), false));
            targets.push((home.join(".kimi-code/skills/triad"), false));
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

Use the installed `triad` CLI from the target Git repository. It fans review out to every runnable subscription-backed provider, then has a leader independently verify and reduce the findings. Every agent works in a disposable Git snapshot.

## Model team

Triad pins the intended subscription models; do not silently substitute API-backed or weaker models:

- Claude: `claude-fable-5-1` for architecture and data flow.
- Codex: `gpt-6-astra` with `max` effort and Standard processing (Fast mode disabled) for correctness and concurrency.
- Kimi: `kimi-code/k3` for regressions and API contracts.
- Cursor: `grok-4.6-fast`, resolved to `cursor-grok-4.6-high-fast`, for adversarial and cross-file analysis.

Treat Map outputs as claims, not votes. The reducer must inspect the code independently, validate each reachable trigger and consequence, deduplicate overlapping claims, and classify findings as `accepted`, `needs-human`, or `rejected`. Agreement between reviewers is supporting context, never proof.

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
- For a long interactive review, run `triad review ... --detach --json`, capture the run ID, monitor it with `triad status <run-id> --json` or `triad follow <run-id> --json`, and present `triad report <run-id>` when terminal.
- For CI or a report-only check, run `triad review ... --dry-run --json`. Exit `0` means no accepted or needs-human findings, `2` means blocking findings, and `3` means a selected provider, reducer, or protocol failure. Add `--require-all` only when missing optional providers must fail CI.
- Use `triad doctor --refresh --json` when the user asks about authentication/availability or a provider fails discovery. It performs status checks, not model-call probes.
- In the final response, state participating and skipped providers, degraded coverage, leader/model changes, and the report path or run ID. Do not present a partially completed run as a completed review.

## Safety and approval

- Keep subscription login only. Never introduce vendor API keys, API billing, automatic overage, Claude `-p`, Agent SDK, or ultrareview.
- Never install a provider, start an interactive login, enable a disabled provider, or change account settings without explicit user approval.
- Reviewers are passive: no edits, deletes, commits, pushes, branches, tags, GitHub comments or reviews, deployments, or external messages. They may inspect code and run existing local tests only inside disposable snapshots.
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

async fn run_review_pipeline_inner(run_id: &str, args: &ReviewArgs) -> Result<i32> {
    let run_dir = storage::run_dir(run_id)?;
    let _lock = RunLock::acquire(&run_dir)?;
    update_state(run_id, RunState::Discovering, None)?;
    let config = Config::load()?;
    let (adapters, statuses) = scheduler::select(&args.providers, args.require_all).await?;
    let target = git::resolve_target(args, &run_dir).await?;
    let context_snapshot = run_dir.join("snapshots/context");
    git::create_snapshot(
        &target,
        &context_snapshot,
        "Triad is preparing review context.",
    )
    .await?;
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

    let mut jobs = Vec::new();
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
    let results = join_all(jobs).await;
    let mut ledger = ProviderLedger::load()?;
    let mut outputs = Vec::new();
    let mut degraded = selected_set.len() < ProviderKind::ALL.len();
    let mut provider_summaries: Vec<_> = manifest
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
        .collect();
    for (adapter, result, violation) in results {
        let record = manifest
            .providers
            .iter_mut()
            .find(|record| record.provider == adapter.kind)
            .unwrap();
        match result {
            Ok(output) if !violation => {
                scheduler::record_success(&mut ledger, adapter.kind);
                record.status = "completed".into();
                record.session_id = output.session_id.clone();
                record.model = output.model.clone();
                provider_summaries.push((adapter.kind, "completed".into()));
                outputs.push((adapter.kind, output.text));
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
        manifest.heartbeat_at = Some(Utc::now());
        save_manifest(&manifest)?;
    }
    ledger.save()?;
    if outputs.is_empty() {
        manifest.state = RunState::Failed;
        manifest.pid = None;
        manifest.error = Some("all reviewers failed".into());
        manifest.degraded = true;
        save_manifest(&manifest)?;
        return Ok(3);
    }

    let successful: Vec<_> = outputs.iter().map(|(provider, _)| *provider).collect();
    let leader = scheduler::choose_leader(&args.leader, &successful, &config)?;
    manifest.leader = Some(leader);
    manifest.state = RunState::Reducing;
    manifest.degraded = degraded;
    manifest.updated_at = Utc::now();
    save_manifest(&manifest)?;
    let provider_results_path = run_dir.join("provider-results.json");
    report::write_provider_results(&provider_results_path, &outputs)?;
    let reducer_schema = run_dir.join("reducer.schema.json");
    report::write_reducer_schema(&reducer_schema)?;
    let reducer_snapshot = run_dir.join("snapshots/reducer");
    git::create_snapshot(&target, &reducer_snapshot, &context_markdown).await?;
    provider::prepare_snapshot(leader, AgentRole::Reducer, &reducer_snapshot)?;
    report::install_context(&reducer_snapshot, Some(&provider_results_path))?;
    let reducer_baseline = git::status_signature(&reducer_snapshot).await?;
    let reducer_adapter = provider::for_kind(&config, leader)?;
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
    let reduction = match reducer_adapter.run(&reducer_context).await {
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
        &reduction,
    );
    let report_path = run_dir.join("report.md");
    storage::atomic_write(&report_path, report_body.as_bytes())?;
    let blocking_findings = reduction
        .findings
        .iter()
        .filter(|finding| matches!(finding.verdict.as_str(), "accepted" | "needs-human"))
        .count();
    manifest.state = if args.dry_run && manifest.error.is_some() {
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
                &serde_json::json!({"run_id": run_id, "state": "awaiting_approval", "degraded": degraded, "report": report_path})
            )?
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
    let config = Config::load()?;
    let (available, _) = scheduler::select("auto", false).await?;
    let available_kinds: Vec<_> = available.iter().map(|adapter| adapter.kind).collect();
    let leader = if requested_leader == "auto" {
        manifest
            .leader
            .filter(|leader| available_kinds.contains(leader))
            .unwrap_or(scheduler::choose_leader("auto", &available_kinds, &config)?)
    } else {
        scheduler::choose_leader(requested_leader, &available_kinds, &config)?
    };
    manifest.state = RunState::Fixing;
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
    let adapter = provider::for_kind(&config, leader)?;
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
    let output = match adapter.run(&context).await {
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
    let tests = report::parse_value(&output.text)
        .unwrap_or_else(|| serde_json::json!({"raw": output.text}));
    storage::write_json(&tests_path, &tests)?;
    manifest.state = if patch.is_empty() {
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
        manifest.error = None;
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
    Ok(if manifest.degraded {
        2
    } else if matches!(
        manifest.state,
        RunState::Failed | RunState::Cancelled | RunState::FixIncomplete
    ) {
        3
    } else {
        0
    })
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
