#!/usr/bin/env bash
# End-to-end test for the syzlang-driven differential-case generation
# prototype.  Parses both descriptions, generates both C cases, checks
# generation determinism, verifies the parser rejects out-of-subset syntax,
# compiles with the contract flags, runs the binaries on the host, and
# verifies every marker in the generated manifests with grep -Fqx.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"

work="${TMPDIR:-/tmp}/syz-differential-proto.$$"
mkdir -p "$work"
trap 'rm -rf "$work"' EXIT

cases=(eventfd timerfd)

echo "== 1. parse descriptions =="
python3 syzlang_parser.py \
    descriptions/eventfd.txt descriptions/timerfd.txt > "$work/ast.json"
python3 - "$work/ast.json" <<'EOF'
import json, sys
ast = json.load(open(sys.argv[1]))
for path, d in sorted(ast.items()):
    print(f"  {path}: {len(d['syscalls'])} syscalls, "
          f"{len(d['flagsets'])} flag sets, {len(d['resources'])} resources, "
          f"{len(d['structs'])} structs")
EOF

echo "== 2. parser rejects out-of-subset syntax =="
cat > "$work/bad.txt" <<'EOF'
resource sock[int32]
EOF
if python3 syzlang_parser.py "$work/bad.txt" 2>"$work/bad1.err"; then
    echo "FAIL: parser accepted unsupported resource base type" >&2
    exit 1
fi
grep -q "unsupported resource base type" "$work/bad1.err"
cat > "$work/bad2.txt" <<'EOF'
eventfd2(initval int32, flags flags[missing_set]) fd
EOF
if python3 syzlang_parser.py "$work/bad2.txt" 2>"$work/bad2.err"; then
    echo "FAIL: parser accepted reference to undefined flag set" >&2
    exit 1
fi
grep -q "unknown flag set" "$work/bad2.err"
cat > "$work/bad3.txt" <<'EOF'
syz_open_dev$loop(dev ptr[in, string["/dev/loop#"]], id intptr) fd
EOF
if python3 syzlang_parser.py "$work/bad3.txt" 2>"$work/bad3.err"; then
    echo "FAIL: parser accepted string[] type outside the subset" >&2
    exit 1
fi
echo "  rejection paths OK"

echo "== 3. generate C cases =="
for c in "${cases[@]}"; do
    python3 generate.py --description "descriptions/$c.txt" \
        --semantics "semantics/$c.json" --out-dir generated
done

echo "== 4. determinism: regenerate and diff =="
for c in "${cases[@]}"; do
    python3 generate.py --description "descriptions/$c.txt" \
        --semantics "semantics/$c.json" --out-dir "$work/regen" >/dev/null
    diff -u "generated/$c-gen-smoke.c" "$work/regen/$c-gen-smoke.c"
    diff -u "generated/$c-gen-smoke.markers" "$work/regen/$c-gen-smoke.markers"
done
echo "  deterministic OK"

echo "== 5. semantics/description cross-validation catches drift =="
python3 - <<'EOF'
import json, subprocess, sys, tempfile, os
sem = json.load(open("semantics/eventfd.json"))
sem["checks"][0]["steps"][0]["call"] = "eventfd"  # not in the description
with tempfile.TemporaryDirectory() as d:
    p = os.path.join(d, "drift.json")
    json.dump(sem, open(p, "w"))
    r = subprocess.run(
        [sys.executable, "generate.py", "--description",
         "descriptions/eventfd.txt", "--semantics", p, "--out-dir", d],
        capture_output=True, text=True)
    assert r.returncode != 0, "generator accepted undeclared syscall"
    assert "not declared" in r.stderr, r.stderr
print("  drift detection OK")
EOF

echo "== 6. compile with contract flags =="
# Contract flags are `cc -static -O2 -Wall -Wextra -Werror`.  Some hosts
# (like Fedora without glibc-static) cannot link -static; in that case fall
# back to dynamic linking — the same flags the existing reference runner
# scripts/ci/seccomp-host-differential.sh uses — and say so explicitly.
link_mode="-static"
if ! cc -static -O2 -Wall -Wextra -Werror \
        -o "$work/linkprobe" -x c - <<<'int main(void){return 0;}' \
        2>"$work/linkprobe.err"; then
    if grep -q "cannot find -lc" "$work/linkprobe.err"; then
        echo "  WARNING: host lacks static libc; falling back to dynamic link"
        link_mode=""
    else
        cat "$work/linkprobe.err" >&2
        exit 1
    fi
fi
for c in "${cases[@]}"; do
    # shellcheck disable=SC2086
    cc $link_mode -O2 -Wall -Wextra -Werror \
        -o "$work/$c-gen-smoke" "generated/$c-gen-smoke.c"
done
echo "  compile OK (${link_mode:-dynamic})"

echo "== 7. run on host and verify marker manifests =="
for c in "${cases[@]}"; do
    out="$work/$c.out"
    "$work/$c-gen-smoke" > "$out"
    while IFS= read -r marker; do
        if ! grep -Fqx "$marker" "$out"; then
            echo "FAIL: $c missing marker: $marker" >&2
            exit 1
        fi
    done < "generated/$c-gen-smoke.markers"
    n="$(wc -l < "generated/$c-gen-smoke.markers")"
    echo "  $c: all $n markers matched"
    sed 's/^/    /' "$out"
done

echo "SYZ_DIFFERENTIAL_PROTOTYPE_OK"
