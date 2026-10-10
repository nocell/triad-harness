#!/usr/bin/env node

// Opt-in live adapter test. Uses native subscription logins only; no API keys,
// provider installation, account changes, publication, or fixes.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

const flags = process.argv.slice(2);
if (flags.includes("--help")) {
  console.log("Usage: node scripts/e2e-subscriptions.mjs [--fixtures-only] [--providers auto|LIST] [--scenario all|bug|clean]\nWithout --fixtures-only, this test consumes subscription quota.");
  process.exit(0);
}
for (let index = 0; index < flags.length; index += 1) {
  if (["--providers", "--scenario"].includes(flags[index])) { index += 1; continue; }
  assert.equal(flags[index], "--fixtures-only", `Unknown option ${flags[index]}; use --help (no model calls made)`);
}
const fixturesOnly = flags.includes("--fixtures-only");
const providersFlag = flags.indexOf("--providers");
const providers = providersFlag < 0 ? "auto" : flags[providersFlag + 1];
assert(providers && !providers.startsWith("--"), "--providers requires a list or auto");
const scenarioFlag = flags.indexOf("--scenario");
const selectedScenario = scenarioFlag < 0 ? "all" : flags[scenarioFlag + 1];
assert(["all", "bug", "clean"].includes(selectedScenario), "--scenario requires all, bug, or clean");
const triad = process.env.TRIAD_BIN || "triad";
const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "triad-subscription-e2e-"));

function isBoundary(finding) {
  if (finding.file !== "reservation.mjs" || (finding.line != null && ![2, 3, 4].includes(finding.line))) return false;
  // Do not let generic inventory advice or a suggested fix satisfy the oracle.
  const evidence = ["claim", "rationale", "evidence", "trigger", "impact"]
    .map(key => finding[key] || "").join(" ");
  const equality = /equal|exact(?:ly)?|full(?:y)?|all (?:the )?(?:available )?(?:stock|inventory)|canReserve\(\s*5\s*,\s*5\s*\)|requested\s*={2,3}\s*available/i;
  const denial = /reject|den(?:y|ies|ied|ial)|false|fail|cannot|can't|refus|prevent|not reserv/i;
  return equality.test(evidence) && denial.test(evidence);
}

function parseEnvelope(text) {
  try { return JSON.parse(text.trim()); } catch {}
  const fenced = text.match(/```(?:json)?\s*(\{[\s\S]*?\})\s*```/);
  if (fenced) return JSON.parse(fenced[1]);
  const start = text.indexOf("{"), end = text.lastIndexOf("}");
  assert(start >= 0 && end >= start, "provider output has no JSON envelope");
  return JSON.parse(text.slice(start, end + 1));
}

assert(isBoundary({ file: "reservation.mjs", line: 3, trigger: "requested equals available", impact: "valid request rejected" }));
assert(!isBoundary({ file: "reservation.mjs", line: 3, claim: "negative available stock fails", suggested_fix: "allow equality" }));
assert.equal(parseEnvelope('Inspection commentary. {"review_status":"complete","limitations":[],"findings":[]}').review_status, "complete");

function run(binary, args, cwd) {
  const result = spawnSync(binary, args, {
    cwd,
    encoding: "utf8",
    timeout: 60 * 60 * 1000,
    maxBuffer: 8 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  return result;
}

function git(repo, ...args) {
  const result = run("git", args, repo);
  assert.equal(result.status, 0, `git ${args[0]} failed`);
  return result.stdout.trim();
}

const baseSource = `// Integer unit counts: reserve any positive quantity up to available stock.
export function canReserve(available, requested) {
  return requested > 0 && requested <= available;
}
`;
const tests = `import test from 'node:test';
import assert from 'node:assert/strict';
import { canReserve } from './reservation.mjs';
test('reservation boundaries', () => {
  assert.equal(canReserve(5, 5), true);
  assert.equal(canReserve(5, 3), true);
  assert.equal(canReserve(5, 6), false);
  assert.equal(canReserve(5, 0), false);
  assert.equal(canReserve(0, 1), false);
  assert.equal(canReserve('foo', 'foo'), false);
  assert.equal(canReserve(5, Number.NaN), false);
});
`;

try {
  for (const scenario of selectedScenario === "all" ? ["bug", "clean"] : [selectedScenario]) {
    const repo = path.join(temporary, scenario);
    fs.mkdirSync(repo);
    git(repo, "init", "-b", "main");
    git(repo, "config", "user.name", "Triad E2E");
    git(repo, "config", "user.email", "triad@test.invalid");
    fs.writeFileSync(path.join(repo, "reservation.mjs"), baseSource);
    fs.writeFileSync(path.join(repo, "reservation.test.mjs"), tests);
    assert.equal(run(process.execPath, ["--test", "reservation.test.mjs"], repo).status, 0);
    git(repo, "add", ".");
    git(repo, "commit", "-m", "baseline with boundary tests");
    const base = git(repo, "rev-parse", "HEAD");
    const changed = scenario === "bug"
      ? baseSource.replace("requested <= available", "requested < available")
      : baseSource.replace(
        "return requested > 0 && requested <= available;",
        "if (!(requested > 0)) return false;\n  return requested <= available;",
      );
    fs.writeFileSync(path.join(repo, "reservation.mjs"), changed);
    const testResult = run(process.execPath, ["--test", "reservation.test.mjs"], repo);
    assert.equal(testResult.status, scenario === "bug" ? 1 : 0, "fixture oracle mismatch");
    git(repo, "commit", "-am", scenario === "bug" ? "boundary regression" : "equivalent local guard");
    const head = git(repo, "rev-parse", "HEAD");
    if (fixturesOnly) {
      console.log(JSON.stringify({ scenario, fixture_tests: scenario === "bug" ? "expected_failure" : "passed" }));
      continue;
    }

    console.log(JSON.stringify({ scenario, state: "reviewing", providers }));
    const result = run(triad, ["review", "--base", base, "--providers", providers,
      "--leader", "auto", "--dry-run", "--json"], repo);
    // Exit 3 may represent an unavailable selected provider even if independent
    // reviewers and the reducer finish; inspect coverage, never turn it green.
    assert([0, 2, 3].includes(result.status), `Triad failed before a report (exit ${result.status})`);
    const summary = JSON.parse(result.stdout.trim());
    assert.equal(summary.state, "completed", "live review did not complete");
    assert(summary.report, "live review produced no report");
    const runDir = path.dirname(summary.report);
    const manifest = JSON.parse(fs.readFileSync(path.join(runDir, "manifest.json"), "utf8"));
    const reduction = JSON.parse(fs.readFileSync(path.join(runDir, "findings.json"), "utf8"));
    const raw = JSON.parse(fs.readFileSync(path.join(runDir, "provider-results.json"), "utf8"));
    assert.equal(manifest.target.base_sha, base);
    assert.equal(manifest.target.head_sha, head);
    assert.equal(manifest.error, null);
    assert.equal(reduction.review_status, "complete");
    assert(manifest.review_packet_sha256, "packet fingerprint was not persisted");
    const completed = manifest.providers.filter(p => p.status === "completed");
    assert(completed.length > 0, "no completed reviewer coverage");
    const blocking = reduction.findings.filter(f => ["accepted", "needs-human"].includes(f.verdict));
    const candidates = Object.fromEntries(Object.entries(raw).map(([provider, text]) => {
      const envelope = parseEnvelope(text);
      assert.equal(envelope.review_status, "complete");
      return [provider, envelope.findings];
    }));
    if (scenario === "bug") {
      assert(reduction.findings.some(f => f.verdict === "accepted" && isBoundary(f)), "reducer missed known boundary regression");
      for (const [provider, findings] of Object.entries(candidates)) {
        assert(findings.some(isBoundary), `${provider} missed known boundary regression`);
      }
    } else {
      assert.equal(blocking.length, 0, "false positive on behavior-preserving change");
      for (const [provider, findings] of Object.entries(candidates)) {
        assert.equal(findings.length, 0, `${provider} emitted a false positive`);
      }
    }
    assert.equal(git(repo, "status", "--porcelain"), "", "source checkout changed");
    assert.equal(fs.readFileSync(path.join(repo, "reservation.mjs"), "utf8"), changed);
    if (manifest.degraded) process.exitCode = 3;
    console.log(JSON.stringify({ scenario, run_id: summary.run_id, report: summary.report,
      result: manifest.degraded ? "passed_with_degraded_coverage" : "passed",
      coverage: manifest.degraded ? "degraded" : "full",
      providers: manifest.providers.map(p => ({ provider: p.provider, model: p.model, status: p.status })),
      accepted: reduction.findings.filter(f => f.verdict === "accepted").length }));
  }
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}
