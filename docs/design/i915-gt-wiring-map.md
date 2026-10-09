# i915 GT 接线地图（Linux 7.2.3 / ADL-N）

## 范围与状态

本文按 Linux 7.2.3 `drivers/gpu/drm/i915/` 的调用结构，记录当前 TheKernel 的默认执行路径、`upstream-gt` 源顺序翻译，以及真正切换运行路径时尚需提供的边界。ADL-N 使用 GT/display 分离：GT 为 Gen12 A0、GuC 提交；display 为 13.x D0。两者仍共享 PCI 设备、MMIO BAR、GGTT、IRQ/电源控制边界。

**`upstream-gt` 是默认关闭的翻译/编译分支，不是已经接入的运行路径。** 目前 kernel 运行使用 `kernel/src/drm/intel/gt.rs` 与 `gt/copy.rs` 的受限 N305 所有者。`gt::init_at_boot()` 仍要求显式 `intel.gt=1`；默认启动不执行 GT 写入。下表中 `upstream-gt` 一栏标出源函数已有翻译、类型声明或缺失实现，均不应解读为已被默认驱动调用。

状态标记：
- **默认**：普通产品构建里的实际 TheKernel 路径。
- **feature**：只有 `upstream-gt` 编译启用的 Rust 来源翻译；需要 kernel 接线才能成为调用者。
- **仅声明**：Rust API/ABI 声明存在，函数体仍缺失。
- **缺失**：当前没有可对应的 Rust 函数。

## 1. 驱动探测与 GT 初始化

| 上游调用（Linux） | 对应 Rust 函数（文件:行 / 构建路径） | LinuxKPI/内核服务依赖及状态 |
|---|---|---|
| `i915_driver_probe()` | **缺失**。TheKernel 没有 Linux DRM PCI probe；自有启动入口是 `kernel/src/drm/intel/gt.rs:457` `init_at_boot()`（默认路径，且需 `intel.gt=1`）。 | PCI 枚举、设备身份与 MMIO 映射：`kernel/src/drm/intel/gt.rs:481` 从 PCI/平台取得。是 TheKernel 产品实现，不是 host 模型；不是 Linux probe 回调兼容层。 |
| `i915_driver_hw_probe()` / `i915_gem_init()` | **缺失**。最接近的是 `gt.rs:481` `initialize()` 和 `gt/copy.rs:2763` `run()`；只初始化本任务覆盖的 N305 BCS/GuC 子集，不等价于 i915 全卡 probe 或完整 GEM 初始化。feature `intel_ggtt_upstream.rs:1558` 翻译 `i915_init_ggtt()` 但未由默认 kernel probe 调用。 | PCI、MMIO、内存/GGTT owner：当前 N305 自有实现可实际执行；还没有为 upstream `DrmI915Private` 建立完整 kernel owner。 |
| `intel_gt_common_init_early()` / `intel_root_gt_init_early()` | **feature**：`intel_gt_upstream.rs:268,293` (`intel_gt.c`, 42/42)。不从默认路径调用。 | spinlock、list、workqueue 初始化：LinuxKPI 有产品实现；workqueue 是 axtask 单队列模型，延迟 work 尚不完整。 |
| `intel_gt_assign_ggtt()` | **feature**：`intel_gt_upstream.rs:358`；GT list/VM 连结逻辑已翻译，但真实 GGTT translation `intel_ggtt_upstream.rs` (`intel_ggtt.c`, 74/74; e.g. `i915_ggtt_probe_hw():2027`, `i915_ggtt_create():2052`, `i915_ggtt_init_hw():338`) 已注册但未切换成 kernel runtime owner。默认 N305 地址空间由 `gt/copy.rs` VM/GGTT owner 构建。 | GGTT/PTE/回收需用 kernel 地址空间与 scanout 保留范围；不能将 host fake 地址空间作为产品证据。 |
| `intel_gt_init_mmio()` / `intel_engines_init_mmio()` | **feature**：`intel_gt_upstream.rs:388` 调用 `intel_engine_cs_upstream.rs:1300`；上游 GT/engine 初始化调用链未接默认路径。 | MMIO 读写/forcewake：`linux/forcewake.rs` 实现域选择与引用计数，真实芯片获取/释放协议由 `IntelUncoreFwGet` 回调提供；无 callback 时 fail-fast。kernel 尚未把 upstream `IntelUncore` 接到当前 `RegisterWindow`。 |
| `intel_gt_init_hw()` / `intel_gt_init()` | **feature**：`intel_gt_upstream.rs:439,987` (`intel_gt.c`, 42/42); `intel_mocs_upstream.rs` (`intel_mocs.c`, 15/15) 提供此路径调用的 MOCS 初始化和索引函数。 默认 N305 `initialize()` 直接执行保守 reset 和 `copy::run()`，不是这些函数的适配器。 | reset、MMIO、DMA retirement 必须由真实 GT owner 执行；不可由 host 测试模型替代。 |
| `intel_uc_init_early()` / `intel_uc_init_late()` / `intel_uc_init_mmio()` | **feature**：`intel_uc_upstream.rs:308,327,344`（`intel_uc.c`，36/36），`intel_huc_upstream.rs`（`intel_huc.c`，29/29）。不从默认路径调用。 | firmware/workqueue/locks 为 LinuxKPI 产品服务；uC 回调指向 GuC/HuC/GSC owner，部分目前仅有声明或未与 kernel owner 连通。 |
| `__uc_init_hw()` / uC firmware fetch、upload | **feature**：`intel_uc_upstream.rs:456,690`；`intel_uc_fw_upstream.rs`（`intel_uc_fw.c`，38/38）；`intel_wopcm_upstream.rs`（10/10）；`intel_huc_fw_upstream.rs`（6/6）。 | rootfs firmware：`linux/firmware.rs` 通过 `axdriver_base::firmware` 的真实文件读取器，保留尺寸/缺失错误；`on_rootfs_ready` 由 kernel 注册。MMIO/DMA/WOPCM 仍需要真实 kernel GT owner。 |
| GuC 固件启动 / `intel_guc_init()` | **feature**：`intel_guc_upstream.rs:584`（`intel_guc.c`，38/38）；`intel_uc_upstream.rs:690` 是上游 uC init-hw。`guc_fw.rs:175-205,457-548` 仍是默认构建的 Gen12 upload/RSA/status 子集，源码模块尚未接默认 owner。 | DMA、MMIO、firmware 为产品服务；PCI revision 经 `linux/i915.rs:27-48` 的安装式 reader 实际读取，缺 provider 时 fail-closed。固件/GGTT lease 与 upstream GEM 类型尚未贯通。 |
| `intel_guc_submission_enable()` / engine 注册 | **feature**：`guc_submission_upstream.rs:4570` 起有 upstream 定义；`intel_uc_upstream.rs:787` 调用该定义。默认手写子集在 `guc_submission.rs:542` `submit_request()`，运行时调用从 `gt/copy.rs:1225` 的 `submit_guc_context_request()` 进入。`intel_gt_upstream.rs:987` 的 init caller 已翻译，仍未接到默认 kernel path。 | CTB 默认子集用真实 MMIO/GGTT 与有限同步轮询；feature LinuxKPI tasklet/workqueue/RCU 是 axtask 运行时适配器（不等于 Linux softirq/per-queue 并行），尚未成为 kernel 的上游 engine 调度路径。 |

## 2. 中断：GT、GuC CT 与 breadcrumbs

| 上游调用 | 对应 Rust 函数（文件:行 / 构建路径） | LinuxKPI/内核服务依赖及状态 |
|---|---|---|
| `gen11_irq_handler()` / display IRQ 分发 | **feature**：`i915_irq_upstream.rs:143` (`gen11_irq_handler`) 与 `:188` (`dg1_irq_handler`)，另有 Gen11/DG1 master disable/reset/postinstall（13/55 selected functions）。`Gen11DisplayIrqHooks` 将 display handler、misc ACK/handler、reset/postinstall 交给 kernel；当前 `kernel/src/drm/intel/irq.rs:526` `display_irq_handler()`、`:665` `dispatch()` 仍是**默认 display-only** 分发，不处理 GT/GuC CT。 | PCI INTx/MSI 和 display IRQ handler 是 kernel 实际 IRQ 路径；GT vector/共享 master enable 尚未交给 upstream-gt。display 分发必须由 callback 保留，不能在 GT 翻译里重复 ACK display 状态。 |
| `gen11_gt_irq_handler()` / GT identity 分发 | **feature**：`intel_gt_irq_upstream.rs:399`（`intel_gt_irq.c`，21/21，已注册/编译/测试）；`:250` 的 `guc_irq_handler()`。当前 kernel 入口 `irq.rs` 未调用它。 | `linux/irq.rs:85` 的 `irq_work_queue()` 使用 axtask 延迟执行（产品实现、非 Linux softirq）；`linux/tasklet.rs:570` `tasklet_schedule()` 用持久 axtask worker（产品 task-context 近似，不是 host-only fake）；尚未接 GT 硬件 IRQ。 |
| GT PM IRQ mask/reset | **feature**：`intel_gt_pm_irq_upstream.rs:24-116`（8/8）。没有 kernel 中断安装/dispatch caller。 | spinlock/IRQ-save 是 `kernel_guard` 产品实现，host 下 guard 为 NoOp；硬件 mask/ACK 仍需 kernel 提供唯一 owner。 |
| GuC CT receive / `ct_receive()` / `ct_handle_msg()` | **feature**：`intel_guc_ct_upstream.rs` (`intel_guc_ct.c`, 44/44)，source-order translation；default `guc_ct.rs:353` 仍是受限同步 `send_busy_loop()` owner，未切换调用者。 | `wait.rs` 基于 axtask sleep/wakeup（产品路径）；tasklet/workqueue 采用 task-context worker 适配；CTB IRQ handler/异步 G2H event caller 尚未接。 |
| `intel_breadcrumbs` / fence completion | **无 upstream IRQ completion caller**。默认 N305 `gt/copy.rs:2443-2532` 消费 Gen12 CSB 与 HWS scratch，作为受限同步作业完成路径；不提升 Linux `i915_request`/timeline fences。 | atomic、wait queue 和 task wakeup 为产品代码；不构成 Linux RCU/softirq/fence scheduler 的等价证明。 |

## 3. 一次 execbuffer 提交

| 上游调用 | 对应 Rust 函数（文件:行 / 构建路径） | LinuxKPI/内核服务依赖及状态 |
|---|---|---|
| `i915_gem_do_execbuffer()` | **feature 文件存在但未接入默认路径**：`i915_gem_execbuffer_upstream.rs:2696`（当前 86/90 标记；模块尚未注册）。默认入口为 `kernel/src/drm/intel/gem_exec.rs:422` `exec_with()` / `:442` `exec_request()`。 | user copy、GEM reservations、DMA pages、sync objects 当前由 kernel 产品对象路径实现；LinuxKPI dma-fence/GEM 完整 runtime 还未接到这些对象。 |
| engine/context 选择与 pin | upstream 依赖 `intel_context_*`、`i915_vma_*`、`intel_engine_*` owner 模块；不代表已由 kernel exec ioctl 调用。默认实现由 `gem_context.rs:38-96` `engine_target()/image_engine()/vm()` 与 `gem_exec.rs:390-470` 选择。 | 内存、锁、wait queues 是 kernel 实现；feature 的 VMA/GEM owner 还没有连接到 `DrmFile` 和 kernel `GemObject`。 |
| PPGTT page-table map/unmap | **feature**：`intel_ppgtt_upstream.rs` (`intel_ppgtt.c`, 18/18) 保留上游 PPGTT 操作；尚无默认 `GemVm`/kernel VM caller。 | 依赖 LinuxKPI page-table/DMA/GGTT owners；真实 GT VM allocation/binding 与 display aperture reservation 必须由 kernel owner 提供。 |
| request 创建与 GuC 提交 | upstream `guc_submission_upstream.rs` 含 `intel_guc_submission_enable()`（`:4570`）及 submit helpers；默认软件契约是 `guc_submission.rs:542`，kernel N305 真正调用 `gt/copy.rs:1225` `submit_guc_context_request()`。 | CTB/MMIO、DMA、GT reset containment 为 N305 产品路径；功能限单个已支持 engine/context，不是 Linux 通用 request queue。 |
| 完成与同步 | **upstream request/timeline fence 执行路径未与 kernel 接通**。默认 `gem_exec.rs:442` `exec_request()` 将 kernel fence/reservation/syncobj 生命周期接到 `gt.rs:672` `submit_user()`；GuC BCS 子路径轮询 HWS scratch + G2H event，见 `gt/copy.rs:2370-2532`。 | fence/reservation/syncobj 是 kernel 产品服务；workqueue/IRQ/tasklet 异步完成路径当前不参与 N305 exec ioctl。 |

## 4. GEM 对象生命周期：create / mmap / domain / wait / close

| 上游操作 | 对应 Rust 函数（文件:行 / 构建路径） | LinuxKPI/内核服务依赖及状态 |
|---|---|---|
| create / handle lookup | 上游 create translation 尚未完成。默认 `kernel/src/drm/intel/gem_exec.rs:167` `object()`、`:186` `create()`；`gem_context.rs` 负责 per-file context/VM。 | `DrmFile` handle table、用户复制和 backing pages 为 kernel 产品实现；未走 upstream DRM GEM object owner。 |
| CPU data/domain/cache policy | 上游 domain/create translation 尚未接入。默认 `gem_exec.rs:269` `domain()`、`:282` `caching()`、`:311` `data()`。 | VM/VMA、CPU cache policy、DMA sync 及不允许的 mmap 类型由 kernel 与 axhal 实现；host tests 只验证同一 API 的模型，不证明硬件 DMA。 |
| mmap / mmap offset | 上游 mman/vma path 未接入。默认 mmap offset/backing 由 `kernel/src/drm/file.rs:871` `mmap_object()`，调用方示例见 `gem_exec.rs:1332`。 | VMA fault/page mapping 为 kernel 产品内存路径；`linux/mm_native.rs` 是 feature LinuxKPI 的 native MM binding，不替代 kernel GEM handle/offset lifetime。 |
| wait / busy completion | 上游 `i915_gem_wait.c` 尚未翻译。默认 `gem_exec.rs:170` `previous()`、`:352` `wait()` 及 kernel fence wait。 | wait queue/task sleep 使用真实 axtask；host profile 的 scheduler/guard 为 test-only替身，不用于产品 acceptance。 |
| close / teardown | 上游 `i915_gem_close` 依 DRM 框架未移植。默认 `kernel/src/drm/file.rs:671` `close_handle()`；file close 收尾位于 `file.rs:195-199`。 | Arc/backing pages 与 in-flight fence 的所有权由 kernel 实现；不可提前解绑仍可能被 DMA 访问的 backing。 |

## 5. 电源、RC6/RPS/SLPC、suspend/resume

| 上游调用 | 对应 Rust 函数（文件:行 / 构建路径） | LinuxKPI/内核服务依赖及状态 |
|---|---|---|
| `intel_gt_pm_get()/put()` | **feature**：LinuxKPI `linux/pm.rs:85,92`；`intel_gt_pm_upstream.rs:241-585` 翻译 `intel_gt_pm.c` 全 20/20。kernel GT owner 尚未安装 `RuntimePmOps`，无真实 callback 时 fail-fast。默认 `gt.rs` owner 以自己的 awake/lease 状态管理，不调用 upstream wakeref。 | runtime PM callback ABI 存在，接线缺失；不能把 host test callback 当成电源域控制。 |
| GT unpark/park / `POWER_DOMAIN_GT_IRQ` | **feature**：`intel_gt_pm_upstream.rs:241,262` 保留上游取/异步释放 display IRQ power-domain 顺序；`kernel/src/drm/intel/native_power.rs:24-28,129-133` 是 display domain owner，但没有 GT IRQ acquire/async-put adapter。 | `linux/pm.rs` 的 GT wakeref 是产品 callback boundary；display power domain 是独立 kernel 产品实现，二者还未接通。 |
| RC6 enable/disable/sanitize | `intel_rc6.c` 上游实现仍待翻译；当前 `intel_rc6_types_upstream.rs` 只有类型。默认 N305不接 RC6 自动状态机。 | GT/display 共享低功耗状态可能影响唯一 console scanout；无独立 host-only 模型可替代硬件域仲裁。 |
| RPS/SLPC | `intel_rps_upstream.rs` 当前只翻译 5/134；`intel_guc_slpc_upstream.rs` 存在 source-order 模块。默认路径不启用 upstream RPS/SLPC。 | MMIO/GuC CT/workqueue/runtime PM 均须有真实 owner；SLPC 调频只可在 GuC firmware 已确认拥有提交与功率策略后启用。 |
| system suspend/resume | `intel_gt_pm_upstream.rs:520-585` 翻译 GT suspend/runtime-PM 与 awake-time函数，`intel_uc_upstream.rs:924-1026` 提供 uC suspend/resume；kernel `gt.rs` 尚无系统 PM hook。 | timer/workqueue/GT wakeref/firmware state 必须跨 suspend 有真实生命周期；当前只支持引导后受限 GT owner 生命周期。 |

## LinuxKPI 服务真实性索引

| 服务 | 当前实现 | 状态判断 |
|---|---|---|
| MMIO/forcewake | `linux/forcewake.rs` 域位、引用计数和 release 写；acquire 走 `IntelUncoreFwGet` per-chip 回调。 | LinuxKPI 算法在产品代码内；**kernel callback 尚未装入**，不等于已可操作 N305。 |
| firmware | `linux/firmware.rs` → `axdriver_base::firmware::request()`；kernel `gt.rs:277` 从 rootfs-ready callback 加载。 | 产品 rootfs reader 的真实实现；不是 host 测试文件模型。 |
| IRQ work | `linux/irq.rs` 将 callback 异步交给 axtask。 | 产品 task-context 实现，不是 Linux hardirq/softirq；host kernel guards 是 NoOp。GT vector 未连接。 |
| tasklet / workqueue | `linux/tasklet.rs` worker pool；`linux/workqueue.rs` 单 FIFO persistent worker。 | 产品 axtask worker 近似；tasklet 不在 softirq 上下文；workqueue 不提供 Linux 并行度，delayed work 未完成。不能标为 Linux 原生服务。 |
| timer / wait queue | `linux/timer.rs` task-context timer worker；`linux/wait.rs` 通过 axtask sleep/wakeup。 | 产品调度/睡眠实现；host profile只验证可运行，不是硬件或时序证据。 |
| RCU | `linux/rcu.rs` 原子 reader population、grace period 和 axtask callback。 | 非 host-only，但为单一全局 reader 计数的保守实现，不等价 Linux per-CPU RCU；尚未用于 kernel默认提交 path。 |
| spinlock/IRQ state | `linux/locks.rs` 通过 `kernel_guard` 控制 target preemption/IF；host guard 为 NoOp。 | `target_os=none` 是真实 kernel guard，host 只是测试替身。 |

## 重复的所有者

切换时必须以同一条 source-of-truth 只保留一个调用 owner；默认手写/受限实现和 `upstream-gt` 来源翻译**并行存在不构成已经切换**。已存在的双实现列为 confirmed；计划补齐的同源文件列为 pending owner gate：

1. `intel_uc_fw.c`：默认 `uc.rs` 的 firmware policy/phase 子集 ↔ feature `intel_uc_fw_upstream.rs`（38/38）。前者保留 N305 loader state owner；feature 版尚未成为调用者。
2. `intel_guc.c`：默认 `guc_fw.rs` / `guc_config.rs` 的 Gen12 CT/notify/parameter slice ↔ feature `intel_guc_upstream.rs`（38/38）。default helper marker 已移除以免伪装成逐函数 source translation；运行时仍由自有 GT owner调用。
3. `intel_guc_submission.c`：默认 `guc_submission.rs` 的单 LRC/CTB 状态子集 ↔ feature `guc_submission_upstream.rs` source-order 翻译。默认路径尚在 `gt/copy.rs`。
4. `intel_guc_capture.c`：默认 `guc_capture.rs` parser/cache slice ↔ feature `intel_guc_capture_upstream.rs` (49/49)。后者保留 source-order 注册/读取/析构逻辑；default diagnostics cache lifecycle/IRQ caller并不完整。
5. `intel_lrc.c`：默认 `lrc.rs` 的 Gen12 XCS image helpers ↔ feature `intel_lrc_upstream.rs`。前者只支撑 N305自有 BCS/RCS 子集；两者有不同类型/owner契约。
6. `intel_execlists_submission.c`：默认 `execlists.rs` 两端口/CSB polling 子集 ↔ feature `intel_execlists_submission_upstream.rs`。未切换完整 request queue/CSB IRQ state machine。
7. `intel_wopcm.c`：默认 `wopcm.rs` Gen12 partition helper ↔ feature `intel_wopcm_upstream.rs`（10/10）。前者被当前 N305固件上传路径调用。
8. `intel_huc.c`：默认 `huc.rs` Gen11+/GuC auth slice ↔ feature `intel_huc_upstream.rs` (29/29)。两个 owner 都存在，接线时须选择唯一运行 owner。
9. `intel_guc_ct.c`：默认 `guc_ct.rs` 手写同步 CTB/HXG slice ↔ feature `intel_guc_ct_upstream.rs` (44/44)；两个实现都不是对方的 runtime delegation，接线时必须二选一。
10. `intel_guc_ads.c`：默认 `guc_ads.rs` 运行期 ABI builder ↔ feature `intel_guc_ads_upstream.rs` (40/40)。feature 版为源翻译/验证 owner，尚未切换成 default runtime backing。

**当前重复所有者条目：10 条**（全部已有两个源代码 owner；均需在接线时确定唯一运行 owner）。

## 接线时 kernel 侧必须提供的接口点

1. **MMIO owner**：受校验的 GT BAR/window，带 read/write/posting-read 与 terminal I/O failure 传播；同时声明 display 与 GT 的 BAR/寄存器 ownership。
2. **GGTT / display scanout 保留区**：GT 与 display 使用同一设备 aperture；kernel需暴露受保护的 scanout/firmware framebuffer范围、GGTT allocator、bind/unbind 与 DMA retirement。不能覆盖当前唯一 console framebuffer。
3. **IRQ/MSI**：共享 PCI function 的 GT/display vector ownership、mask/ack ordering 与 top-level handler；Gen11+ GT handler须接 display handler callback，不能重复处理 display bits。
4. **forcewake 与 runtime PM**：安装真实芯片的 `IntelUncoreFwGet` acquire/ack callback，`RuntimePmOps` 需与现有设备/runtime power owner一致；安装 PCI revision reader供 `INTEL_REVID(i915)` 使用；未安装必须拒绝 GT MMIO/uC init。
5. **rootfs firmware**：保留 `firmware::on_rootfs_ready()` callback 与 `/lib/firmware/i915/...` rootfs path；避免在 PCI probe 早期同步读 rootfs。
6. **reset、DMA 与执行 fences**：reset域须声明影响的 engine/display；要求有 completion/fence、device DMA停止证明后才能解绑/释放对象页；未知状态要 quarantine。
7. **power-domain callback**：GT unpark/park 需与 display `POWER_DOMAIN_GT_IRQ` 对接，且 asynchronous put 保留 display power ref生命周期。
8. **平台身份/GT拓扑**：传入 PCI ID、revision、fuses/engine mask、graphics/media version、GGTT/VM owner；当前 N305 admission 是 exact 46D0 A0，不可泛化成所有 Gen12。

## 会碰 display 的地方

当前列 **6 条 display 交叉风险/接口**，切换时逐条审阅：

1. **共享 MMIO BAR 与 IRQ 总控**：`gen11_irq_handler()` 顶层包含 display 和 GT 分发；Rust 必须保留 `kernel/src/drm/intel/irq.rs` 的 display callback/ACK顺序，不能重新 ACK/屏蔽 display HPD、AUX、vblank状态。
2. **GGTT shared address space**：scanout surface / cursor / firmware framebuffer pages 属于显示；GT GGTT bind、evict、TLB invalidation 必须严格绕开/等候这些 pins。
3. **整 GT reset**：上游 `intel_gt_gpu_reset_clobbers_display()` / `intel_gt_pm.c::gt_sanitize()` 可能执行全引擎 reset；默认 N305只做被证明隔离的 BCS/GuC域操作。未知 stepping 或 reset传播时不得扩大范围。
4. **display power domain**：上游 `__gt_unpark()` 会取 `POWER_DOMAIN_GT_IRQ` 并在 park 后 async put；GT 不能绕过现有 display power ref计数。
5. **DC/DMC 状态与中断延迟**：GT activity 改变 display低功耗/IRQ电源保持时间，可能影响显示中断延迟和模式转换；需走同一个 power-domain owner，不可仅写寄存器。
6. **唯一 console scanout**：N305屏幕是唯一控制台；display reset、GGTT重用、全局电源下电及错误恢复都必须保留当前 firmware/native scanout，除非显式 modeset owner完成新 framebuffer切换。
