#!/usr/bin/env bash
# Materialize the Perl modules Buildroot checks into a task-local prefix.
# This deliberately augments, rather than bypasses, Buildroot's host checks.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/.." && pwd)
MANIFEST=$REPO_ROOT/config/graphics/host-perl-modules.sha256
prefix=
module_root=${THEKERNEL_GRAPHICS_PERL_MODULE_ROOT:-}

usage() {
    cat <<'EOF'
Usage: scripts/setup-graphics-local-deps.sh --prefix DIR --module-root DIR

Copy a hash-pinned set of already-installed, pure-Perl core modules into DIR.
The source directory must be a compatible installed Perl share tree containing
English.pm, FindBin.pm, ExtUtils/MakeMaker.pm, and IPC/Cmd.pm. The command
refuses unknown module contents and verifies that
the resulting prefix satisfies the same four module loads Buildroot requires.
EOF
}

while (($#)); do
    case "$1" in
        --prefix) prefix=${2:-}; shift 2 ;;
        --module-root) module_root=${2:-}; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

[ -n "$prefix" ] || { printf '%s\n' '--prefix is required' >&2; exit 2; }
[ -n "$module_root" ] || { printf '%s\n' '--module-root is required (or set THEKERNEL_GRAPHICS_PERL_MODULE_ROOT)' >&2; exit 2; }
[ -r "$MANIFEST" ] || { printf 'missing module manifest: %s\n' "$MANIFEST" >&2; exit 1; }
perl_binary=$(command -v perl) || { printf '%s\n' 'perl is required to validate the local module prefix' >&2; exit 1; }

while read -r expected relative; do
    [ -n "${expected:-}" ] || continue
    case "$expected" in \#*) continue ;; esac
    if [ -d "$module_root/$relative" ]; then
        actual=$(cd "$module_root" && find "$relative" -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | awk '{print $1}')
        [ "$actual" = "$expected" ] || { printf 'unexpected checksum for %s\n' "$module_root/$relative" >&2; exit 1; }
        install -d "$prefix/lib/perl5/$relative"
        cp -a "$module_root/$relative/." "$prefix/lib/perl5/$relative/"
        continue
    fi
    source_file=$module_root/$relative
    [ -r "$source_file" ] || { printf 'missing trusted Perl module: %s\n' "$source_file" >&2; exit 1; }
    actual=$(sha256sum "$source_file" | awk '{print $1}')
    [ "$actual" = "$expected" ] || { printf 'unexpected checksum for %s\n' "$source_file" >&2; exit 1; }
    install -D -m 0644 "$source_file" "$prefix/lib/perl5/$relative"
done <"$MANIFEST"

install -d "$prefix/bin"
printf '#!/bin/sh\nexec "%s" -I"%s/lib/perl5" "$@"\n' "$perl_binary" "$prefix" >"$prefix/bin/perl"
chmod 0755 "$prefix/bin/perl"
"$prefix/bin/perl" -MEnglish -MExtUtils::MakeMaker -MFindBin -MIPC::Cmd -e '1'
printf 'local Perl modules ready: %s/lib/perl5\n' "$prefix"
