#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
CI_DIR="$REPO_ROOT/scripts/ci"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

while IFS= read -r script; do
    bash -n "$script"
done < <(find "$CI_DIR" -type f -name '*.sh' -print | sort)
for script in "$REPO_ROOT"/tests/guest/nightly/*; do
    sh -n "$script"
done
bash -n "$0"
python3 "$CI_DIR/validate-rfc-index.py" >/dev/null

# The host and pinned Debian developer container must never execute artifacts
# from the same primary or maintained-sibling Cargo target. This is a static
# contract test because the remainder of this file deliberately stays
# Docker-free.
grep -Fq \
    '"${THEKERNEL_CI_TARGET_DIR:-target/ci-per-commit}")' \
    "$CI_DIR/per-commit.sh"
grep -Fq \
    '"${THEKERNEL_CI_SIBLING_TARGET_DIR:-target/ci-maintained-siblings}")' \
    "$CI_DIR/per-commit.sh"
grep -Fq \
    'THEKERNEL_CI_TARGET_DIR: /workspace/target/ci-per-commit-container' \
    "$REPO_ROOT/dev-env/compose.yaml"
grep -Fq \
    'THEKERNEL_CI_SIBLING_TARGET_DIR: /workspace/target/ci-maintained-siblings-container' \
    "$REPO_ROOT/dev-env/compose.yaml"
grep -Fq 'ci_run_step axfault-core-tests' "$CI_DIR/per-commit.sh"
grep -Fq -- '-p thekernel-axfault' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step axcbpf-core-tests' "$CI_DIR/per-commit.sh"
grep -Fq -- '-p thekernel-axcbpf' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step seccomp-core-tests' "$CI_DIR/per-commit.sh"
grep -Fq -- '-p thekernel-linux-seccomp' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step kernel-seccomp-adapter-tests' "$CI_DIR/per-commit.sh"
grep -Fq -- 'seccomp::tests -- --test-threads=1' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step kernel-keyring-tests' "$CI_DIR/per-commit.sh"
grep -Fq -- '--minimum 66 --filter keyring:: --' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step kernel-userfaultfd-tests' "$CI_DIR/per-commit.sh"
grep -Fq -- '--minimum 72 --filter userfaultfd:: --' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step kernel-io-uring-adapter-tests' "$CI_DIR/per-commit.sh"
grep -Fq -- '--minimum 4 --filter io_uring:: --' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step kernel-packet-adapter-tests' "$CI_DIR/per-commit.sh"
grep -Fq -- '--minimum 24 --filter packet --' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step vendored-smoltcp-udp-tests' "$CI_DIR/per-commit.sh"
grep -Fq 'RUSTUP_TOOLCHAIN=1.85.0' "$CI_DIR/per-commit.sh"
grep -Fq -- '--minimum 41 --filter socket::udp::test --' \
    "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step ci-script-tests "$STEP_TIMEOUT_SECS"' \
    "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step packet-core-tests' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step packet-core-check' "$CI_DIR/per-commit.sh"
grep -Fq -- '-p thekernel-linux-packet --all-targets' "$CI_DIR/per-commit.sh"
grep -Fq -- '-p thekernel-linux-packet --no-default-features' \
    "$CI_DIR/per-commit.sh"
grep -Fq 'runner.temp }}/thekernel-pr-gate/' \
    "$REPO_ROOT/.github/workflows/ci.yml"
grep -Fq 'self-contained evidence directory' \
    "$REPO_ROOT/.github/workflows/ci.yml"
grep -Fq "github.event_name == 'push'" "$REPO_ROOT/.github/workflows/ci.yml"
grep -Fq 'Enable isolated unprivileged user namespaces' \
    "$REPO_ROOT/.github/workflows/ci.yml"
grep -Fq '/proc/sys/kernel/apparmor_restrict_unprivileged_userns' \
    "$REPO_ROOT/.github/workflows/ci.yml"
grep -Fq 'sudo tee "$restriction"' \
    "$REPO_ROOT/.github/workflows/ci.yml"

# Container jobs are resolved before any runner step can source versions.env,
# and GitHub does not expose the env context to container.image. Keep the two
# default images fixed to the current toolchain tag, preserve the repository
# variable override, and make this test derive the expected literal from the
# canonical version file so a future toolchain bump cannot drift silently.
rust_toolchain=$(sed -n 's/^RUST_TOOLCHAIN=//p' \
    "$REPO_ROOT/dev-env/versions.env")
[ -n "$rust_toolchain" ]
[ "$(grep -c '^RUST_TOOLCHAIN=' "$REPO_ROOT/dev-env/versions.env")" -eq 1 ]
expected_ci_image="      image: \${{ vars.THEKERNEL_DEV_IMAGE || format('ghcr.io/{0}/thekernel-dev:${rust_toolchain}', github.repository_owner) }}"
[ "$(grep -Fxc -- "$expected_ci_image" \
    "$REPO_ROOT/.github/workflows/ci.yml")" -eq 2 ]
if grep -Fq 'thekernel-dev:latest' "$REPO_ROOT/.github/workflows/ci.yml"; then
    printf '%s\n' 'test-ci-scripts: CI container fallback still uses latest' >&2
    exit 1
fi

# Publishing always emits the version-derived toolchain tag. Only a main
# branch push may additionally move latest; a manual feature-branch dispatch
# therefore cannot replace the default floating image.
grep -Fqx '            type=raw,value=${{ env.RUST_TOOLCHAIN }}' \
    "$REPO_ROOT/.github/workflows/publish-dev-image.yml"
grep -Fqx \
    "            type=raw,value=latest,enable=\${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}" \
    "$REPO_ROOT/.github/workflows/publish-dev-image.yml"
[ "$(grep -Fc 'type=raw,value=latest' \
    "$REPO_ROOT/.github/workflows/publish-dev-image.yml")" -eq 1 ]
mkdir -p "$tmp/shared-cargo-target"
ln -s "$tmp/shared-cargo-target" "$tmp/aliased-cargo-target"
if env \
    THEKERNEL_CI_TARGET_DIR="$tmp/shared-cargo-target" \
    THEKERNEL_CI_SIBLING_TARGET_DIR="$tmp/aliased-cargo-target" \
    "$CI_DIR/per-commit.sh" --log-dir "$tmp/target-alias-log" \
    >"$tmp/target-alias.stdout" 2>"$tmp/target-alias.stderr"; then
    printf 'test-ci-scripts: aliased Cargo targets were accepted\n' >&2
    exit 1
fi
grep -Fq \
    'per-commit and maintained-sibling Cargo targets must be distinct' \
    "$tmp/target-alias.stderr"
grep -Fqx $'schema\tthekernel-ci-owned-run-v1' \
    "$tmp/target-alias-log/.thekernel-ci-owned-run"
mkdir -p "$tmp/per-commit-log-target"
printf '%s\n' preserve >"$tmp/per-commit-log-target/sentinel"
if "$CI_DIR/per-commit.sh" --log-dir "$tmp/per-commit-log-target" \
    >"$tmp/per-commit-reuse.stdout" 2>"$tmp/per-commit-reuse.stderr";
then
    printf '%s\n' 'test-ci-scripts: per-commit reused a non-empty log directory' >&2
    exit 1
fi
grep -Fq 'refusing to reuse non-empty run directory' \
    "$tmp/per-commit-reuse.stderr"
grep -Fqx preserve "$tmp/per-commit-log-target/sentinel"
ln -s "$tmp/per-commit-log-target" "$tmp/per-commit-log-alias"
if "$CI_DIR/per-commit.sh" --log-dir "$tmp/per-commit-log-alias" \
    >"$tmp/per-commit-alias.stdout" 2>"$tmp/per-commit-alias.stderr";
then
    printf '%s\n' 'test-ci-scripts: per-commit accepted a symlinked log directory' >&2
    exit 1
fi
grep -Fq 'symbolic-link component' "$tmp/per-commit-alias.stderr"

python3 "$REPO_ROOT/tests/ci/test_vendor_provenance.py"
python3 "$REPO_ROOT/tests/ci/test_mm_performance_parser.py"
python3 "$REPO_ROOT/tests/ci/test_compare_mm_performance.py"
python3 "$REPO_ROOT/tests/ci/test_mm_performance_host.py"
python3 "$REPO_ROOT/tests/ci/test_mm_performance_guest.py"
python3 "$REPO_ROOT/tests/ci/test_rootfs_image_reproducibility.py"
"$REPO_ROOT/tests/ci/test-mm-performance-boundary.sh"
"$SCRIPT_DIR/test-release-consumer-gate.sh"
"$REPO_ROOT/tests/ci/test-packet-evidence-scripts.sh"

# Filtered Rust tests must first discover a non-trivial set. Cargo considers a
# typo which matches zero tests successful, so the adapter gate cannot rely on
# the test process exit status alone.
cat >"$tmp/fake-rust-tests" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
count=${FAKE_DISCOVERED_TESTS:-24}
printf 'running %s tests\n' "$count"
printf '%s\n' executed >"$FAKE_TEST_EXECUTION"
printf 'test result: ok. %s passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n' \
    "$count"
EOF
chmod +x "$tmp/fake-rust-tests"
FAKE_TEST_EXECUTION="$tmp/filtered-test.executed" \
    "$CI_DIR/rust-filtered-test-gate.sh" \
    --minimum 24 --filter packet -- "$tmp/fake-rust-tests" \
    >"$tmp/filtered-test.out"
[ -s "$tmp/filtered-test.executed" ]
grep -Fq 'rust-filtered-test-gate: toolchain=rustc ' \
    "$tmp/filtered-test.out"
grep -Fq 'rust-filtered-test-gate: PASS executed=24 minimum=24 filter=packet' \
    "$tmp/filtered-test.out"
if FAKE_DISCOVERED_TESTS=23 FAKE_TEST_EXECUTION="$tmp/too-small.executed" \
    "$CI_DIR/rust-filtered-test-gate.sh" \
    --minimum 24 --filter packet -- "$tmp/fake-rust-tests" >/dev/null 2>&1; then
    printf '%s\n' 'test-ci-scripts: undersized filtered test set was accepted' >&2
    exit 1
fi
[ -s "$tmp/too-small.executed" ]

# Destructive CI work directories are claimed once with a regular owner marker.
# Source roots, symlink components, and non-empty reuse are rejected without
# touching the existing data.
# shellcheck source=../../scripts/ci/lib.sh
source "$CI_DIR/lib.sh"
owned_run=$(ci_prepare_owned_run_dir \
    fixture "$tmp/owned-run" "$REPO_ROOT" "$REPO_ROOT/.state")
[ "$owned_run" = "$tmp/owned-run" ]
grep -Fqx $'schema\tthekernel-ci-owned-run-v1' \
    "$owned_run/.thekernel-ci-owned-run"
printf '%s\n' preserve >"$owned_run/sentinel"
if (ci_prepare_owned_run_dir \
    fixture "$owned_run" "$REPO_ROOT" "$REPO_ROOT/.state") \
    >/dev/null 2>&1
then
    printf '%s\n' 'test-ci-scripts: non-empty owned run directory was reused' >&2
    exit 1
fi
grep -Fqx preserve "$owned_run/sentinel"
mkdir -p "$tmp/owned-target"
printf '%s\n' preserve >"$tmp/owned-target/sentinel"
ln -s "$tmp/owned-target" "$tmp/owned-alias"
if (ci_prepare_owned_run_dir \
    fixture "$tmp/owned-alias/new" "$REPO_ROOT" "$REPO_ROOT/.state") \
    >/dev/null 2>&1
then
    printf '%s\n' 'test-ci-scripts: symlinked owned run path was accepted' >&2
    exit 1
fi
grep -Fqx preserve "$tmp/owned-target/sentinel"
if (ci_prepare_owned_run_dir \
    fixture "$REPO_ROOT" "$REPO_ROOT" "$REPO_ROOT/.state") \
    >/dev/null 2>&1
then
    printf '%s\n' 'test-ci-scripts: source root was accepted as a run directory' >&2
    exit 1
fi
if grep -F 'rm -rf "$WORKDIR"' "$REPO_ROOT/scripts/system-test.sh" >/dev/null ||
    grep -F 'rm -rf "$workdir"' "$CI_DIR/boot-shell-gate.sh" >/dev/null ||
    grep -F 'rm -rf "$work_dir"' "$CI_DIR/focused-cargo-test.sh" >/dev/null ||
    grep -F 'rm -rf "$run_dir"' "$CI_DIR/nightly/lib.sh" >/dev/null
then
    printf '%s\n' 'test-ci-scripts: a direct gate still recursively clears caller paths' >&2
    exit 1
fi

# The developer container must see maintained sibling checkouts at the exact
# absolute paths produced by Cargo's ../thekernel-* patch dependencies. Keep
# this test Docker-free by capturing the final compose invocation.
dev_fixture="$tmp/dev-fixture"
mkdir -p \
    "$dev_fixture/scripts" \
    "$dev_fixture/dev-env" \
    "$dev_fixture/fake-bin" \
    "$tmp/dev-ax" \
    "$tmp/dev-linux-abi-primary" \
    "$tmp/dev-rootfs"
cp "$REPO_ROOT/scripts/dev-shell.sh" "$dev_fixture/scripts/"
: >"$dev_fixture/dev-env/versions.env"
: >"$dev_fixture/dev-env/compose.yaml"
: >"$tmp/dev-ax/Cargo.toml"
git -C "$dev_fixture" init --quiet
: >"$tmp/dev-linux-abi-primary/Cargo.toml"
git -C "$tmp/dev-linux-abi-primary" init --quiet
git -C "$tmp/dev-linux-abi-primary" add Cargo.toml
git -C "$tmp/dev-linux-abi-primary" \
    -c user.name=CI -c user.email=ci@example.invalid \
    commit --quiet -m fixture
git -C "$tmp/dev-linux-abi-primary" worktree add --quiet --detach \
    "$tmp/dev-linux-abi" HEAD
# The linked checkout must carry the package manifest expected by dev-shell.
[ -f "$tmp/dev-linux-abi/Cargo.toml" ]
cat >"$dev_fixture/fake-bin/docker" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$DEV_SHELL_DOCKER_ARGS"
EOF
chmod +x "$dev_fixture/fake-bin/docker"
env \
    PATH="$dev_fixture/fake-bin:$PATH" \
    DEV_SHELL_DOCKER_ARGS="$tmp/dev-shell.args" \
    THEKERNEL_ROOTFS_HOST_DIR="$tmp/dev-rootfs" \
    THEKERNEL_AX_REPO="$tmp/dev-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/dev-linux-abi" \
    "$dev_fixture/scripts/dev-shell.sh" -- make kernel-rv
grep -Fxq -- "$tmp/dev-ax:/thekernel-ax:ro,z" "$tmp/dev-shell.args"
grep -Fxq -- "$tmp/dev-linux-abi:/thekernel-linux-abi:ro,z" \
    "$tmp/dev-shell.args"
grep -Fxq -- \
    "$tmp/dev-linux-abi-primary/.git:$tmp/dev-linux-abi-primary/.git:ro,z" \
    "$tmp/dev-shell.args"
grep -Fxq -- 'make' "$tmp/dev-shell.args"
grep -Fxq -- 'kernel-rv' "$tmp/dev-shell.args"
if env \
    PATH="$dev_fixture/fake-bin:$PATH" \
    DEV_SHELL_DOCKER_ARGS="$tmp/dev-shell-missing.args" \
    THEKERNEL_ROOTFS_HOST_DIR="$tmp/dev-rootfs" \
    THEKERNEL_AX_REPO="$tmp/missing-dev-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/dev-linux-abi" \
    "$dev_fixture/scripts/dev-shell.sh" -- true >/dev/null 2>&1; then
    printf 'test-ci-scripts: dev shell accepted a missing maintained sibling\n' >&2
    exit 1
fi
[ ! -e "$tmp/dev-shell-missing.args" ]

# Hosted CI must consume exact, retrievable sibling commits without mutating an
# arbitrary pre-existing checkout. Local repositories keep this deterministic
# and exercise the same fetch-by-commit path as the hosted jobs.
make_sibling_fixture() {
    local path=$1
    local package=$2
    mkdir -p "$path/src"
    git -C "$path" init --quiet
    printf '[package]\nname = "%s"\nversion = "0.1.0"\n' "$package" \
        >"$path/Cargo.toml"
    printf '#![no_std]\n' >"$path/src/lib.rs"
    git -C "$path" add Cargo.toml src/lib.rs
    git -C "$path" -c user.name=CI -c user.email=ci@example.invalid \
        commit --quiet -m fixture
    git -C "$path" rev-parse HEAD
}

ax_fixture_head=$(make_sibling_fixture "$tmp/source-ax" thekernel-ax-fixture)
linux_abi_fixture_head=$(
    make_sibling_fixture "$tmp/source-linux-abi" thekernel-linux-abi-fixture
)
provision_env=(
    THEKERNEL_AX_REPOSITORY="$tmp/source-ax"
    THEKERNEL_AX_REF="$ax_fixture_head"
    THEKERNEL_LINUX_ABI_REPOSITORY="$tmp/source-linux-abi"
    THEKERNEL_LINUX_ABI_REF="$linux_abi_fixture_head"
    THEKERNEL_AX_REPO="$tmp/checkouts/thekernel-ax"
    THEKERNEL_LINUX_ABI_REPO="$tmp/checkouts/thekernel-linux-abi"
)
env "${provision_env[@]}" \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null
env "${provision_env[@]}" \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null
[ "$(git -C "$tmp/checkouts/thekernel-ax" rev-parse HEAD)" = "$ax_fixture_head" ]
[ "$(git -C "$tmp/checkouts/thekernel-linux-abi" rev-parse HEAD)" = \
    "$linux_abi_fixture_head" ]
[ -z "$(git -C "$tmp/checkouts/thekernel-ax" status --porcelain=v1)" ]

if env "${provision_env[@]}" THEKERNEL_AX_REF=main \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null 2>&1; then
    printf 'test-ci-scripts: non-commit sibling ref was accepted\n' >&2
    exit 1
fi
mkdir -p "$tmp/unmanaged-linux-abi"
if env "${provision_env[@]}" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/unmanaged-linux-abi" \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null 2>&1; then
    printf 'test-ci-scripts: unmanaged sibling checkout was changed\n' >&2
    exit 1
fi
if THEKERNEL_AX_REPOSITORY= THEKERNEL_AX_REF="$ax_fixture_head" \
    THEKERNEL_LINUX_ABI_REPOSITORY="$tmp/source-linux-abi" \
    THEKERNEL_LINUX_ABI_REF="$linux_abi_fixture_head" \
    "$CI_DIR/provision-maintained-siblings.sh" >/dev/null 2>&1; then
    printf 'test-ci-scripts: missing sibling repository was accepted\n' >&2
    exit 1
fi

# The PR source-build branch must audit exact sibling release artifacts before
# invoking make, preserve the release set under its normal artifact log tree,
# and skip both operations when --skip-build is requested.
pr_fixture="$tmp/pr-fixture"
mkdir -p "$pr_fixture/scripts/ci" "$pr_fixture/fake-bin"
cp \
    "$CI_DIR/pr-gate.sh" \
    "$CI_DIR/pr-gate-evidence.sh" \
    "$CI_DIR/exact-source-lib.sh" \
    "$CI_DIR/verify-pr-gate-evidence.sh" \
    "$CI_DIR/lib.sh" \
    "$pr_fixture/scripts/ci/"
cat >"$pr_fixture/scripts/ci/release-consumer-gate.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'release-consumer %s\n' "$*" >>"$PR_FIXTURE_TRACE"
output=
while (($#)); do
    case "$1" in
        --output-release-set) output=${2:-}; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$output" ]
mkdir -p "$(dirname -- "$output")"
printf 'package\tversion\tsha256\trepository_head\n' >"$output"
EOF
cat >"$pr_fixture/scripts/ci/boot-shell-gate.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'boot %s\n' "$*" >>"$PR_FIXTURE_TRACE"
log_dir=
while (($#)); do
    case "$1" in
        --log-dir) log_dir=${2:-}; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$log_dir" ]
mkdir -p "$log_dir"
printf 'step\tstatus\texit_code\tlog\n' >"$log_dir/status.tsv"
for arch in rv la; do
    mkdir -p "$log_dir/$arch"
    printf 'shell\n' >"$log_dir/$arch.commands"
    printf 'CI_BOOT_GATE_PASS\n' >"$log_dir/$arch/qemu.log"
    printf '{}\n' >"$log_dir/$arch/qemu-runner-receipt.json"
done
EOF
cat >"$pr_fixture/scripts/system-test.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'system %s\n' "$*" >>"$PR_FIXTURE_TRACE"
workdir=
while (($#)); do
    case "$1" in
        --workdir) workdir=${2:-}; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$workdir" ]
mkdir -p "$workdir"
cat >"$workdir/console.log" <<'MARKERS'
THEKERNEL_PACKET_UDP_PRECONDITION_OK
THEKERNEL_PACKET_CREATE_OK
THEKERNEL_PACKET_RECEIVE_OK
THEKERNEL_PACKET_FAULT_OWNERSHIP_OK
THEKERNEL_PACKET_SEND_FLAGS_BOUNDARY accepted=OOB,MORE,DONTROUTE,EOR,CONFIRM,NOSIGNAL
THEKERNEL_PACKET_SEND_FLAGS_OK
THEKERNEL_PACKET_SEND_OK
THEKERNEL_PACKET_OPTIONS_OK
THEKERNEL_PACKET_OK
THEKERNEL_SYSTEM_TEST_PACKET_OK
MARKERS
if [ -n "${SYSTEM_CONSOLE_CRLF:-}" ]; then
    sed -i 's/$/\r/' "$workdir/console.log"
fi
if [ -n "${SYSTEM_DUPLICATE_PACKET_OK:-}" ]; then
    if [ -n "${SYSTEM_CONSOLE_CRLF:-}" ]; then
        printf 'THEKERNEL_PACKET_OK\r\n' >>"$workdir/console.log"
    else
        printf 'THEKERNEL_PACKET_OK\n' >>"$workdir/console.log"
    fi
fi
EOF
cat >"$pr_fixture/scripts/ci/clippy-gate.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'clippy %s\n' "$*" >>"$PR_FIXTURE_TRACE"
EOF
cat >"$pr_fixture/fake-bin/make" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ -n "${PR_MUTATE_RESTORE_FILE:-}" ]; then
    backup=$(mktemp)
    cp -- "$PR_MUTATE_RESTORE_FILE" "$backup"
    restore_source() {
        cp -- "$backup" "$PR_MUTATE_RESTORE_FILE"
        rm -f -- "$backup"
    }
    trap restore_source EXIT
    printf '%s\n' mutated >"$PR_MUTATE_RESTORE_FILE"
    [ "$(cat source-token)" = committed ]
fi
if [ -n "${PR_MUTATE_ORIGIN_FILE:-}" ]; then
    printf '%s\n' mutated >"$PR_MUTATE_ORIGIN_FILE"
fi
printf 'make %s\n' "$*" >>"$PR_FIXTURE_TRACE"
case "$*" in
    kernels) printf fixture >kernel-rv; printf fixture >kernel-la ;;
    'kernel-rv-shell kernel-la-shell rootfs')
        mkdir -p .state/shell .state/rootfs
        printf fixture >.state/shell/kernel-rv
        printf fixture >.state/shell/kernel-la
        printf fixture >.state/rootfs/rootfs-rv.img
        printf fixture >.state/rootfs/rootfs-la.img
        ;;
esac
EOF
chmod +x \
    "$pr_fixture/scripts/ci/release-consumer-gate.sh" \
    "$pr_fixture/scripts/ci/clippy-gate.sh" \
    "$pr_fixture/scripts/ci/boot-shell-gate.sh" \
    "$pr_fixture/scripts/system-test.sh" \
    "$pr_fixture/fake-bin/make"

printf '%s\n' /kernel-rv /kernel-la /.state >"$pr_fixture/.gitignore"
printf '%s\n' committed >"$pr_fixture/source-token"
git -C "$pr_fixture" init --quiet
git -C "$pr_fixture" add .gitignore source-token scripts fake-bin
git -C "$pr_fixture" -c user.name=CI -c user.email=ci@example.invalid \
    commit --quiet -m fixture
for sibling in pr-ax pr-linux-abi; do
    mkdir -p "$tmp/$sibling"
    git -C "$tmp/$sibling" init --quiet
    printf '%s\n' fixture >"$tmp/$sibling/source"
    git -C "$tmp/$sibling" add source
    git -C "$tmp/$sibling" -c user.name=CI -c user.email=ci@example.invalid \
        commit --quiet -m fixture
done

pr_trace="$tmp/pr-gate.trace"
ax_exact=$(git -C "$tmp/pr-ax" rev-parse HEAD)
linux_abi_exact=$(git -C "$tmp/pr-linux-abi" rev-parse HEAD)
env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    PR_MUTATE_RESTORE_FILE="$pr_fixture/source-token" \
    THEKERNEL_AX_REF="$ax_exact" \
    THEKERNEL_LINUX_ABI_REF="$linux_abi_exact" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --log-dir "$tmp/pr-gate-logs" >/dev/null
grep -Fxq \
    "release-consumer --arch both --ax-head $ax_exact --linux-abi-head $linux_abi_exact --output-release-set $tmp/pr-gate-logs/release-consumer/release-set.tsv" \
    "$pr_trace"
# Linting precedes the release build so a lint failure costs no image.
[ "$(sed -n '1p' "$pr_trace")" = 'clippy --profile la' ]
[ "$(sed -n '2p' "$pr_trace")" = \
    "release-consumer --arch both --ax-head $ax_exact --linux-abi-head $linux_abi_exact --output-release-set $tmp/pr-gate-logs/release-consumer/release-set.tsv" ]
[ "$(sed -n '3p' "$pr_trace")" = 'make kernels' ]
[ "$(sed -n '4p' "$pr_trace")" = 'make kernel-rv-shell kernel-la-shell rootfs' ]
grep -Fq 'clippy --profile la' "$pr_trace"
grep -Fq 'boot --arch both --skip-build' "$pr_trace"
grep -Fq \
    "system --arch rv --skip-build --timeout 300 --workdir $tmp/pr-gate-logs/system/rv" \
    "$pr_trace"
grep -Fq \
    "system --arch la --skip-build --timeout 300 --workdir $tmp/pr-gate-logs/system/la" \
    "$pr_trace"
[ -s "$tmp/pr-gate-logs/release-consumer/release-set.tsv" ]
grep -q $'^release-consumer\tpass\t0\t' "$tmp/pr-gate-logs/status.tsv"
grep -Fqx $'result\tPASS' "$tmp/pr-gate-logs/evidence/receipt.tsv"
grep -Fqx $'release_evidence\tYES' "$tmp/pr-gate-logs/evidence/receipt.tsv"
grep -Fqx $'result\tPASS' "$tmp/pr-gate-logs/evidence/gate-envelope.tsv"
grep -Fqx $'release_qualified\tYES' \
    "$tmp/pr-gate-logs/evidence/gate-envelope.tsv"
grep -Fqx $'artifact_hashes_revalidated\tPASS' \
    "$tmp/pr-gate-logs/evidence/receipt.tsv"
grep -Fqx $'source_execution\tcommit-materialized' \
    "$tmp/pr-gate-logs/evidence/receipt.tsv"
"$CI_DIR/verify-pr-gate-evidence.sh" \
    "$tmp/pr-gate-logs/evidence" >/dev/null
"$CI_DIR/verify-pr-gate-evidence.sh" --require-release-pass \
    "$tmp/pr-gate-logs/evidence" >/dev/null
"$tmp/pr-gate-logs/evidence/verify.sh" \
    "$tmp/pr-gate-logs/evidence" >/dev/null
"$tmp/pr-gate-logs/evidence/verify.sh" --require-release-pass \
    "$tmp/pr-gate-logs/evidence" >/dev/null
[ "$(cat "$pr_fixture/source-token")" = committed ]
for bundled_path in \
    bundle/logs/release-kernels.log \
    bundle/logs/boot/status.tsv \
    bundle/logs/boot/rv.commands \
    bundle/logs/system/rv/console.log \
    bundle/products/rootfs-la.img
do
    grep -Fq $'\t'"$bundled_path" \
        "$tmp/pr-gate-logs/evidence/artifacts.tsv"
    grep -Fq "  $bundled_path" \
        "$tmp/pr-gate-logs/evidence/checksums.sha256"
done

# The materialized child may finish successfully while the caller's origin
# changes. Only the outer wrapper can publish the terminal result: it must emit
# a checksum-covered FAIL envelope and must not leak a canonical PASS line.
set +e
env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    PR_MUTATE_ORIGIN_FILE="$pr_fixture/source-token" \
    THEKERNEL_AX_REF="$ax_exact" \
    THEKERNEL_LINUX_ABI_REF="$linux_abi_exact" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --log-dir "$tmp/pr-gate-origin-change-logs" \
    >"$tmp/pr-gate-origin-change.out" 2>&1
status=$?
set -e
printf '%s\n' committed >"$pr_fixture/source-token"
[ "$status" -eq 1 ]
if grep -Fqx 'PR gate: PASS' "$tmp/pr-gate-origin-change.out"; then
    printf '%s\n' 'test-ci-scripts: inner PR gate published terminal PASS' >&2
    exit 1
fi
grep -Fq 'PR gate: FAIL reason=origin-source-changed' \
    "$tmp/pr-gate-origin-change.out"
grep -Fqx $'result\tFAIL' \
    "$tmp/pr-gate-origin-change-logs/evidence/gate-envelope.tsv"
grep -Fqx $'child_exit_code\t0' \
    "$tmp/pr-gate-origin-change-logs/evidence/gate-envelope.tsv"
grep -Fqx $'origin_source_revalidated\tFAIL' \
    "$tmp/pr-gate-origin-change-logs/evidence/gate-envelope.tsv"
grep -Fqx $'reason\torigin-source-changed' \
    "$tmp/pr-gate-origin-change-logs/evidence/gate-envelope.tsv"
"$tmp/pr-gate-origin-change-logs/evidence/verify.sh" \
    "$tmp/pr-gate-origin-change-logs/evidence" >/dev/null
if "$tmp/pr-gate-origin-change-logs/evidence/verify.sh" \
    --require-release-pass \
    "$tmp/pr-gate-origin-change-logs/evidence" >/dev/null 2>&1
then
    printf '%s\n' 'test-ci-scripts: origin-change evidence was release-qualified' >&2
    exit 1
fi
(cd "$tmp/pr-gate-origin-change-logs/evidence" && \
    sha256sum -c checksums.sha256 >/dev/null)
git -C "$pr_fixture" diff --quiet -- source-token

# Every actual kernel/rootfs/log/receipt is part of the portable manifest.
# Mutating a staged kernel must make both the standard replay and verifier fail.
printf mutation >>"$tmp/pr-gate-logs/evidence/bundle/products/kernel-rv"
if (cd "$tmp/pr-gate-logs/evidence" && \
    sha256sum -c checksums.sha256 >/dev/null 2>&1); then
    printf '%s\n' 'test-ci-scripts: mutated PR artifact passed checksum replay' >&2
    exit 1
fi
if "$CI_DIR/verify-pr-gate-evidence.sh" \
    "$tmp/pr-gate-logs/evidence" >/dev/null 2>&1; then
    printf '%s\n' 'test-ci-scripts: mutated PR artifact passed bundle verification' >&2
    exit 1
fi

# Destructive log aliases are rejected before materialization. Existing data,
# including a previous owned evidence run, must remain byte-for-byte intact.
receipt_before=$(sha256sum "$tmp/pr-gate-logs/evidence/receipt.tsv" | awk '{print $1}')
if env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-gate-logs" >/dev/null 2>&1; then
    printf '%s\n' 'test-ci-scripts: PR gate overwrote owned evidence' >&2
    exit 1
fi
[ "$(sha256sum "$tmp/pr-gate-logs/evidence/receipt.tsv" | awk '{print $1}')" = \
    "$receipt_before" ]

mkdir -p "$tmp/pr-unowned-logs"
printf '%s\n' preserve >"$tmp/pr-unowned-logs/sentinel"
if env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-unowned-logs" >/dev/null 2>&1; then
    printf '%s\n' 'test-ci-scripts: PR gate accepted unowned non-empty logs' >&2
    exit 1
fi
grep -Fqx preserve "$tmp/pr-unowned-logs/sentinel"

mkdir -p "$tmp/pr-symlink-marker-logs"
printf '%s\n' preserve >"$tmp/pr-marker-target"
ln -s "$tmp/pr-marker-target" \
    "$tmp/pr-symlink-marker-logs/.thekernel-pr-gate-owned"
if env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-symlink-marker-logs" >/dev/null 2>&1; then
    printf '%s\n' 'test-ci-scripts: PR gate accepted symlink ownership marker' >&2
    exit 1
fi
grep -Fqx preserve "$tmp/pr-marker-target"

ln -s "$pr_fixture" "$tmp/pr-source-alias"
mkdir -p "$tmp/pr-external-target"
ln -s "$tmp/pr-external-target" "$tmp/pr-external-alias"
if env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-external-alias" >/dev/null 2>&1;
then
    printf '%s\n' 'test-ci-scripts: PR gate accepted an external symlink log directory' >&2
    exit 1
fi
if find "$tmp/pr-external-target" -mindepth 1 -print -quit | grep -q .; then
    printf '%s\n' 'test-ci-scripts: external symlink target was modified' >&2
    exit 1
fi
for unsafe_log in / "$pr_fixture" "$pr_fixture/nested-log" \
    "$tmp/pr-source-alias" "$tmp/pr-source-alias/nested-log" \
    "$tmp/pr-ax" "$tmp/pr-ax/nested-log" \
    "$tmp/pr-linux-abi" "$tmp/pr-linux-abi/nested-log"
do
    if env PATH="$pr_fixture/fake-bin:$PATH" \
        PR_FIXTURE_TRACE="$pr_trace" \
        THEKERNEL_AX_REPO="$tmp/pr-ax" \
        THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
        "$pr_fixture/scripts/ci/pr-gate.sh" \
        --skip-build --log-dir "$unsafe_log" >/dev/null 2>&1; then
        printf 'test-ci-scripts: PR gate accepted unsafe log path: %s\n' \
            "$unsafe_log" >&2
        exit 1
    fi
done

: >"$pr_trace"
if env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REF=main \
    THEKERNEL_LINUX_ABI_REF="$linux_abi_exact" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --log-dir "$tmp/pr-gate-invalid-ref-logs" >/dev/null 2>&1; then
    printf 'test-ci-scripts: PR gate accepted a non-exact sibling ref\n' >&2
    exit 1
fi
[ ! -s "$pr_trace" ]

for reuse_path in \
    kernel-rv kernel-la \
    .state/shell/kernel-rv .state/shell/kernel-la \
    .state/rootfs/rootfs-rv.img .state/rootfs/rootfs-la.img
do
    mkdir -p -- "$(dirname -- "$pr_fixture/$reuse_path")"
    printf fixture >"$pr_fixture/$reuse_path"
done
if env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_PR_GATE_MATERIALIZED=1 \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-gate-unverified-internal" \
    >/dev/null 2>&1; then
    printf '%s\n' 'test-ci-scripts: unverified internal PR gate was accepted' >&2
    exit 1
fi
[ ! -e "$tmp/pr-gate-unverified-internal" ]
env -u THEKERNEL_AX_REF -u THEKERNEL_LINUX_ABI_REF \
    PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-gate-skip-logs" >/dev/null
# clippy-la, dual-arch-boot, system-rv, system-la.
[ "$(wc -l <"$pr_trace")" -eq 4 ]
grep -Fq 'clippy --profile la' "$pr_trace"
grep -Fq 'boot --arch both --skip-build' "$pr_trace"
grep -Fq 'system --arch rv --skip-build' "$pr_trace"
grep -Fq 'system --arch la --skip-build' "$pr_trace"
if grep -Eq '^(release-consumer|make) ' "$pr_trace"; then
    printf 'test-ci-scripts: --skip-build ran release or source build\n' >&2
    exit 1
fi
grep -Fqx $'result\tPASS' "$tmp/pr-gate-skip-logs/evidence/receipt.tsv"
grep -Fqx $'release_evidence\tNO' "$tmp/pr-gate-skip-logs/evidence/receipt.tsv"
grep -Fqx $'result\tPASS' \
    "$tmp/pr-gate-skip-logs/evidence/gate-envelope.tsv"
grep -Fqx $'release_qualified\tNO' \
    "$tmp/pr-gate-skip-logs/evidence/gate-envelope.tsv"
grep -Fqx $'reason\treuse-non-release' \
    "$tmp/pr-gate-skip-logs/evidence/gate-envelope.tsv"
if "$tmp/pr-gate-skip-logs/evidence/verify.sh" --require-release-pass \
    "$tmp/pr-gate-skip-logs/evidence" >/dev/null 2>&1
then
    printf '%s\n' 'test-ci-scripts: reused artifacts became release evidence' >&2
    exit 1
fi

# QEMU console lines use CRLF in practice. Logical exact-once validation strips
# one transport CR without weakening marker or boundary cardinality.
: >"$pr_trace"
env -u THEKERNEL_AX_REF -u THEKERNEL_LINUX_ABI_REF \
    PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    SYSTEM_CONSOLE_CRLF=1 \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-gate-crlf-logs" >/dev/null
grep -Fqx $'result\tPASS' "$tmp/pr-gate-crlf-logs/evidence/receipt.tsv"
grep -Fqx $'rv_packet_markers\tPASS' \
    "$tmp/pr-gate-crlf-logs/evidence/receipt.tsv"
grep -Fqx $'la_packet_markers\tPASS' \
    "$tmp/pr-gate-crlf-logs/evidence/receipt.tsv"

set +e
env -u THEKERNEL_AX_REF -u THEKERNEL_LINUX_ABI_REF \
    PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    SYSTEM_CONSOLE_CRLF=1 \
    SYSTEM_DUPLICATE_PACKET_OK=1 \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-gate-crlf-duplicate-logs" \
    >/dev/null 2>&1
status=$?
set -e
[ "$status" -eq 1 ]
grep -Fqx $'result\tFAIL' \
    "$tmp/pr-gate-crlf-duplicate-logs/evidence/receipt.tsv"
grep -Eq $'^(rv|la)_packet_markers\tFAIL$' \
    "$tmp/pr-gate-crlf-duplicate-logs/evidence/receipt.tsv"

# Successful child steps cannot promote an incomplete semantic console. This
# fixture intentionally omits PACKET_OPTIONS and must be downgraded by the
# final evidence validator.
sed '/THEKERNEL_PACKET_OPTIONS_OK/d' \
    "$pr_fixture/scripts/system-test.sh" >"$tmp/incomplete-system-test.sh"
cp "$tmp/incomplete-system-test.sh" "$pr_fixture/scripts/system-test.sh"
git -C "$pr_fixture" add scripts/system-test.sh
git -C "$pr_fixture" -c user.name=CI -c user.email=ci@example.invalid \
    commit --quiet -m incomplete-marker-fixture
set +e
env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-gate-marker-fail-logs" >/dev/null 2>&1
status=$?
set -e
[ "$status" -eq 1 ]
grep -Fqx $'result\tFAIL' "$tmp/pr-gate-marker-fail-logs/evidence/receipt.tsv"
grep -Fqx $'command_exit_code\t0' \
    "$tmp/pr-gate-marker-fail-logs/evidence/receipt.tsv"
grep -Fqx $'effective_exit_code\t1' \
    "$tmp/pr-gate-marker-fail-logs/evidence/receipt.tsv"
grep -Eq $'^(rv|la)_packet_markers\tFAIL$' \
    "$tmp/pr-gate-marker-fail-logs/evidence/receipt.tsv"
"$tmp/pr-gate-marker-fail-logs/evidence/verify.sh" \
    "$tmp/pr-gate-marker-fail-logs/evidence" \
    >"$tmp/pr-gate-marker-fail.verify"
grep -Fq 'PR evidence verifier: INTEGRITY_OK' \
    "$tmp/pr-gate-marker-fail.verify"
if grep -Fq 'PR evidence verifier: PASS' \
    "$tmp/pr-gate-marker-fail.verify"; then
    printf '%s\n' 'test-ci-scripts: integrity-only verification printed PASS' >&2
    exit 1
fi
if "$tmp/pr-gate-marker-fail-logs/evidence/verify.sh" \
    --require-release-pass "$tmp/pr-gate-marker-fail-logs/evidence" \
    >/dev/null 2>&1
then
    printf '%s\n' 'test-ci-scripts: failed semantic evidence became releasable' >&2
    exit 1
fi

# A failed gate still writes a FAIL receipt and preserves the artifact census.
cat >"$pr_fixture/scripts/system-test.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'system %s\n' "$*" >>"$PR_FIXTURE_TRACE"
exit 9
EOF
chmod +x "$pr_fixture/scripts/system-test.sh"
git -C "$pr_fixture" add scripts/system-test.sh
git -C "$pr_fixture" -c user.name=CI -c user.email=ci@example.invalid \
    commit --quiet -m failing-fixture
set +e
env PATH="$pr_fixture/fake-bin:$PATH" \
    PR_FIXTURE_TRACE="$pr_trace" \
    THEKERNEL_AX_REPO="$tmp/pr-ax" \
    THEKERNEL_LINUX_ABI_REPO="$tmp/pr-linux-abi" \
    "$pr_fixture/scripts/ci/pr-gate.sh" \
    --skip-build --log-dir "$tmp/pr-gate-fail-logs" >/dev/null 2>&1
status=$?
set -e
[ "$status" -eq 9 ]
grep -Fqx $'result\tFAIL' "$tmp/pr-gate-fail-logs/evidence/receipt.tsv"
grep -Fqx $'command_exit_code\t9' "$tmp/pr-gate-fail-logs/evidence/receipt.tsv"
"$tmp/pr-gate-fail-logs/evidence/verify.sh" \
    "$tmp/pr-gate-fail-logs/evidence" >/dev/null

# The semantic system gate accepts the runner's intentional-stop status only
# after the exact final marker is written, then validates the complete marker
# sequence from the captured console log.
system_fixture="$tmp/system-fixture"
mkdir -p \
    "$system_fixture/scripts/ci" \
    "$system_fixture/scripts/ci/differential/manifests" \
    "$system_fixture/fake-bin" \
    "$system_fixture/.state/rootfs"
cp "$REPO_ROOT/scripts/system-test.sh" "$system_fixture/scripts/"
cp "$CI_DIR/lib.sh" "$system_fixture/scripts/ci/"
cp "$CI_DIR/differential/manifests/futex.markers" \
    "$CI_DIR/differential/manifests/epoll-guest.markers" \
    "$CI_DIR/differential/manifests/signal-order.markers" \
    "$system_fixture/scripts/ci/differential/manifests/"
printf fixture >"$system_fixture/kernel-rv"
printf fixture >"$system_fixture/kernel-la"
printf fixture >"$system_fixture/.state/rootfs/rootfs-rv.img"
printf fixture >"$system_fixture/.state/rootfs/rootfs-la.img"
cat >"$system_fixture/fake-bin/python3" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$@" >"$FAKE_SYSTEM_RUNNER_ARGS"
workdir=
arch=
cpus=
memory=
while (($#)); do
    case "$1" in
        --arch) arch=${2:-}; shift 2 ;;
        --cpus) cpus=${2:-}; shift 2 ;;
        --memory) memory=${2:-}; shift 2 ;;
        --workdir) workdir=${2:-}; shift 2 ;;
        *) shift ;;
    esac
done
[ -n "$workdir" ]
[ -n "$arch" ]
[ -n "$cpus" ]
[ -n "$memory" ]
mkdir -p "$workdir"
cat >"$workdir/console.log" <<MARKERS_BEFORE_SETRLIMIT
THEKERNEL_SYSTEM_TEST_INIT_EXEC_1_OK
THEKERNEL_SYSTEM_TEST_INIT_EXEC_2_OK
THEKERNEL_SYSTEM_TEST_START
THEKERNEL_SYSTEM_TEST_MOUNTS_OK
THEKERNEL_SYSTEM_TEST_ROOTFS_OK
THEKERNEL_SYSTEM_TEST_TMPFS_OK
THEKERNEL_SYSTEM_TEST_PROCFS_OK
THEKERNEL_SYSTEM_TEST_MM_PRESSURE_OK
THEKERNEL_MM_PRESSURE_WORKER_OK
THEKERNEL_MM_PRESSURE_RECLAIM_OK
THEKERNEL_MM_PRESSURE_OK
THEKERNEL_SYSTEM_TEST_MM_PRESSURE_RECLAIM_OK
THEKERNEL_SYSTEM_TEST_PROCESS_OK
THEKERNEL_EXEC_SMOKE_OK
THEKERNEL_SYSTEM_TEST_EXEC_OK
CI_SIGNAL_WAIT_BOUNDARY_PASS
THEKERNEL_SYSTEM_TEST_SIGNAL_WAIT_OK
CI_WAIT_BOUNDARY_CLOCK_PERCPU_OK online_cpus=$cpus
CI_WAIT_BOUNDARY_TIMERFD_CANCEL_OK
CI_WAIT_BOUNDARY_ITIMER_PERIODIC_OK min_hits=3
CI_WAIT_BOUNDARY_ITIMER_CPU_OK no_syscall_loop=1
CI_WAIT_BOUNDARY_RLIMIT_CPU_ESCALATION_OK soft_after_signal=2 hard_signal=SIGKILL
CI_WAIT_BOUNDARY_RLIMIT_CPU_HARD_ONLY_OK signal=SIGKILL sigxcpu=0
CI_WAIT_BOUNDARY_PRLIMIT_PRECEDENCE_OK bad_new=EFAULT bad_pid_before_resource=ESRCH
CI_WAIT_BOUNDARY_PRLIMIT_TRANSACTION_OK old_new=atomic invalid=rollback copyout_fault=committed
MARKERS_BEFORE_SETRLIMIT
case "$arch" in
    rv) printf '%s\n' 'CI_WAIT_BOUNDARY_SETRLIMIT_PRECEDENCE_OK bad_new=EFAULT' ;;
    la) printf '%s\n' 'CI_WAIT_BOUNDARY_SETRLIMIT_PRECEDENCE_NA syscall=absent' ;;
    *) exit 2 ;;
esac >>"$workdir/console.log"
cat >>"$workdir/console.log" <<'MARKERS_AFTER_WAIT'
CI_WAIT_BOUNDARY_SETITIMER_PRECEDENCE_OK bad_new=EFAULT
CI_WAIT_BOUNDARY_FUTEX_WAKE_OK
CI_WAIT_BOUNDARY_FUTEX_TIMEOUT_OK
CI_WAIT_BOUNDARY_FUTEX_WAITV_OK
CI_WAIT_BOUNDARY_PASS
THEKERNEL_SYSTEM_TEST_WAIT_BOUNDARY_OK
MARKERS_AFTER_WAIT
fixture_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
cat \
    "$fixture_root/scripts/ci/differential/manifests/futex.markers" \
    "$fixture_root/scripts/ci/differential/manifests/epoll-guest.markers" \
    "$fixture_root/scripts/ci/differential/manifests/signal-order.markers" \
    >>"$workdir/console.log"
cat >>"$workdir/console.log" <<'MARKERS_AFTER_DIFFERENTIAL'
THEKERNEL_SYSTEM_TEST_FUTEX_DIFFERENTIAL_OK
THEKERNEL_SYSTEM_TEST_EPOLL_DIFFERENTIAL_OK
THEKERNEL_SYSTEM_TEST_SIGNAL_ORDER_DIFFERENTIAL_OK
THEKERNEL_IO_URING_OK
THEKERNEL_SYSTEM_TEST_IO_URING_OK
THEKERNEL_USERFAULTFD_API_OK
THEKERNEL_USERFAULTFD_REGISTER_OK
THEKERNEL_USERFAULTFD_COPY_WP_ERROR_OK
THEKERNEL_USERFAULTFD_COPY_OK
THEKERNEL_USERFAULTFD_ZEROPAGE_OK
THEKERNEL_USERFAULTFD_DONTWAKE_WAKE_OK
THEKERNEL_USERFAULTFD_ERROR_OUTPUT_OK
THEKERNEL_USERFAULTFD_PARTIAL_OK
THEKERNEL_USERFAULTFD_COPYOUT_FAULT_OK
THEKERNEL_USERFAULTFD_EXEC_COPY_OK
THEKERNEL_USERFAULTFD_OK
THEKERNEL_SYSTEM_TEST_USERFAULTFD_OK
THEKERNEL_PACKET_UDP_PRECONDITION_OK
THEKERNEL_PACKET_CREATE_OK
THEKERNEL_PACKET_RECEIVE_OK
THEKERNEL_PACKET_FAULT_OWNERSHIP_OK
THEKERNEL_PACKET_SEND_FLAGS_BOUNDARY accepted=OOB,MORE,DONTROUTE,EOR,CONFIRM,NOSIGNAL
THEKERNEL_PACKET_SEND_FLAGS_OK
THEKERNEL_PACKET_SEND_OK
THEKERNEL_PACKET_OPTIONS_OK
THEKERNEL_PACKET_OK
THEKERNEL_SYSTEM_TEST_PACKET_OK
THEKERNEL_SECCOMP_API_OK
THEKERNEL_SECCOMP_FILTER_ERRORS_OK
THEKERNEL_SECCOMP_UNALIGNED_OK
THEKERNEL_SECCOMP_FILTER_OK
THEKERNEL_SECCOMP_ERRNO_OK
THEKERNEL_SECCOMP_FASTPATH_OK
THEKERNEL_SECCOMP_UNKNOWN_OK
THEKERNEL_SECCOMP_ERRNO_ZERO_OK
THEKERNEL_SECCOMP_LOG_OK
THEKERNEL_SECCOMP_TRAP_OK
THEKERNEL_SECCOMP_TRAP_ROLLBACK_OK
THEKERNEL_SECCOMP_INHERIT_OK
THEKERNEL_SECCOMP_THREAD_APPEND_ISOLATION_OK
THEKERNEL_SECCOMP_FORK_APPEND_ISOLATION_OK
THEKERNEL_SECCOMP_PROC_OK
THEKERNEL_SECCOMP_EXEC_OK
THEKERNEL_SECCOMP_STRICT_OK
THEKERNEL_SECCOMP_PRCTL_STRICT_OK
THEKERNEL_SECCOMP_STRICT_KILL_OK
THEKERNEL_SECCOMP_UNSUPPORTED_OK
THEKERNEL_SECCOMP_KILL_THREAD_OK
THEKERNEL_SECCOMP_KILL_PROCESS_OK
THEKERNEL_SECCOMP_KILL_UNKNOWN_OK
THEKERNEL_SECCOMP_KILL_SCOPE_OK
THEKERNEL_SECCOMP_EXIT_RECLAIM_OK
THEKERNEL_SECCOMP_RESOURCE_OK
THEKERNEL_SECCOMP_RESOURCE_ROLLBACK_OK
THEKERNEL_SECCOMP_OK
THEKERNEL_SYSTEM_TEST_SECCOMP_OK
THEKERNEL_SYSTEM_TEST_PASS
MARKERS_AFTER_DIFFERENTIAL
exit "${FAKE_SYSTEM_RUNNER_STATUS:-0}"
EOF
chmod +x "$system_fixture/fake-bin/python3"
cat >"$system_fixture/fake-bin/make" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'THEKERNEL_KERNEL_ASID_FAST_SWITCH=%s\n' \
    "${THEKERNEL_KERNEL_ASID_FAST_SWITCH:-}" >"$FAKE_SYSTEM_MAKE_ARGS"
printf '%s\n' "$@" >>"$FAKE_SYSTEM_MAKE_ARGS"
EOF
chmod +x "$system_fixture/fake-bin/make"
if env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$tmp/system-unsafe.args" \
    "$system_fixture/scripts/system-test.sh" \
    --arch rv --skip-build --workdir "$system_fixture" >/dev/null 2>&1
then
    printf '%s\n' 'test-ci-scripts: system gate accepted its source root as workdir' >&2
    exit 1
fi
[ -s "$system_fixture/kernel-rv" ]
system_args="$tmp/system-runner.args"
env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$system_args" \
    FAKE_SYSTEM_RUNNER_STATUS=75 \
    "$system_fixture/scripts/system-test.sh" \
    --arch rv --skip-build --workdir "$tmp/system-run" >/dev/null
grep -Fqx $'owner\tsystem-test-rv' \
    "$tmp/system-run/.thekernel-ci-owned-run"
grep -Fxq -- '--stop-after-marker' "$system_args"
grep -Fxq -- 'THEKERNEL_SYSTEM_TEST_PASS' "$system_args"
grep -Fxq -- '--cpus' "$system_args"
grep -Fxq -- '1' "$system_args"
grep -Fxq -- '--memory' "$system_args"
grep -Fxq -- '128M' "$system_args"
system_args_la="$tmp/system-runner-la.args"
env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$system_args_la" \
    FAKE_SYSTEM_RUNNER_STATUS=75 \
    "$system_fixture/scripts/system-test.sh" \
    --arch la --skip-build --workdir "$tmp/system-run-la" >/dev/null
grep -Fxq -- '--cpus' "$system_args_la"
grep -Fxq -- '1' "$system_args_la"
grep -Fxq -- '--memory' "$system_args_la"
grep -Fxq -- '256M' "$system_args_la"
grep -Fxq -- 'la' "$system_args_la"
set +e
env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$tmp/system-invalid-la-memory.args" \
    "$system_fixture/scripts/system-test.sh" \
    --arch la --memory 128M --skip-build \
    --workdir "$tmp/system-invalid-la-memory" >/dev/null 2>&1
status=$?
set -e
[ "$status" -eq 2 ]
system_args_smp="$tmp/system-runner-smp.args"
system_make_args="$tmp/system-make-smp.args"
env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$system_args_smp" \
    FAKE_SYSTEM_MAKE_ARGS="$system_make_args" \
    FAKE_SYSTEM_RUNNER_STATUS=75 \
    "$system_fixture/scripts/system-test.sh" \
    --arch rv --cpus 4 --workdir "$tmp/system-run-smp" >/dev/null
grep -Fxq -- 'SMP=4' "$system_make_args"
grep -Fxq -- 'MEM=128M' "$system_make_args"
grep -Fxq -- 'kernel-rv' "$system_make_args"
grep -Fxq -- 'rootfs-rv' "$system_make_args"
grep -Fxq -- '--cpus' "$system_args_smp"
grep -Fxq -- '4' "$system_args_smp"
grep -Fxq -- '--memory' "$system_args_smp"
grep -Fxq -- '128M' "$system_args_smp"
system_args_asid="$tmp/system-runner-asid.args"
system_make_args_asid="$tmp/system-make-asid.args"
env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$system_args_asid" \
    FAKE_SYSTEM_MAKE_ARGS="$system_make_args_asid" \
    FAKE_SYSTEM_RUNNER_STATUS=75 \
    "$system_fixture/scripts/system-test.sh" \
    --arch rv --asid-fast-switch \
    --workdir "$tmp/system-run-asid" >/dev/null
grep -Fxq -- 'THEKERNEL_KERNEL_ASID_FAST_SWITCH=1' "$system_make_args_asid"
set +e
env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$tmp/system-invalid-asid-skip.args" \
    "$system_fixture/scripts/system-test.sh" \
    --arch rv --asid-fast-switch --skip-build \
    --workdir "$tmp/system-invalid-asid-skip" >/dev/null 2>&1
status=$?
set -e
[ "$status" -eq 2 ]
for invalid_cpus in 0 4097 not-a-number; do
    set +e
    env PATH="$system_fixture/fake-bin:$PATH" \
        FAKE_SYSTEM_RUNNER_ARGS="$tmp/system-invalid-$invalid_cpus.args" \
        "$system_fixture/scripts/system-test.sh" \
        --arch rv --cpus "$invalid_cpus" --skip-build \
        --workdir "$tmp/system-invalid-$invalid_cpus" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 2 ]; then
        printf 'test-ci-scripts: invalid CPU count %s returned %s, expected 2\n' \
            "$invalid_cpus" "$status" >&2
        exit 1
    fi
done
for invalid_memory in 0 64M 2G 128 128MB bad; do
    set +e
    env PATH="$system_fixture/fake-bin:$PATH" \
        FAKE_SYSTEM_RUNNER_ARGS="$tmp/system-invalid-memory-$invalid_memory.args" \
        "$system_fixture/scripts/system-test.sh" \
        --arch rv --memory "$invalid_memory" --skip-build \
        --workdir "$tmp/system-invalid-memory-$invalid_memory" >/dev/null 2>&1
    status=$?
    set -e
    if [ "$status" -ne 2 ]; then
        printf 'test-ci-scripts: invalid memory %s returned %s, expected 2\n' \
            "$invalid_memory" "$status" >&2
        exit 1
    fi
done
set +e
env PATH="$system_fixture/fake-bin:$PATH" \
    FAKE_SYSTEM_RUNNER_ARGS="$system_args" \
    FAKE_SYSTEM_RUNNER_STATUS=23 \
    "$system_fixture/scripts/system-test.sh" \
    --arch rv --skip-build --workdir "$tmp/system-run-fail" >/dev/null
status=$?
set -e
[ "$status" -eq 23 ] || {
    printf 'test-ci-scripts: system gate returned %s, expected 23\n' "$status" >&2
    exit 1
}

cat >"$tmp/pass.log" <<'EOF'
CI_BOOT_GATE_START
CI_BOOT_GATE_ROOTFS_OK
CI_BOOT_GATE_TMPFS_OK
CI_BOOT_GATE_PROCFS_OK
CI_BOOT_GATE_BIND_OK
CI_BOOT_GATE_PASS
System is shutting down
EOF
"$CI_DIR/validate-boot-log.sh" rv "$tmp/pass.log" >/dev/null

cp "$tmp/pass.log" "$tmp/fail.log"
printf 'CI_BOOT_GATE_FAIL injected\n' >>"$tmp/fail.log"
if "$CI_DIR/validate-boot-log.sh" la "$tmp/fail.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: failure marker was accepted\n' >&2
    exit 1
fi

# Nightly guest validation must reject an explicit subsystem failure even if
# every expected success marker is also present later in the same log.
cat >"$tmp/nightly-wait-fail.log" <<'EOF'
CI_WAIT_BOUNDARY_FAIL injected errno=5 (Input/output error)
CI_WAIT_BOUNDARY_PASS
System is shutting down
EOF
if (
    # shellcheck source=../../scripts/ci/nightly/lib.sh
    source "$CI_DIR/nightly/lib.sh"
    nightly_validate_guest_log \
        "$tmp/nightly-wait-fail.log" clean CI_WAIT_BOUNDARY_PASS
) >/dev/null 2>&1; then
    printf 'test-ci-scripts: nightly validator accepted a wait-boundary failure marker\n' >&2
    exit 1
fi

cp "$tmp/pass.log" "$tmp/idle-timeout.log"
printf 'qemu-runner: QEMU idle timeout after 30s without console output\n' \
    >>"$tmp/idle-timeout.log"
if "$CI_DIR/validate-boot-log.sh" rv "$tmp/idle-timeout.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: QEMU idle timeout was accepted\n' >&2
    exit 1
fi

cp "$tmp/pass.log" "$tmp/missing.log"
sed -i '/CI_BOOT_GATE_BIND_OK/d' "$tmp/missing.log"
if "$CI_DIR/validate-boot-log.sh" rv "$tmp/missing.log" >/dev/null 2>&1; then
    printf 'test-ci-scripts: missing marker was accepted\n' >&2
    exit 1
fi

for category in pressure oom-failpoint fs-powercut nonloopback-network \
    smp-tlb-shootdown mm-performance; do
    "$CI_DIR/nightly-gate.sh" --list | grep -q "^${category}"
done

# The host-test linker must add -no-pie only to executables. Shared objects
# include proc macros and would fail to link if the C runtime expected main.
cat >"$tmp/fake-cc" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$FAKE_CC_LOG"
EOF
chmod +x "$tmp/fake-cc"
FAKE_CC_LOG="$tmp/shared-link.args" THEKERNEL_HOST_CC="$tmp/fake-cc" \
    "$CI_DIR/host-test-linker.sh" -shared input.o -o output.so
if grep -qx -- -no-pie "$tmp/shared-link.args"; then
    printf 'test-ci-scripts: shared link received -no-pie\n' >&2
    exit 1
fi
FAKE_CC_LOG="$tmp/executable-link.args" THEKERNEL_HOST_CC="$tmp/fake-cc" \
    "$CI_DIR/host-test-linker.sh" input.o -o output
grep -qx -- -no-pie "$tmp/executable-link.args"

env \
    THEKERNEL_NIGHTLY_PRESSURE_ENABLED=0 \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_ENABLED=0 \
    THEKERNEL_NIGHTLY_MM_PERFORMANCE_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly" >/dev/null

[ "$(awk -F '\t' '$2 == "skip" { count += 1 } END { print count + 0 }' "$tmp/nightly/nightly-status.tsv")" -eq 6 ]
grep -Fqx $'owner\tnightly-gate' "$tmp/nightly/.thekernel-ci-owned-run"
grep -Fqx $'owner\tnightly-steps' \
    "$tmp/nightly/steps/.thekernel-ci-owned-run"

# Configured adapters retain the same three-state contract as repository
# adapters. In particular, exit 78 must remain unsupported and make the whole
# gate return 78; it must never be rewritten to pass.
set +e
env \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND='exit 78' \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_ENABLED=0 \
    THEKERNEL_NIGHTLY_MM_PERFORMANCE_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly-unsupported" >/dev/null
status=$?
set -e
[ "$status" -eq 78 ] || {
    printf 'test-ci-scripts: unsupported adapter returned %s, expected 78\n' "$status" >&2
    exit 1
}
grep -q $'^pressure\tunsupported\t' "$tmp/nightly-unsupported/nightly-status.tsv"

set +e
env \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND='exit 9' \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_ENABLED=0 \
    THEKERNEL_NIGHTLY_MM_PERFORMANCE_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly-fail" >/dev/null
status=$?
set -e
[ "$status" -eq 1 ] || {
    printf 'test-ci-scripts: failed adapter returned %s, expected 1\n' "$status" >&2
    exit 1
}
grep -q $'^pressure\tfail\t' "$tmp/nightly-fail/nightly-status.tsv"

env \
    THEKERNEL_NIGHTLY_PRESSURE_COMMAND='exit 0' \
    THEKERNEL_NIGHTLY_OOM_FAILPOINT_ENABLED=0 \
    THEKERNEL_NIGHTLY_FS_POWERCUT_ENABLED=0 \
    THEKERNEL_NIGHTLY_NONLOOPBACK_NETWORK_ENABLED=0 \
    THEKERNEL_NIGHTLY_SMP_TLB_SHOOTDOWN_ENABLED=0 \
    THEKERNEL_NIGHTLY_MM_PERFORMANCE_ENABLED=0 \
    "$CI_DIR/nightly-gate.sh" --log-dir "$tmp/nightly-pass" >/dev/null
grep -q $'^pressure\tpass\t' "$tmp/nightly-pass/nightly-status.tsv"

set +e
env \
    THEKERNEL_NIGHTLY_ARCHES=invalid \
    "$CI_DIR/nightly/pressure.sh" >"$tmp/invalid-arch.log" 2>&1
status=$?
set -e
[ "$status" -eq 1 ] || {
    printf 'test-ci-scripts: invalid adapter architecture returned %s, expected 1\n' "$status" >&2
    exit 1
}
grep -Fq 'THEKERNEL_NIGHTLY_ARCHES must be rv, la, or both' "$tmp/invalid-arch.log"
if grep -Fq 'UNSUPPORTED' "$tmp/invalid-arch.log"; then
    printf 'test-ci-scripts: invalid adapter architecture was misclassified as unsupported\n' >&2
    exit 1
fi

if THEKERNEL_SMP_TLB_CPUS=1 \
    "$CI_DIR/nightly/smp-tlb-shootdown.sh" \
    >"$tmp/invalid-smp-tlb-cpus.log" 2>&1; then
    printf 'test-ci-scripts: SMP TLB adapter accepted a one-CPU matrix\n' >&2
    exit 1
fi
grep -Fq 'THEKERNEL_SMP_TLB_CPUS must contain values from 2 to 64' \
    "$tmp/invalid-smp-tlb-cpus.log"

# Verify the shared runner records and enforces a real wall-clock timeout.
# shellcheck source=../../scripts/ci/lib.sh
source "$CI_DIR/lib.sh"
export CI_LOG_DIR="$tmp/runner"
ci_prepare_log_dir "$CI_LOG_DIR"
ci_run_step quick-pass 5 bash -c 'printf ok' >/dev/null
if ci_run_step must-timeout 1 bash -c 'sleep 5' >/dev/null 2>&1; then
    printf 'test-ci-scripts: timeout step unexpectedly passed\n' >&2
    exit 1
else
    status=$?
    [ "$status" -eq 124 ] || {
        printf 'test-ci-scripts: timeout returned %s, expected 124\n' "$status" >&2
        exit 1
    }
fi
grep -q $'^must-timeout\ttimeout\t124\t' "$CI_LOG_DIR/status.tsv"

# The boot wrapper binds the actual bytes accepted by the runner to the staged
# command stream. SIGPIPE 141 is never an exact-evidence success, even if an
# independently recorded relay happens to match the complete source.
mkdir -p "$tmp/fake-bin" "$tmp/fake-work"
real_python3=$(command -v python3)
cat >"$tmp/fake-bin/python3" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = -m ] && [ "${2:-}" = tools.qemu_runner ] && \
    [ "${3:-}" = finalize-input ]; then
    exec "$REAL_PYTHON3" "$@"
fi
[ -z "${FAKE_QEMU_RUNNER_ARGS:-}" ] || printf '%s\n' "$@" >"$FAKE_QEMU_RUNNER_ARGS"
receipt=
previous=
for argument in "$@"; do
    if [ "$previous" = --receipt ]; then
        receipt=$argument
        break
    fi
    previous=$argument
done
[ -n "$receipt" ]
forwarded=$(mktemp)
trap 'rm -f "$forwarded"' EXIT
source_eof=true
relay_complete=true
if [ "${FAKE_INPUT_MODE:-complete}" = truncate ]; then
    IFS= read -r line || true
    printf '%s\n' "${line:-}" >"$forwarded"
    source_eof=false
    relay_complete=false
else
    cat >"$forwarded"
fi
sha256=$(sha256sum "$forwarded" | awk '{ print $1 }')
bytes=$(stat -c '%s' "$forwarded")
lines=$(awk 'END { print NR + 0 }' "$forwarded")
mkdir -p "$(dirname "$receipt")"
"$REAL_PYTHON3" - "$receipt" "$sha256" "$bytes" "$lines" \
    "$source_eof" "$relay_complete" <<'PY'
import json
import pathlib
import sys

receipt, digest, byte_count, line_count, source_eof, relay_complete = sys.argv[1:]
payload = {
    "schema_version": 2,
    "state": "awaiting_producer",
    "interaction": {"external_input_producer": True},
    "stdin": {
        "state": "awaiting_producer",
        "sha256": digest,
        "bytes": int(byte_count),
        "line_count": int(line_count),
        "observed_bytes": int(byte_count),
        "source_eof": source_eof == "true",
        "broken_pipe": False,
        "relay_complete": relay_complete == "true",
    },
}
pathlib.Path(receipt).write_text(json.dumps(payload), encoding="utf-8")
PY
exit "${FAKE_QEMU_RUNNER_STATUS:-0}"
EOF
chmod +x "$tmp/fake-bin/python3"
for _ in $(seq 1 20000); do
    printf 'echo serial-input-padding-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n'
done >"$tmp/commands"
env PATH="$tmp/fake-bin:$PATH" REAL_PYTHON3="$real_python3" \
    FAKE_QEMU_RUNNER_STATUS=0 \
    "$CI_DIR/boot-shell-runner.sh" rv /dev/null /dev/null \
    "$tmp/fake-work" "$tmp/commands" 1 1 0
if env PATH="$tmp/fake-bin:$PATH" REAL_PYTHON3="$real_python3" \
    FAKE_QEMU_RUNNER_STATUS=23 \
    "$CI_DIR/boot-shell-runner.sh" rv /dev/null /dev/null \
    "$tmp/fake-work" "$tmp/commands" 1 1 0; then
    printf 'test-ci-scripts: QEMU runner failure was hidden by pipe handling\n' >&2
    exit 1
else
    status=$?
    [ "$status" -eq 23 ] || {
        printf 'test-ci-scripts: QEMU runner failure returned %s, expected 23\n' "$status" >&2
        exit 1
    }
fi

if env PATH="$tmp/fake-bin:$PATH" REAL_PYTHON3="$real_python3" \
    FAKE_QEMU_RUNNER_STATUS=0 FAKE_INPUT_MODE=truncate \
    "$CI_DIR/boot-shell-runner.sh" rv /dev/null /dev/null \
    "$tmp/fake-work" "$tmp/commands" 1 1 0; then
    printf 'test-ci-scripts: truncated stdin stream was accepted\n' >&2
    exit 1
else
    status=$?
    [ "$status" -eq 141 ] || {
        printf 'test-ci-scripts: truncated stdin returned %s, expected 141\n' "$status" >&2
        exit 1
    }
fi

printf 'exit\n' >"$tmp/short-commands"
env PATH="$tmp/fake-bin:$PATH" REAL_PYTHON3="$real_python3" \
    FAKE_QEMU_RUNNER_STATUS=75 \
    FAKE_QEMU_RUNNER_ARGS="$tmp/qemu-runner.args" \
    THEKERNEL_QEMU_CPUS=8 \
    "$CI_DIR/boot-shell-runner.sh" rv kernel image "$tmp/fake-work" \
    "$tmp/short-commands" 1 1 0 extra.img STOP_MARKER || status=$?
[ "${status:-75}" -eq 75 ]
grep -Fxq -- '--input-after-marker' "$tmp/qemu-runner.args"
grep -Fxq -- 'THEKERNEL_SHELL_READY' "$tmp/qemu-runner.args"
awk '
    $0 == "--cpus" {
        getline
        found = ($0 == "8")
    }
    END { exit !found }
' "$tmp/qemu-runner.args"
awk '
    $0 == "--ready-timeout" {
        getline
        found = ($0 == "1")
    }
    END { exit !found }
' "$tmp/qemu-runner.args"
grep -Fxq -- '--extra-block' "$tmp/qemu-runner.args"
grep -Fxq -- 'extra.img' "$tmp/qemu-runner.args"
grep -Fxq -- '--stop-after-marker' "$tmp/qemu-runner.args"
grep -Fxq -- 'STOP_MARKER' "$tmp/qemu-runner.args"
if env PATH="$tmp/fake-bin:$PATH" THEKERNEL_QEMU_CPUS=0 \
    "$CI_DIR/boot-shell-runner.sh" rv kernel image "$tmp/fake-work" \
    "$tmp/short-commands" 1 1 0 >/dev/null 2>&1; then
    printf '%s\n' 'test-ci-scripts: runner accepted zero QEMU CPUs' >&2
    exit 1
fi

# The one-shot host peer rejects unauthenticated traffic and returns the nonce
# only after receiving the exact guest probe.
peer_port_file="$tmp/peer.port"
python3 "$CI_DIR/nightly/network-peer.py" \
    --nonce ci-unit --port-file "$peer_port_file" --timeout 10 \
    >"$tmp/peer.log" 2>&1 &
peer_pid=$!
for ((attempt = 0; attempt < 100; attempt += 1)); do
    [ -s "$peer_port_file" ] && break
    sleep 0.01
done
[ -s "$peer_port_file" ]
python3 - "$(tr -d '\r\n' <"$peer_port_file")" <<'PY'
import socket
import sys

with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5) as connection:
    connection.sendall(b"THEKERNEL_NETWORK_PROBE ci-unit\n")
    connection.shutdown(socket.SHUT_WR)
    assert connection.recv(4096) == b"THEKERNEL_NETWORK_REPLY ci-unit\n"
PY
wait "$peer_pid"
grep -Fq 'network-peer: validated guest request' "$tmp/peer.log"

# Compile the project guest helpers on the host. The guest OOM gate supplies the
# strict overcommit policy that makes its finite request fail deterministically.
cc -O2 -std=c11 -Wall -Wextra -Werror -pthread \
    "$REPO_ROOT/tests/guest/tools/mm-performance.c" \
    -o "$tmp/mm-performance"
cc -O2 -std=c11 -Wall -Wextra -Werror -pthread \
    "$REPO_ROOT/tests/guest/tools/smp-tlb-shootdown.c" \
    -o "$tmp/smp-tlb-shootdown"
cc -O2 -std=c11 -Wall -Wextra -Werror -pthread \
    "$REPO_ROOT/tests/guest/tools/wait-boundary.c" \
    -o "$tmp/wait-boundary"
"$CI_DIR/seccomp-host-differential.sh" \
    --workdir "$tmp/seccomp-host-differential" \
    --allow-inherited-profile >"$tmp/seccomp-host-differential.out"
grep -Eq '^seccomp-host-differential: (PASS|SKIP) ' \
    "$tmp/seccomp-host-differential.out"
cc -O2 -std=c11 "$REPO_ROOT/tests/guest/tools/oom-admission.c" \
    -o "$tmp/nightly-oom-admission"
"$tmp/nightly-oom-admission" --expect-success 4096 >/dev/null
"$tmp/nightly-oom-admission" --expect-failure 18446744073709551615 >/dev/null

# The SMP TLB parser requires the complete per-CPU, per-page-size case matrix
# and rejects a stale result even when a success marker remains in the log.
smp_tlb_log="$tmp/smp-tlb.log"
printf '%s\n' \
    'SMP_TLB_TOPOLOGY online_cpus=2 control_cpu=0 worker_count=1 worker_cpus=1' \
    >"$smp_tlb_log"
for window in 1 2 3; do
    printf 'SMP_TLB_LIVENESS window=%s window_ns=1000000000 cpus=2 tasks_per_cpu=2 status=ok min_delta=1\n' \
        "$window" >>"$smp_tlb_log"
done
for pages in 1 64; do
    for case_name in mprotect_revoke_write munmap_fixed_replace \
        mremap_fixed_old_alias fork_cow_snapshot; do
        printf 'SMP_TLB_CASE case=%s pages=%s worker_cpu=1 status=ok stale_count=0\n' \
            "$case_name" "$pages" >>"$smp_tlb_log"
    done
done
printf '%s\n' 'SMP_TLB_GATE status=ok stale_count=0' >>"$smp_tlb_log"
"$CI_DIR/validate-smp-tlb-log.sh" "$smp_tlb_log" 2 >/dev/null
sed \
    -e 's/control_cpu=0/control_cpu=2/' \
    -e 's/worker_cpus=1/worker_cpus=3/' \
    -e 's/worker_cpu=1/worker_cpu=3/g' \
    "$smp_tlb_log" >"$tmp/smp-tlb-out-of-range-topology.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-out-of-range-topology.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted out-of-range CPU IDs\n' >&2
    exit 1
fi
sed 's/worker_cpus=1/worker_cpus=1,/' \
    "$smp_tlb_log" >"$tmp/smp-tlb-trailing-worker.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-trailing-worker.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted a trailing worker token\n' >&2
    exit 1
fi
sed '0,/^SMP_TLB_CASE /{/^SMP_TLB_CASE /s/status=ok/status=stale/;}' \
    "$smp_tlb_log" >"$tmp/smp-tlb-stale.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-stale.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted a stale case\n' >&2
    exit 1
fi
sed '0,/^SMP_TLB_CASE /{/^SMP_TLB_CASE /d;}' \
    "$smp_tlb_log" >"$tmp/smp-tlb-incomplete.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-incomplete.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted an incomplete matrix\n' >&2
    exit 1
fi
sed '0,/^SMP_TLB_LIVENESS /{/^SMP_TLB_LIVENESS /d;}' \
    "$smp_tlb_log" >"$tmp/smp-tlb-incomplete-liveness.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-incomplete-liveness.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted incomplete liveness evidence\n' >&2
    exit 1
fi
sed '0,/min_delta=1/{s/min_delta=1/min_delta=0/;}' \
    "$smp_tlb_log" >"$tmp/smp-tlb-stalled-liveness.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-stalled-liveness.log" 2 >/dev/null 2>&1; then
    printf 'test-ci-scripts: SMP TLB parser accepted a stalled spin task\n' >&2
    exit 1
fi
sed \
    -e '0,/status=ok stale_count=0/{s/status=ok stale_count=0/status=stale stale_count=1/;}' \
    -e 's/^SMP_TLB_GATE status=ok stale_count=0$/SMP_TLB_GATE status=fail kind=stale stale_count=1/' \
    "$smp_tlb_log" >"$tmp/smp-tlb-mutation.log"
"$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-mutation.log" 2 stale >/dev/null
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$smp_tlb_log" 2 stale >/dev/null 2>&1; then
    printf 'test-ci-scripts: mutation parser accepted a clean positive log\n' >&2
    exit 1
fi
sed 's/kind=stale stale_count=1/kind=stale stale_count=2/' \
    "$tmp/smp-tlb-mutation.log" >"$tmp/smp-tlb-mutation-count-drift.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-mutation-count-drift.log" 2 stale >/dev/null 2>&1; then
    printf 'test-ci-scripts: mutation parser accepted stale-count drift\n' >&2
    exit 1
fi
sed 's/kind=stale stale_count=1/kind=operational stale_count=1/' \
    "$tmp/smp-tlb-mutation.log" >"$tmp/smp-tlb-mutation-operational.log"
if "$CI_DIR/validate-smp-tlb-log.sh" \
    "$tmp/smp-tlb-mutation-operational.log" 2 stale >/dev/null 2>&1; then
    printf 'test-ci-scripts: mutation parser accepted an operational failure\n' >&2
    exit 1
fi

# The unfiltered suite must keep running: the focused gates cannot see tests no
# filter names, nor interference between tests sharing kernel globals.
grep -Fq 'ci_run_step kernel-full-suite' "$CI_DIR/per-commit.sh"
grep -Fq 'rust-full-test-gate.sh' "$CI_DIR/per-commit.sh"

# The floor must reject a shrinking suite and accept the real count.
full_gate_out="$tmp/full-gate.txt"
if "$CI_DIR/rust-full-test-gate.sh" --minimum 5 -- \
    printf 'running 4 tests\n' >"$full_gate_out" 2>&1; then
    printf 'test-ci-scripts: full test gate accepted a shrinking suite\n' >&2
    exit 1
fi
grep -Fq 'executed 4 tests; require at least 5' "$full_gate_out"
"$CI_DIR/rust-full-test-gate.sh" --minimum 4 -- \
    printf 'running 4 tests\n' >"$full_gate_out" 2>&1
grep -Fq 'executed 4 tests (minimum 4)' "$full_gate_out"
# Two harness reports (lib plus integration binaries) sum rather than reset.
"$CI_DIR/rust-full-test-gate.sh" --minimum 7 -- \
    printf 'running 4 tests\nrunning 3 tests\n' >"$full_gate_out" 2>&1
grep -Fq 'executed 7 tests (minimum 7)' "$full_gate_out"
# A run that produced no harness report at all proves nothing.
if "$CI_DIR/rust-full-test-gate.sh" --minimum 1 -- \
    printf 'no harness output\n' >"$full_gate_out" 2>&1; then
    printf 'test-ci-scripts: full test gate accepted a missing harness report\n' >&2
    exit 1
fi
if "$CI_DIR/rust-full-test-gate.sh" --minimum 0 -- true >/dev/null 2>&1; then
    printf 'test-ci-scripts: full test gate accepted a zero minimum\n' >&2
    exit 1
fi

# The clippy gate must stay wired into both gates, and must keep linting the
# host profile alongside an architecture profile: `dead_code`, drop glue, and
# `c_char` casts answer differently per profile, so one profile alone would let
# the other rot.
grep -Fq 'ci_run_step clippy-host' "$CI_DIR/per-commit.sh"
grep -Fq -- 'clippy-gate.sh" --profile host' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step clippy-rv' "$CI_DIR/per-commit.sh"
grep -Fq -- 'clippy-gate.sh" --profile rv' "$CI_DIR/per-commit.sh"
grep -Fq 'ci_run_step clippy-la' "$CI_DIR/pr-gate.sh"
grep -Fq -- 'clippy-gate.sh" --profile la' "$CI_DIR/pr-gate.sh"
# The lint policy is a reviewable table, not a scattering of local overrides.
grep -Fq '[workspace.lints.clippy]' "$REPO_ROOT/Cargo.toml"
for manifest in \
    "$REPO_ROOT/Cargo.toml" \
    "$REPO_ROOT/kernel/Cargo.toml" \
    "$REPO_ROOT/crates/axnet-ng/Cargo.toml" \
    "$REPO_ROOT/crates/axtask-compat/Cargo.toml" \
    "$REPO_ROOT/crates/process-adapter/Cargo.toml" \
    "$REPO_ROOT/crates/readiness-adapter/Cargo.toml"; do
    grep -Fq 'workspace = true' "$manifest" \
        || {
            printf 'test-ci-scripts: %s does not opt into the workspace lints\n' \
                "$manifest" >&2
            exit 1
        }
done
"$CI_DIR/clippy-gate.sh" --profile bogus >/dev/null 2>&1 && {
    printf 'test-ci-scripts: clippy gate accepted an unknown profile\n' >&2
    exit 1
}

# The diagnostic partition decides what fails the build, so exercise it
# directly rather than trusting a clean run to have proved anything.
clippy_owned='{"reason":"compiler-message","message":{"level":"warning","code":{"code":"clippy::x"},"rendered":"owned\n","spans":[{"is_primary":true,"file_name":"kernel/src/a.rs"}]}}'
clippy_vendored='{"reason":"compiler-message","message":{"level":"warning","code":{"code":"clippy::y"},"rendered":"vendored\n","spans":[{"is_primary":true,"file_name":"third_party/rust-patches/z/src/b.rs"}]}}'
clippy_registry='{"reason":"compiler-message","message":{"level":"warning","code":{"code":"clippy::z"},"rendered":"registry\n","spans":[{"is_primary":true,"file_name":"/usr/local/cargo/registry/c.rs"}]}}'
clippy_vendored_error='{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0001"},"rendered":"broken\n","spans":[{"is_primary":true,"file_name":"third_party/rust-patches/z/src/b.rs"}]}}'
clippy_owned_absolute="{\"reason\":\"compiler-message\",\"package_id\":\"path+file://$REPO_ROOT/kernel#thekernel-kernel@0.2.0-preview.2\",\"message\":{\"level\":\"warning\",\"code\":{\"code\":\"clippy::absolute_owned\"},\"rendered\":\"absolute owned\\n\",\"spans\":[{\"is_primary\":true,\"file_name\":\"$REPO_ROOT/kernel/src/lib.rs\"}]}}"
clippy_sibling="{\"reason\":\"compiler-message\",\"package_id\":\"path+file://$REPO_ROOT/../thekernel-ax#thekernel-axtask@0.1.0\",\"message\":{\"level\":\"warning\",\"code\":{\"code\":\"clippy::sibling\"},\"rendered\":\"sibling\\n\",\"spans\":[{\"is_primary\":true,\"file_name\":\"src/lib.rs\"}]}}"

printf '%s\n' "$clippy_vendored" "$clippy_registry" \
    | python3 "$CI_DIR/clippy-report.py" --profile test >"$tmp/clippy-clean.txt" || {
        printf 'test-ci-scripts: clippy report failed on vendored-only input\n' >&2
        exit 1
    }
grep -Fq 'clippy[test]: clean' "$tmp/clippy-clean.txt"
grep -Fq 'vendored diagnostics (reported, not gated): 2' "$tmp/clippy-clean.txt"

if printf '%s\n' "$clippy_owned" "$clippy_vendored" \
    | python3 "$CI_DIR/clippy-report.py" --profile test >"$tmp/clippy-owned.txt"; then
    printf 'test-ci-scripts: clippy report passed an owned diagnostic\n' >&2
    exit 1
fi
grep -Fq 'owned diagnostics: 1' "$tmp/clippy-owned.txt"
grep -Fq 'vendored diagnostics (reported, not gated): 1' "$tmp/clippy-owned.txt"

# Cargo's package identity, not span spelling, owns the boundary. An absolute
# span inside this repository still gates, while a sibling's relative `src/`
# span remains under that sibling's independent lint gate.
if printf '%s\n' "$clippy_owned_absolute" \
    | python3 "$CI_DIR/clippy-report.py" --profile test \
        >"$tmp/clippy-owned-absolute.txt"; then
    printf 'test-ci-scripts: clippy report treated an owned absolute span as vendored\n' >&2
    exit 1
fi
grep -Fq 'owned diagnostics: 1' "$tmp/clippy-owned-absolute.txt"
printf '%s\n' "$clippy_sibling" \
    | python3 "$CI_DIR/clippy-report.py" --profile test \
        >"$tmp/clippy-sibling.txt" || {
        printf 'test-ci-scripts: clippy report gated a maintained sibling diagnostic\n' >&2
        exit 1
    }
grep -Fq 'vendored diagnostics (reported, not gated): 1' "$tmp/clippy-sibling.txt"

# A compile error inside vendored source still means the gate proved nothing.
if printf '%s\n' "$clippy_vendored_error" \
    | python3 "$CI_DIR/clippy-report.py" --profile test >"$tmp/clippy-error.txt"; then
    printf 'test-ci-scripts: clippy report ignored a vendored compile error\n' >&2
    exit 1
fi

printf 'test-ci-scripts: PASS\n'
