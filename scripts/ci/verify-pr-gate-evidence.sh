#!/usr/bin/env bash
set -euo pipefail

REQUIRE_RELEASE_PASS=0

usage() {
    printf '%s\n' \
        'Usage: scripts/ci/verify-pr-gate-evidence.sh [--require-release-pass] EVIDENCE_DIR' \
        '' \
        'The default mode verifies only bundle integrity and census completeness.' \
        '--require-release-pass additionally requires the outer PASS envelope,' \
        'a source build, and release_evidence=YES.'
}

while (($#)); do
    case "$1" in
        --require-release-pass) REQUIRE_RELEASE_PASS=1; shift ;;
        -h|--help) usage; exit 0 ;;
        --) shift; break ;;
        -*) printf 'PR evidence verifier: unknown option: %s\n' "$1" >&2; exit 2 ;;
        *) break ;;
    esac
done
if [ "$#" -ne 1 ]; then
    usage >&2
    exit 2
fi

EVIDENCE_DIR=$(realpath -e -- "$1")
for required in \
    source-set.tsv artifacts.tsv receipt.tsv checksums.sha256 verify.sh bundle
do
    [ -e "$EVIDENCE_DIR/$required" ] || {
        printf 'PR evidence verifier: missing %s\n' "$required" >&2
        exit 1
    }
done

if find "$EVIDENCE_DIR" -type l -print -quit | grep -q .; then
    printf '%s\n' 'PR evidence verifier: symbolic links are forbidden' >&2
    exit 1
fi
if find "$EVIDENCE_DIR" ! -type d ! -type f -print -quit | grep -q .; then
    printf '%s\n' 'PR evidence verifier: special files are forbidden' >&2
    exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf -- "$tmp"' EXIT

awk '{
    path = $2
    sub(/^\*/, "", path)
    if (path == "" || path ~ /^\// || path ~ /(^|\/)\.\.($|\/)/) exit 10
    print path
}' "$EVIDENCE_DIR/checksums.sha256" | LC_ALL=C sort >"$tmp/manifest-paths"

(
    cd -- "$EVIDENCE_DIR"
    find . -type f ! -path ./checksums.sha256 -print | sed 's|^\./||'
) | LC_ALL=C sort >"$tmp/actual-paths"
cmp -s "$tmp/actual-paths" "$tmp/manifest-paths" || {
    printf '%s\n' 'PR evidence verifier: checksum census is incomplete' >&2
    diff -u "$tmp/manifest-paths" "$tmp/actual-paths" >&2 || true
    exit 1
}

(
    cd -- "$EVIDENCE_DIR"
    sha256sum -c checksums.sha256 >/dev/null
)

exec {artifacts_fd}<"$EVIDENCE_DIR/artifacts.tsv"
IFS= read -r artifacts_schema <&"$artifacts_fd"
IFS= read -r artifacts_header <&"$artifacts_fd"
[ "$artifacts_schema" = $'schema\tpr-gate-artifact-set-v2' ]
[ "$artifacts_header" = $'artifact\tsize_bytes\tsha256\tpath' ]
declare -A seen_artifacts=()
artifact_count=0
while IFS=$'\t' read -r artifact size sha path extra <&"$artifacts_fd"; do
    [ -n "$artifact" ] && [ -z "${extra:-}" ]
    [[ "$size" =~ ^[0-9]+$ ]]
    [[ "$sha" =~ ^[0-9a-f]{64}$ ]]
    [[ "$path" == bundle/* ]]
    [[ "$path" != /* && "$path" != *'/../'* && "$path" != ../* ]]
    [ -z "${seen_artifacts[$path]:-}" ]
    seen_artifacts[$path]=1
    actual_path="$EVIDENCE_DIR/$path"
    [ -f "$actual_path" ] && [ ! -L "$actual_path" ]
    [ "$(stat -c %s "$actual_path")" = "$size" ]
    [ "$(sha256sum "$actual_path" | awk '{print $1}')" = "$sha" ]
    printf '%s\n' "$path" >>"$tmp/artifact-paths.unsorted"
    artifact_count=$((artifact_count + 1))
done
exec {artifacts_fd}<&-
[ "$artifact_count" -gt 0 ]
LC_ALL=C sort "$tmp/artifact-paths.unsorted" >"$tmp/artifact-paths"

(
    cd -- "$EVIDENCE_DIR"
    find bundle -type f -print | LC_ALL=C sort
) >"$tmp/bundle-paths"
cmp -s "$tmp/artifact-paths" "$tmp/bundle-paths" || {
    printf '%s\n' 'PR evidence verifier: artifact census is incomplete' >&2
    exit 1
}

if [ "$REQUIRE_RELEASE_PASS" -eq 1 ]; then
    envelope="$EVIDENCE_DIR/gate-envelope.tsv"
    [ -f "$envelope" ] && [ ! -L "$envelope" ] || {
        printf '%s\n' 'PR evidence verifier: release envelope is missing' >&2
        exit 1
    }
    receipt_sha=$(sha256sum "$EVIDENCE_DIR/receipt.tsv" | awk '{print $1}')
    awk -F '\t' '
        NR == 1 {
            if ($0 != "schema\tpr-gate-receipt-v2") exit 10
            next
        }
        NF != 2 || seen[$1]++ { exit 11 }
        { value[$1] = $2 }
        END {
            if (value["result"] != "PASS" ||
                value["build_mode"] != "source" ||
                value["release_evidence"] != "YES") exit 12
        }
    ' "$EVIDENCE_DIR/receipt.tsv" || {
        printf '%s\n' 'PR evidence verifier: inner receipt is not release-qualified' >&2
        exit 1
    }
    awk -F '\t' -v receipt_sha="$receipt_sha" '
        NR == 1 {
            if ($0 != "schema\tpr-gate-envelope-v1") exit 20
            next
        }
        NF != 2 || seen[$1]++ { exit 21 }
        { value[$1] = $2 }
        END {
            if (value["result"] != "PASS" ||
                value["child_exit_code"] != "0" ||
                value["origin_source_revalidated"] != "PASS" ||
                value["release_qualified"] != "YES" ||
                value["reason"] != "release-qualified" ||
                value["inner_receipt_sha256"] != receipt_sha) exit 22
        }
    ' "$envelope" || {
        printf '%s\n' 'PR evidence verifier: outer envelope is not release-qualified' >&2
        exit 1
    }
    printf 'PR evidence verifier: RELEASE_OK evidence=%s\n' "$EVIDENCE_DIR"
else
    printf 'PR evidence verifier: INTEGRITY_OK evidence=%s\n' "$EVIDENCE_DIR"
fi
