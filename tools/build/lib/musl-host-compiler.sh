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
    # The `if !` guard is load-bearing: a bare `rpm_path=$(fetch_rpm ...)`
    # assignment would let a failure inside fetch_rpm exit only the command
    # substitution's subshell, and the build would continue with an empty path.
    if ! rpm_path=$(fetch_rpm "$package" "${MUSL_RPM_FILE[$package]}" "${MUSL_RPM_SHA256[$package]}"); then
        return 1
    fi
    # The extraction is idempotent: the sysroot only needs to exist once.
    if [ ! -e "$MUSL_PREFIX/lib64/libc.a" ]; then
        # Not a pipe: cpio stops reading at the archive trailer and exits, and
        # rpm2cpio can still have the final padding write in flight when it
        # does -- an intermittent SIGPIPE (exit 141) with nothing actually
        # wrong, which pipefail then reports as a failed unpack.
        rpm2cpio "$rpm_path" > "$MUSL_ROOT/.payload.cpio" || return 1
        ( cd "$MUSL_ROOT" && cpio -idm --quiet < .payload.cpio ) || return 1
        rm -f "$MUSL_ROOT/.payload.cpio"
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
# The wrapper lives at a stable path, not under the per-run work directory.  It
# is the compiler recorded in every build system generated from it -- meson
# bakes it into its configuration and into the commands it regenerates -- so a
# wrapper under `mktemp -d` makes the generated trees unrepeatable and makes
# their regeneration fail once the directory is gone.
MUSL_CC="$MUSL_ROOT/musl-gcc"
cat >"$MUSL_CC" <<EOF
#!/bin/sh
exec "\${REALGCC:-gcc}" "\$@" -specs "$LOCAL_SPECS"
EOF
chmod 0755 "$MUSL_CC"
log "musl compiler: $MUSL_CC"
