# Triad

Triad is a local Rust CLI that reviews one Git change with every available subscription-backed coding agent, consolidates the findings, and prepares a patch only after a separate approval command.

Triad is an independent project and is not affiliated with Anthropic, OpenAI, Moonshot AI, Cursor, or xAI.

Supported providers:

- Claude Code through an interactive `claude --bg` subscription session, pinned to `claude-fable-5[1m]` — never `claude -p` or Agent SDK usage.
- Codex CLI through ChatGPT login, pinned to `gpt-5.6-sol` with `max` reasoning.
- Kimi Code through membership login, pinned to `kimi-code/k3`.
- Cursor Agent through browser login, pinned to `grok-4.6-fast` (resolved to the current CLI model ID `cursor-grok-4.6-high-fast`).

Triad never reads vendor OAuth tokens and removes known API-key variables from every child process. Reviewers operate in independent disposable Git clones; the source checkout is not modified.

Cursor reviewers trust only the already-created disposable snapshot, run in read-only Ask mode with sandboxing enabled, and receive project-local deny rules for writes, secrets, destructive commands, network tools, and external CLIs. Global MCP servers are disabled for the run's snapshot and repository MCP configurations are replaced with empty run-local configs. The separately approved fixer allows writes only inside its disposable snapshot. Triad never passes Cursor `--force`, `-f`, or `--yolo`.

On macOS, Triad prefers the current official Codex binary bundled with ChatGPT over an older global `codex`; an explicit `[providers.codex].binary` still wins. Codex runs with `--ignore-user-config`, user hooks disabled, the explicit model/effort pair, ChatGPT subscription auth, and a role-appropriate sandbox. It never silently falls back to an older model. CLI versions older than `0.145.0` are reported as incompatible with `gpt-5.6-sol`.

Reviewers are strictly passive. Their prompts forbid editing or deleting files, commits, pushes, branches, tags, GitHub comments/reviews/issues, messages, deployments, and all other external actions. They may only inspect code, propose findings, and run existing local unit tests or read-only checks inside their disposable snapshots. Triad also removes each snapshot's Git remote, isolates Git/GitHub credentials, and discards any result whose snapshot files or HEAD changed.

## Install

```bash
git clone https://github.com/nocell/triad-harness.git
cd triad-harness
cargo install --path .
triad doctor --refresh
```

Cursor CLI is currently optional. Triad will not install it without confirmation:

```bash
triad provider install cursor
triad provider install cursor --yes
triad provider login cursor
```

Vendor extra-usage or overage must be disabled in each account. Most providers do not expose an exact machine-readable remaining balance, so Triad records observed successes, quota failures, reset times, and cooldowns instead of pretending to know the balance.

## Review

```bash
# Current branch against the remote default branch
triad review

# Current branch against an explicit base
triad review --base origin/main

# GitHub PR from inside its repository
triad review 1234

# Staged, unstaged, and untracked changes
triad review --uncommitted

# Long-running detached review
triad review 1234 --detach
triad follow <run-id>
triad report <run-id>
```

`--providers auto` launches one reviewer for every runnable provider. Use `--require-all` to fail before model calls if any requested provider is unavailable. A quota failure produces a degraded report and updates the local circuit breaker.

### CI dry run

```bash
triad review --uncommitted --dry-run --json
```

`--dry-run` still consumes provider subscription usage and persists the report and run artifacts, but it terminates after reduction, never creates an approval/fix stage, and cannot be passed to `triad fix`. Exit code `0` means no `accepted` or `needs-human` findings, `2` means the review found a blocking issue, and `3` means a selected provider or the reducer failed. Missing optional providers are recorded as degraded coverage without failing an otherwise clean dry run; combine it with `--require-all` when CI requires every requested provider.

## Approval and fix

Review and fix are deliberately separate:

```bash
triad report <run-id>
triad fix <run-id>
triad fix <run-id> --only TRIAD-001,TRIAD-004
```

The fixer works in a fresh disposable checkout and writes `fix.patch` and `tests.json` into the run directory. It does not commit, push, or apply the patch to the source checkout.

## Provider and run management

```bash
triad providers
triad provider disable kimi
triad provider enable kimi
triad provider login claude

triad runs
triad status <run-id>
triad cancel <run-id>
triad resume <run-id> --detach
```

Data is stored with user-only permissions under the platform local-data directory. Override locations for hermetic automation with `TRIAD_CONFIG_HOME` and `TRIAD_DATA_HOME`.

## Sub-agent skills

Triad can install thin invocation skills. This is also confirmation-gated:

```bash
triad install-skill --host all
triad install-skill --host all --yes
```

The Codex skill is installed as `$triad` under `~/.codex/skills/triad`; use `/skills` to find it in Codex. It covers interactive reviews, CI dry runs, provider diagnostics, and approval-gated isolated fixes. The skills stop after the report and prohibit calling `triad fix` until the user separately approves the patch stage.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The E2E suite uses four fake vendor CLIs. It exercises discovery, subscription-auth checks, parallel review, reducer selection, approval-gated fixing, API-key and GitHub-auth stripping, missing push remotes, protocol-violation rejection, and source-checkout isolation without consuming real model quota.
