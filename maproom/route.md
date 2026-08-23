<!-- 仅由用户决定何时更新；除非用户明确要求更新 route.md，否则本文件只读，不因项目推进而自动改写。 -->

# TheKernel Route

近期只按以下三个门依次推进，不再让 Linux ABI、调度、DMA、JIT、RCU、组件发布和 Semantic World 同时争夺一条主线。

1. **安全与证据真实性。** 先消除不可信本地进程可触发的无界分配、权限绕过和地址回绕，保留 DMA publish 后 fail-closed 的资源责任，并使 receipt、guest shutdown、raw samples 和 PMU 可用性只表达实际观测到的事实。本门未关闭时不扩展研究机制。
2. **`q35-preview-v0` 产品门。** 固定 x86_64 q35/UEFI/SMP4/1 GiB 组合和单一 source-combination identity，从 clean source 通过 build/lint/host tests、portable Linux differential 和 guest KTAP，零 FAIL、零 SKIP、无 panic/timeout，并在 suite 完成后正常关机。该 preview 明确不承诺通用发行版、容器、强多租户、长时内存压力服务、生产存储、裸机或整体超过 Linux。
3. **单一高价值缺口。** 在 preview 门稳定后，每次只从 Linux `v6.12.103` x86_64 syscall/UAPI 矩阵或已有性能机制中选择一个证据最充分的缺口，以语义 oracle、资源/teardown 和同拓扑 raw measurements 闭合后再选下一个。syscall 分支数、单次 benchmark 或新子系统行数都不计为进度。

Semantic World 保留为长期架构方向，但不再通过无产品消费者的 UTS pilot 进入启动或普通 Linux ABI 热路径。它只在出现真实纵向消费者且与 Nexus/CSER 和 vISA 的 canonical authority 边界一同闭合时重新进入实施。

按用户当前决定，真实 ACPI IRQ routing、MSI/MSI-X、IOMMU/DMAR、NVMe、常见 NIC、现代电源管理以及完整 XSAVE/XCR0/AVX 生命周期暂不纳入近期路线。
