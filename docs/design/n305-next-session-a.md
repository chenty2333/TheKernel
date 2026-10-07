# 下次 N305 真机验证：CPU 电源管理与平台诊断

2026-10-07 起从整合后的 `/home/ava/Desktop/TheKernel` main 重建，状态目录
`/home/ava/.cache/thekernel-targets`。此前 A 的独立验证是历史结果，不能代替
整合后的真机验收。ACPICA 和安全准入的 MWAIT 现在均为默认路径。**不写内部 NVMe，
不改 BIOS/SPI/主机网络；sudo 服务和 U 盘写入均由用户执行。**

报告中的旧集成镜像曾完成 RTL8168H DHCP/ping/netconsole，但 3 次里 1 次
停流；首个 NUL 已确认来自 Serial。那次 loglevel=7 成功不证明 A2 修复。
本轮新增实现/诊断均尚未在 N305 真机上验证。先测网络/UART，再测详细日志，
最后分别打开有风险的看门狗、关机、USB 启动和 DbC；不要一次全部打开。

## 1. 重建成对 ELF/rootfs，准备前台 PXE

```sh
cd /home/ava/Desktop/TheKernel
export THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets
export THEKERNEL_RTL8168_FIRMWARE_DIR=/home/ava/.cache/thekernel-targets/refs/firmware/rtl_nic
python3 tools/thekernel.py build --platform n305 --profile shell
scripts/n305-netboot.sh --help
```

默认 `mem1g` 是编译变体名，不是假称真机只有 1GiB；实际内存由固件发现。
按 `docs/design/n305-tonight.md` 选定真实主机接口/地址并准备（下面接口只是
该流程的示例，先确认，不由代理配置）：

```sh
scripts/n305-netboot.sh prepare --mode kernel \
  --out "$THEKERNEL_STATE_DIR/n305-next-a" \
  --interface enp0s31f6 --address 192.168.10.1 \
  --kernel "$THEKERNEL_STATE_DIR/out/x86_64/n305/shell/mem1g/kernel-x86_64" \
  --rootfs "$THEKERNEL_STATE_DIR/out/rootfs/x86/rootfs-x86.img" \
  --kernel-cmdline 'quiet n305.net=dhcp n305.netconsole=192.168.10.1:6666 tty.input_trace=1'
```

输出目录必须新建，不能复用旧 prepare 目录。用户按今晚流程 sudo 前台启动
生成的 `start.sh`，确认 DHCP/UDP 接收端就绪后才打开 N305 并选择 UEFI PXE。
固件只能在 rootfs 的 `/lib/firmware/rtl_nic/`，随附 `LICENSE.r8169`；不嵌进内核。
若没有新 shell/网络，先区分 PXE 未到内核、内核未到 init、DHCP/日志失败，
不要因为主机没收到包就直接宣布整机死锁。保留可用 quiet 镜像退路。

## 2. 网络：重复启动、PHY/环诊断、无 QEMU 残留

屏幕或 `dmesg` 应有 `01:00.0 10ec:8168`、原始 TxConfig/XID。报告里是
`0x57100f80` / XID `0x571`，掩码 0x7cf 后为 H 的 0x541；以新读数为准，
不认识的 XID 不初始化，不改掩码硬套。需真正看到固件 `applied` 和实际流量：

```sh
wc -c /lib/firmware/rtl_nic/rtl8168h-2.fw
cat /lib/firmware/rtl_nic/LICENSE.r8169
ip -4 addr show dev eth0
ip route
ping -c 3 192.168.10.1
```

参考固件为 976 字节，但长度不等于正确性。自动 DHCP 会产生
`N305_DHCP_BOUND` / `N305_NETCONSOLE_READY`；主机必须收到新的 UDP 日志字节，
不只看 READY。若手动 DHCP，用带租约清理的既有脚本：
`udhcpc -i eth0 -s /etc/thekernel/n305-dhcp.script -n -q`。

至少重复 3 次，每次记清实际启动结果。FW/PHY 失败会在 quiet 下以优先级 3
醒目报告命名步骤、解释器 PC/操作和超时；观察主机链路是否断开再协商，但
不要把没有链路跳变当成固件失败的证明。后续已有 RX 后静默 30 秒，或硬件
仍占有 TX 且 5 秒不前进，会有限次输出 ChipCmd、IntrStatus/Mask、PHY/BMSR、
环 head/tail/ownership/计数和 FW 阶段。**RX 安静不是停流证明**；配合主动
ping/ARP 重现。此轮未增加猜测性复位/恢复，偶发根因仍未知。

N305 启动不再预置 10.0.2.15/10.0.2.2。主机 DHCP 不下发 router 时，租约后
不得有 QEMU 地址、旧子网或 `default via 10.0.2.2`；同网段 ping/UDP 仍应正常。
租约回调只清理该接口 IPv4 路由再换地址，不清理其他 NIC。不要临时添加
默认路由掩盖问题。BusyBox `ip` 没有 `-s`，不要依赖它显示计数器。

## 3. Serial NUL：先核对 UART 检测结果

```sh
dmesg | grep -E 'uart|vt-input'
cat /proc/cmdline
```

新 UART 接入前会做 scratch、有限 LSR/IIR 检查和两组 modem-loopback；查看
原始探测字段/拒绝原因。不存在时不启用该 UART 的 VT/RX IRQ。**没有物理
插座本身不等于逻辑 UART 不存在**；若新探测接受但仍有
`source=Serial byte=0x00`，继续拿原始读数定位，不能说已修好。
没有过滤 NUL，真实 Ctrl-Space/其他通道的 NUL 应保留。诊断只有首 64 字节；
不要输入密码，结束后去掉 `tty.input_trace=1`。没有记录不能证明没有输入。

## 4. A2：详细日志重复 2–3 次，查看独立进度

另建 PXE prepare 目录，把参数换成：

```
loglevel=7 boot.progress=1 n305.net=dhcp n305.netconsole=192.168.10.1:6666 tty.input_trace=1
```

QEMU N305、8 核/8GiB、firmware-fb、真实 DHCP/UDP 日志和可读屏幕均没有复现；
**没有根因修复声明，也不把中午的单次成功当成修复。** 复现脚本：
`python3 scripts/ci/n305-verbose-qemu-smoke.py`（沿用上述状态目录）。

进 shell 后读 `cat /proc/boot-progress`。启用时会在 init 发布后每 5 秒、最多
12 次输出 BOOT_PROGRESS：阶段（PID1、alarm 开始/结束、init join）、各 CPU
上线状态/时钟中断计数、screen_write/present 开始和完成次数及最后检查点。
如果又卡住，比较屏幕、UDP 和可用 DbC 读数：阶段是否还在前进、哪些 CPU 的
timer_irqs/last_timer_ns 仍更新、屏幕 scope 是否长期未完成。跨 CPU 快照是
近似值/发起者计数，**不是锁所有者或完整帧证明**。记录实际停顿时间和位置，
回退 quiet；不要猜锁、调度器或自动改寄存器。诊断默认关，且观察任务仍依赖
调度器；不保证全核死锁后继续输出。

## 5. A5：看门狗单独 opt-in，现场先验证喂狗

默认没有 `/dev/watchdog`、不触碰 TCO。确认镜像可启动、有人能恢复电源，
再单独用 `watchdog.timeout=60` 参数。计时从 PCI 初始化开始，**不是从 shell
开始**；不得先让启动后的 60 秒耗尽。N305 应识别 `8086:54a3`、TCO v6，
不认识/资源禁用/NO_REBOOT 读回失败时拒绝启用并告警，不猜 PMC 基址。

先运行 `/opt/thekernel-tests/bin/thekernel-watchdog-check feed`：该 helper 会把
超时设为 **6 秒**，连续喂 20 秒、魔术关闭，再等待 14 秒，要求
`ITCO_FEED_AND_MAGIC_CLOSE_OK` 且没有重启。不要在无人看管的首次原生测试中
运行 `expire`。正常喂狗确认后，才在 RAM/PXE 根文件系统且不写内部盘的现场
运行 `...watchdog-check expire`：应停止喂狗并真正重启，不能把日志当成重启。
之后回到不带 watchdog 参数的镜像。QEMU 已实测喂狗不重启和停喂后二次启动；
它是 ICH9 v2，**不能验证 N305 v6**。详见 `itco-watchdog.md`。

## 6. A6：短按电源键 → 通知 init → flush → S5

本项不带 watchdog 参数。采集 FADT 为固定 PM1 电源键，SCI9；MADT flags 0xd
是 level/active-high；实际 DSDT 静态 `_S5` 解得 [7,0]。这些是字节事实，
不是原生电源转换通过。先确认 `dmesg | grep acpi-power` 中的固定键/S5/SCI
发现结果，再**短按**按钮一次（不要长按硬断电）。应通知 init，至少看到
`acpi-power: filesystems flushed; entering S5`，机器随后关机。

没有 SCI/S5 或路由不支持时保留 shell/既有回退，别猜端口。此实现没有完整
AML、`_PTS` 或控制方法键通知。QEMU 的 QMP `system_powerdown` 已实测真实
有序 S5/进程 exit 0；不是宿主 kill/QMP quit。详见 `acpi-fixed-power.md`。

## 7. A7：生成 USB 启动镜像，用户安全写盘

先用第 1 步重新构建最新配套 kernel/rootfs（不拿旧 A7 验证镜像当最新版）：

```sh
python3 scripts/build-usb-boot.py \
  --kernel "$THEKERNEL_STATE_DIR/out/x86_64/n305/shell/mem1g/kernel-x86_64" \
  --rootfs "$THEKERNEL_STATE_DIR/out/rootfs/x86/rootfs-x86.img" \
  --output "$THEKERNEL_STATE_DIR/n305-usb-new.img" --kernel-cmdline quiet
python3 tools/thekernel.py run --platform n305 --profile shell --accel kvm \
  --no-build --graphics-profile firmware-fb \
  --usb-disk "$THEKERNEL_STATE_DIR/n305-usb-new.img" --usb-boot
```

USB GRUB 固定 root=usb，不传 Multiboot rootfs；显式 USB 根不能回落到内部盘。
QEMU 已用 USB-only 拓扑验证写入、sync、真实 reboot 后读回同一内容。
只支持 512 字节扇区 BOT/LUN0，不支持 UAS/热拔根盘/GPT 备份修复。
`/dev/vda` 是现有根设备别名，不证明 VirtIO；看 `USB rootfs` 驱动和启动拓扑。

确认 USB 棒真实设备、全部分区已卸载后，先看 dry run；**只有用户**执行第二行：

```sh
python3 scripts/write-usb-boot.py --image "$THEKERNEL_STATE_DIR/n305-usb-new.img" --device /dev/sdX
sudo python3 scripts/write-usb-boot.py --image "$THEKERNEL_STATE_DIR/n305-usb-new.img" --device /dev/sdX --yes
```

拒绝 NVMe/别名、非可移动、非 USB、分区、挂载/holders/swap/容量不足的设备。
不接受 removable=0 的 USB 盒，不绕过检查。`--yes` 要 root；默认只打印命令。
棒上所有数据会被覆盖，本轮代理没有做任何物理写入。原生 UEFI USB 启动/I/O
仍未验证。`usb-root-boot.md` 有完整限制和生成流程。

## 8. A8：真实 HID 报告协议

QEMU usb-kbd/mouse/tablet 已验证 KEY_A、REL_X=17、ABS_X=16384 的实际 evdev
事件；主机畸形/数组/Report ID/长条目/Consumer/gamepad 描述符测试通过。
下次分别接键盘，测修饰键+按键数组/NKRO；接支持的 Consumer 键、游戏手柄，
用既有 evdev 客户端检查键/轴事件；再测 CH9329 绝对坐标。USB HID 使用报告
协议，不再依赖另一套 boot 键盘解码。未知 usage 跳过，不以 KEY_UNKNOWN 代替。
拔插/报告溢出或描述符拒绝应告警/忽略接口，不能让屏幕消失；原生设备尚未
验证，尤其不要把 QEMU tablet 当成 CH9329 验收。详见 `usb-hid-report.md`。

## 9. A9：DbC 另建 feature 镜像，先确认专用线

默认关闭。用 `build ... --usb-dbc` 重建，ELF 在 `mem1g-usb-dbc` 而不是 mem1g；
PXE prepare 必须换成这个 ELF，并配同次 rootfs。只使用明确支持双主机调试的
**USB 3.x SuperSpeed A–A 专用调试线，SuperSpeed 交叉且 VBUS 断开**；不接
普通带电 A–A、USB2 线、数据迁移桥或充电线。目标必须是支持 DbC 的直连根端口。

主机用户操作 `sudo modprobe usb_debug`，看 `lsusb -d 1d6b:0010` 和新出现的
`/dev/ttyUSB*`，不要假定 ttyUSB0。用户设置已确认的端口为 raw/-echo 后读取，
另一个终端发送 `echo DBC_REAL_INPUT_OK` 加换行。必须实际收到新内核日志、
shell 输出和该标记，不能只看 “configured”。身份是 Linux 调试兼容测试
VID/PID，不是分配给 TheKernel 的 USB-IF VID。具体命令/线材规范见 `usb-dbc.md`。

没有 cap/CNR/已有固件 DbC/配置失败时保留普通屏幕。已配置后断线/异常会停
DbC、保留 DMA 到重启；**不自动重连，重启目标再试**。日志/DMA 与 VT 输入
用独立任务，VT 输入阻塞不拖住日志任务；仍不是 earlyboot、panic-safe 或
调度器独立通道。QEMU 只验证“确有 xHCI 但 cap absent”后的实际 USB 输入、
可读 shell 屏幕和正常退出；寄存器/环/数据靠 fake 测试，**真机 DbC 未验证**。

## 10. 更新 Alpine 采集（仅需要新增信息时）

按今晚 capture 流程重建 overlay；不要复用昨天的 payload。检查：
MCFG 为 0xc0000000，FACS checksum 为 n/a，kernel-ecam 明确来源或 UNAVAILABLE，
FADT/PNP0C0C 枚举保留实际状态；声卡 `/proc/asound/card*/codec#*` 交给 B。
逐个看 capture-status，不用 PCI 控制器 ID 猜 codec。新 collector 未上真机。
调度器 Reschedule 注册空档、B 的 NVMe/HDA/GT 及主机采集卡故障不在 A 本轮范围。

## CPU power management — 未在硬件上验证

- 本轮没有真机授权，不启动/改动 N305。以下步骤留给有授权的现场会话。
- 先以 `cpuidle.mwait=0` 建立 HLT 排障基线，再移除该参数验证默认自动
  MWAIT。`cpuidle.mwait=1` 等同默认，不强制绕过硬件/深度状态准入；非法
  值或裸参数退回 HLT，重复参数最后一项生效。保持既有受保护的启动流程，
  不开启固件 HWP，不改变 NVMe 策略。
- 在每个 CPU 上核对 family6/modelBE、MONITOR、CPUID.5 扩展/中断唤醒/
  子状态、实际状态表、稳定单调时钟和已编程的 LAPIC 定时器；不猜测未知
  硬件。GMT 的 MWAIT C1（0x00）不可用，C1E 可以准入；缺少 ARAT 必须阻断
  C6/C8/C10，不能通过 `=1` 或 sysfs disable 写入解禁。
- 比较默认/强制关闭的 name/desc/latency/residency/disable/usage/time；
  运行 CPU power 回归、各 CPU 定时器唤醒、跨 CPU futex 唤醒和持续负载。
  关闭/不支持路径必须只有 HLT，默认 MWAIT 不得导致挂起、丢唤醒或校验错误。
- 分别记录软件 entry/time、稳定性、硬件 core/package residency 和外部墙上
  功耗/温度；前三者不能替代节能实测。固定工作负载且没有并发编译/VM 时
  才比较实际功耗。上述真机项目均为“未在硬件上验证”。
- Read cpufreq_supported, cpuinfo range and sampled current frequency. If HWP
  is not firmware enabled, or the nominal reference/package-control policy
  is unsupported, keep the unsupported result; do not bypass admission.
  If supported, save initial settings, explicitly select powersave/EPP and a
  bounded min/max, verify busy APERF/MPERF behavior and scheduler uclamp clipping
  within policy bounds. Restore saved settings after the authorized experiment.
  Verify that default scheduling never changes the firmware request.
- Read coretemp hwmon labels/input/max/crit/crit_alarm and run `sensors`.
  Compare core/package temperatures with firmware/reference readings; check
  invalid DTS status and read-only permissions. Do not clear thermal log bits.
- Run real cpupower frequency-info/idle-info. Measure power/temperature and
  latency under fixed independent workloads, with no concurrent builds/VMs.
  QEMU success and the software idle counters do not establish N305 savings.
