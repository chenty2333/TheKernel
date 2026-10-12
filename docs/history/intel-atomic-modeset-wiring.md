# Native Intel atomic-modeset projection

`kernel/src/drm/intel/atomic_modeset_wiring.rs` builds a narrow translated
`AtomicState` projection from the current Native mode-change request and invokes
only pure source checks whose inputs are fully modeled: digital-port conflict,
PHY-to-Type-C mapping, and baseline pipe bpp. It admits Pipe A, TC1/TC2, RGB
8-bpc, one linear XRGB8888 primary plane, and canonical VIC 16/95 timings.
Unsupported source actions are classified as owned by the indivisible
`tc_modeset::program` transaction or refused; the classifier never converts a
missing callback into success.

The projection remains a pure preflight. The default TC transaction is
unchanged. With `intel.native_modeset=1`, `present()` reuses its GGTT preparation
and runs `run_native_commit_tail`: disable, pre-plane CDCLK, shared DPLL,
encoder pre-enable, CRTC/encoder enable, primary-plane update, post-plane
CDCLK/readback, then power put. The adapter remains single Pipe-A/TC1/TC2,
linear XRGB8888/RGB565 and canonical VIC 16/95. It does not implement the full
upstream all-output dispatcher.

Failure unwinds attempted phases in reverse order (including partially
executed callbacks), releases the stopped CRTC's DPLL reservation, unpins the
candidate only after DMA quiescence, then puts the nested power lease. An
uncertain stop retains the candidate binding. No failure retries the legacy
writer. The persistent firmware-preserving power pin remains held, so the
nested lease cannot power down the current route or enter DC states.

## 剩余工作

- 步骤 8：多 CRTC 接线。
- 步骤 8：cursor 接线。
- 步骤 8：多 plane 接线。
- 步骤 8：颜色寄存器接线。
- G1-16：运行时 PSR 通知和 DMC DC6 计数 observer 尚未接线。
- G1-18：电源井 IRQ init/reset/synchronize 回调尚未接线。
- `intel.native_modeset=1` 尚未在 N305 上点亮过。
- 默认 TC 路径已改用翻译函数，但未上真机。
