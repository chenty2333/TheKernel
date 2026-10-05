# N305 下次显示/GPU/HDMI 音频验证 — Codex D

2026-10-05。**全部未在硬件上验证；这一轮不启动真机、PXE服务或写USB/NVMe。**
下面是未来用户执行的步骤，不是已验证结果。当前 TC1 编程/回滚、fastboot
完整接管和GT/HDMI audio尚未实现，不能照本文件强行启用未实现阶段。

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

## 只读时序 / fastboot 门槛

`intel.modeset=1` 仍由旧事务只接受exact D0、single pipe-A combo HDMI。真实TC1配置
应被拒绝且固件画面保持更新；**REFUSED 是正确安全结果，不是原生显示成功**。
新的MIT timing adapter仅在旧事务已证明live powered pipe A后读七个timing
寄存器，打印 `intel-i915-readout`；包括display13 SCL对vblank start的覆盖。
这条日志只是partialreadout，不能证明fastboot或正确的portclock/plane归属。

完整TC readout实现后，先核对native firmware实际的：pipe/transcoder/DDI TC1、
电源、TC legacy mode/PLL route和锁定、完整时序/portclock/CDCLK、plane
format/modifier/stride/offset/SURFLIVE、scaler/color/WM、GGTT physical backing。
如果全部与目标1080p60和可保活的固件framebuffer一致，fastboot应**零mode
重编程**，保留图像且文字可更新。只有分辨率相同、扫描线变化或读到寄存器
数值不构成完整等价/ownership证据。

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

- GT默认关闭；实现reset/retirement之前不开放 `intel.gt=1` 的写路径。
  forcewake acquire/release→bounded noop breadcrumb→BCS copy/fill。
  比较每一个目标字节和redzones，强制timeout/reset后console仍活、BO不提前
  释放。然后才测试RCS/Mesa；OpenGL iris与Vulkan ANV分别验证，不用BCS成功
  代替render成功。N305/i915 GuC 应是 tgl 系列，**不是 adlp_guc**。显示 D0 不等于 GT stepping；本地 i915 的 GT/media revision0 映射是 A0，workaround 必须用各自的表。
- HDMI audio依赖实际TC link、audio powerwell、ELD和HDAcomponent握手。现在
  尚未接入，不把模拟ELD/analogcodec枚举当HDMI音频通过。将来跟modesetopt-in
  开启，验证显示器audio能力/ELD、HDA HDMI pin/converter、48kHz双声道真实
  输出、静音/停止/拔线的生命周期；拔线或回滚要先撤掉audio valid，再安全
  停止DMA。当前HDA模拟音频不得为了让程序exit0而冒充HDMIcodec。
