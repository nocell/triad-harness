#!/bin/sh
set -eu

uid=$(id -u)
gid=$(id -g)

# `scripts/triad-docker` runs with the host UID/GID so bind-mounted state never
# becomes root-owned. libnss-wrapper gives uncommon host IDs a valid passwd
# entry without mutating the read-only image filesystem.
if ! getent passwd "$uid" >/dev/null 2>&1; then
  passwd_file="/tmp/triad-passwd-$uid"
  group_file="/tmp/triad-group-$gid"
  cp /etc/passwd "$passwd_file"
  cp /etc/group "$group_file"
  printf 'triad:x:%s:%s:Triad container user:/home/triad:/bin/sh\n' "$uid" "$gid" >> "$passwd_file"
  printf 'triad:x:%s:\n' "$gid" >> "$group_file"
  export NSS_WRAPPER_PASSWD="$passwd_file"
  export NSS_WRAPPER_GROUP="$group_file"
  export LD_PRELOAD="/usr/local/lib/libnss_wrapper.so${LD_PRELOAD:+:$LD_PRELOAD}"
fi

exec /usr/local/bin/triad "$@"
