#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd -P)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/triad-docker-test.XXXXXX")
fixture=$(cd "$fixture" && pwd -P)
trap 'rm -rf "$fixture"' EXIT HUP INT TERM

mkdir -p "$fixture/bin" "$fixture/home/.claude" "$fixture/home/.codex" "$fixture/home/.cursor" "$fixture/repo"
git -C "$fixture/repo" init --quiet

docker_log="$fixture/docker-args"
fake_docker="$fixture/bin/docker"
# shellcheck disable=SC2016 # The generated fake expands these at runtime.
printf '#!/bin/sh\nprintf "%%s\\n" "$@" > "$TRIAD_TEST_DOCKER_LOG"\n' > "$fake_docker"
chmod 0755 "$fake_docker"

PATH="$fixture/bin:$PATH" \
HOME="$fixture/home" \
XDG_DATA_HOME="$fixture/state" \
TRIAD_TEST_DOCKER_LOG="$docker_log" \
TRIAD_DOCKER_IMAGE="triad:test" \
TRIAD_DOCKER_WORKSPACE="$fixture/repo" \
  "$repo_root/scripts/triad-docker" review --uncommitted --dry-run --json

assert_arg() {
  if ! grep -Fx -- "$1" "$docker_log" >/dev/null; then
    echo "missing Docker argument: $1" >&2
    sed -n '1,240p' "$docker_log" >&2
    exit 1
  fi
}

assert_arg "--read-only"
assert_arg "--cap-drop=ALL"
assert_arg "--security-opt=no-new-privileges"
assert_arg "type=bind,src=$fixture/repo,dst=/workspace,readonly"
assert_arg "type=bind,src=$fixture/state/triad/docker-home,dst=/home/triad"
assert_arg "type=bind,src=$fixture/home/.claude,dst=/home/triad/.claude"
assert_arg "type=bind,src=$fixture/home/.codex,dst=/home/triad/.codex"
assert_arg "type=bind,src=$fixture/home/.cursor,dst=/home/triad/.cursor"
assert_arg "triad:test"
assert_arg "review"
assert_arg "--dry-run"

if grep -E 'API_KEY|AUTH_TOKEN|docker\.sock|SSH_AUTH_SOCK' "$docker_log" >/dev/null; then
  echo "wrapper leaked a forbidden secret or privileged mount" >&2
  exit 1
fi

if PATH="$fixture/bin:$PATH" HOME="$fixture/home" TRIAD_TEST_DOCKER_LOG="$docker_log" \
  TRIAD_DOCKER_WORKSPACE="$fixture/repo" "$repo_root/scripts/triad-docker" review --detach >/dev/null 2>&1; then
  echo "wrapper accepted unsupported --detach" >&2
  exit 1
fi
