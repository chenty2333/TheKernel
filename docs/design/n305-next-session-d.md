# N305 下次显示/GPU/HDMI 音频验证 — Codex D

2026-10-05。**全部未在硬件上验证；这一轮不启动真机、PXE服务或写USB/NVMe。**
下面是未来用户执行的步骤，不是已验证结果。受限 TC1/TC2 固件等价 fastboot/KMS
调用链已接通并做主机模型测试；TC 模式重编程、RCS/Mesa、HDMI audio 仍未实现；GT BCS 软件链见下。

## 基线和板级资料

1. 用已知可启动的固件帧缓冲配置构建 detached worktree 的 n305 shell。
   状态目录 `wt-intel`，nice10、6个构建任务。无 `intel.modeset=1`、
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
4. DPMS、gamma、cursor、缩放、不同 pitch/格式/模式请求目前应返回不支持，
   不能出现成功返回却没有实际效果。HPD IRQ 和真正中断时间戳尚未接通；
   当前 vblank 是任务轮询硬件帧计数，不制造软件帧。
5. fastboot 不使用 combo-only 的 `intel.modeset.fail_write` 注入参数，带此
   参数会在写之前拒绝。当前失败恢复已在主机模型验证：plane store 可能
   已落地，恢复前一 surface 并看到 fresh frame 后才释放新 GGTT；恢复不确定
   则保留 DMA owners 并终止后续提交。真机故障注入入口尚未开放，不强行用
   旧参数测试 TC。观察画面是未来真机回滚验收的必要部分。

## TC1 原生模式与失败恢复（实现前不执行）

TC hidden PHY/PLL/power before-image完整、故障前缀模型测试和bounded恢复
通过以后，才允许用户以 `intel.modeset=1` 请求1080p60。预期是模式切换到
1920x1080、native测试图案及清晰持续更新的console，无长期黑屏/花屏/underrun。
新状态通过实际 SURFLIVE/scanline 和完整 register/GGTT 证明后才能发布给DRM。
用未修改Weston DRM后端检验connector/CRTC/plane、TEST_ONLY、真实atomic
commit/pageflip/fence；当前固定帧缓冲CPU拷贝adapter不计Intel nativeatomic。

用户确认后再做一次性失败注入。先 `intel.modeset.fail_write=1`，再按实际
报告的forwardwrite数选择晚期前缀；超出末次写不会触发失败。屏幕应恢复
启动前的固件模式、pitch/plane/surface并继续刷文字。`ROLLBACK_MMIO_VERIFIED`
只证明寄存器/GGTT/scanline契约，必须再看monitor像素。`ROLLBACK_FAILED` 或
无进展要停止后续尝试、保留DMAowners，不能当恢复成功。TC版本不能直接
复用只懂combo的旧snapshot，必要的间接PHYselector/analog不可当普通RAM回放。

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
  GEM/submit/binary-sync 软件链已接通，仅支持受限 linear BCS/no-reloc/default context；
  真实用户程序尚未验收。之后再测试 RCS、iris OpenGL 和 ANV Vulkan，分别验收。
  N305 GuC 应是 tgl 系列，不是 adlp_guc；display D0 不等于 GT/media A0。
- HDMI audio依赖实际TC link、audio powerwell、ELD和HDAcomponent握手。现在
  尚未接入，不把模拟ELD/analogcodec枚举当HDMI音频通过。将来跟modesetopt-in
  开启，验证显示器audio能力/ELD、HDA HDMI pin/converter、48kHz双声道真实
  输出、静音/停止/拔线的生命周期；拔线或回滚要先撤掉audio valid，再安全
  停止DMA。当前HDA模拟音频不得为了让程序exit0而冒充HDMIcodec。
