#!/usr/bin/env node
"use strict";

const crypto = require("node:crypto");
const fs = require("node:fs");
const https = require("node:https");
const os = require("node:os");
const path = require("node:path");
const { execFileSync, spawnSync } = require("node:child_process");
const packageJson = require("../package.json");
const { releaseTarget } = require("../lib/platform.js");

const REPOSITORY = "nocell/triad-harness";
const MAX_DOWNLOAD_BYTES = 100 * 1024 * 1024;

function fail(message) {
  process.stderr.write(`triad-harness: ${message}\n`);
  process.exit(1);
}

function download(url, redirects = 0) {
  if (redirects > 5) return Promise.reject(new Error("too many HTTP redirects"));
  return new Promise((resolve, reject) => {
    const request = https.get(
      url,
      {
        headers: {
          Accept: "application/octet-stream",
          "User-Agent": `triad-harness-npx/${packageJson.version}`,
        },
        timeout: 30_000,
      },
      (response) => {
        if (response.statusCode >= 300 && response.statusCode < 400 && response.headers.location) {
          response.resume();
          resolve(download(new URL(response.headers.location, url).toString(), redirects + 1));
          return;
        }
        if (response.statusCode !== 200) {
          response.resume();
          reject(new Error(`download returned HTTP ${response.statusCode}`));
          return;
        }
        const chunks = [];
        let total = 0;
        response.on("data", (chunk) => {
          total += chunk.length;
          if (total > MAX_DOWNLOAD_BYTES) {
            response.destroy(new Error("download exceeded 100 MiB safety limit"));
            return;
          }
          chunks.push(chunk);
        });
        response.on("end", () => resolve(Buffer.concat(chunks)));
        response.on("error", reject);
      },
    );
    request.on("timeout", () => request.destroy(new Error("download timed out")));
    request.on("error", reject);
  });
}

async function installedBinary() {
  if (process.env.TRIAD_BINARY) return process.env.TRIAD_BINARY;

  let target;
  try {
    target = releaseTarget();
  } catch (error) {
    fail(error.message);
  }
  const version = packageJson.version;
  const asset = `triad-v${version}-${target.os}-${target.arch}.tar.gz`;
  const cacheBase = process.env.XDG_CACHE_HOME || path.join(os.homedir(), ".cache");
  const installDir = path.join(cacheBase, "triad-harness", version, `${target.os}-${target.arch}`);
  const binary = path.join(installDir, "triad");
  if (fs.existsSync(binary)) return binary;

  fs.mkdirSync(installDir, { recursive: true, mode: 0o700 });
  const releaseBase = `https://github.com/${REPOSITORY}/releases/download/v${version}`;
  const [archive, sums] = await Promise.all([
    download(`${releaseBase}/${asset}`),
    download(`${releaseBase}/SHA256SUMS`),
  ]);
  const expectedLine = sums
    .toString("utf8")
    .split(/\r?\n/)
    .map((line) => line.trim().split(/\s+/))
    .find((parts) => parts.length === 2 && parts[1] === asset);
  const expected = expectedLine?.[0];
  if (!expected || !/^[a-f0-9]{64}$/i.test(expected)) {
    throw new Error(`release checksum for ${asset} is missing or malformed`);
  }
  const actual = crypto.createHash("sha256").update(archive).digest("hex");
  if (actual.toLowerCase() !== expected.toLowerCase()) {
    throw new Error(`SHA-256 mismatch for ${asset}`);
  }

  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "triad-npx-"));
  try {
    const archivePath = path.join(temporary, asset);
    fs.writeFileSync(archivePath, archive, { mode: 0o600 });
    execFileSync("tar", ["-xzf", archivePath, "-C", temporary, "triad"], {
      stdio: "pipe",
    });
    const extracted = path.join(temporary, "triad");
    fs.chmodSync(extracted, 0o755);
    const candidate = `${binary}.tmp-${process.pid}`;
    fs.copyFileSync(extracted, candidate);
    fs.chmodSync(candidate, 0o755);
    try {
      fs.renameSync(candidate, binary);
    } catch (error) {
      fs.rmSync(candidate, { force: true });
      if (!fs.existsSync(binary)) throw error;
    }
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
  return binary;
}

installedBinary()
  .then((binary) => {
    const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
    if (result.error) throw result.error;
    if (result.signal) {
      process.kill(process.pid, result.signal);
      return;
    }
    process.exit(result.status ?? 1);
  })
  .catch((error) => fail(error.message));
