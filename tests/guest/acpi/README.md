# Authored ACPICA policy fixtures

These small SSDTs are original TheKernel regression data, **not captured OEM
firmware**. `thermal.asl` starts below critical and reaches `_CRT` ten seconds
after its first `_TMP` evaluation. It must only be injected into QEMU.

Regenerate with the pinned ACPICA 20260930 `iasl -p OUTPUT STEM.asl`. The checked-in
AML permits the QEMU regression without a host tool installation.

Run `scripts/ci/acpica-policy-qemu-smoke.py` with a private TheKernel state directory,
`CARGO_BUILD_JOBS=6` and `nice -n 10`. It uses the formal product builder/runner,
checks live thermal sysfs values, and requires critical -> ordered flush -> AML
S5. Shell EOF, QMP quit and host termination are not accepted as trip proof.
