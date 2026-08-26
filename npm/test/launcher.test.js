"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const test = require("node:test");
const { releaseTarget } = require("../lib/platform.js");

test("release target maps supported operating systems and architectures", () => {
  assert.deepEqual(releaseTarget("darwin", "arm64"), { os: "darwin", arch: "arm64" });
  assert.deepEqual(releaseTarget("darwin", "x64"), { os: "darwin", arch: "x64" });
  assert.deepEqual(releaseTarget("linux", "arm64"), { os: "linux", arch: "arm64" });
  assert.deepEqual(releaseTarget("linux", "x64"), { os: "linux", arch: "x64" });
  assert.throws(() => releaseTarget("win32", "x64"), /supports macOS and Linux/);
  assert.throws(() => releaseTarget("linux", "ia32"), /unsupported linux architecture/);
});

test("launcher forwards arguments and exit status to an explicit binary", () => {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), "triad-launcher-test-"));
  const binary = path.join(temporary, "triad");
  fs.writeFileSync(binary, "#!/bin/sh\nprintf 'forwarded:%s\\n' \"$*\"\nexit 7\n", {
    mode: 0o755,
  });
  const launcher = path.join(__dirname, "..", "bin", "triad.js");
  const result = spawnSync(process.execPath, [launcher, "doctor", "--refresh"], {
    encoding: "utf8",
    env: { ...process.env, TRIAD_BINARY: binary },
  });

  assert.equal(result.status, 7);
  assert.equal(result.stdout, "forwarded:doctor --refresh\n");
  assert.equal(result.stderr, "");
  fs.rmSync(temporary, { recursive: true, force: true });
});
