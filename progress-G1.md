1. 移植地图 | 完成 | ad6dfb32 | 建立 v7.2.3 display 12/13 来源、现有 Rust 对应和许可证门禁；后续逐项增补。
2. VBT 和电源 | 进行中 | bd26db35,41eb1de7,8f7b8001,6b796ee4 | 增加 DMC 固件路径/尺寸映射、rootfs staging、延迟请求及 CSS/package 和主/pipe DMC 头解析，并翻译 display-12/13 电源域/Well 数据表；事件 fixup、MMIO 编程、完整 VBT 和内核电源序列接线尚未完成。
2b. DMC 延迟请求和解析 | 部分完成 | 8f7b8001 | 从实际 PCI revision 选择步进并在 rootfs-ready 回调解析、保留主/pipe 程序；尚无 DMC event fixup 或 MMIO loader。
2c. Intel power-map 表 | 部分完成 | 6b796ee4 | 翻译 TGL/RKL/ADL-S/XELPD power-domain 与 power-well descriptors；`intel_display_power.c`/`power_well.c` 行为和 kernel consumer 仍待继续。
2. VBT / route mapping | 部分完成 | 43f7ec6d,54de259d | 将 child-device DVO/DSI 端口解析及 AUX 映射改为按 display version 与平台选择 i915 表，补齐 display-12 CRT 端口别名；尚未完成整个 intel_bios.c、电源 map 和 DMC MMIO。
