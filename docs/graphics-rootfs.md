# Graphics rootfs and desktop acceptance

`scripts/build-graphics-rootfs.sh` is separate from the normal BusyBox test
rootfs.  It pins Buildroot 2025.02.2 to commit
`6824cf90ae0fb88c63f8f2f8cfde0ee105eb3317` in
`config/graphics/pins.env`; Buildroot's package hash policy is forced on, so a
package with no checked-in hash is rejected rather than silently downloaded.
The package download directory is explicit and reusable.

First validate the checked-in inputs without fetching or building anything:

```bash
bash -n scripts/build-graphics-rootfs.sh
scripts/build-graphics-rootfs.sh --check
```

Use a locally verified checkout at that exact revision, or make the download
intent explicit:

```bash
scripts/build-graphics-rootfs.sh --flavor headless-abi-smoke \
  --buildroot-dir /src/buildroot-2025.02.2 --output .state/out/graphics-headless
scripts/build-graphics-rootfs.sh --flavor q35-software-desktop --fetch-buildroot \
  --output .state/out/graphics-q35
```

## User-local host dependencies

Buildroot keeps its own dependency check enabled. By default the wrapper leaves
the system Perl, `PATH`, and `PERL5LIB` untouched, so Buildroot's own complete
dependency check is authoritative. On a machine whose Perl was installed
without the needed core modules, opt into a task-local module prefix from a
compatible, already-installed Perl tree. The helper copies the hash-pinned
module closure listed in `config/graphics/host-perl-modules.sha256`; it refuses
an unlisted or checksum-mismatched source and does not suppress Buildroot's
check. An explicit prefix supplies its `perl` wrapper first on `PATH`.

```bash
TASK_CACHE=/home/ava/.cache/thekernel-codex.pJrrL1
PERL_CORE=/path/to/compatible/perl/lib
scripts/build-graphics-rootfs.sh --flavor headless-abi-smoke \
  --buildroot-dir .state/buildroot/buildroot-2025.02.2 \
  --output "$TASK_CACHE/graphics-rootfs-headless" \
  --download-dir "$TASK_CACHE/graphics-downloads" \
  --host-deps-dir "$TASK_CACHE/graphics-host-deps" \
  --perl-module-root "$PERL_CORE" \
  --tmpdir "$TASK_CACHE/tmp"
```

Use a source tree only after independently identifying it as an installed,
compatible Perl core. This checkout's working host has such files under a
rootless container-storage path; their hashes are pinned in the manifest above.
No `sudo`, system package installation, or `BR2_*` dependency-check override
is used. The second flavor uses the same cache and host-dependency prefix only
after the headless rootfs build completes.

The first flavor includes libdrm, libevdev, libinput, libseat/seatd (daemon), Wayland,
pixman, and Weston with the headless backend. It is a BusyBox SysV setup: eudev
starts at `S10`, seatd at `S70`, and the compositor session at `S80`. `/run` is
a tmpfs from the Buildroot SysV skeleton; `S80weston` creates
`/run/user/<weston-uid>` as mode `0700` and starts Weston as the non-root
`weston` account with `XDG_RUNTIME_DIR` set there.

The init script does not read `/etc/profile.d`: each flavor installs the
root-readable, single-value `/etc/thekernel-graphics-flavor` file. `S80weston`
strictly accepts only `headless-abi-smoke` or `q35-software-desktop` from that
file and passes it to `graphics-session`, which selects the corresponding
headless or DRM Weston configuration.

This is explicitly a **seatd + libseat** arrangement, not logind: Buildroot
does not select systemd/logind, seatd owns `/run/seatd.sock` as `root:seat`,
and `weston` is a member of `seat` and the single additional `render` group.
The checked-in udev policy sets
`/dev/dri/card*` to `video`, `/dev/dri/renderD*` to `render`, and
`/dev/input/event*` to `input`, all `0660`; those are auditable direct-device
groups and are intentionally not granted to the compositor, which receives
activated FDs from seatd instead. `render` is the narrow exception: it permits
only the `renderD*` node for the separately gated virgl EGL workload.

Run `/usr/local/bin/graphics-abi-smoke` inside the guest; its
`THEKERNEL_GRAPHICS_ABI_SMOKE_READY` marker means the S70/S80 path produced a
Wayland Unix socket while seatd is alive. This is a userspace ABI probe, not a
rendered-frame or input-device test. The launch profiles are also usable
directly as `graphics-session headless-abi-smoke` and
`graphics-session q35-software-desktop`; the former uses the headless backend,
while the latter selects DRM.

The headless overlay also runs that probe as `S90graphics-abi-smoke`, so its
marker is available on the guest serial console without a login shell. Build a
kernel once, then boot the already-built graphics image explicitly (the image
is mounted snapshot-only by the runner):

```bash
tools/thekernel.py build --profile system
tools/thekernel.py graphics-smoke --no-build --profile system \
  --rootfs "$TASK_CACHE/graphics-rootfs-headless/images/rootfs.ext2" \
  --graphics-profile headless \
  --screenshot "$TASK_CACHE/graphics-headless.ppm"
```

`headless` is the default graphics-smoke profile. It keeps the virtio GPU and
virtio input devices attached while avoiding a host window. `graphics-smoke`
binds both its QMP screendump and intentional stop to
`THEKERNEL_GRAPHICS_ABI_SMOKE_READY`; it preserves the PPM after QMP validates
the screendump. The general `run --rootfs IMAGE` argument likewise requires an
existing image and otherwise leaves the standard rootfs-builder behavior
unchanged.

`q35-software-desktop` selects both Mesa's swrast and virgl Gallium drivers,
plus Weston's DRM backend. Its S90 path first requires seatd and its socket,
card0, an evdev event node, Weston with `drm-backend.so`, and a Wayland socket.
Both the software and virgl paths use the same Wayland EGL/GLES client to
submit a fullscreen 800x600 frame
with a 200x200 red block at (300,200), and emits
`THEKERNEL_Q35_WESTON_READY` only after that frame callback. Use
`graphics-smoke --flavor q35-software-desktop --graphics-profile headless`
to bind the QMP 800x600 PPM and exact RGB block oracle to this software KMS
acceptance marker.

For real virgl coverage, run the same flavor with
`--graphics-profile virgl-interactive`. The tool waits only for
`THEKERNEL_Q35_VIRGL_READY`; after the software marker the guest requires
`/dev/dri/renderD128`, checks that `weston` has the required `render`
supplementary device group, and launches the EGL/GLES workload with
`MESA_LOADER_DRIVER_OVERRIDE=virgl`. EGL setup, rendering, or its frame
callback failures are non-zero and never emit that marker. The same 800x600
red-block pixel oracle is captured after the virgl marker. This is Weston-only
acceptance: Xorg remains disabled and unclaimed.

Xorg (`BR2_PACKAGE_XORG7`) and wlroots (`BR2_PACKAGE_WLROOTS`) stay disabled in
both fragments. The q35 image deliberately carries both `kms_swrast` and
`virgl`: the headless profile gates the software KMS path, while the separate
`virgl-interactive` profile gates render-node, EGL/GLES, and virgl command
submission. Xorg and wlroots are not part of this acceptance contract.

No Buildroot or package downloads, cross-toolchain build, or guest boot are
performed by `--check`; the full wrapper build validates the produced ext4
image, accounts and groups, compositor backends, Mesa drivers, and client
linkage before reporting success.
