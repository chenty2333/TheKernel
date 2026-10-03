# N305 今晚操作（2026-10-03）

只在 `/home/ava/Worktrees/TheKernel/dev` 的 `dev` 分支准备；不改 main、不 push。
下面的主机命令都在该目录执行。需要 sudo 的命令逐条写出；其他命令不需要。
今天未执行需要 root 的主机网络切换，也没有启动主机常驻服务。

## 0. 接线和准备

- 主机有线口 `enp0s31f6` 专用于 N305，不能同时服务其他设备；不要接到公共网络。
- N305 开 UEFI PXE，关闭 Secure Boot。HDMI/DP 接显示器或 HDMI 采集卡；记录实际接口。
- CH9329 接 N305，采集卡 USB 接开发主机。不是开发主机的内置摄像头。
- 全程不写 N305 内部磁盘。采集结果先在 Alpine RAM 中，再上传主机。
- 主机需已有 dnsmasq、NetworkManager、GRUB EFI 工具、Python、ffmpeg/ffplay。

主机终端 A：

```sh
cd /home/ava/Worktrees/TheKernel/dev
export THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets
export PREP=/home/ava/.cache/thekernel-targets/hw-prep-2026-10-03
```

已生成三个互斥的启动目录：`$PREP/capture`、`$PREP/kernel`、`$PREP/graphics-pxe`。
不要同时启动两个服务。如果更换有线口，先停止服务，再重新生成；不能只改启动脚本。
重生成命令在文末。

## 1. 先启动采集服务和预览

终端 A（**需要 sudo**，保持前台）：

```sh
sudo "$PREP/capture/start.sh"
```

应看到 `N305 PXE active on enp0s31f6`。脚本只创建一次性、不自动连接的
NetworkManager profile，区域 `trusted`，没有默认路由或 DNS；原 profile 不修改。
退出时删除临时 profile、恢复原连接 UUID，或恢复原先未连接状态。
若端口占用、网口非 Ethernet、trusted 未生效等，脚本失败并走同一个清理路径。
**真实主机的 root 网络切换/恢复尚未实跑**；检查启动终端是否有 `RESTORE FAILED`。

终端 B（不需要 sudo）：

```sh
cd /home/ava/Worktrees/TheKernel/dev
/home/ava/.cache/thekernel-targets/hw-prep-2026-10-03/capture/preview.sh
```

已知 eEver/HDMI by-id 节点会自动选择；没接采集卡则明确失败，不会打开内置摄像头。
不同型号可先查看节点，再明确传入**确认属于采集卡的视频节点**：

```sh
ls -l /dev/v4l/by-id/
/home/ava/.cache/thekernel-targets/hw-prep-2026-10-03/capture/preview.sh --device /dev/v4l/by-id/实际采集卡-video-index0
```

预览使用单一流，同时每秒更新 `$PREP/capture/screen.jpg`。不要再开第二个读卡进程。
今天没有连接 HDMI 采集卡，预览实物画面未验证。

## 2. 第一次开机：Alpine 自动采集并上传

给 N305 开机、选择有线 UEFI PXE。应依次看到：

1. `N305 PXE: requesting DHCP`、加载 Alpine 内核和 initramfs。
2. `n305-capture:` 各采集阶段：PCI、ACPI、DMI、CPU、显示、USB、网络。
3. `CAPTURE COMPLETE`，分别列出 OK / UNAVAILABLE / FAIL 数量。
4. `CAPTURE UPLOADED ... host has results`；主机终端 A 同时出现 `CAPTURE RECEIVED`。

自动采集包括完整 PCI 配置、MCFG、EDID 原始字节、modetest、i915/NIC dmesg、
网口/驱动信息、USB 描述符和 HID report descriptor。签名 APK 和 modloop 从主机提供，
目标机器不需要访问互联网。固件作为 Alpine 软件包运行时加载，不编进 TheKernel。

采集后**不会自动关机**，摘要留在屏幕。主机查看：

```sh
ls "$PREP/capture/received"
BUNDLE=$(find "$PREP/capture/received" -mindepth 1 -maxdepth 1 -type d -name 'n305-*' | sort | tail -n 1)
cat "$BUNDLE/capture-status.txt"
cat "$BUNDLE/SUMMARY.txt"
python3 scripts/ci/hw_facts_bundle.py "$BUNDLE" --require ecam --require display
cat "$BUNDLE/network/ip-link.txt"
cat "$BUNDLE/network/ethtool-eth0.txt"
```

不要用 `--expect-ecam 0xe0000000` 强行匹配旧编译值。以实际 MCFG 为准。
看 PCI 中网卡是 `10ec:8125` 还是 igc 支持的 Intel ID；看 EDID 的实际 1080p60 模式、
i915 是否成功绑定、CH9329 的接口/report ID/绝对或相对模式。
工具缺失、无 EDID、某个探测失败都不是硬件通过；逐项读状态和失败文件。
若上传失败，**不要关机**：屏幕有 curl 重试命令，数据仍在 RAM；先恢复主机服务和链路。

确认主机已有数据后，在终端 A 按 Ctrl-C。也可终端 B 停止（**需要 sudo**）：

```sh
sudo /home/ava/.cache/thekernel-targets/hw-prep-2026-10-03/capture/stop.sh
```

如果报告恢复失败，先读旧 UUID，再手工恢复（第二条**需要 sudo**）：

```sh
cat "$PREP/capture/previous-connection"
sudo nmcli connection up uuid 实际旧UUID
```

旧值为空或 `--` 表示原来未连接，不应编造一个旧连接。不要删除无关 profile。

## 3. 第二次开机：TheKernel shell、显卡和网卡

终端 A（**需要 sudo**）：

```sh
sudo "$PREP/kernel/start.sh"
```

N305 重启并选 PXE。GRUB 输出加载内核/rootfs 的进度；rootfs 传输需耐心等候。
预期屏幕：

- `pci-ecam: ... source=mcfg`，今天报告的真实值是 `0xc0000000`，但以本次为准。
- 每个 PCI function 的 BDF、vendor:device、class 和六个原始 BAR word。
- MADT source override 不再因错误的结构长度而漏读；HPET 来自 ACPI 表。
- Intel phase 0 只读探测。存在有效固件控制台时，应看到停止 power/modeset 写入的原因，
  以及 firmware KMS `/dev/dri/card0` 的固定尺寸。**这不是 Intel 原生模式设置通过。**
- igc 和 RTL8125 均在默认 N305 构建里；不存在的设备打印一行 no supported device。
- 找到 RTL8125 时先打印 MAC revision、warm-PHY 限制和硬件未验证状态；未知 revision 拒绝。
- DHCP 成功有 `N305_DHCP_BOUND ... ip=192.168.10.10` 等；失败有 `N305_DHCP_FAILED`。
- `N305_NETCONSOLE_READY` 和交互 shell 提示。主机终端显示 UDP 日志，并写到
  `$PREP/kernel/kernel-udp.log`。这是用户态、尽力转发，不覆盖早期启动或 panic。

在 N305 shell（不需要 sudo，shell 本身是 root）：

```sh
ip link
ip -4 addr show dev eth0
ip -4 route
ping -c 3 192.168.10.1
ls /dev/dri /dev/input
cat /proc/cmdline
```

只拿到 DHCP offer/lease 的日志不算地址和路由应用成功；必须有 BOUND 和实际 ping。
此前无键盘时的 `^@` 原因仍未定位，不要据此判断 USB 键盘成功或失败。
网卡没找到/链路 down/DHCP 超时，继续用屏幕：记录 PCI ID、revision 和停止阶段，
不要把 RTL warm PHY 说成冷启动驱动完成。若无网卡，UDP 通道也不可用。

GRUB 显式请求 `1920x1080x32,auto`，**不使用 `gfxpayload=keep`**。
看固件 framebuffer 日志的实际尺寸：仍为 800x600 就没有达到 1080p。
软件 KMS 暴露的 60Hz 是事件节拍，不是实测输出刷新率；采集卡/显示器确认真实模式。
不要为了 1080p 绕过 Intel 保屏保护：旧模式设置尚无完整硬件回滚。

确认 shell 和信息后停止 `$PREP/kernel/start.sh`（Ctrl-C），或（**需要 sudo**）：

```sh
sudo "$PREP/kernel/stop.sh"
```

## 4. 第三次开机：Weston 和 SDL KMSDRM

终端 A（**需要 sudo**）：

```sh
sudo "$PREP/graphics-pxe/start.sh"
```

N305 再次 PXE，进入同一内核的图形 rootfs。先在目标 shell 执行：

```sh
/etc/init.d/S10udevd start
/etc/init.d/S70seatd start
/etc/init.d/S80weston start
cat /run/user/$(id -u weston)/weston.log
```

Weston 应输出 Starting Weston: OK，日志显示 drm-backend 和 Pixman 软件渲染。
没有 render node 是预期，不代表硬件加速。启动失败先查日志和 `/dev/dri/card0`。
结束 Weston 再试直接 KMS；不要让两个进程争用 DRM master：

```sh
/etc/init.d/S80weston stop
LIBGL_ALWAYS_SOFTWARE=1 SDL_VIDEODRIVER=kmsdrm /usr/local/bin/n305-sdl-kms-smoke
echo $?
```

预期 `driver=KMSDRM expected=red`，约十秒红画面，再回到可读控制台，退出码 0。
不能只凭 FRAME_READY 判断红画面成功，必须看屏幕。
不要只设 `MESA_LOADER_DRIVER_OVERRIDE`：这里需要明确的软件 GBM/EGL 路径。
SDL 当前会报告保存的 fbcon CRTC 无法通过客户 FD 恢复；测试程序另外恢复 KD_TEXT。
实际控制台恢复和画面仍需今晚确认。Vanilla Conquer/游戏资源未集成或运行，不列为通过。

## 5. 收尾

- 终端 A Ctrl-C，或（**需要 sudo**）`sudo "$PREP/graphics-pxe/stop.sh"`。
- 终端 B Ctrl-C 关闭采集卡预览；确认原有主机连接恢复。
- 数据已在主机后，N305 可关机；TheKernel 的 QEMU 关机端口不是这台实机的 ACPI S5，
  如不能软关机使用机器电源按钮。
- 本次尚未做 U 盘启动迁移，等真机验证后再做。

## 可复用的生成命令（主机，不需要 sudo）

仅在相应服务停止后执行；`--fetch` 的首次下载较大，已缓存可复用。

```sh
scripts/n305-netboot.sh prepare --mode capture --fetch --out "$PREP/capture" --interface enp0s31f6
scripts/n305-netboot.sh prepare --mode kernel --out "$PREP/kernel" --interface enp0s31f6 --kernel "$THEKERNEL_STATE_DIR/out/x86_64/n305/shell/mem1g/kernel-x86_64" --rootfs "$THEKERNEL_STATE_DIR/out/rootfs/x86/rootfs-x86.img"
scripts/n305-netboot.sh prepare --mode kernel --out "$PREP/graphics-pxe" --interface enp0s31f6 --kernel "$THEKERNEL_STATE_DIR/out/x86_64/n305/shell/mem1g/kernel-x86_64" --rootfs "$PREP/graphics/rootfs.ext2"
```

PXE 坑已固化：临时 trusted zone 放行 DHCP/TFTP；dnsmasq `user=root` 才能读
`/home/ava`；TFTP 文件 644；GRUB quiet；服务前台运行、退出清理恢复。
请求 1080p 不等于硬件已经支持它。

## 完成边界和本地验证

| 项目 | 状态 | 边界 |
|---|---|---|
| P0 | 完成 | 运行时 ECAM/MMIO、APIC/HPET、MADT 和完整 PCI 清单；真机新版本未启动。 |
| P1 | 完成前置准备 | 生成/恢复脚本和预览接入；root 主机切换、真实采集卡未实跑。 |
| P2 | 完成 | QEMU 真实 UEFI PXE→Alpine→必需探测/EDID→HTTP 上传；目标硬件待采集。 |
| P3 | 完成前置准备 | 固定模式 dumb KMS；不提供硬件加速或原生刷新率证明。 |
| P4 | 部分完成 | 运行时只读 probe、失败诊断和证明后 DRM 注册；安全起见暂停未有回滚的原生 modeset，完整 i915 顺序审计未完成。 |
| P5 | 部分完成 | igc 已有 TX/RX，新增 RTL8125B/BG MAC rings、默认双驱动、DHCP/UDP；RTL 冷 PHY/固件加载未实现，原生两种网卡均未在硬件上验证。 |
| P6 | 部分完成 | 组合接口和 bounded HID parser，QEMU 键盘/相对/绝对真实事件；CH9329 待验证，已有 xHCI halt 失败的致命断言尚未重设计。 |

本地实测：Alpine UEFI PXE 自动采集、必需工具安装/探测、bochs EDID、三个 USB HID
report descriptor 和 HTTP 上传成功。QEMU 的 DMI sysfs 有一个不可读文件，原样记录 FAIL；
EDID 解码工具和 kernel config 不可用，原样记录 UNAVAILABLE，未冒充全采集通过。

N305-profile QEMU firmware-fb：USB 三类真实事件、Weston DRM 启动、SDL KMSDRM
红色矩形的严格 RGB 像素检查、退出后控制台两色/字形结构检查均通过。
DHCP 在非默认的 192.168.10.0/24 网段实际应用地址/路由，ping 有应答，UDP 内核日志收到
11 包；首次 ARP 未缓存的 ICMP 丢包仍可出现，不是无损网络的证明。

最终提交前：完整 host 命令通过（Python 621，跳过 3；kernel 2535，组件测试通过），
guest KVM 51/51 且正常退出，q35/N305 lint 均通过且改动行无警告，fbcon 的可读文字/
像素结构检查通过。期间一轮 guest 在 fatal-fault 用例超时；原因未定位，未宣称已修复该
不稳定性，最终完整重跑通过。

生成的 kernel 和 graphics-pxe 目录均另经真实 UEFI TFTP 启动验证：自动 DHCP 应用
192.168.10.10、UDP 内核日志、shell 就绪。GRUB 的 1080p 请求在该 QEMU GOP 上实际
落为 1280x800，因此仍不能承诺真机 1080p。

保屏保证限于可恢复的驱动错误。共享 DRM core 仍有既有不可恢复的初始化分配，
不是全内核 OOM 容错；已有 xHCI halt 致命断言也未重设计。
本地模型测试和 QEMU 不证明 Intel GPU/igc/RTL/CH9329 的实物行为。
