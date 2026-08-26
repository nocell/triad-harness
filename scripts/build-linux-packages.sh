#!/bin/sh
set -eu

if [ "$#" -ne 6 ]; then
  echo "usage: $0 RELEASE_TAG BINARY DEB_ARCH RPM_ARCH OUTPUT_DIR REPOSITORY" >&2
  exit 2
fi

release_tag=$1
binary=$2
deb_arch=$3
rpm_arch=$4
output_dir=$5
repository=$6
version=${release_tag#v}

case "$release_tag" in
  v[0-9]*) ;;
  *)
    echo "release tag must start with v followed by a digit" >&2
    exit 2
    ;;
esac

test -x "$binary"
mkdir -p "$output_dir"
work_dir=$(mktemp -d "${TMPDIR:-/tmp}/triad-packages.XXXXXX")
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

deb_root="$work_dir/deb"
mkdir -p "$deb_root/DEBIAN" "$deb_root/usr/bin"
install -m 0755 "$binary" "$deb_root/usr/bin/triad"
cat > "$deb_root/DEBIAN/control" <<EOF
Package: triad-harness
Version: $version
Section: devel
Priority: optional
Architecture: $deb_arch
Maintainer: Triad Contributors <triad-contributors@users.noreply.github.com>
Homepage: https://github.com/$repository
Description: Subscription-backed frontier-model MapReduce code review harness
EOF
dpkg-deb --root-owner-group --build "$deb_root" "$output_dir/triad-harness_${version}_${deb_arch}.deb"

rpm_root="$work_dir/rpm"
mkdir -p "$rpm_root/BUILD" "$rpm_root/BUILDROOT" "$rpm_root/RPMS" "$rpm_root/SOURCES" "$rpm_root/SPECS" "$rpm_root/SRPMS"
install -m 0755 "$binary" "$rpm_root/SOURCES/triad"
cat > "$rpm_root/SPECS/triad-harness.spec" <<EOF
Name: triad-harness
Version: $version
Release: 1
Summary: Subscription-backed frontier-model MapReduce code review harness
License: MIT
URL: https://github.com/$repository
BuildArch: $rpm_arch

%description
Triad runs passive multi-provider code reviews in disposable snapshots.

%install
install -D -m 0755 %{_sourcedir}/triad %{buildroot}%{_bindir}/triad

%files
%{_bindir}/triad
EOF
rpmbuild --define "_topdir $rpm_root" -bb "$rpm_root/SPECS/triad-harness.spec"
install -m 0644 "$rpm_root/RPMS/$rpm_arch/triad-harness-$version-1.$rpm_arch.rpm" "$output_dir/"

dpkg-deb --info "$output_dir/triad-harness_${version}_${deb_arch}.deb"
rpm -qpi "$output_dir/triad-harness-$version-1.$rpm_arch.rpm"
