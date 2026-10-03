# 下次 N305 真机验证：Codex A / dev

工作目录 `/home/ava/Worktrees/TheKernel/dev`，状态目录必须是
`/home/ava/.cache/thekernel-targets/wt-dev`。不使用 B 的产物，不写内部 NVMe。
**以下 RTL8168 路径未在硬件上验证。** 这里只准备命令，sudo 服务由用户执行。

## 1. 准备带固件的网络启动产物

```sh
cd /home/ava/Worktrees/TheKernel/dev
export THEKERNEL_STATE_DIR=/home/ava/.cache/thekernel-targets/wt-dev
export THEKERNEL_RTL8168_FIRMWARE_DIR=/home/ava/.cache/thekernel-targets/refs/firmware/rtl_nic
python3 tools/thekernel.py build --platform n305 --profile shell
scripts/n305-netboot.sh --help
```

使用 `docs/design/n305-tonight.md` 的 kernel 准备流程和
`scripts/n305-netboot.sh`，明确指定本次 dev 的 ELF/rootfs 和新的输出目录。
不要用昨晚缓存的 kernel 目录替代刚构建的产物。先由用户启动生成的前台
`start.sh`，确认 DHCP/日志接收端就绪，再打开 N305 电源和选择 UEFI PXE。
暂时用 `quiet` 避开尚待诊断的详细日志问题；不要据此宣称问题已经修好。

## 2. RTL8168 识别、固件、DHCP 和主机收到日志

屏幕应显示或在运行后 `dmesg` 中看到：

- `01:00.0 10ec:8168`，原始 TxConfig 和 `XID=...`。
- 只有掩码 0x7cf 下的 0x541 会运行 H 序列；不认识的 XID 打印
  `unsupported; no reset`，保留屏幕。保存该 XID，不要改掩码硬套。
- `NIC firmware /lib/firmware/rtl_nic/rtl8168h-2.fw: applied`；不存在则明确
  `degraded warm-PXE PHY only`。上传/寄存器超时不能算固件成功。
- MSI 成功或 10 ms 轮询兜底；随后应有实际 DHCP 地址。

在 guest shell 中：

```sh
wc -c /lib/firmware/rtl_nic/rtl8168h-2.fw
cat /lib/firmware/rtl_nic/LICENSE.r8169
ip link
ip addr
udhcpc -i eth0 -n -q
```

固件参考文件为 976 字节，但不要仅凭长度认定有效。确认线和主机侧 DHCP
服务后检查获得的地址，再执行已有 `thekernel-netconsole HOST PORT`。
主机接收终端必须真正出现 guest 的新日志字节；只有 READY 日志不算通过。
若无链路/无租约，用屏幕查看 `dmesg`；不要反复复位 PHY 猜寄存器，也不要
改主机防火墙或接触 Windows NVMe。网络日志不覆盖早期启动和 panic。

## 后续项目

A2 详细日志卡住、A3 VT 的 NUL 来源、A4 采集脚本，以及 A5–A9 的看门狗、
ACPI 电源、USB 启动、通用 HID、DbC 尚未在此检查单中列为已实现/已通过。
后续实现会在同一文件追加各自的可执行验证步骤。

## 3. 详细日志问题：仍须真机定位

QEMU 的 4/8 核 loglevel=7 和 8 核 quiet 均进 shell，TCG 的实际 framebuffer
文字检查通过；没有复现真机停顿，**没有根因修复**。本地复现命令：

```sh
python3 tools/thekernel.py run --platform n305 --profile shell --smp 8 \
  --accel kvm --graphics-profile firmware-fb --kernel-cmdline loglevel=7
```

PXE 时通过 `n305-netboot.py prepare` 的既有参数配置 GRUB；run 的追加选项
只影响本地单次 QEMU ESP，不会自动改 PXE 目录。先跑 quiet 建立可用网络，
再比 loglevel=7。屏幕再次停在 alarm 时，检查主机是否仍有 guest DHCP/UDP
日志或 shell 网络活动；记录停顿位置和 elapsed 时间。网络未起来不能倒推
整机死锁；硬件停顿原因未明前保留 quiet 退路，不盲改屏幕锁。

## 4. 提示符前 NUL 的来源：先诊断再修复

准备 dev PXE 时明确追加诊断参数（准备不需要 sudo）：

```sh
scripts/n305-netboot.sh prepare --mode kernel \
  --out /home/ava/.cache/thekernel-targets/wt-dev/n305-next-a \
  --interface enp0s31f6 --address 192.168.10.1 \
  --kernel /home/ava/.cache/thekernel-targets/wt-dev/out/x86_64/n305/shell/mem1g/kernel-x86_64 \
  --rootfs /home/ava/.cache/thekernel-targets/wt-dev/out/rootfs/x86/rootfs-x86.img \
  --kernel-cmdline tty.input_trace=1
```

本次 shell 产物需按第 1 步构建，并使用对应 rootfs；不要把别人/以前构建的
ELF 和 rootfs 拼在一起。用户随后按既有今晚流程用 sudo 启动生成的前台
start.sh，本次代理不执行 sudo 或启动服务。

guest 中 `dmesg | grep vt-input` 应显示 `seq=... source=... vt=... byte=0x..`。
查 `byte=0x00` 是 Serial、UsbKeyboard、VirtualKeyboard 还是 OtherEvdev。
**真实来源仍未知，未修复。** 无记录时先确认 `/proc/cmdline` 的诊断参数，
再确认是否确有接收字节；不能用无日志证明没有 NUL。仅记录首 64 字节，
不要输入密码；收集结果后移除参数。不得先把所有 NUL 过滤掉掩盖来源。

## 5. 更新后的 Alpine 采集（给 HD Audio 和电源键工作提供数据）

按 `docs/design/n305-tonight.md` 的 capture 准备参数重新运行
`scripts/n305-netboot.sh prepare --mode capture ...`，**重建 overlay，不能复用
昨天的 payload**。生成脚本仍由用户 sudo 前台启动；已有接口恢复流程不变。
用户确认上传到主机后查看：

- `acpi/mcfg-decoded.txt` 应为 `0xc0000000`，seg 0，bus 00–ff。
- `acpi/checksums.txt` 的 FACS 应为 `n/a`；其他 SDT 的 BAD 不会被隐藏。
- `acpi/kernel-ecam.txt` 应明确来源：retained log 或 kernel /proc/iomem。
  两者皆无时应是 UNAVAILABLE，不是假的成功或 grep FAIL。
- `acpi/power-button-fadt.txt`：昨晚字节显示固定 PM1 电源键、SCI 9、
  PM1a_EVT 0x1800、PM1a_CNT 0x1804；同时读 PNP0C0C 枚举文件，不用字符串
  搜索 AML 来声称某设备不存在。
- `audio/card*-codec#*.txt`：把含 `Codec:`、Vendor Id、Node/Pin 的报告交给 B。
  逐个核对 capture-status.txt；发现节点或成功加载模块不等于取得 codec 数据。

新 collector 未上真机。缺 codec 文件时先看 `audio/load-hda.txt` 和其 stderr，
不要依据控制器 PCI ID 猜 codec。此轮 A5–A9 尚未实现，不打开看门狗，不写 U 盘，
不声称 SCI/S5、通用 HID 或 DbC 已可用。
