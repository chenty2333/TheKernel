#!/usr/bin/env bash
#
# musl-host-compiler.sh -- provision the pinned host-side musl C compiler.
#
# Sourced by the payload builders.  It unpacks the pinned Fedora musl RPMs into
# the state root (never installed: this host has no usable root) and rewrites
# the specs file so the unpacked copy works from wherever the cache lives.
#
# On success it sets:
#   MUSL_PREFIX  the musl sysroot inside the unpacked RPM tree
#   MUSL_CC      wrapper script for the host gcc driven through musl specs
#
# It requires the caller to define STATE_ROOT, SOURCE_CACHE, WORK_ROOT, log()
# and fetch_rpm(), because those are the caller's cache and error policy.

MUSL_ROOT="$STATE_ROOT/musl-host"
MUSL_PREFIX="$MUSL_ROOT/usr/x86_64-linux-musl"

mkdir -p "$MUSL_ROOT"
for package in "${MUSL_RPM_PACKAGES[@]}"; do
    rpm_path=$(fetch_rpm "$package" "${MUSL_RPM_FILE[$package]}" "${MUSL_RPM_SHA256[$package]}")
    # The extraction is idempotent: the sysroot only needs to exist once.
    if [ ! -e "$MUSL_PREFIX/lib64/libc.a" ]; then
        ( cd "$MUSL_ROOT" && rpm2cpio "$rpm_path" | cpio -idm --quiet )
    fi
done

[ -f "$MUSL_PREFIX/lib64/libc.a" ] || {
    printf 'musl sysroot is incomplete: %s/lib64/libc.a missing\n' "$MUSL_PREFIX" >&2
    exit 1
}

# The specs file records the install prefix it was generated for.  Rewrite it
# so the unpacked copy stays usable wherever the cache lives.
LOCAL_SPECS="$MUSL_ROOT/local-musl-gcc.specs"
sed "s|/usr/x86_64-linux-musl|$MUSL_PREFIX|g" \
    "$MUSL_PREFIX/lib64/musl-gcc.specs" >"$LOCAL_SPECS"
MUSL_CC="$WORK_ROOT/musl-gcc"
cat >"$MUSL_CC" <<EOF
#!/bin/sh
exec "\${REALGCC:-gcc}" "\$@" -specs "$LOCAL_SPECS"
EOF
chmod 0755 "$MUSL_CC"
log "musl compiler: $MUSL_CC"
