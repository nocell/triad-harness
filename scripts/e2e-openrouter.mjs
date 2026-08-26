#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import https from "node:https";
import os from "node:os";
import path from "node:path";

const fixtureOnly = process.argv.includes("--fixture-only");
const key = process.env.OPENROUTER_API_KEY;
if (!fixtureOnly && !key) {
  throw new Error("OPENROUTER_API_KEY is required for the live E2E test");
}

const model = process.env.OPENROUTER_MODEL || "openai/gpt-5.4-mini";
const fixture = {
  repository: "https://github.com/dtolnay/anyhow.git",
  pullRequest: "https://github.com/dtolnay/anyhow/pull/420",
  base: "f5e145c683a2cb958268d1bbeb5dedabca0b0fc7",
  head: "8cf66f79361d568067a75848aec30d3b2072be5c",
  file: "build.rs",
};

function git(arguments_, options = {}) {
  return execFileSync("git", arguments_, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    ...options,
  });
}

function postJson(body) {
  return new Promise((resolve, reject) => {
    const payload = Buffer.from(JSON.stringify(body));
    const request = https.request(
      "https://openrouter.ai/api/v1/chat/completions",
      {
        method: "POST",
        headers: {
          Authorization: `Bearer ${key}`,
          "Content-Type": "application/json",
          "Content-Length": payload.length,
          "HTTP-Referer": "https://github.com/nocell/triad-harness",
          "X-Title": "Triad public fixture E2E",
        },
        timeout: 120_000,
      },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => {
          const text = Buffer.concat(chunks).toString("utf8");
          if (response.statusCode < 200 || response.statusCode >= 300) {
            reject(new Error(`OpenRouter returned HTTP ${response.statusCode}: ${text.slice(0, 500)}`));
            return;
          }
          try {
            resolve(JSON.parse(text));
          } catch {
            reject(new Error("OpenRouter returned malformed JSON"));
          }
        });
      },
    );
    request.on("timeout", () => request.destroy(new Error("OpenRouter request timed out")));
    request.on("error", reject);
    request.end(payload);
  });
}

const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "triad-openrouter-e2e-"));
const repository = path.join(temporary, "anyhow");
try {
  git(["init", "--quiet", repository]);
  git(["-C", repository, "remote", "add", "origin", fixture.repository]);
  git(["-C", repository, "fetch", "--quiet", "--depth=1", "origin", fixture.base]);
  git(["-C", repository, "fetch", "--quiet", "--depth=1", "origin", fixture.head]);
  const reverseDiff = git([
    "-C",
    repository,
    "diff",
    "--no-ext-diff",
    "--unified=80",
    fixture.head,
    fixture.base,
    "--",
    fixture.file,
  ]);
  if (!reverseDiff.includes("ENOTEMPTY") || reverseDiff.length > 30_000) {
    throw new Error("public PR fixture changed or exceeded the E2E size bound");
  }

  if (fixtureOnly) {
    console.log(`Public PR fixture is available and bounded (${reverseDiff.length} bytes)`);
    process.exitCode = 0;
  } else {
    const response = await postJson({
      model,
      max_tokens: 1_500,
      messages: [
        {
          role: "system",
          content:
            "You are a high-precision code reviewer. Report only reachable correctness or portability defects introduced by the patch. Return JSON matching the supplied schema.",
        },
        {
          role: "user",
          content: `Review this proposed reverse patch derived from ${fixture.pullRequest}. Explain concrete triggers and impact.\n\n${reverseDiff}`,
        },
      ],
      response_format: {
        type: "json_schema",
        json_schema: {
          name: "review_findings",
          strict: true,
          schema: {
            type: "object",
            additionalProperties: false,
            properties: {
              findings: {
                type: "array",
                items: {
                  type: "object",
                  additionalProperties: false,
                  properties: {
                    title: { type: "string" },
                    claim: { type: "string" },
                    evidence: { type: "string" },
                    trigger: { type: "string" },
                    impact: { type: "string" },
                  },
                  required: ["title", "claim", "evidence", "trigger", "impact"],
                },
              },
            },
            required: ["findings"],
          },
        },
      },
    });
    const content = response?.choices?.[0]?.message?.content;
    const result = typeof content === "string" ? JSON.parse(content) : content;
    if (!result || !Array.isArray(result.findings) || result.findings.length === 0) {
      throw new Error("OpenRouter reviewer returned no findings for the known reverse bug fixture");
    }
    const evidence = JSON.stringify(result.findings).toLowerCase();
    if (!/(nfs|enotempty|directory[^a-z]+not[^a-z]+empty|remove_dir_all|cleanup)/i.test(evidence)) {
      throw new Error("OpenRouter reviewer missed the expected NFS directory cleanup failure");
    }
    console.log(`OpenRouter E2E passed with ${model}: ${result.findings.length} finding(s)`);
  }
} finally {
  fs.rmSync(temporary, { recursive: true, force: true });
}
