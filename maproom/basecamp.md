<!-- 仅由用户决定何时更新；除非用户明确要求更新 basecamp.md，否则本文件只读，不因项目推进而自动改写。 -->

# TheKernel Basecamp

## 当前位置

TheKernel 是基于 ArceOS 组件的 Rust `no_std` x86_64 单体内核预览版，已能运行真实 Linux 用户态，并具有较广的进程、文件、内存、网络、安全和异步 I/O 覆盖。当前可判定的产品边界是 `q35-preview-v0`，而不是完整 Linux、生产内核或整体性能产品。

x86_64 是唯一产品平台。q35 + UEFI/OVMF 的 Multiboot2、四核 SMP、定向 TLB shootdown、PCID/INVPCID 和按固件分配的 PCI memory BAR 已进入实际产品路径；AArch64、RISC-V64 和 LoongArch 不再构成任何实现或验收责任。通用机制由 `thekernel-ax` 承载，Linux policy 由 `thekernel-linux-abi` 承载，主仓库负责 syscall、对象、文件系统、驱动和平台集成。两个产品 sibling 的精确提交现在由单一 source-combination 记录控制，vISA 不再是产品构建依赖。

## 已形成的基础

近期实现已经超过旧 maproom 所描述的阶段。内核运行时使用 allocation-free transactional EEVDF；credential 与 seccomp/filter chain 已经是小型 epoch/RCU 的两个真实消费者，具有 per-CPU quiescent tracking、bounded retire queue 和 task-context destruction。cBPF 保留 interpreter canonical semantics，同时已有 x86_64 translator、bounded W^X publication、seccomp JIT 和 `SO_ATTACH_FILTER` packet 路径。这些是现存实现事实，不自动构成性能领先结论。

io_uring registered buffers 的 ext4/virtio-blk physical path 已包含最多 32 个在途 owner、多 extent/SG 计划、publish 前回滚、publish 后不可 fallback、IRQ→task completion 和 typed reset/quarantine。失效设备的 custody 现在按设备隔离，不再耗尽健康 sibling 的全局 completion 槽位；ring final close 可请求 task-context device reset，但在 lower layer 不能给出 `Quiesced/Retired` proof 时仍保留精确 owner，不以超时伪造物理退休。

无产品消费者的 UTS Semantic World pilot 已从启动、`ProcessData`、Cargo 图和 Linux UTS namespace 路径移除。普通 `uname`/hostname、fork、unshare 和 setns 不再消耗 provider slot 或暴露研究性 fence `EBUSY`。Semantic World 仍是长期思想，不是当前产品 runtime。

不可信本地进程可触发的几个确定性边界也已收紧：proc-style 值文件默认不再 world-writable，且写入/截断不会按用户 offset 或长度无界扩容；AF_NETLINK 在 usercopy/分配前限长并使用可失败分配；loop 绑定与全局状态变更需要初始 user namespace 的 `CAP_SYS_ADMIN`；`clone3` 用 checked range 验证显式栈。一份 first-party unsafe boundary index 现在列出机制 island、核心不变量和定向验证，但它不宣称所有 unsafe 已被独立证明。

## 当前证据边界

已有测试和历史性 KVM/TCG 运行不自动构成当前候选版证据。新 receipt 必须绑定 source combination 与各仓 worktree 状态，区分 suite marker、正常 guest shutdown 和 runner 主动停止。`raw_sample_count=0`、PMU unavailable 或 `perf-not-installed` 不能支持 formal/complete 性能结论。当前工作树本身有大量未提交变更，因此不是一个可发布候选版。

## 面前的工作

近期不再同时扩展调度、DMA、JIT、RCU、组件发布和 Semantic World。先让安全与证据真实性收口，然后使 `q35-preview-v0` 在单一 source combination 上通过零 FAIL/零 SKIP、无 panic/timeout 且正常关机的完整门禁。此后每次只选一个 Linux `v6.12.103` ABI 或实测性能缺口闭合，再决定下一项。
