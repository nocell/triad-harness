#!/bin/sh
set -eu

if [ "$#" -lt 3 ] || [ "$#" -gt 4 ]; then
  echo "usage: $0 RELEASE_TAG SHA256SUMS OUTPUT [REPOSITORY]" >&2
  exit 2
fi

release_tag=$1
sums_file=$2
output=$3
repository=${4:-nocell/triad-harness}

case "$release_tag" in
  v[0-9]*) ;;
  *)
    echo "release tag must start with v followed by a digit" >&2
    exit 2
    ;;
esac

arm64_sha=$(awk '$2 ~ /darwin-arm64\.tar\.gz$/ {print $1}' "$sums_file")
x64_sha=$(awk '$2 ~ /darwin-x64\.tar\.gz$/ {print $1}' "$sums_file")
version=${release_tag#v}

case "$arm64_sha$x64_sha" in
  *[!0-9a-fA-F]*)
    echo "release checksums are missing or malformed" >&2
    exit 2
    ;;
esac

if [ "${#arm64_sha}" -ne 64 ] || [ "${#x64_sha}" -ne 64 ]; then
  echo "release checksums are missing or malformed" >&2
  exit 2
fi

cat > "$output" <<EOF
class Triad < Formula
  desc "Subscription-backed frontier-model MapReduce code review harness"
  homepage "https://github.com/$repository"
  version "$version"
  license "MIT"

  on_arm do
    url "https://github.com/$repository/releases/download/$release_tag/triad-$release_tag-darwin-arm64.tar.gz"
    sha256 "$arm64_sha"
  end

  on_intel do
    url "https://github.com/$repository/releases/download/$release_tag/triad-$release_tag-darwin-x64.tar.gz"
    sha256 "$x64_sha"
  end

  def install
    bin.install "triad"
  end

  test do
    assert_match "Usage: triad", shell_output("#{bin}/triad --help")
  end
end
EOF
