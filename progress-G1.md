1. 移植地图 | 完成 | ad6dfb32 | 建立 v7.2.3 display 12/13 来源、现有 Rust 对应和许可证门禁；后续逐项增补。
2. VBT 和电源 | 进行中 | bd26db35,41eb1de7 | 增加 DMC 固件路径/尺寸映射、受许可证约束的 rootfs staging 和 rootfs-ready deferred request；固件解析/编程、上游完整 VBT 和完整 power-well map 尚未移植。
2b. DMC 延迟请求 | 部分完成 | 待提交 | 对已识别 ADL-P/N 使用 rootfs-ready 回调加载并保留二进制；还没有解析包或写入 DMC MMIO。
2. VBT / route mapping | 部分完成 | 待提交 | 将 child-device DVO/DSI 端口解析及 AUX 映射改为按 display version 与平台选择 i915 表；尚未完成整个 intel_bios.c、电源 map 和 DMC MMIO。
