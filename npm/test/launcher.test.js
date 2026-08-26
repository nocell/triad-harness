"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");
const test = require("node:test");

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
