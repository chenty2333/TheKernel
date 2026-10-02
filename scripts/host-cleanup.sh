#!/usr/bin/env bash
# Audit and, when explicitly requested, remove legacy TheKernel host daemons.
#
# The product does not need a host boot service or a host-networked container:
# development uses an ephemeral Compose `run --rm` container and QEMU user
# networking. Keep this script deliberately narrow so it cannot stop an
# unrelated project or rewrite a user's NetworkManager configuration.
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/host-cleanup.sh [--fix]

Without --fix, report legacy TheKernel boot/netboot containers and exact
systemd units. With --fix, disable restart policies, stop/remove those
containers, and disable exact user-scope units. System-scope units are only
changed when this script is run as root; otherwise they are reported.
EOF
}

fix=0
case "${1:-}" in
    "") ;;
    --fix) fix=1 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'host-cleanup: unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
esac

legacy_containers=(thekernel-boot thekernel-netboot)
legacy_units=(thekernel-boot.service thekernel-netboot.service thekernel-netboot.timer)
found=0

if command -v docker >/dev/null 2>&1; then
    for name in "${legacy_containers[@]}"; do
        if ! docker inspect "$name" >/dev/null 2>&1; then
            continue
        fi
        found=1
        restart=$(docker inspect -f '{{.HostConfig.RestartPolicy.Name}}' "$name" 2>/dev/null || printf unknown)
        status=$(docker inspect -f '{{.State.Status}}' "$name" 2>/dev/null || printf unknown)
        printf 'legacy container: %s (status=%s restart=%s)\n' "$name" "$status" "$restart"
        if [[ "$fix" == 1 ]]; then
            docker update --restart=no "$name" >/dev/null
            docker stop "$name" >/dev/null 2>&1 || true
            docker rm "$name" >/dev/null 2>&1 || true
            printf 'removed legacy container: %s\n' "$name"
        fi
    done
else
    printf '%s\n' 'docker: unavailable; skipped legacy container audit' >&2
fi

unit_present() {
    local scope=$1 unit=$2
    if [[ "$scope" == user ]]; then
        systemctl --user --no-legend --plain list-unit-files "$unit" 2>/dev/null | grep -Fq "$unit" && return 0
        systemctl --user --no-legend --plain list-units --all "$unit" 2>/dev/null | grep -Fq "$unit" && return 0
    else
        systemctl --no-legend --plain list-unit-files "$unit" 2>/dev/null | grep -Fq "$unit" && return 0
        systemctl --no-legend --plain list-units --all "$unit" 2>/dev/null | grep -Fq "$unit" && return 0
    fi
    return 1
}

for scope in user system; do
    for unit in "${legacy_units[@]}"; do
        if ! unit_present "$scope" "$unit"; then
            continue
        fi
        found=1
        printf 'legacy %s unit: %s\n' "$scope" "$unit"
        [[ "$fix" == 1 ]] || continue
        if [[ "$scope" == user ]]; then
            systemctl --user disable --now "$unit" >/dev/null 2>&1 || true
            systemctl --user reset-failed "$unit" >/dev/null 2>&1 || true
            printf 'disabled legacy user unit: %s\n' "$unit"
        elif [[ "$(id -u)" == 0 ]]; then
            systemctl disable --now "$unit" >/dev/null 2>&1 || true
            systemctl reset-failed "$unit" >/dev/null 2>&1 || true
            printf 'disabled legacy system unit: %s\n' "$unit"
        else
            printf 'cannot change system unit without root: %s\n' "$unit" >&2
        fi
    done
done

if [[ "$found" == 0 ]]; then
    printf '%s\n' 'TheKernel host cleanup: no legacy boot/netboot container or unit found'
elif [[ "$fix" == 0 ]]; then
    printf '%s\n' 'No changes made. Re-run with --fix to remove only these exact legacy entries.'
fi
