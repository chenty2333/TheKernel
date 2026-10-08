# i915 Gen12 GT/GEM 移植地图

## 范围与约束

上游基线为 Linux `7.2.3`（`/home/ava/Desktop/linux-7.2.3/drivers/gpu/drm/i915`），平台为 Tiger Lake、Rocket Lake、Alder Lake-S/P/N。逐文件核对 SPDX 后再翻译：本任务列出的 GT/GEM/查询文件大部分为 `MIT`；`i915_perf.c` 及若干头文件/辅助文件需单独核验，若为 GPL-2.0 则不复制，只按 ABI/硬件规范实现或暂略。任何 GPL-only 的 DRM 框架、调度器和内存管理代码均不搬入。

Rust 侧复用 `kernel/src/drm/intel/{gt.rs,gt_probe.rs,gem_exec.rs,gem_context.rs,gtt.rs}`、`kernel/src/drm/intel/gt/`、`kernel/src/drm/intel/gtt/`、`kernel/src/drm/{render,dmabuf,fence,syncobj.rs,syncobj/}` 与 `crates/ax/tk-intel-gt/`。GPU 引擎寄存器访问、PCI/DMA/IRQ 继续映射到 TheKernel 现有设备接口；用户 ABI 映射到现有 DRM ioctl/GEM、fence 与 syncobj 接口。不得整体移植 Linux DRM 核心。

## 文件映射

| 上游文件/模块（Linux i915） | SPDX/处理 | TheKernel 对应与状态 |
|---|---|---|
| `gt/uc/{intel_uc.c,intel_uc_fw.c,intel_guc.c,intel_guc_fw.c,intel_guc_ads.c,intel_guc_ct.c,intel_guc_log.c,intel_huc.c,intel_huc_fw.c}` | 所列 `.c` 文件 SPDX 均为 MIT；平台固件名/版本取自 `intel_uc_fw.c` | 部分已落地：默认策略、设备 ID 到 uC 平台选择、固件候选与 CSS/version 校验、rootfs request、Gen12 DMA staging/upload、GuC READY poll、Gen11+ GuC MMIO auth；GSC/CT/ADS/log 与完整生命周期仍缺 |
| `gt/intel_wopcm.c` | MIT, Copyright © 2017-2019 Intel Corporation | `tk-intel-gt/wopcm.rs` 推导 Gen12 GuC/HuC WOPCM partition、验证固件/reserved range 和锁定寄存器；`gt/copy.rs` 在 DMA 上传前编程并验证 WOPCM，未接 media-GT/deprivileged layout |
| `gt/{intel_execlists_submission.c,intel_engine_cs.c,intel_context.c,intel_lrc.c,intel_ring.c,intel_timeline.c,intel_breadcrumbs.c,intel_engine_heartbeat.c}`、`gt/uc/intel_guc_submission.c` | 这些 `.c` 文件 SPDX 为 MIT；提交策略以 `gt/uc/intel_uc.c` 平台默认值为准 | `kernel/src/drm/intel/gt/`、`gt.rs`、`crates/ax/tk-intel-gt/{lrc,rcs,bcs}.rs`；扩展引擎队列、抢占/时间片、VCS/VECS 与完成通知 |
| `gem/{i915_gem_execbuffer.c,i915_gem_object.c,i915_gem_shmem.c,i915_gem_userptr.c,i915_gem_stolen.c,i915_gem_mman.c,i915_gem_tiling.c,i915_gem_domain.c,i915_gem_shrinker.c,i915_gem_context.c}` 及 `gt/intel_gt.c`（eviction） | 上述 GEM `.c` 文件 SPDX 均为 MIT；文件名/函数边界按上游目录 | `gem_exec.rs`, `gem_context.rs`, `gtt.rs`, `gtt/`, `render.rs`, `dmabuf.rs`, `fence.rs`, `syncobj/`；去除非上游限制，映射用户指针与 mmap 到现有 VM/DMA 能力 |
| `gt/intel_rps.c`, `intel_rc6.c`, `intel_gt_pm.c`, `intel_llc.c`, `intel_workarounds.c`, `intel_reset.c` | 目标 `.c` 文件 SPDX 为 MIT | GT 新增电源、Gen12 workarounds、引擎/整卡复位；复用 `gt_probe.rs`、`tk-intel-gt/reset.rs` 与寄存器访问 |
| `i915_query.c`, `i915_getparam.c`, `i915_perf.c` | 前两者有 MIT SPDX；`i915_perf.c` 头部带完整 MIT 许可文本（无 SPDX 行） | `render.rs` 与 DRM ioctl/UAPI；实现 ABI 可见 query/getparam 和 OA 可行部分 |
| Linux `drivers/gpu/drm/` 通用 DRM/GEM/VM/调度器框架 | 不移植框架层 | 映射到 `kernel/src/drm/{render,dmabuf,fence,syncobj.rs,syncobj/}` 和现有 VM、DMA、文件描述符接口；确需通用能力时新增最小接口 |
| 用户态载荷：Mesa anv、`vulkaninfo`、intel-media-driver、libva、`vainfo` | 各仓库/文件独立核许可证；iHD 按 MIT 处理 | 在现有 N305 iris smoke payload 旁增加可复现构建/guest 加载检查；无设备时只验证预期拒绝行为 |

### GuC submission 切片

`guc_submission.rs` 已补入上游 v69 context/process descriptors、v70 scheduling WQ descriptors、context registration/policy/action 编码、multi/single context ID partition、request scheduling state transitions、sched-state 位/blocked 引用计数，以及 multi-LRC WQ item/no-op wrap 编码。`CtDmaMemory` 保留了 GuC submission state，并有 CTB register/request/event 发布 helper，但既有 N305 RCS/BCS submitter 尚未调用它；还缺真实 engine/context/RCS backend life-cycle、tasklet/IRQ callsites、preemption/time-slice scheduler 与 reset integration。这是 ABI/queue 切片，不可据此宣称 GuC 默认提交可工作。

`guc_ct.rs` 已提供同步 TLB 完成和 FIFO deferred-event dispatch adapter（分别对应上游 receive-context 路径与 incoming-request worker）、nonblocking send busy-loop、ring reset 和 firmware-running 时的显式 disable action；GuC 事件业务 handler、VMA fini/owner teardown 以及 kernel 侧 G2H interrupt/tasklet/workqueue 调用链仍未接入。

`guc_capture.rs` 是 `intel_guc_capture.c` 的数据面切片：按上游 ring 语义解包跨环的 group/capture/register 记录；`guc_log::process_capture_log` 从共享 log state 读取 capture 指针/overflow/flush 并在解析后按上游顺序 ack，`copy::handle_guc_capture_notification` 有 drain/CTB flush-complete handler API（仅当显式提供 cache 时可保留 nodes；startup 不分配 cache，因为 diagnostics 工作已暂停）；另有 match/take/recycle node API。Parser 保留 metadata/order、跳过未知类型，含分组/节点池/匹配/格式化与 ADS capture-list helpers，但没有实际 G2H IRQ/workqueue caller、cache allocation/destroy lifecycle 或 coredump consumer。capture/log/relay/coredump 的剩余工作保持暂停。

`guc_log.rs` 已有 log sizing、overflow/read-pointer/relay snapshot、control-log/force-flush/flush-complete action payload 和 log-level controller；`copy.rs` 有 input-driven ADS/log GGTT VMA owner、GuC scratch parameter writer、workqueue-side debug/capture snapshots and CT flush-complete adapters。按 2026-10-09 协调者优先级，剩余 capture / log / relay / coredump 工作暂停；这些只是源码模型/adapter，没有 IRQ/tasklet/workqueue/coredump callers，也没有 relay/output delivery 或 cache destroy 生命周期。不要在第 2 项继续扩展这些诊断外围，除非启动 ADS/CTL 所需的最小 log buffer allocation。最新 Task 2 接线仅覆盖被严格筛选的 N305/ADL-N：从已验证 PCI 设备、GT topology、VDBOX/VEBOX fuse、doorbell count 和 GuC CSS 构建启动 ADS，分配基线 log buffer 并在 GuC DMA 前写 CTL scratch 参数；GuC load 成功后初始化 CTB self-config/enable，再通过 CTB 发送重复 enable action 并同步轮询匹配 G2H HXG response。此 ADS 的动态 MMIO regsets、golden LRC、capture lists 未接，CTB IRQ/tasklet/workqueue callers 与异步 event dispatch 也未接；TGL/RKL/ADL-S/P 的 ADS/CTL callers 未接，因此不能据此声称完整 GuC submission 可用。capture/log/relay 剩余工作保持暂停。

`guc_fw::suspend_guc` 镜像 `intel_guc_suspend()`：submission active 时尝试 CLIENT_SOFT_RESET、忽略其失败并复位 GuC 域；resume 无额外 GuC action。其 PM callback、work flush、CT/ADS/log owner teardown 仍未集成。

uC firmware upload 现在在 HuC/GuC DMA 前根据 CSS+uKernel upload size 计算 2 MiB Gen12 WOPCM partition，验证 locked/valid state 与 firmware/reserved bounds，再按上游顺序写入并回读验证 `GUC_WOPCM_SIZE` 和 `DMA_GUC_WOPCM_OFFSET`；接着执行仅 GuC 域的 GDRST（Gen12.0 双复位 + 50us settle）。此调用仅适用于当前集成 GT 目标；media-GT 的 BIOS/deprivileged pre-lock layout 未接入。

## 移植边界

不翻译 Linux DRM 框架、debugfs/sysfs 管理面、非 Gen12 平台路径及任务未列出的显示功能。GSC、LMEM-only/独显路径以及需要尚不存在的内核内存/用户页能力部分，先核对 Gen12 ADL 集显调用路径；不能安全映射的功能记录为未移植，不以占位成功掩盖。上游代码按函数保留控制流和错误顺序，翻译文件顶部登记来源/完整版权行，每个翻译函数用 `// upstream: <文件> <函数>()` 标注；MIT 全文及来源登记遵守 `COMMON.md`。

## 已接入的 uC 传输切片

`crates/ax/tk-intel-gt/src/uc.rs` 已包含 TGL/RKL/ADL-S/P/N 固件候选表、CSS 大小校验、固件版本检查、ADL-S HuC-only 与 ADL-N 默认 HuC authentication + GuC submission 策略、GuC submission ABI 版本规则和设备 ID 到平台映射。`huc.rs` 将 Gen11+/Gen12 legacy GuC auth status bit、action/wait 调用和 `intel_huc_check_status()` 错误码映射接入上述 GuC/HuC 上传路径。rootfs-ready 回调从 `/lib/firmware/i915/...` 取回固件；在 MIA 确认处于 reset 时，将 HuC 先于 GuC 暂存到固定物理页/GGTT，并按 `intel_uc_fw.c` DMA 顺序上传、按 `intel_guc_fw.c` 写 RSA scratch 和轮询 READY。GuC RSA 支持固定大小 MMIO 与大 RSA GGTT-VMA 两路。FirmwareImage 跟踪 Available/Loadable/Transferred/Running/LoadFail 阶段，GuC BootROM/UKernel 失败映射保留可判别原因；GuC 控制参数与 soft-scratch 序列和 baseline log sizing 已翻译。N305/ADL-N 启动现在从 exact 46D0 A0 PCI 身份、GT 拓扑、媒体 fuses、doorbell count、GuC CSS 收集基础 ADS 输入，创建 ADS/log VMA 并在 GuC DMA 前发布 GUC_CTL 参数；GuC load 后注册/enable CTB 并做一次同步控制 action/HXG response 往返验证（`242fe835`, video logical map `f91a9b40`）。现在 N305 的同步 BCS 用户作业会用 v70 注册一个短命 context、通过 CTB 启用/提交、轮询 scratch 和 G2H mode event，收到 context-deregister G2H event 后才释放该次作业的 GGTT 映射；GuC 已运行但 CT 尚不可用时 fail closed，不回退 ELSQ。动态 ADS MMIO regsets、golden LRC、capture lists 未接，TGL/RKL/ADL-S/P startup inputs 未接。异步 G2H IRQ/tasklet/workqueue、RCS GuC 作业（仍缺静态化 MCR/DSS context-WA 输入）、VCS/VECS 作业、preemption/time-slicing/request-fence pipeline、debug/capture/relay/coredump 剩余调用链仍未完成/按 2026-10-09 协调者优先级暂停。HuC RSA 所在 GGTT 映射保持到 auth action 与 `GEN11_HUC_KERNEL_LOAD_INFO` verified 状态均完成，再安全释放。DMA/auth/解绑不确定时保留页与绑定并 quarantine owner。不能据此声称全平台 GuC、RCS、媒体解码或硬件验收完成。

`guc_ads.rs` 当前翻译 GuC ADS 固定 ABI、固定/动态区域布局、通过 CTB 更新全局 policy、engine class mapping、first available default-LRC selection、enabled masks，并可消费调用方提供的 MMIO regset、golden LRC、capture lists、runtime GT system-info 来构造完整静态/动态 blob；包含 `guc_mmio_regset_init` 的 ring/WA/whitelist/MOCS/EU-perf 清单组装与 MCR flags、Gen12 IP 12.55 LRC skip-size、source 对齐的 duplicate/sort、按启用引擎预留 golden slots、capture null page、WAKLV firmware/platform gates、完整 blob reset 和 engine-usage offsets。`copy.rs` 增加了按 builder size 分配/pin/rebuild/reset 的 ADS VMA owner，但真实 GT/MCR/default-state/capture inputs 和 GuC startup `guc_init_params` caller 仍未接，故不代表 GuC submission 可用。

### Task 3 translation checkpoint

As of 2026-10-09, source-order Rust transcripts with per-function markers exist in unregistered `*_upstream.rs` files for `intel_guc_submission.c` (280), `intel_context.c` (33), `intel_lrc.c` (63), `intel_engine_cs.c` (75), `intel_timeline.c` (22), `intel_breadcrumbs.c` (26), and `intel_execlists_submission.c` (136); `intel_ring.c` has a compiled 10/10 backend module. `copy.rs` uses that ring module for private prebound BCS/RCS images. This does not establish full engine/request integration: the other transcripts remain outside the module tree because their Linux GEM/RCU/timeline/MMIO bindings are not implemented.

The compiled Gen12 `tk-intel-gt::execlists::{write_desc,execlists_submit_ports}` slice now replaces the private caller's hand-written ELSQ port writes. It keeps the source reverse-port order, writes both ports even when one is empty, and explicitly loads the queue. This remains only the hardware-facing queue-write portion; request queues, CSB completion, preemption/time-slicing, and the default GuC-submission engine path are still absent.

The same module now has source-derived Gen12 CSB status decoding (`__gen12_csb_parse()` / `gen12_csb_parse()`) with explicit refusal for upstream-impossible states. The private synchronous BCS/RCS caller drains the 12-entry HWS CSB after its scratch breadcrumb and before reset. This is a narrow polling validation, not the upstream IRQ/tasklet state machine: it does not promote software requests, complete timeline fences, or implement preemption/timeslicing.

CSB reads also preserve TGL HSDES#22011248461/22011327657 handling: poll the HWS slot for up to 10us, fall back to the engine's MMIO status-buffer mirror if it remains `U64_MAX`, then poison the consumed slot. `gt.rs` restricts those mirror reads to the two aligned read-only CSB ranges for the owned BCS/RCS engine.

The CT receive bridge now treats a valid non-event G2H HXG as a pending fence response rather than an unknown submission event, and publishes the consumed receive head for the waiter. Only scheduling-mode and deregistration events are dispatched today; IRQ/tasklet/workqueue integration and execution completion callers remain outstanding.

`CtDmaMemory::finish_guc_submission_response()` consumes such a response by fence after its caller has waited, returns the reserved G2H credit, and republishes the shared descriptor. It is an owner adapter only; no production GuC engine caller invokes this sequence yet.

N305 now has a synchronous single-LRC BCS GuC caller: from the ADS engine map it picks the BCS0 GuC logical mask, allocates a context ID, registers the existing GGTT LRCA and normal policy over CTB, submits the context, polls HWS scratch while draining G2H mode events, and deregisters/waits for its completion event before unbinding. This is still not the upstream engine scheduler: RCS GuC execution is refused because MCR/DSS context-WA inputs are not captured into ADS; VCS/VECS, asynchronous IRQ/tasklet workers, context reset/failure handlers, software request queues, and preemption/timeslicing are not integrated. Direct ELSQ remains only for pre-GuC boot/selftests; if firmware owns submission but CT is absent or stale, user jobs refuse instead of bypassing GuC.

The N305 GuC ADS caller supplies full RCS/BCS LRC image sizes for page-aligned golden-context slot reservation. The ADS builder subtracts `LRC_SKIP_SIZE` only when it writes the GuC `engine_state_size` field; the firmware-visible context address and reserved slot cover the full image, even when the golden image is currently absent.

Before GuC startup, the ADS builder now copies actual retained BCS default-state bytes captured by the existing boot path, and includes an RCS default image only when that state was captured. Missing enabled classes retain the source-compatible zero golden pointer/state while their full page-aligned slot remains reserved; this does not claim VCS/VECS default-context capture.
