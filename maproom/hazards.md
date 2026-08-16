# TheKernel Hazards

- **不要虚构外部兼容负担。** TheKernel 当前是没有生产部署和外部用户的个人项目。除非用户明确提出，不要增加弃用期、兼容层、迁移工具、双轨实现、发布过渡或为假想消费者保留旧 API/ABI。发现更好的设计时可以直接破坏、替换或删除。
- **不要把过去的 TheKernel 行为当成 Linux ABI。** Linux 兼容的参照是经过核对的 Linux 可观察语义；旧实现的偏差不需要为了“向后兼容”而保留。
- **x86_64 host 测试不是 x86_64 内核支持。** 当前 host target 能覆盖大量 Rust 逻辑，但不能证明 boot、异常、syscall entry、页表、中断、SMP、设备或用户态路径成立。
- **不要为已取消的平台保留负担。** AArch64、RISC-V64 与 LoongArch 均不属于支持范围；不要为它们增加抽象、条件分支、构建、测试或文档，现存路径可以直接删除。
- **crate 存在不等于迁移完成。** `thekernel-linux-abi` 已包含 signal 和 usercopy 等 crate，主内核仍可能使用旧实现；判断完成度必须检查真实依赖和调用路径。
- **三个仓库是一个正在协同演化的系统。** TheKernel、`thekernel-ax` 和 `thekernel-linux-abi` 当前通过精确版本、path patch 和 release consumer gate 连接。允许破坏性修改，但跨仓库接口变化必须在同一工作中闭合，不能留下互不匹配的源码状态。
- **OSComp 和单一 microbenchmark 不能定义架构。** 优化必须对应真实子系统，并检查压力、资源和 Linux 可见语义；不得恢复程序名特判、假输出、隐藏 busy polling 或无界资源。
- **“超过 Linux”必须限定比较条件。** 不同硬件、配置、功能集或缓存状态下的数字不能支持领先结论；必须明确工作负载、指标和正确性边界。
- **x86_64 是唯一产品架构。** host 测试、通用组件的天然可移植性或其他架构源码的存在，都不能把 AArch64、RISC-V64、LoongArch 重新变成验收对象；平台功能与性能主线只以 x86_64 推进。
