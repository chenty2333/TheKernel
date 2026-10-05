# ADL-P/N i915 显示代码移植计划

2026-10-05；基线 main `730f768e`；工作区 detached
`/home/ava/Worktrees/TheKernel/intel`。本文件是 D1 的评估，**不是移植完成
或真机点亮的声明**。来源为本地 Linux 7.2.3
`drivers/gpu/drm/i915/display/`，除注明外下面各文件已检查为 MIT。

## 已核实的事实和关键差异

- `intel_display_device.c` 的 `adl_p_desc` 将 ADL-N 列为 ADL-P 子平台；
  `xe_lpd_display` 的显示版本是 13。`46d0` 的 ADL-N display stepping 表
  与普通 ADL-P 不同，不能直接按 PCI revision 叫 A0/A1。
- 2026-10-04 TheKernel UDP 日志识别了 `8086:46d0`，寄存器可读，默认路径
  未写显示寄存器；GT ACK 为 0。它没有运行原生 modeset，也不是点亮证据。
- 2026-10-03 采集包确实有 `graphics/debugfs-0000:00:02.0/i915_vbt`
  （8704 字节）、`i915_opregion`（8192 字节）和 HDMI EDID（256 字节）。
  不需要把“没有 VBT”作为前提。
- **实际 Linux HDMI 路由是 `DDI TC1/PHY TC1`**（`i915_display_info`），
  当前 Linux 模式是 3840x2160@30，297000 kHz；同一显示器公布了
  1920x1080@60：148500 kHz，H 1920/2008/2052/2200，
  V 1080/1084/1089/1125，正 H/V 同步。CDCLK 为 192000 kHz。
  这是采集时 Linux 的状态，不是 TheKernel 启动固件状态。
- 现有 `rollback::Transaction::begin` 只接受 pipe A + combo HDMI DDI A/B，
  拒绝 Type-C。这一限制应保留，直到 TC cold/power、DKL PHY 和 TC PLL
  的完整恢复协议实现。不能把 TC1 重命名成 DDI A，也不能套 combo PLL。
- 现有 Intel 代码约 24708 行（不含寄存器子目录）。它是原创参考实现，
  不是可声称“已移植 i915”的代码。`linear.rs` 提供的是固定帧缓冲 CPU
  拷贝 KMS 适配，不是 Intel atomic 硬件提交，也没有 i915 render uAPI。

## 完整显示依赖及替换清单

以下函数是入口/核心函数清单，**不是全量调用图已经移植的声称**。
估算为裁剪到 display 13、单 HDMI、无 DSC/MST/DSI 后的 Rust 行数，包含
类型与错误路径，不包含测试；实际随核对变化。源码函数名以本地树为准。

| 阶段 | Linux 文件和关键函数/数据 | TheKernel 处置 | 估计 Rust |
|---|---|---|---:|
| 身份/stepping | `intel_display_device.c`: `adl_p_desc`, `xe_lpd_display`, `adl_p_adl_n_steppings`, runtime fuse 裁剪 | PCI/ECAM 映射保留；移植 ADL-P/N 身份和 fuse 筛选，替换 `id.rs` 平台推断 | 300–600 |
| VBT/OpRegion | `intel_bios.c`: `intel_bios_is_valid_vbt`, `find_raw_section`, `parse_general_definitions`, `dvo_port_to_port`, `parse_ddi_port`, DDC/AUX/HDMI caps；`intel_vbt_defs.h` 相应结构；`intel_opregion.c`: `intel_opregion_setup` VBT 查找 | 新 `bios.rs`/`opregion.rs`；先纯字节解析。保留平台 ACPI，只读 PCI ASLS；不移植 SWSCI/ASLE 写握手到默认路径 | 1800–3000 |
| 状态读出/fastboot | `intel_modeset_setup.c`: `intel_modeset_readout_hw_state`；`intel_display.c`: `hsw_get_pipe_config`, `intel_get_transcoder_timings`, pipe source；`intel_ddi.c`: `intel_ddi_get_hw_state`, `intel_ddi_get_config`, `icl_ddi_tc_get_config`；`skl_universal_plane.c`: `skl_plane_get_hw_state`, framebuffer readout；CDCLK/DPLL/WM 的 get_state | 先只读移植，保留 `firmware_scanout.rs` identity/progression 验证；完整状态等价 + framebuffer 归属后才可 fastboot；仅分辨率相等不够 | 1800–3000 |
| 电源井/DC/DBUF | `intel_display_power.c`, `intel_display_power_well.c`, `intel_display_power_map.c`: ADL-P wells、`hsw_power_well_enable/disable`, `icl_display_core_init`, DC/DBUF，`intel_display_power_tc_cold.c` TC cold 阻塞 | 替换 `power.rs` 硬件序列；适配 refcount、时间、sleep/poll，拒绝在中断里阻塞。snapshot/rollback 必须同步扩展 | 2500–4000 |
| CDCLK | `intel_cdclk.c`: `icl_readout_refclk`, `bxt_de_pll_readout`, `bxt_get_cdclk`, `bxt_set_cdclk`, `adlp_cdclk_pll_crawl`、ADL-P 表及 voltage/PCODE 握手 | 替换 `clk.rs` 硬件部分；fastboot 首期只读保留现值，不为 1080p 无谓换频；重设前实现 PCODE 与回滚 | 1200–2200 |
| PHY/TC | `intel_combo_phy.c` combo 的 init/verify/uninit；`intel_tc.c`: `adlp_tc_phy_connect/disconnect`, legacy/DP-alt/TBT mode 和 lock；`intel_dkl_phy.c` 间接 HIP/PHY 访问；`intel_ddi.c` DKL 设置和 `adlp_tbt_to_dp_alt_switch_wa` | TC1 是目标 HDMI 的必需路径；替换 `phy.rs`，引入 `tc.rs`/`dkl_phy.rs`，不要裁掉“USB-C”就误删 legacy HDMI | 2200–3800 |
| PLL | `intel_dpll_mgr.c`: `icl_calc_mg_pll_state`, `icl_calc_dpll_state`, TC PLL enable/disable/get_state、`adlp_cmtg_clock_gating_wa`；`intel_dpll.c` port clock/routing | 替换 `pll.rs`；combo 与 TC 两个域分开，不把 148500 输入 combo 的结果当 N305 TC 结果 | 1800–3200 |
| DDI 信号/使能 | `intel_ddi.c`: pre_pll_enable/pre_enable/enable/disable/post_disable、transcoder funcs、get_config；`intel_ddi_buf_trans.c` ADL-P HDMI DKL/combo 表 | 替换 `output.rs`/`swing.rs`；保留所有适用 stepping workaround、延时和超时，先 encoder enable 后 transcoder | 2500–4000 |
| HDMI/EDID/infoframe | `intel_hdmi.c`: clock_valid/compute_config、AVI/SPD/audio infoframes、HDMI enable；`intel_gmbus.c` GPIO fallback、DDC pin 表；`intel_dp_aux.c` AUX 只在实际需要时 | 现有 EDID/mode 表和边界验证保留，移植 HDMI policy/DDC board mapping、SCDC/infoframe；1080p 不用 SCDC 高 TMDS，但不能伪造能力 | 1600–2800 |
| Pipe/transcoder/plane | `intel_display.c` timing/source/pipe/transcoder enable/disable；`skl_universal_plane.c` surface/control/stride/offset/update/disable、`adlp_plane_ctl_arb_slots`；相关 regs | 替换 `pipe.rs` 寄存器序列，保留 framebuffer ownership/console abstraction；fastboot 不重写固件 plane | 2000–3500 |
| DBUF/DDB/watermark | `skl_watermark.c` memory latency、DDB 分配、atomic compute/commit/get_hw_state | 替换现有近似水位；不可用单一固定 WM 代表 i915 的带宽证明；需 memory/PCODE latency 输入 | 2000–3500 |
| DMC | `intel_dmc.c` parse_fw/load_program、ADL-P DMC 配置和 event handlers | 新模块，独立 firmware loader，保留默认不加载/不进 DC；DMC 不是初次 HDMI 点亮的前置条件 | 800–1400 |
| IRQ/vblank/hotplug | `intel_display_irq.c`, `intel_hotplug.c`, `intel_crtc.c` vblank | 保留任务级通知接口，移植 W1C/masks/HPD storm/reset、vblank 时间；读轮询不宣称硬件 IRQ 已实现 | 1200–2200 |
| HDMI audio | `intel_audio.c` HSW/DDI codec enable/disable、get_config、ELD、audio clock；HDA component callbacks | 显示稳定后移植；HDA 只改最小握手接口；默认跟随 modeset opt-in | 800–1500 |
| DRM adapter | Linux atomic 状态对象不直搬；TheKernel `DisplayAdapter`, atomic/KMS/GEM/fence | 保留 TheKernel 框架，新增真实 Intel adapter； TEST_ONLY 只算状态，commit 串行化、fail-closed rollback，不能 CPU copy 冒充 pageflip | 1000–1800 |

合计估算约 2.4–4.1 万行 Rust（含适配，不含测试），不是一两次寄存器写即可
完成的功能。DP 的链路训练/MST、DSC/PSR/eDP、DSI、HDCP、GT、其他显示代
际不属于 D2 首个 HDMI 目标；TC legacy HDMI 必须保留。遇到 ADL-P 共享分支
需逐一核对，不能只 grep `DISPLAY_VER == 13` 就裁掉通用 Gen11/12 路径。

## 安全和许可证

硬件读出与编程分离。`RegisterIo` trait 注入 MMIO，主机模拟可断言全部
访问顺序；错误、无电源、越界读不能默认为 0。轮询既限时又限次数。
只有 `intel.modeset=1` 才考虑写显示，`intel.gt=1` 与显示独立；现有完整
snapshot/rollback 和 GGTT before-image 保留，未知 TC 隐藏状态不接受提交。
fastboot 不只检查目标模式，还需 exact timing/port clock/bpp/color/scaling/
format/modifier/stride/offset/plane ownership，及稳定 SURFLIVE/扫描线进展。

新 `crates/ax/tk-intel-display` 为 no_std、MIT，每个翻译文件记录 Linux
7.2.3 源文件/函数及原版权，新模块附 LICENSE-MIT 和 NOTICE。这里只审核
上表文件的 MIT 头，没有凭目录概括全部许可。GPL 的 `intel_acpi.c` 和
trace 文件不移植；DRM helper、GT 与 HDA 各文件到使用时重新审核许可证。
文档登记只记录实际翻译的函数，计划项不能登记成已完成。ACPI 区域归 E，
D 不改其文件。原生硬件行为始终标 **未在硬件上验证**。

## 实施顺序与测试门槛

1. D1 本评估（仅文档）。
2. D2a 新 crate、ADL-P/N 身份、VBT/OpRegion 安全解析，回放真实采集
   数据；先做只读固件 timing/route/plane 读出，接到 kernel 诊断中。
3. D2b 完整 fastboot 状态验证、固件 framebuffer 归属/GGTT 证明和 KMS
   固定模式接管；每个遗漏字段都是“不支持”，不是自动重设。
4. D2c TC1 电源/PHY/PLL/HDMI/pipe/WM 编程依赖逐项移植；每一子项独立
   提交，与源码推导的期望轨迹/故障前缀对比。具备完整 TC 回滚才放行。
5. D2d Native atomic + IRQ/hotplug + optional DMC，保留固件保底。
6. D3 采集 EDID→时序→**TC** PLL/CDCLK 全流程差分、下一次真机步骤。
   必须区别“源码期望轨迹测试”“编译 upstream C 的独立 oracle”和
   “实际真机”；没运行的不能称通过。
7. D4 先路线评估，再 forcewake/reset、BCS、GEM/uAPI/fence、RCS；不能
   在显示依赖未完成时把裸 batch 提交开放给 Mesa。
8. D5 HDMI 音频（可先纯 ELD 算法测试，实际握手依赖 D2 TC 链路）。

每个提交：相关 crate + `kernel/src/drm` 主机测试、lint 和 fbcon。影响内核
运行再跑 KVM guest。每三提交/最后跑全 host、guest、两平台 lint。使用独立
`THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-intel`、6 个构建
任务、nice 10。这些 QEMU/host 结果不能确认 TC PHY、屏幕像素或音频输出。

## Continuation: DKL firmware state readout

DKL access and the display-13 TC PLL readout are implemented in the MIT crate.
The power-reference/lock backend contracts prevent reads from dark PHYs and
selector races. Firmware discovery restores the whole shared HIP index and
verifies it, including failures after potentially landed stores; uncertain
restoration is a quarantine error, not a successful readout. The upstream raw
read/mask order is unchanged. No kernel call site enables these accessors yet;
PLL readout alone is not full firmware equivalence or fastboot takeover.

Measured: local compiled i915 C agrees on 192 primitive-operation traces across
four ports/16 banks and 12 PLL states/read traces. Five regression tests cover
layout, unchanged RMW stores, dark/disabled domains, 18 fault prefixes and both
restoration write/read failures. No physical MMIO or monitor output was tested.

### Plane reconstruction

Plane format/modifier/rotation/stride/main-size readout now follows the upstream
initial-plane path, including two CTL reads (get_hw_state then reconstruction).
Display13 exposes five universal planes per pipe. ADL-P has Yf, not 4-tile.
Readout preserves unknown-format fallback as i915 does; fastboot admission
must not accept it as XRGB evidence. The plane-local admission helper rejects
nonlinear/DPT, auxiliary or multiplane formats, rotation/reflection, encryption,
async flip, keying and non-bypassed plane color. It is necessary, not sufficient:
full link/pipe/scaler/color/WM/GGTT/latch stability still belongs to takeover.
Known difference: main-size multiplication uses u64 rather than wrapping u32.
Measured: compiled upstream C agrees on 1792 states and exact read traces;
four model tests cover missing/dark domains, unsupported layouts and admission.
No kernel plane writes or hardware output were enabled by this slice.

### Color discovery

Pipe configuration, optional pipe/output CSC matrices, 129-entry degamma,
256-entry legacy gamma and 1024-entry precision gamma now follow display13
upstream readout. Indexed palette selectors are saved/restored/verified under a
backend lock, even when a write lands before reporting failure. Buffers are
caller-owned; failed captures cannot publish partially filled arrays. Raw CSC
and LUT dwords are retained separately from decoded fields.
Known upstream limitation: multi-segment readout only yields nine super-fine
entries; fine/coarse entries are unreliable in i915 too. This port explicitly
marks that state incomplete and must not claim complete LUT equivalence.
Compiled-i915 C comparison passes 192 states/entry streams/traces. Five host
regressions cover bypass/dark domains, buffer bounds, unsupported precision,
all indexed-access failure prefixes, CSC and LUT packing. No runtime color
programming or hardware validation was introduced.

### Scaler discovery

Display13 pipe-scaler getter and window readout now follow i915, with separate
already-enabled PANEL_FITTER power. An extra ownership helper inspects both
controls so a plane-bound scaler cannot be missed by the pipe-only getter.
Any active scaler remains outside plane-only fastboot admission; geometry and
filter programming belong to native modeset. Compiled-C comparison passes48
configurations/read traces, including pipe-D0x800 stride and direct window
sizes. Dark/missing domains and reserved bindings are covered. No runtime call
site or scaler programming was enabled.

### Watermark/DDB discovery

Six display13 latency levels plus transition/SAGV/SAGV-transition are read for
five exposed planes and the cursor. DDB readout follows the display11+ path
(no NV12/extra MIN_BUF_CFG read); disabled end0 stays disabled, otherwise end
becomes exclusive+1. Raw DBUF controls/enabled slice mask and MBUS_CTL are kept
for the outer stable-state proof. DDB remains MBUS-relative until pipe/slice
mapping is reconstructed; this slice alone cannot authorize allocation or a
watermark change. Computation, global bandwidth/MDCLK policy, SAGV and PCODE
programming remain for modeset. Compiled-i915 comparison passes64 states and
all65 reads. Missing/dark registers and layout boundaries are covered. No
runtime or hardware WM programming was enabled by this slice.

### TC PHY discovery

TC ready/owned and modular-FIA field readout are ported with already-enabled
core/port/cold-domain contracts. Display13 pin assignment uses DFLEXPA1, not
the newer TCSS field. All-ones TCSS reads mean not-ready, never permission for
DKL access. Original before-image discovery captures19 DKL setup words (both
lane groups plus UC status), restores/verifies HIP and quarantines uncertain
restoration. 48 compiled-C predicate/FIA state/read-offset cases and all43
fault prefixes on four ports pass. No TC cold/ownership acquisition was enabled;
HPD mode selection and complete DDI/encoder admission still follow.
