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
- 当 usercopy 完成主内核迁移并收窄 unsafe 边界后，将其作为通用的显式用户内存访问机制，而不是 signal 的附属工具。

`thekernel-axpmu` 在 x86_64 后端、counter lease 和能力探测契约闭合前不构成主要独立发布物。`thekernel-axsched` 与 `thekernel-axtask` 保留分层 crate，但作为一条协调的调度/任务子系统发布，不承诺彼此任意版本可交叉组合。

Linux ABI 中的 credential、VFS、FD、MM、process、signal、seccomp、packet 和 io_uring 保留领域 crate 以便独立理解和验证，但整个 `thekernel-linux-abi` 是一条协调发布线，不把每个领域都变成一个需要单独维护的产品。主内核中的 process/readiness adapter、`axtask-compat`、具体主板平台和内核对象组装代码不独立发布；过渡 adapter 在迁移结束后直接删除。

## 可验证的性能机制

性能优化的首选方向不是引入更多隐式共享状态，而是把共享操作改写为可跟踪的所有权转移、不可复用的 generation 和有界批处理。优先级如下：

1. 实现按 address space 和活跃 CPU 定向的 TLB 失效，结合 x86_64 PCID/INVPCID，用每地址空间 generation、可合并请求和不可提前回收的 grace token 取代面向所有在线 CPU 的广播式刷新。
2. 将 scheduler run queue、timer、deferred work、network RX/TX 和 allocator hot cache 向 per-CPU 单所有者分区演化。本地操作不需要全局锁，跨 CPU 通过有界 handoff queue 传递一次性 transfer token，并显式报告 full、offline 和 stale。
3. 为 credential、FD/OFD 视图、signal routing、mount/dentry 等读多写少状态建立小型 epoch/RCU 机制。writer 在 publication 前完成分配，发布不可变快照并获得 retire token；回收队列满时拒绝新发布，对象只能在明确 grace period 后于 task context 销毁。RCU 只用于不可变、读多写少状态，不作为普遍的锁替代。
4. 将 IRQ 与后台处理转换为有界 continuation：中断只发布工作所有权，任务上下文按 budget 处理 network、timer、deferred finalizer 和 io_uring completion，剩余工作保留 token 并重新排队，不 busy-poll，不在 IRQ 中分配或销毁。暂不为它创建通用框架，等至少两个子系统形成相同契约后再抽取。
5. 在现有 migration 所有权和生命周期基础上实现 EEVDF，使用任务内嵌的 augmented tree 保持就绪路径 O(log n) 且零分配，并用参考模型验证 eligibility、lag、sleeper、reweight 和跨 CPU migration。Idle stealing 作为独立机制，只允许有界 victim scan、cache-hotness 阈值和可回滚的 ready-task transfer。
6. 为 cBPF 增加 x86_64 JIT，但 JIT 必须在 publication 前失败、产生不可变代码并遵守 W^X；后端使用 interpreter/JIT differential testing 或 translation validation，编译失败仍能保留解释器的完整语义。
7. 在 MM pin 语义成熟后扩展 io_uring registered buffers 和真实零拷贝路径。每次 I/O 持有 ring/table/slot/generation-bound lease，pin、DMA/IOMMU mapping 和 completion 分别使用有界 accounting 和可排空的 teardown；不满足条件时显式回退到 copy 路径。

暂不优先全局 lock-free 改写、无界 work stealing、在线自调参调度器、为 benchmark 定制的用户态 bypass ABI，也不使用“先发布错误状态、再后台修复”换取快路径。这些方向会模糊所有权、资源边界或 Linux 可见语义，不适合成为 TheKernel 的基础。

## 当前主张的边界

TheKernel 尚未达到完整 Linux ABI、全面生产可用或整体性能超过 Linux。现有组件和纵向功能切片证明了设计方向可行，但 crate 的存在不等于主内核已经完成集成，host 测试也不等于目标架构已经能够启动和运行完整用户态。

当前尚未闭合的高层问题是完整 Linux ABI 的分阶段边界。参考硬件、组件发布粒度和可验证性能机制已有当前决定，但具体机制仍必须用真实代码、Linux 语义和硬件数据检验；实测否定时可以直接改变实现或优先级。
