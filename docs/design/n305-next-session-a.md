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
