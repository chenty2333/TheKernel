1. 移植地图 | 完成 | ad6dfb32 | 建立 v7.2.3 display 12/13 来源、现有 Rust 对应和许可证门禁；后续逐项增补。
2. VBT 和电源 | 进行中 | bd26db35,41eb1de7,8f7b8001,6b796ee4,7f818a7e,abc51731 | 增加 DMC 路径、rootfs staging/解析、power-domain maps、HSW well handshake 和 Gen12 DC-state retry。DMC event/MMIO、完整 VBT/power map consumer、DC5/6/9 runtime/refcount 生命周期尚未完成。
2b. DMC 延迟请求和解析 | 部分完成 | 8f7b8001 | 从实际 PCI revision 选择步进并在 rootfs-ready 回调解析、保留主/pipe 程序；尚无 DMC event fixup 或 MMIO loader。
2c. Intel power-map 表 | 部分完成 | 6b796ee4 | 翻译 TGL/RKL/ADL-S/XELPD power-domain 与 power-well descriptors；kernel map consumer 和其余平台生命周期仍待继续。
2d. HSW power-well handshake | 部分完成 | 7f818a7e | 移植并接入请求者、fuse、enable/disable 和 state 查询；仅 PW_1/DDI-IO 当前路径可用，IRQ 耦合 well/其余 i915 电源管理仍待移植。
2e. Gen12 DC-state field write | 部分完成 | abc51731 | 移植 `gen9_dc_mask`/`gen9_write_dc_state` 并用于启动时禁用 DC state；DMC-controlled DC5/6/9 进入/退出尚未接通。
2. VBT / route mapping | 部分完成 | 43f7ec6d,54de259d | 将 child-device DVO/DSI 端口解析及 AUX 映射改为按 display version 与平台选择 i915 表，补齐 display-12 CRT 端口别名；尚未完成整个 intel_bios.c、电源 map 和 DMC MMIO。
2. 电源生命周期差距盘点 | 文档化 | pending | 明确列出异步 put、完整 modeset 域接线、IRQ-coupled well callbacks 和 DC5/6/9/DMC runtime 的未完成边界；转入时钟项前不再扩展该项实现。
