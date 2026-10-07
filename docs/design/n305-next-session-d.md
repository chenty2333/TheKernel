# N305 下次显示/GPU/HDMI 音频验证 — Codex D

2026-10-05。**全部未在硬件上验证；这一轮不启动真机、PXE服务或写USB/NVMe。**
下面是未来用户执行的步骤，不是已验证结果。受限 TC1/TC2 固件等价 fastboot/KMS
调用链已接通并做主机模型测试；已接通受限 TC 模式重编程、RCS/iris 提交和 HDMI audio 软件链；真实 Mesa 初始化、像素及声音仍未验收。

## 基线和板级资料

1. 从整合后的 `/home/ava/Desktop/TheKernel` main，以已知可启动的固件帧缓冲
   配置重建 n305 shell；状态目录 `/home/ava/.cache/thekernel-targets`。
   旧 `wt-intel` 镜像及测试是独立工作区历史结果，不代表整合镜像已真机验收。无 `intel.modeset=1`、
   无 `intel.gt=1`、**无 `nvme.allow_write=1`**。不要改其他Codex网络或服务。
2. 屏幕应继续可读、可以更新文字、打开shell。记录只有默认只读Intel probe，
   不把该屏幕归功于新驱动。读 `/sys/kernel/debug/dri/0/intel_gpu`。
3. 在已有Linux启动环境读 `.../i915_vbt`、`i915_opregion`、`i915_display_info`
   和HDMI EDID。已有2026-10-03 capture的VBT可解析：BDB249、combo B为DP，
   TC1/TC2为legacy HDMI，DDC硬件selector9/10，HDMI levelshift5。所接屏幕
   采集时在TC1、4K30，并公布1080p60。新插线/固件配置可能改变实际连接，
   不用旧capture替代当前readout。OpRegion2.1 RVDA是base+8192，数据8704字节。
4. TheKernel尚没有新VBT导出节点。添加节点时应返回缓存的原始VBT字节且只读，
   不能在每次read时重触发映射/硬件读取，也不能在共享伪文件系统里大改。

## 固件等价 fastboot / 原生 KMS（未在硬件上验证）

先保持原有只读启动基线。未来由用户单独启用 `intel.modeset=1`，不加
`intel.gt=1`。仅 exact N305 display D0、single pipe A、VBT-confirmed TC1/TC2
legacy HDMI、线性 XR24、无颜色/缩放/DSC/VRR 等状态可进入；其他配置应明确
REFUSED，固件 console 继续可读，不算原生显示成功。驱动读取本次启动的
ASLS/VBT、PLL/PHY/pipe/plane/WM；不会把旧 Linux capture 当本次固件状态。

1. 看到 `intel-fastboot: native fixed-mode KMS registered`，确认模式与当前
   firmware/GOP 完全相同（可能4K30，不强制切1080p60）。没有 PLL/link/timing/
   WM 重编程。日志只说明软件接管判断，仍需用户确认屏幕与文字实际正常。
2. fbdev/console 使用 DRM dumb GEM。确认 console 的新原生 SURFLIVE 地址和
   hardware frame counter 持续变化；原始 firmware GGTT PTE 保留。
3. 启动未修改 Weston 的 DRM + pixman 后端，确认真实图像更新与连续翻页。
   KMS/atomic fence 要在 SURFLIVE 和后续新硬件帧后完成，不能只看 ioctl=0。
   关闭 Weston/返回文字 VT，确认既有 fbdev atomic restore/repaint 返回可读
   console。此步骤不证明 iris/ANV/GPU渲染。
4. DPMS、gamma、cursor、缩放、非目标 pitch/格式/模式请求目前应返回不支持，
   不能出现成功返回却没有实际效果。专用 MSI 仅在源/PCI原状态可完整归属时接通，拒绝时回退轮询。
   IRQ唤醒任务，KMS仍用硬件帧计数与模式epoch，不制造软件帧；时间戳
   仍为任务观察值，不宣称ISR时间戳。验证插拔同一显示器的connector变化、
   新EDID则明确拒绝；IRQ源故障保留callback/vector/window并回退。
5. fastboot 不使用 combo-only 的 `intel.modeset.fail_write` 注入参数，带此
   参数会在写之前拒绝。当前失败恢复已在主机模型验证：plane store 可能
   已落地，恢复前一 surface 并看到 fresh frame 后才释放新 GGTT；恢复不确定
   则保留 DMA owners 并终止后续提交。真机故障注入入口尚未开放，不强行用
   旧参数测试 TC。观察画面是未来真机回滚验收的必要部分。

## TC1/TC2 原生模式与失败恢复（软件已接通，未在硬件上验证）

仅本次固件已供电并持有所有权的 legacy HDMI 可接管。确认 connector
公布固件 exact timing 和经 EDID、PLL、CDCLK、保留 WM/DDB 容量验证的
1080p60；未公布的模式必须拒绝。用 Weston/atomic 请求1080p60，检查
实际分辨率、持续翻页、SURFLIVE/fresh hardware frame、清晰 console，
再切回固件模式。不是固定帧缓冲 CPU 拷贝测试。

冷端口/未知所有权、不同显示器、需要新 CDCLK/PCODE/WM 策略的模式
仍拒绝；不要用旧 combo fail_write 参数，它在 TC 写前拒绝。TC 失败
前缀已做模型验证，未来硬件注入必须另经授权：原 AVI、PLL/PHY、
plane/timing/GGTT 完整恢复且 fresh frame 后才释放候选页。无法证明
退休时保留 DMA 和电源，禁止继续写；读回正确仍需用户看实际画面。

## GT 和 HDMI 音频（各自依赖实现门槛）

- GT默认关闭。未来用户单独以 `intel.gt=1` 启动（不需要 `intel.modeset=1`），
  应先看到精确 N305 GT/media A0、forcewake 和 BCS-only reset admission。
  GuC 未处于 MIA reset、RCS 忙、DMA translation/ownership 无法证明时明确拒绝。
  BCS 链已经实现：private PPGTT/LRC → ELSQ → hardware breadcrumb → BCS reset
  retirement →16384目标字节/源不变/双方两端4KiB guards逐字节核对。
  只有实际结果通过才应出现 `BCS_COPY_BYTES_AND_GUARDS_VERIFIED`；这仍不是
  RCS/Mesa rendering。主机模型成功不能代替该真机验收。失败时有 bounded
  timeout/reset，无法证明退休则保留全部 DMA owners、禁止再提交；确认 console
  持续可读。当前没有开放真机故障注入，不使用旧 display fail_write 参数。
  GEM/submit/binary/timeline/sync-file、持久 VM/上下文和标准多 BO softpin 链已接通；
  真实用户程序尚未验收。图形镜像现在包含 `intel-bcs-smoke`，未来用户显式运行
  `intel-bcs-smoke --execute /dev/dri/renderD128`，只有真实 ioctl 提交、16384 字节
  readback、binary syncobj wait 和 mmap-after-close 全通过才出现用户态成功标记。
  本轮只编译及测试无参数拒绝入口，未打开主机 DRM。之后分别测试 RCS/iris/ANV。
  N305 GuC 应是 tgl 系列，不是 adlp_guc；display D0 不等于 GT/media A0。
- HDMI audio依赖实际TC link、audio powerwell、ELD和HDAcomponent握手。现已
  接入软件链，不把模拟ELD/analogcodec枚举当HDMI音频通过。跟modesetopt-in
  开启，验证显示器audio能力/ELD、HDA HDMI pin/converter、48kHz双声道真实
  输出、静音/停止/拔线的生命周期；拔线或回滚要先撤掉audio valid，再安全
  停止DMA。当前HDA模拟音频不得为了让程序exit0而冒充HDMIcodec。

### RCS fixed shader acceptance (software implemented; NOT hardware verified)

Future user execution only: first establish the BCS baseline, then explicitly
select `intel.gt=1 intel.rcs=1`. Expect the RCS-specific byte/guard marker only
following real hardware completion and render-domain reset retirement. Neither
QEMU unknown-GPU refusal nor the host memory model counts as rendering success.
Then run `intel-bcs-smoke --rcs-execute /dev/dri/renderD128` in the graphics image;
its fixed shader page, exact16384-byte readback, buffer guards, output binary
sync and mmap-after-close must all pass. Unsupported revisions/layouts/pages/
flags should refuse, and ambiguous DMA retirement should retain owners and
close further submissions. Do not use the display fail_write switch for GT.
This does not establish iris/OpenGL or ANV/Vulkan support; test those separately
only after their actual general submit/context/query contracts are implemented.

The explicit BCS/RCS client now first checks CHIPSET_ID and two-stage engine
QUERY, then creates a per-file context and submits through that context. Native
fuse topology/CS clock are available through GETPARAM/QUERY only after successful
GT admission; missing facts fail rather than returning the product specification.
This remains a bounded acceptance client, not evidence that Mesa initializes or
executes. Default-state isolation requires successful native capture; shared user VM and
nonprivileged standard softpin batches are now implemented, not hardware verified.

GT cache-policy preflight now reads actual media disable fuses and pins only
present VCS0/VCS2/VECS0 wake domains. A busy ring/lost ACK/unavailable mapping
must refuse before shared PAT/MOCS/L3 changes; it must not stop or reset media.
The firmware RCS preflight likewise requires empty head/tail and MODE_IDLE.
These new reads/wake checks are software-verified only, not a native observation.

The explicit BCS/RCS client now submits a fresh timeline output point through
the source exec extension and checks SYNCOBJ_TIMELINE_WAIT after exact readback.
Binary fence arrays remain supported separately. Links/unknown extensions,
reserved words and nonzero same-point WAIT+SIGNAL are refused; source header
layout is32 bytes plus24 timeline bytes. Client build/install and model checks
are not a native GPU or Mesa acceptance result.

The explicit BCS/RCS client now also requires CLOEXEC output sync_files, terminal
POLLIN and a second submission using the first descriptor as FENCE_IN, with
fresh timeline point2. This transport is compiled/model-verified only. The
current execution remains bounded synchronous; standard iris softpin admission
now uses the same fence transport rather than the fixed startup shader.

The explicit acceptance client now creates a real VM and attaches it through
CREATE_EXT/SETPARAM, then destroys the VM ID before its two jobs; success requires
retained page-table ownership. This is not saved-context/general-Mesa proof.
The same Mesa26.1.2 iris runtime is built in the Intel state's mesa-iris-stage.
Use intel-mesa-smoke --initialize NODE and --execute NODE as distinct acceptance
steps with the implemented general-submission/context path.
Neither marker has been measured; default graphics images still use virgl or
software and their success cannot substitute for these native checks.

Native GT bootstrap now records reset defaults through two kernel idle contexts
and scoped retirement before RCS shader selftest. Context-isolation class bits
are conditional on successful native capture, never product labels or models.
Per-slot state storage is source/model-verified only; actual repeated-context
state saving/restoration and default captures remain physical acceptance items.
General Mesa submission/residency is software implemented. Do not infer iris
initialization/pixels from minimal RCS or these software tests. First run
--initialize separately, then --execute; require the real Intel renderer and
triangle readback, never software fallback. --softpin-execute additionally
checks high-address BCS objects through standard batch-first/handle-LUT ABI.

HDMI音频软件链已接通。未来单独验收 sink ELD、TC1/TC2各自确认过的
HDA数字pin/converter、48kHz双声道可听波形，以及拔线/停止/模式回滚时
先撤ELD再安全退休DMA。不能把模拟器analog播放或源C时序对照当HDMI声音。
冷/unowned TC、不同sink、新CDCLK/WM策略及非目标格式仍明确拒绝；
DMC/GuC保持可选，不是当前软件调用链前置。

## 可用目标用户态镜像（2026-10-07；仅软件装载已验证）

`wt-intel/graphics-n305-iris/images/rootfs.ext2` 包含同版目标构建 Mesa26.1.2
iris 与既有 intel-mesa-smoke。通过 existing n305-iris-smoke flavor/runner
在 TheKernel QEMU guest 验证实际库路径及 LD_BIND_NOW 全符号绑定；无参入口
按预期拒绝，未打开任何 Intel GPU，也没有初始化/渲染成功标记。
下次用户授权的 N305 验收先独立运行 --initialize，确认真实 Intel renderer，
再 --execute 检查 GLSL三角形/内外像素；最小RCS、库装载与真实Mesa像素不可互代。
镜像默认不启用硬件写入；未来启动仍需分别明确 intel.modeset=1、intel.gt=1、
intel.rcs=1，不能增加 nvme.allow_write=1。默认Q35软件/virgl镜像保持原样。
