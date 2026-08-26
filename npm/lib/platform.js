"use strict";

function releaseTarget(platform = process.platform, architecture = process.arch) {
  const supportedPlatforms = new Set(["darwin", "linux"]);
  if (!supportedPlatforms.has(platform)) {
    throw new Error(`unsupported platform ${platform}; Triad supports macOS and Linux`);
  }
  if (architecture !== "arm64" && architecture !== "x64") {
    throw new Error(`unsupported ${platform} architecture ${architecture}`);
  }
  return { os: platform, arch: architecture };
}

module.exports = { releaseTarget };
