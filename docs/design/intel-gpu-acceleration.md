# ADL-N GPU 加速路线评估

2026-10-05；D4 的路线评估，**实现尚未完成、未在硬件上验证**。
以本地 Linux 7.2.3 和已缓存 Mesa 26.1.2 源码为准；不把版本外的行为
当作承诺。GT 与显示按实际依赖独立推进；完整 HDMI/DMC/audio 不是 BCS 前置条件。

## 不可混淆的边界

- `iris` 是 Mesa Gallium/OpenGL 驱动；Vulkan 使用 `ANV`，不是“iris
  的 Vulkan”。Weston 有 EGL/OpenGL 不代表 Vulkan 或浏览器已加速。
- 现有 `render.rs` 是 virtio/virgl uAPI，不能冒充 i915/xe。现有
  `GemBacking::shared_pages`、PRIME、syncobj 和 fence 可复用上层 ownership，
  但不具备 Intel VM、LR context、引擎调度、reset 或 GPU cache 契约。
- BCS copy/fill 不等于 alpha blending/shader 渲染，更不证明 Mesa 可运行。
  首阶段内核自有 bounded copy 的成功不能宣称 execbuf uAPI 完成。
- i915 显示 ADL-N 是 ADL-P 子平台；**GuC 选择不同**。本地
  `i915/gt/uc/intel_uc_fw.c::__uc_fw_auto_select` 为 ADL-N 改选 ADL-S，
  因为 ADL-N 无 HWConfig，不能使用可能读取 HWConfig 的 ADL-P GuC。
  N305/i915 目标是 `i915/tgl_guc_70.bin`，HuC 是 `i915/tgl_huc.bin`。
  xe 也有 ALDERLAKE_N→tgl GuC 的明确表项（最低版本选择另行核对）。

- **Display stepping 与 GT stepping 是不同表**。本地
  `i915/display/intel_display_device.c` 的 ADL-N revision0→display D0，
  但 `i915/intel_step.c::adlp_n_revids` 对 revision0 使用 graphics/media A0。
  新显示 crate 的 `Step::D0` 绝不能拿来选择 GT/engine workarounds。

## 路线比较（工作量为评估，不是测得的交付日期）

| 路线 | 工作量/主要依赖 | Mesa/uAPI | 优点和风险 |
|---|---|---|---|
| Rust 忠实翻译 i915 gt/gem | 本地 C/H 输入 gt 94274行（226文件）、gem30587行（65文件），实际裁剪后仍是数万行；forcewake、GT电源/uncore、WA、48-bit PPGTT/39-bit DMA、LR context/execlists 或 GuC、GEM/execbuf、hangcheck/reset、cache/coherency、dma-resv/fence | i915 QUERY/GETPARAM、GEM_CREATE/CREATE_EXT、MMAP/MMAP_OFFSET、SET_DOMAIN/SET_CACHING、GEM_CONTEXT_CREATE_EXT/SETPARAM/DESTROY、EXECBUFFER2、WAIT/BUSY/MADVISE、reset stats、PRIME 与 syncobj/timeline | 与当前 Rust 架构和 i915 ADL-N成熟路径最接近；可逐函数核对，但最大风险是裁剪漏 WA 和伪造 ABI 功能支持 |
| Rust 翻译 xe | 新 VM_BIND/exec queue 模型较清晰，但还要 GuC CT/ADS/context registration/submission/reset、VM residency、async fences、scheduler/TTM/BO管理，工作量仍数万行 | XE_DEVICE_QUERY/GEM_CREATE/GEM_MMAP_OFFSET/VM_CREATE/VM_BIND/VM_DESTROY/EXEC_QUEUE_CREATE/DESTROY/EXEC/WAIT_USER_FENCE、syncobj与GPU用户fence | Mesa26.1.2 iris有xe backend；但Linux7.2.3 `xe_pci.c::adl_n_desc` 明确 `require_force_probe=true`，不是已支持的默认路径；不是“现代所以无需大量内核依赖” |
| C i915/xe + LinuxKPI | C 驱动本身减少手译，但需完整 kernel API 兼容：锁/RCU/workqueue/timer、PCI/DMA/runtime PM、memory mapping/shmem/TTM、DRM核心、fence/dma-resv、interrupt、ACPI等；当前仓库没有这层 | 原生 C 驱动 uAPI，前提是 ioctl/mmap/PRIME/sync/copy-user 都可正确桥接 | FreeBSD [drm-kmod](https://github.com/freebsd/drm-kmod)确实使用LinuxKPI，证明模式成熟，不证明TheKernel已有足够KPI；许多依赖许可需单独审核。为这一GPU建立另一套内核对象体系代价很高 |

uAPI 清单来自本地 Mesa 26.1.2 `src/gallium/drivers/iris/{i915,xe}/`
及 `src/intel/{common,dev}/`，不是仅按 Linux header 猜“返回 0 就可用”。
完整 ABI 仍需按用户程序实际 ioctl 请求的 flags/extensions/query 项逐条核对。
`execbuf` 不可接受未校验用户指针/对象数组，unsupported flags 要返回错误。

## 推荐及有序实现门槛

选择 **Rust/i915，先最小 BCS execlists，之后扩展 i915 GEM/execbuf + RCS，
GuC 独立阶段**。不同时做 xe、LinuxKPI 或版本兼容层。理由是已有 i915
显示基线、Rust ownership 与退役机制，且 Linux 默认 ADL-N 支持比 xe
force_probe 更适合当 authority。execlists 首阶段是受限实验路径，不是
声称复制了 Linux ADL-P 默认 GuC submission。达到 Mesa 支持前，不能向
GETPARAM/QUERY 谎报 softpin/context VM/timeline sync/reset 能力。

1. GT 独立 admission：精确 GT/media A0、独占 forcewake、GuC reset 排除控制器
   竞争、直接 DMA 与 owned pinned RAM；不等待 TC 模式设置/HPD/DMC/audio。
2. 移植 ADL-N forcewake domain 引用和 bounded ACK、GT runtime power、uncore
   保存/恢复与 stepping WA。已读到 ACK 不是拥有 forcewake。无超时退路
   就不准写引擎寄存器；先实现 engine/GT reset、request fault retirement。
3. 系统 pinned pages + 39-bit DMA 验证，GGTT 分配与固件显示不重叠，48-bit
   PPGTT 四级页表/scratch/error entries、TLB/cache invalidation。reset 后
   不确定 DMA retired 时保留 owners，不免费页。
4. 移植 `intel_engine_cs.c`, `intel_lrc.c`, `intel_context.c`, `intel_ring.c`,
   `intel_execlists_submission.c` 的 BCS0 必需分支。单 context/request，
   bounded breadcrumb wait。ring/HWSP/LRC地址与context descriptor全部验界。
5. 编码匹配 generation 的 copy/fill，校验尺寸/format/pitch/extent/overlap。
   用 disposable BO+redzones 做精确字节比较，挂死/close/cancel/reset时仍保
   活依赖。不能先把用户任意 batch 塞进 privileged BCS。
6. 接入 per-file `IntelGemBacking` + context/VM registry + execbuf object
   validation、dma-buf reservation fences、CPU/GPU cache ownership。现有
   GEM handle/PRIME alias 保持同一 backing Arc；mmap token 不泄漏物理地址。
   fence signal 只在真实 breadcrumb或已确认reset retirement时发生。
7. RCS 引擎/工作区/WA/context-state，按 Mesa实际调用完成 QUERY/GETPARAM/
   contexts/EXECBUFFER2 flags、共享 syncobj timeline、poll 和错误/reset stats。
   真正运行未修改 `eglinfo`、OpenGL三角形/Weston EGL和ANV `vulkaninfo` 后，
   才分别报告加速成功；QEMU不能验证Intel执行。
8. GuC 固件解析/验证/authentication、ADS/CT队列、registration/scheduling/
   reset/hangcheck作为同一路线的后续实现，不要以填几个uAPI结构代替它。

## 固件和许可登记

已从主机 linux-firmware 的同名 `.xz` 解压到用户指定外部 refs 目录：
`tgl_guc_70.bin`、`tgl_huc.bin`、以及比较用的 `adlp_guc_70.bin`；同时保留
主机 `/usr/share/licenses/linux-firmware/LICENSE.i915`，登记在 refs/INDEX.md。
它们是 Intel 的**二进制固件许可，不是 MIT**；不能改固件或给它换成 crate
许可。没有进 Git、没有打包进内核、没有加载。i915 Linux 7.2.3 首选 tgl
GuC 70 系列最低表项为70.12.1；使用前还须检查当前CSS版本/长度/签名和认证
失败路径，外部文件存在并不证明满足要求。HuC 是媒体认证，不是普通BCS复制
或OpenGL的硬前提。DMC仅显示电源管理，与GT固件分别加载和退役。

## 当前实现状态

固件 fixed-mode fastboot/KMS 已接通；GT 的代码路径和测量边界见下节。
默认 GT 关闭，只有 `intel.gt=1` 才能进入 exact N305 admission。

## Runtime execution entry (2026-10-05)

`intel.gt=1` now reaches an independent native boot hook after the display hook,
even when display modesetting is disabled/refused. Exact N305 revision0 selects
GT/media A0. The kernel adapter admits only curated owned-wake GT registers;
display registers and global/GuC reset masks are excluded. It executes source
forcewake clear/get/fallback, corroborates GuC MIA reset (no controller race),
then BCS stop/prefetch/pending-MI-forcewake, ready-for-reset and two BCS-domain
GDRSTs with50us settling and verified cancellation. Failed ownership is terminal.
The owner retains wake and now continues into the complete private BCS chain
below. This still is software implementation, not measured GPU copy/rendering.

## BCS private execution/result path (software implemented; hardware unverified)

The boot hook now publishes all DMA owners before binding/loading, builds one
private low-2MiB PPGTT with read-only source/batch/scratch and writable disposable
destination, creates source Gen12 BCS LRC+WA pages, programs verified UC index3,
then submits the exact source linear32 batch/ring via ELSQ. Only masked polling
interrupts are used. Hardware breadcrumb has a500ms bounded wait; the source
BCS stop/reset must succeed before unbinding even a completed context. Ambiguous
reset/binding retains the pages, context, VM and GGTT under the terminal owner.
Every16384 payload byte, unchanged source and both4KiB guards on each object
must match before the native success marker can be printed. The native code
never simulates the copy. Host interpreter/result/fault tests and compiled-C
images/commands validate software only, not N305 GPU output or RCS rendering.
No arbitrary batch/execbuf is exposed yet. Next: reuse this memory/submission
ownership in per-file GEM/validated submission/sync, then RCS and real Mesa;
no generic multi-generation scheduler or parallel Intel backend is introduced.

## Bounded GEM/submit/sync runtime integration

After successful native BCS bootstrap only, the existing primary/render DRM
files route the narrow Intel private ioctls to existing GEM handles, mmap/PRIME
backings, allocation accounting, reservation fences and binary syncobjs.
System GEM is limited to64KiB per object for the current private VM windows.
CREATE, WB MMAP_OFFSET, PREAD/PWRITE, BUSY/WAIT and default-context EXECBUFFER2
are implemented. EXECBUFFER2 accepts only source/destination/batch, fixed softpin
windows10000/20000/30000, NO_RELOC, one linear32 fast-copy+END and optional binary
fence arrays. All other flags/commands/contexts are refused, not silently ignored.
The caller's batch is snapshotted/decoded then rebuilt in kernel-owned memory;
no arbitrary privileged commands or CPU-copy execution fallback are exposed.
Explicit producer and atomic predecessor fences are respected before the final
snapshot; copied source/destination RAM stays fixed/pinned through quiescence.
Shared completion/error edges reach GEM reservations and output syncobjs only
following native result/retirement handling. mmap/PRIME views retain allocation
charge beyond close. Render-node registration is independent of native KMS and
is gated by successful GT bootstrap, not a fabricated virtio render adapter.

Measured: full affected DRM host regression, model GEM→private VM/copy bytes/
sync/error/mmap-after-close tests and compiled Linux7.2.3 wire facts. Physical
BCS/user-program acceptance is still pending; this is not Mesa-compatible i915.
RCS context/WA/3D command/state and Mesa queries/contexts/submit capabilities
remain the next functional dependency, not HDMI/DMC/audio.

The graphics-image tool builder now includes the original `intel-bcs-smoke`
acceptance client. It requires an explicit `--execute` node argument and never
falls back to a CPU copy or counts ENOTTY/unknown GPU as success. Native exec,
exact16384-byte readback, binary output-sync wait and shared mmap-after-handle-
close must all succeed. Only compilation and non-executing argument refusal
have run here; host DRM nodes were not opened, physical acceptance is pending.

## Minimal RCS software chain (physical rendering unverified)

Independent GT bootstrap can additionally run a bounded shader rectangle when
both `intel.gt=1 intel.rcs=1` are explicitly selected. It proves firmware RCS
idle/GuC exclusion/direct DMA, owns render-only reset, reapplies source N305
engine/context WAs and creates the14+2-page source RCS image/private VM.
One immutable64x64 linear RGBA texture-sampled rectangle uses licensed IGT
Gen12 PS instructions and state. No arbitrary user shader or privileged LRI
is accepted. The state page uses explicit verified UC cache policy, rather than
unknown firmware MOCS0. Full source RCS flush/TLB/AUX/mandatory instruction-state
WA and a GGTT completion marker precede bounded reset retirement and exact
source/destination/redzone checks. Completion alone is never permission to free
context/VM memory. Uncertain binding/reset retains the entire ownership graph.

After successful native RCS bootstrap, the existing GEM interface accepts only
this exact snapshotted render page in default-context EXEC_RENDER/NO_RELOC;
normal binary sync and GEM reservations are reused. The acceptance client has
`--rcs-execute NODE`; it requires actual shader submission, byte/guard checks,
output-sync wait and shared mmap-after-close before a RCS success marker.
The host memory interpreter is not an EU simulator or proof of rendering.
Sources/compiled-C tests cover context, WA lists, command/ring and IGT state;
no host DRM node, physical GPU or protected-content path was exercised.

Still missing for Mesa: persistent per-file context state/VM mapping, general shader
batch validation/engine requests, complete QUERY/GETPARAM/context extensions,
relocations/softpin residency and cache-domain contracts, scheduling/reset stats,
asynchronous execution/FENCE_SUBMIT and unmodified iris/ANV program acceptance.
The bounded renderer is not advertised as full i915/Mesa support. TC/HPD/audio
are not prerequisite dependencies for that GT work.

## Userspace discovery and bounded per-file contexts

A successfully admitted live GT now identifies its DRM interface as `i915`,
including the independent render node on the firmware-display adapter. Without
that bootstrap the old display/virtio identity is unchanged. Existing GETPARAM
and two-stage QUERY expose only implemented capabilities/engines, actual fused
single-slice/DSS/paired-EU masks and source-derived hardware timestamp clock.
Unknown readout returns an error, not a product-spec estimate. Engine discovery
reports BCS and, only after successful opted-in RCS bootstrap, RCS; no media,
GuC, scheduler, secure/async/timeline-exec or default-state isolation is claimed.
In particular `HAS_CONTEXT_ISOLATION=0`: the source requires a captured engine
`default_state`, which this bounded, inhibited-restore path does not yet have.

Legacy and extension-free CREATE/DESTROY contexts are per-file and capped at256.
Lookup pins a context gate through a concurrent destroy; new lookups return
ENOENT, while admitted synchronous work retires safely. The existing private
VM/hardware context is rebuilt for each complete immutable job, and a sleepable
per-context gate orders those jobs. Arbitrary retained graphics state, shared
VM/engine-map extensions and a general Mesa batch are still refused.
GTT_SIZE reports the current bounded256KiB address range, not a fictitious full
Mesa address space. The explicit acceptance client uses discovery and a created
context for the actual same GEM→BCS/RCS→sync chain. Compilation/model/source-C
checks are not unmodified iris/ANV or physical shader acceptance.

Shared completion safety prerequisite: core userspace SYNCOBJ_SIGNAL must
replace the object's backing with a completed software stub; it must not signal
an already-published GPU/GEM reservation or captured sync_file. Timeline software
signals append a consumer view that still depends on earlier producers, including
late device points below a software signal. Consumer views now flatten immutable
leaf dependencies and use existing fence waits/poll registrations; no new GPU
scheduler or producer-fence ownership mechanism is introduced. GPU completion
remains solely with the device path. Error propagation remains conservative.

Shared-cache admission also excludes unowned media activity. Actual disable
fuses select only the source ADL-P/N VCS0/VCS2/VECS0 domains. Their corresponding
forcewake ACKs must be owned, and the source hardware idle checks require empty
head/tail plus MODE_IDLE. Unknown fuses, lost ACK, active ring or unavailable
MMIO refuse before PAT/MOCS/L3 writes. RCS firmware-idle preflight now also checks
head/tail, not MODE_IDLE alone. No media work/reset/codec support is introduced;
sole-controller GuC exclusion and default-off GT/RCS flags remain required.

The same bounded EXECBUFFER2 now accepts one timeline-fence extension (up to64
entries), separately from binary fence arrays. Input fence identities must
already be materialized and are captured before batch snapshot; nonzero
WAIT+SIGNAL on the same point is refused as upstream requires. Fresh positive
output points and binary point0 use the existing completion leaf/reservations
and consumer chains. Every post-admission failure terminally errors that leaf;
userspace software SIGNAL cannot fake the GEM completion. Extension links,
unknown flags/names/reserved words and mixed array/extension forms are rejected.
The 32-byte extension header and56-byte wire record match independently compiled
Linux headers. The explicit client uses timeline output/wait; only compilation,
models and source comparisons are verified. Persistent user
VM/context/general Mesa batches remain unfinished.

EXEC_FENCE_IN/OUT now uses the caller's captured descriptor table and the same
GEM/timeline completion leaf. Input sync_file lookup failures return EINVAL.
Output slots are CLOEXEC, reserved/prepared before execution, invisible until
successful execution and result copyout. Copyout failure releases the slot but
never undoes completed GPU work or signals a different fence. OUT with the
write-only ioctl is refused rather than reproducing Linux's documented fd leak.
This is still the bounded synchronous adapter: input waits and execution have
bounded deadlines, not Mesa's asynchronous arbitrary-batch interface. FENCE_SUBMIT
remains unsupported. Descriptor ownership/rollback tests and client compilation
are software checks, not physical GPU execution.


VM_CREATE/DESTROY and CONTEXT_PARAM_VM now retain actual initialized/pinned
PPGTT pages, with a charged RAM owner and a gate shared by contexts using that
root. VM IDs are file-local, aliases returned by GETPARAM own independent
references, and deleting an ID never detaches a context or admitted job. VM
assignment is limited to mutable proto-contexts; CREATE_EXT SETPARAM uses the
source32-byte extension header. The native bounded BCS/RCS adapter uses that
same root, retains it on ambiguous DMA retirement, and never reuses it while
another sharing context runs. The current256KiB address window and immutable
batch restriction are still temporary limitations, not full Mesa VM/state
support: per-engine saved images and general48-bit residency/submission follow.

The existing target Mesa26.1.2 build was softpipe/virgl-only. The same cached
26.1.2 source now builds iris/EGL/GBM/GLES in the Intel task's own state directory
(`mesa-iris`, install `mesa-iris-stage`); no source version substitution. Its
required native CLC tools were built from the same Mesa source, using isolated
signature-checked Fedora LLVM22/SPIR-V development packages and existing host
LLVM/tools, without installing host packages. The cross toolchain/sysroot is
read-only; BISON_PKGDATADIR points at its existing data. No GPU was opened.
`intel-mesa-smoke --initialize NODE` uses real GBM/EGL and validates the Intel
renderer; `--execute NODE` compiles ES3 triangle shaders and checks interior
and exterior pixels after actual draw/finish/readback. Loader override rejects
software/virgl/zink fallback. It is built by the existing graphics guest-tool
builder, not an alternative ABI harness. Compilation/no-argument refusal are
verified; initialization/render markers have NOT been observed. Minimal RCS
selftest, actual Mesa initialization and native Mesa pixels remain separate
acceptance layers. The actual iris CREATE_EXT chain now accepts RCS/RCS/BCS engine maps,
RECOVERABLE=0, default PRIORITY=0 and shared VM SETPARAM. Exec ring bits index
that immutable map (including duplicate physical-engine slots), rather than
being misinterpreted as legacy engine numbers; empty/invalid slots, nonexistent
instances, repeated assignment and engine extensions refuse. Creation checks
actual runtime RCS availability, not physical platform capability alone. These
mapped slots still run bounded complete-state jobs, not retained user images.
Iris REG_READ uses the source RCS timestamp whitelist with8B_WA: actual owned
forcewake ACKs precede upper/low/upper read with three source attempts. Still-torn
reads fail instead of publishing an invented time. Legacy readq mode is refused
rather than approximated with non-atomic dwords. Compiled-C traces validate
rollover; no hardware timestamp or real Mesa initialization has been measured.
Next required implementation is general48-bit residency/cache policy and
per-slot saved/default state plus nonprivileged arbitrary batch submission.
