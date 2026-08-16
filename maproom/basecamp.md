<!-- 仅由用户决定何时更新；除非用户明确要求更新 basecamp.md，否则本文件只读，不因项目推进而自动改写。 -->

# TheKernel Basecamp

## 当前位置

TheKernel 是一个基于 ArceOS 组件的 Rust `no_std` 单体内核预览版，已经具备较广的 syscall、进程、文件、虚拟内存、网络和安全子系统覆盖，但尚不是完整 Linux 替代品，也尚未达到生产可用状态。

x86_64 已成为唯一产品平台。当前 q35 + UEFI/OVMF 的 Multiboot2 启动、ACPI/MADT 拓扑发现和四核 SMP 路径已经能够拉起全部 CPU，并运行完整系统测试；同一内核 ELF 同时保留 Multiboot1 fallback。定向 TLB shootdown 已在 release 构建和真实四核 guest 中闭合，PCID/INVPCID 正路径与不提供这些能力的回退路径均可运行完整系统测试；PCI memory BAR 也已改为按固件实际分配动态映射，不再依赖固定的 q35 高位 MMIO 窗口。AArch64、RISC-V64 与 LoongArch 均已退出支持范围，其平台实现、产品构建、测试与工具路径已从当前工程中移除，不构成兼容或验证责任。

项目已经形成三个代码层次：`thekernel-ax` 承载通用任务、调度、readiness、fault、PMU、TLB 和 cBPF 机制；`thekernel-linux-abi` 承载 Linux 凭据、VFS、FD、MM、进程、信号、usercopy、io_uring、packet 和 seccomp 策略；TheKernel 主仓库负责实际对象、syscall 和平台集成。主内核已经消费其中大部分组件，但 signal 和 usercopy 仍未迁移到新的 Linux ABI crate，一些 adapter 和 vendored patch 也仍是过渡边界。

## 已形成的基础

近期工作已经建立了有界资源、显式上下文、事务化 publication、generation-safe 生命周期和类型化错误等共同设计语言。seccomp、AF_PACKET、有限 io_uring、userfaultfd、credential、FD/readiness、VFS 和 MM 等方向均已有实际纵向切片；futex/futex2 的 private、private-mapping 与 file-backed identity 已按 Linux key domain 分离并覆盖 remap 生命周期，rseq 的事件状态机、signal abort 与 x86_64 `ucontext` 布局也已接入完整 guest。Linux differential tests、PMU/ASID 诊断、SMP TLB/I-cache 协调和 load-aware scheduling 为进一步兼容性与性能工作提供了基础。

旧 RFC 文档和隐藏的 feature-layering 设计文档已经从当前文档体系中移除。项目方向、当前状态和工作规则现在分别由 maproom 的 terrain、route、basecamp 和 hazards 承载；源码附近的 README、测试说明与 provenance 文档继续保留其局部职责。

## 面前的工作

项目已经收敛为只支持 x86_64。接下来集中推进真实 Ryzen/Intel 硬件路径和后续性能机制，不再为其他 ISA 安排恢复、兼容或验证工作。Linux 语义仍需继续扩展，已抽取的组件也需要真正取代主内核中的重复或过渡实现；signal 与 usercopy 的主内核迁移仍是最明显的组件化缺口。

性能目标也仍处于“具备测量和机制基础、尚未形成全面结论”的阶段。后续工作需要在真实子系统和明确比较条件下取得可复现收益，而不是从已有 benchmark 结果外推整体领先。
