<!-- 仅由用户决定何时更新；除非用户明确要求更新 terrain.md，否则本文件只读，不因项目推进而自动改写。 -->

# TheKernel Terrain

## 项目性质与目标

TheKernel 当前是个人项目，没有生产部署、外部用户或既有版本兼容承诺。项目的目标不是维护现状，而是成长为一个高性能、Linux ABI 完整、可发布并可用于生产的操作系统内核；同时让内核能够被拆解为高度可验证、可靠、可维护、可移植的组件。

在达到这一目标之前，旧实现、内部 API/ABI、crate 边界、配置格式和仓库结构都不是兼容性约束。只要存在明确的语义、性能、可靠性、可验证性、维护性或移植性收益，就可以直接进行破坏性修改、替换或删除，不为假想用户保留弃用期、兼容层、迁移工具或双轨实现。Linux ABI 的参照物是 Linux 的可观察语义，而不是 TheKernel 过去暴露过的偏差。

## 核心方向

TheKernel 同时追求四件彼此约束的事情：

- 用新颖而有实证价值的方法提高性能，在明确、可复现的子系统和工作负载上对齐并超过 Linux；
- 持续扩大 Linux ABI 覆盖，补齐 syscall 背后的生命周期、并发、错误、权限和资源语义；
- 将通用机制、Linux ABI 策略和具体内核集成继续拆分，使性能与兼容性提升不以不可验证的耦合为代价；
- 只支持 x86_64，将全部平台、性能和完整系统工作集中在这一架构；不再支持 AArch64、RISC-V64 或 LoongArch。

“超过 Linux”不是一个无边界的宣传结论。它应落在指定硬件、配置、工作负载和指标上，并同时保留正确性、压力行为和资源边界。新颖性本身不是价值；只有实测收益和更好的系统结构才是价值。

## 长期架构方向

TheKernel 的长期目标不止是成为一颗支持组件和扩展的 Rust Linux 兼容内核，而是成长为一个治理 versioned semantic-provider graph 的 **semantic-world kernel**。Linux 是第一套、最重要的旗舰 personality，也是当前产品、性能和兼容性工作的中心，但不被理论上写死为唯一可能的系统语义。

长期架构的核心原则是：

> **Implementations are replaceable; semantic obligations are not.**

功能实现可以被替换、重新部署或隔离；已经承诺给应用的对象身份、状态连续性、权限边界、外部效果和资源责任不能随旧实现一起消失。TheKernel 因此可以被概括为：

```text
minimal stable authority substrate
+ resolved semantic provider graphs
```

稳定核心保留不能安全委托给可死亡 provider 的本机事实和不变量，包括 identity、capability、资源所有权与 accounting、binding publication、generation、native-resource custody，以及 CPU、页表、中断、时钟、DMA 和上下文切换等机制。设备是否还可能访问某段内存是 TheKernel 必须观测并产生类型化证明的本机物理事实，但当一个 escaped effect 已经注册到 Nexus/CSER 时，该事实不使 TheKernel 成为 settlement 或 retirement canonical state 的第二个权威。Linux ABI、文件系统算法、网络策略、调度策略和 agent runtime 等功能则可以逐步形成明确的 semantic provider 边界。provider 可以编译进宏内核，也可以部署在 compartment、进程、Wasm、microVM 或设备中；semantic component、deployment component、protection domain 和 custody domain 不必重合。

provider 更新至少区分三种绑定：新对象默认进入哪个 generation 的 `DefaultAdmissionBinding`，既有对象继续由谁服务的 `ObjectBinding`，以及已经提交的异步效果由哪个 effect authority 完成结算的 `SettlementBinding`。未接入外部 effect authority 的内建 Linux 路径可以使用最小本地状态机闭合本机设备生命周期；一旦同一 effect identity 交给 Nexus/CSER，TheKernel 只保留资源 lease、设备观测和 proof-producing adapter，并以 exact query/receipt 协调，不再独立推进同一 canonical terminal state。executor、effect 与 resource/custody 也具有独立 lifetime；停止旧代码、停止接收新对象、逻辑结果已知、底层资源不再被访问和旧 artifact 可以回收是不同事件。

这是一条渐进方向，而不是立即建设通用插件框架的要求。当前首先把内建 Linux 实现提升为高性能、完整且 provider-ready 的协调 personality。真实对象、更新或异步效果产生生命周期问题时，采用能够完整闭合该问题的最小纵向范围，而不是只做能够通过一次 smoke test 的占位机制；普通同步 syscall 继续使用简单直接的本地路径。复杂解析、验证和 provider 选择发生在创建或绑定阶段，稳定热路径使用已经解析并缓存的 generation-bound handle，不经过通用消息总线。

Semantic World 的完整研究构想还包括 CSER-derived effect authority、vISA continuity、Nix-style artifact closure 和 OS-native agentic control plane。它们是长期上位模型，不是当前已经实现的能力，也不能取代近期 Linux ABI、x86_64 性能、可靠性和发布工作。agent 只能消费受 capability 约束的权威对象、事件和动作；模型推断不能推进资源退休或其他系统真相。

这一长期模型已经有两个独立实现提供设计压力。Nexus/CSER 拥有逃逸出 executor 或 provider lifetime 的 effect identity、逻辑 custody、logical outcome、physical claim、settlement、retirement 和 recovery-artifact release；vISA 拥有 portable continuation scope、state lineage、continuity profile、semantic safe point、portable snapshot 和 activation validation。TheKernel 不复制它们的权威，而拥有 world、provider binding、native object、capability、resource accounting，以及 admission、operation 和 execution fence；对 DMA 等设备交互，它还保留不可委托的本机 lease，直到产生可交给 Nexus 的 quiescence/retirement proof。三者可以通过精确 identity、generation、query 和 receipt 协调，但任何本地 projection、日志、snapshot 或协调记录都不能成为另一个权威的平行真相源。

Nexus 已经用 portable authoritative core、独立 model、journal/checkpoint recovery、Loom/property differential、reply/DMA claim 和 OSTD 嵌入证明了 escaped-effect 生命周期可以形成精确状态机；vISA 已经用 pure reducer、restartable coordinator、durable capture、lineage CAS、lost-ack exact query 和 Wasmtime reference vertical 证明了 portable continuation 可以与 native binding 分离。这些成果不是 TheKernel 已集成的产品能力，但说明当前 Linux 底座采用的 ownership、prepare/publish、generation 和 retirement 边界将来可以自然提升为 Semantic World，而不必推翻重写。

## 分层模型

TheKernel 的代码按责任而不是当前目录划分：

1. 架构与 HAL：CPU、页表、TLB、异常、中断、时钟、DMA 和平台启动原语。
2. 通用机制：调度、任务、等待、驱动、网络、文件系统、缓存和通用状态机，不包含 Linux syscall 或 errno 策略。
3. Linux ABI 支持：进程、凭据、信号、FD/OFD、VFS、MM、readiness 等 Linux 可见规则。
4. 内核集成：syscall/UAPI 解析、usercopy、对象适配和子系统组合。
5. 产品与工具：构建、启动、测试、基准、诊断和发布工具。

分层服务于实际收益，可以继续破坏性调整。组件化不是把代码机械搬进 crate，而是让所有权、资源、错误、并发和平台依赖变得显式，并允许组件被独立理解和验证。

## 设计原则

- 用户可触发路径采用有界资源和显式 accounting；不以无界队列、缓存、pin 或后台工作换取表面性能。
- 可失败的分配、准入和验证在状态发布之前完成；跨阶段生命周期优先采用 prepare/commit/rollback 和 generation-safe token。
- 公共契约显式接收上下文、能力和不可变快照，避免依赖隐式 current task、全局 FD table 或全局 filesystem context。
- 错误应保留 OOM、容量、重试、不支持和语义失败的区别，直到 ABI 边界完成映射。
- unsafe、架构相关代码和不可验证的外部交互应收敛到小而清晰的边界。
- 性能抽象必须证明其成本；如果组件边界造成可测的关键路径损失，可以重新设计边界，而不是为了形式完整保留它。
- 不保留假成功、benchmark-name 特判、隐藏 busy polling、伪造输出或无法解释的兼容行为。

## 完整纵向切片与扩展性

`minimal stable authority substrate` 表示稳定核心只保留不能安全委托的权威，不表示每项实现都以最小功能、最小测试或最小完成度为目标。进入主线的核心纵向切片应同时闭合：

1. 可观察语义，包括正常、错误、取消、资源耗尽和并发结果；
2. owner、lease、generation、publication、settlement 和 teardown 生命周期；
3. 热路径成本、有界资源和明确的性能退化边界；
4. model、component、fault-injection、differential、系统和性能验证中与风险相称的部分；
5. 当前容量、unsupported、fallback 和未来扩展的显式边界。

当前范围可以有限，但不能含糊。pre-publication 失败可以释放资源并选择安全 fallback；任何外部效果或设备 descriptor 一旦 publish，后续只能完成、失败、保留为 indeterminate、reset/quarantine 或继续等待物理退休，不能再伪装成“未发生”并切换到另一实现。executor 退出、fd close、timeout、日志缺失和 polling exhaustion 都不是 logical outcome 或 physical quiescence 的证据。

完整实现也不等于提前建设没有消费者的万能框架。共同抽象应从至少两个真实纵向消费者中提炼；尚未需要的部署后端、持久化协议和兼容层可以不存在。项目没有外部 API 稳定负担，因此良好扩展性来自清晰权威和可替换实现，而不是冻结当前接口或预留大量条件分支。

## 平台边界

x86_64 是唯一支持的架构，也是唯一承担启动、Linux ABI、性能优化、完整系统验证和发布责任的平台。AArch64、RISC-V64 与 LoongArch 全部退出项目目标；相关实现、构建、配置、测试、文档和条件分支可以直接删除，不需要弃用流程、替代基线或兼容层。新接口和组件设计也不为这些架构保留抽象负担。

项目仍重视清晰的架构边界和组件内部的可移植设计，因为这有助于验证、维护和隔离 unsafe；但“可移植性”不再表示维护第二 ISA，也不能成为保留无实际消费者的平台代码的理由。

x86_64 的首选虚拟平台是 QEMU `q35` + UEFI/OVMF，主要真实硬件是现有的主流 Ryzen/Intel 台式机。后续平台机制、性能结论和发布验收都围绕这些环境建立。

## 组件边界与发布粒度

crate 是编译和验证边界，不自动是独立产品或独立发布承诺。独立发布只适合责任能用一句话说清、不依赖 TheKernel 具体对象、所有权与失败契约在边界内闭合，并能在 host 上用替身后端独立测试的机制。内部 API 可以继续随实际收益破坏，不为独立发布虚构兼容负担。

值得作为小而独立的通用机制发布的核心集合是：

- `thekernel-axcbpf`：有界的验证与执行机制；
- `thekernel-axtlb`：不含架构指令和 IPI 驱动的 shootdown 状态机；
- `thekernel-axpoll`：通用 readiness 注册和唤醒所有权；
- `thekernel-axfault`：有界、generation-safe 的请求/回复 broker；
- usercopy 已进入主内核真实调用路径；在访问契约和 unsafe 边界进一步闭合后，将其作为通用的显式用户内存访问机制，而不是 signal 的附属工具。

`thekernel-axpmu` 在 x86_64 后端、counter lease 和能力探测契约闭合前不构成主要独立发布物。`thekernel-axsched` 与 `thekernel-axtask` 保留分层 crate，但作为一条协调的调度/任务子系统发布，不承诺彼此任意版本可交叉组合。

Linux ABI 中的 credential、VFS、FD、MM、process、signal、seccomp、packet 和 io_uring 保留领域 crate 以便独立理解和验证，但整个 `thekernel-linux-abi` 是一条协调发布线，不把每个领域都变成一个需要单独维护的产品。主内核中的 process/readiness adapter、`axtask-compat`、具体主板平台和内核对象组装代码不独立发布；过渡 adapter 在迁移结束后直接删除。

## 可验证的性能机制

性能优化的首选方向不是引入更多隐式共享状态，而是把共享操作改写为可跟踪的所有权转移、不可复用的 generation 和有界批处理。优先级如下：

1. 按 address space 和活跃 CPU 定向 TLB 失效，结合 x86_64 PCID/INVPCID，用每地址空间 generation、可合并请求和不可提前回收的 grace token 避免无条件全核广播。
2. 将 scheduler run queue、timer、deferred work、network RX/TX 和 allocator hot cache 向 per-CPU 单所有者分区演化。本地操作不需要全局锁，跨 CPU 通过有界 handoff queue 传递一次性 transfer token，并显式报告 full、offline 和 stale。
3. 为 credential、FD/OFD 视图、signal routing、mount/dentry 等读多写少状态建立小型 epoch/RCU 机制。writer 在 publication 前完成分配，发布不可变快照并获得 retire token；回收队列满时拒绝新发布，对象只能在明确 grace period 后于 task context 销毁。RCU 只用于不可变、读多写少状态，不作为普遍的锁替代。
4. 将 IRQ 与后台处理转换为有界 continuation：中断只发布工作所有权，任务上下文按 budget 处理 network、timer、deferred finalizer 和 io_uring completion，剩余工作保留 token 并重新排队，不 busy-poll，不在 IRQ 中分配或销毁。暂不为它创建通用框架，等至少两个子系统形成相同契约后再抽取。
5. EEVDF 使用任务内嵌的 augmented tree 保持就绪路径 O(log n) 且零分配，并用参考模型验证 eligibility、lag、sleeper、reweight 和跨 CPU migration。参数、实际 elapsed-runtime accounting、remote reschedule 和调度延迟需要系统测量；idle stealing 作为独立机制，只允许有界 victim scan、cache-hotness 阈值和可回滚的 ready-task transfer。
6. 为 cBPF 增加 x86_64 JIT，但 JIT 必须在 publication 前失败、产生不可变代码并遵守 W^X；后端使用 interpreter/JIT differential testing 或 translation validation，编译失败仍能保留解释器的完整语义。
7. 扩展 io_uring registered buffers 和真实物理 DMA 路径。每次 I/O 持有 ring/table/slot/generation-bound lease，pin、cache-range ownership、DMA/IOMMU mapping、device request、logical completion 和 physical retirement 分别使用有界 accounting 与可排空 teardown；多 SG、多 extent、多 request 和 queue depth 必须保留 publish 前可回滚、publish 后不可 fallback 的边界。

暂不优先全局 lock-free 改写、无界 work stealing、在线自调参调度器、为 benchmark 定制的用户态 bypass ABI，也不使用“先发布错误状态、再后台修复”换取快路径。这些方向会模糊所有权、资源边界或 Linux 可见语义，不适合成为 TheKernel 的基础。

## 验证与跨系统比较

大规模验证以语义义务和机制不变量为对象，而不是以当前函数、crate 或日志文本为对象。Linux ABI 使用同一静态 workload 在 Linux、Asterinas 和 TheKernel 上比较规范化的 return、errno、observable state 和资源生命周期；Linux 是 Linux 可观察语义的主要参照，Asterinas 是 safe-Rust Linux 实现和 unsafe containment 的重要对照。Zircon/Fuchsia 不作为 Linux ABI oracle，而用于比较 object/capability lifecycle、异步等待、driver isolation 和机制性能。

关键状态机优先保留 pure model 或独立 normalized oracle，使用 property sequence、受控 interleaving/Loom、failure atomicity 和 recovery reconstruction 检查实现。产品集成在 q35/UEFI/SMP4 的 TCG correctness lane 与 KVM comparison lane 中验证；Linux guest 与 TheKernel guest 必须使用相同虚拟硬件、CPU pinning、workload 和输入。Ryzen/Intel 裸机负责验证 KVM 无法替代的 IPI、APIC timer、cache/TLB、PMU、真实 IRQ、DMA/IOMMU、NIC/NVMe 和极端 tail latency。测试和 benchmark 应能扩展到更多实现，但不为未来矩阵提前建设与当前任务无关的 evidence、release 或 provenance 基础设施。

## 当前主张的边界

TheKernel 尚未达到完整 Linux ABI、全面生产可用或整体性能超过 Linux。现有组件、EEVDF 产品切换、定向 TLB、网络 continuation 和窄范围物理 DMA 证明了设计方向可行，但局部 RCU 消费者和其他机制不等于整个内核已经形成统一 per-CPU/RCU/effect substrate，KVM 结果也不等于裸机或硬件一般性结论。

当前唯一可发布主张命名为 `q35-preview-v0`：x86_64、QEMU `q35` + UEFI/OVMF、4 vCPU、1 GiB RAM 和 virtio 设备，威胁模型只覆盖 guest 内不可信本地进程对已实现 Linux ABI 的权限与有界资源隔离。候选组合必须绑定并匹配单一 source-combination identity，从 clean worktree 重建，host tests、portable Linux differential 和 guest KTAP 零 FAIL/零 SKIP，并在 suite marker 后正常关机；marker、timeout 或 runner 主动杀死 QEMU 都不构成产品通过。这个名称不包含完整 ABI、通用发行版兼容、容器、强多租户、长时内存压力服务、生产存储、裸机或整体高性能承诺。

Linux ABI 和性能对照固定于 Linux stable `v6.12.103`（commit `25c09b42358e73e1476e517b296edb6344f2e4bd`），并要求报告绑定 kernel config、OVMF、rootfs、helper 和拓扑的内容身份。“完整 Linux ABI”只能由该 baseline 的 x86_64 syscall/UAPI 矩阵逐项证明，dispatcher 分支数不是完成度。性能主张必须在同拓扑上保留 raw repeats，至少五次 fresh run，报告 throughput/CPU 及 P50/P99/P99.9；`raw_sample_count=0`、PMU unavailable 或 `perf-not-installed` 都只能产生 unavailable/degraded 结论，不能进入 formal/complete 或“超过 Linux”结论。

当前尚未闭合的高层问题是完整 Linux ABI 的分阶段边界，以及最小 stable authority substrate、provider-ready Linux personality 和首个 Semantic World 纵向实验的准确边界。参考硬件、组件发布粒度和可验证性能机制已有当前决定，但具体机制仍必须用真实代码、Linux 语义和硬件数据检验；实测否定时可以直接改变实现或优先级。

TheKernel 当前还没有通用 World runtime、动态 personality resolver、CSER journal、vISA continuity、Nix semantic closure 或 agent-native control plane。相关术语不能被用来夸大当前完成度，也不能为了概念完整而提前污染内核热路径、复制权威状态或制造没有实际消费者的 receipt 和基础设施。
