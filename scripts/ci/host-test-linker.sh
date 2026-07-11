#!/usr/bin/env bash
set -euo pipefail

# Rust host tests are PIEs by default on modern Linux distributions. ArceOS'
# per-CPU accessors deliberately use absolute relocations, so the final test
# executable must be linked as a non-PIE. Proc-macro and other shared objects
# must remain shared; passing -no-pie to those links makes the C runtime expect
# a main symbol. This wrapper distinguishes the two link products without
# rewriting rustc flags for every dependency.

cc=${THEKERNEL_HOST_CC:-cc}
for arg in "$@"; do
    case "$arg" in
        -shared|--shared)
            exec "$cc" "$@"
            ;;
    esac
done

exec "$cc" "$@" -no-pie
