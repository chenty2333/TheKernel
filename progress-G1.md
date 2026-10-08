1. 移植地图 | 完成 | ad6dfb32 | 建立 v7.2.3 display 12/13 来源、现有 Rust 对应和许可证门禁；后续逐项增补。
2. VBT 和电源 | 进行中 | bd26db35,41eb1de7,8f7b8001,6b796ee4,7f818a7e,abc51731 | 增加 DMC 路径、rootfs staging/解析、power-domain maps、HSW well handshake 和 Gen12 DC-state retry。DMC event/MMIO、完整 VBT/power map consumer、DC5/6/9 runtime/refcount 生命周期尚未完成。
2b. DMC 延迟请求和解析 | 部分完成 | 8f7b8001 | 从实际 PCI revision 选择步进并在 rootfs-ready 回调解析、保留主/pipe 程序；尚无 DMC event fixup 或 MMIO loader。
2c. Intel power-map 表 | 部分完成 | 6b796ee4 | 翻译 TGL/RKL/ADL-S/XELPD power-domain 与 power-well descriptors；kernel map consumer 和其余平台生命周期仍待继续。
2d. HSW power-well handshake | 部分完成 | 7f818a7e | 移植并接入请求者、fuse、enable/disable 和 state 查询；仅 PW_1/DDI-IO 当前路径可用，IRQ 耦合 well/其余 i915 电源管理仍待移植。
2e. Gen12 DC-state field write | 部分完成 | abc51731 | 移植 `gen9_dc_mask`/`gen9_write_dc_state` 并用于启动时禁用 DC state；DMC-controlled DC5/6/9 进入/退出尚未接通。
2. VBT / route mapping | 部分完成 | 43f7ec6d,54de259d | 将 child-device DVO/DSI 端口解析及 AUX 映射改为按 display version 与平台选择 i915 表，补齐 display-12 CRT 端口别名；尚未完成整个 intel_bios.c、电源 map 和 DMC MMIO。
2. 电源生命周期差距盘点 | 文档化 | dbfbe824 | 明确列出异步 put、完整 modeset 域接线、IRQ-coupled well callbacks 和 DC5/6/9/DMC runtime 的未完成边界；转入时钟项前不再扩展该项实现。
3. CDCLK transition planning | 部分完成 | 71c1eb18 | 在 `tk-intel-display::cdclk` 翻译 crawl/squash/CD2X 分类和 midpoint 算法；原 crate 测试 4 项通过。
3b. DPLL manager arithmetic | 部分完成 | d4d650b2 | 翻译 ICL/TGL combo PLL divider search, DP/TBT table, CFGCR encode/decode; 3 targeted tests pass. Runtime MMIO and `intel_dpll.c` remain.
3c. Shared DPLL reference policy | 部分完成 | 12d36f98 | 翻译平台候选掩码、TC→MG PLL id、active port selection、state-matching reserve/refcount/release；2 tests pass.
3d. Generic CRTC DPLL dispatch | 部分完成 | 95b990c8 | 翻译 DPLL `compute_clock/get_dpll` guards、stale state clear、±1 kHz clock comparison；3 tests pass，暂未接进 kernel atomic path。
3e. Runtime CDCLK MMIO transition | 部分完成 | pending | kernel `clk::transition` 接入 i915 full-PLL disable/enable 与 TGL crawl request/ack、CDCLK_CTL 写入；crate cdclk tests 4/4、kernel `cargo check --tests` 成功。Atomic/PCode/peripheral-lock call site 仍未连接。
3f. Combo DPLL0/1 power sequence | 部分完成 | ff632686 | kernel adapter 接上 i915 power-state→CFGCR→enable/lock 与 disable/power-off 顺序；`cargo check -p tk-kernel --tests` 通过，单元测试只 compile-check 未运行。尚未连 modeset call site。
3h. DKL DP/HDMI PLL calculations | 部分完成 | pending | 翻译 DKL `icl_mg_pll_find_divisors` 的 DP 精确 8.1GHz 和 HDMI DCO-window 分支；6 个 targeted tests 通过。
3i. DKL TC PLL runtime adapter | 部分完成 | f5141ee1 | kernel 动态寄存器 adapter 接入 TGL/ADL-P/N TC1/2 enable/disable、HIP selector serialization 和 power/lock polling；map tests compile-check，`cargo check -p tk-kernel --tests` 通过；等待 modeset call site 与真实 power refs。
3j. TBT PLL power sequence | 部分完成 | 2e7a4151 | 增加 TBT CFGCR0/1+enable register entries，复用 i915 PLL power/lock/power-off 顺序；`cargo check -p tk-kernel --tests` 通过，测试 compile-check。
3k. DPLL platform inventories | 部分完成 | f78ba138 | 翻译 i915 TGL/RKL/DG1/ADL-S/ADL-P/N/EHL/JSL DPLL descriptor arrays and source ordering；6 targeted tests pass.
