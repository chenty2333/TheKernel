# i915 Gen12 GT/GEM 移植地图

## 范围与约束

上游基线为 Linux `7.2.3`（`/home/ava/Desktop/linux-7.2.3/drivers/gpu/drm/i915`），平台为 Tiger Lake、Rocket Lake、Alder Lake-S/P/N。逐文件核对 SPDX 后再翻译：本任务列出的 GT/GEM/查询文件大部分为 `MIT`；`i915_perf.c` 及若干头文件/辅助文件需单独核验，若为 GPL-2.0 则不复制，只按 ABI/硬件规范实现或暂略。任何 GPL-only 的 DRM 框架、调度器和内存管理代码均不搬入。

Rust 侧复用 `kernel/src/drm/intel/{gt.rs,gt_probe.rs,gem_exec.rs,gem_context.rs,gtt.rs}`、`kernel/src/drm/intel/gt/`、`kernel/src/drm/intel/gtt/`、`kernel/src/drm/{render,dmabuf,fence,syncobj.rs,syncobj/}` 与 `crates/ax/tk-intel-gt/`。GPU 引擎寄存器访问、PCI/DMA/IRQ 继续映射到 TheKernel 现有设备接口；用户 ABI 映射到现有 DRM ioctl/GEM、fence 与 syncobj 接口。不得整体移植 Linux DRM 核心。

## 文件映射

| 上游文件/模块（Linux i915） | SPDX/处理 | TheKernel 对应与状态 |
|---|---|---|
| `gt/uc/{intel_uc.c,intel_uc_fw.c,intel_guc.c,intel_guc_fw.c,intel_guc_ads.c,intel_guc_ct.c,intel_guc_log.c,intel_huc.c,intel_huc_fw.c}` | 所列 `.c` 文件 SPDX 均为 MIT；平台固件名/版本取自 `intel_uc_fw.c` | 部分已落地：默认策略、设备 ID 到 uC 平台选择、固件候选与 CSS/version 校验、rootfs request、Gen12 DMA staging/upload、GuC READY poll、Gen11+ GuC MMIO auth；GSC/CT/ADS/log 与完整生命周期仍缺 |
| `gt/{intel_execlists_submission.c,intel_engine_cs.c,intel_context.c,intel_lrc.c,intel_ring.c,intel_timeline.c,intel_breadcrumbs.c,intel_engine_heartbeat.c}`、`gt/uc/intel_guc_submission.c` | 这些 `.c` 文件 SPDX 为 MIT；提交策略以 `gt/uc/intel_uc.c` 平台默认值为准 | `kernel/src/drm/intel/gt/`、`gt.rs`、`crates/ax/tk-intel-gt/{lrc,rcs,bcs}.rs`；扩展引擎队列、抢占/时间片、VCS/VECS 与完成通知 |
| `gem/{i915_gem_execbuffer.c,i915_gem_object.c,i915_gem_shmem.c,i915_gem_userptr.c,i915_gem_stolen.c,i915_gem_mman.c,i915_gem_tiling.c,i915_gem_domain.c,i915_gem_shrinker.c,i915_gem_context.c}` 及 `gt/intel_gt.c`（eviction） | 上述 GEM `.c` 文件 SPDX 均为 MIT；文件名/函数边界按上游目录 | `gem_exec.rs`, `gem_context.rs`, `gtt.rs`, `gtt/`, `render.rs`, `dmabuf.rs`, `fence.rs`, `syncobj/`；去除非上游限制，映射用户指针与 mmap 到现有 VM/DMA 能力 |
| `gt/intel_rps.c`, `intel_rc6.c`, `intel_gt_pm.c`, `intel_llc.c`, `intel_workarounds.c`, `intel_reset.c` | 目标 `.c` 文件 SPDX 为 MIT | GT 新增电源、Gen12 workarounds、引擎/整卡复位；复用 `gt_probe.rs`、`tk-intel-gt/reset.rs` 与寄存器访问 |
| `i915_query.c`, `i915_getparam.c`, `i915_perf.c` | 前两者有 MIT SPDX；`i915_perf.c` 头部带完整 MIT 许可文本（无 SPDX 行） | `render.rs` 与 DRM ioctl/UAPI；实现 ABI 可见 query/getparam 和 OA 可行部分 |
| Linux `drivers/gpu/drm/` 通用 DRM/GEM/VM/调度器框架 | 不移植框架层 | 映射到 `kernel/src/drm/{render,dmabuf,fence,syncobj.rs,syncobj/}` 和现有 VM、DMA、文件描述符接口；确需通用能力时新增最小接口 |
| 用户态载荷：Mesa anv、`vulkaninfo`、intel-media-driver、libva、`vainfo` | 各仓库/文件独立核许可证；iHD 按 MIT 处理 | 在现有 N305 iris smoke payload 旁增加可复现构建/guest 加载检查；无设备时只验证预期拒绝行为 |

## 移植边界

不翻译 Linux DRM 框架、debugfs/sysfs 管理面、非 Gen12 平台路径及任务未列出的显示功能。GSC、LMEM-only/独显路径以及需要尚不存在的内核内存/用户页能力部分，先核对 Gen12 ADL 集显调用路径；不能安全映射的功能记录为未移植，不以占位成功掩盖。上游代码按函数保留控制流和错误顺序，翻译文件顶部登记来源/完整版权行，每个翻译函数用 `// upstream: <文件> <函数>()` 标注；MIT 全文及来源登记遵守 `COMMON.md`。

## 已接入的 uC 传输切片

`crates/ax/tk-intel-gt/src/uc.rs` 已包含 TGL/RKL/ADL-S/P/N 固件候选表、CSS 大小校验、固件版本检查、ADL-S HuC-only 与 ADL-N 默认 HuC authentication + GuC submission 策略、GuC submission ABI 版本规则和设备 ID 到平台映射。`huc.rs` 将 Gen11+/Gen12 legacy GuC auth status bit、action/wait 调用和 `intel_huc_check_status()` 错误码映射接入上述 GuC/HuC 上传路径。rootfs-ready 回调从 `/lib/firmware/i915/...` 取回固件；在 MIA 确认处于 reset 时，将 HuC 先于 GuC 暂存到固定物理页/GGTT，并按 `intel_uc_fw.c` DMA 顺序上传、按 `intel_guc_fw.c` 写 RSA scratch 和轮询 READY。GuC RSA 支持固定大小 MMIO 与大 RSA GGTT-VMA 两路。FirmwareImage 跟踪 Available/Loadable/Transferred/Running/LoadFail 阶段，GuC BootROM/UKernel 失败映射保留可判别原因；GuC 控制参数与 soft-scratch 序列、log sections 大小/单位/verbosity 策略、debug/crash ring snapshot 和 overflow accounting 已有翻译，但 ADS/log GGTT 缓冲区分配、参数写入启动调用、relay/IRQ/event 处理还没接入。CTB ABI、环形读写、HXG 解析、request fence/response credit 与 G2H event action 分类/credit 返还 helper 已实现；对于默认开启 submission 的平台，CTB VMA 分配及 self-config 注册/enable 已接入并由 owner 保留，但 CT shared buffer 的 peer poll/IRQ intake、event worker 和 H2G submission clients 仍未连接。GuC MMIO send 支持四个 send register 的 HXG busy/retry/failure 处理及可选响应复制；HuC RSA 所在 GGTT 映射保持到 GuC auth action 与 `GEN11_HUC_KERNEL_LOAD_INFO` verified 状态均完成，再安全释放。DMA/auth/解绑不确定时保留页与绑定并 quarantine owner。已列出的 Gen12 PCI IDs 路由到各自平台策略；N305 专属 PPGTT/RCS 路径仍要求精确 46D0 A0，其他 ADL-N 变体不走该路径。GuC submission 调度、ADS/log 资源及 resume/re-upload 仍未移植，所以这不等同于媒体解码或 GuC submission 已可用。

`guc_ads.rs` 当前翻译 GuC ADS 固定 ABI、固定/动态区域布局、全局 policy 更新 action、engine mapping table 与 enabled mask，并可消费调用方提供的 MMIO regset、golden LRC、capture lists、runtime GT system-info 来构造完整静态/动态 blob；包含 source 对齐的 duplicate/sort、capture null page、WAKLV firmware/platform gates、私有区 reset 和 engine-usage offsets。硬件相关输入的收集（engine workaround/MCR regset、context default-state 和 capture subsystem）、ADS GGTT VMA 的安全分配/销毁与 reset owner、startup parameter 接线仍未完成；builder 不是 GuC 可用性证明。
