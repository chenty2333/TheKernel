# 常用工具的 procfs / sysfs 覆盖

## 范围与方法

2026-10-05，main，起点 `65ec56c8`。只做 QEMU guest，不做真机。
以 `/home/ava/Desktop/linux-7.2.3` 为输出格式和行为基准。
`cpufreq`、`cpuidle`、`hwmon` 属于 Codex A；这里只记录消费者，不实现。

先在主机运行 `strace -f -e trace=%file -s 256`，下面只汇总 proc/sys
文件访问（包括不存在的可选探测）。PID、BDF、CPU、设备编号归一化。
目录 fd 的相对访问另外用 `-yy` 检查；不能把未见到绝对路径当作没有访问。
主机程序成功不是 TheKernel 的 guest 验收。下面的初始判断来自现有注册和
格式化代码；运行验证单独记录。未保留主机进程列表或完整跟踪归档。

## 工具清单（初始盘点）

| 工具 / 实测调用 | 所需节点或接口 | main 初始覆盖 / 缺口 |
| --- | --- | --- |
| 项目 BusyBox `ps`, `top -b -n 1` | `/proc/<pid>/{stat,cmdline}`, `/proc/{stat,loadavg,meminfo}` | 存在；stat 的全局事件行缺失，CPU 仅四个时间字段；guest 待验 |
| BusyBox `free`, `uptime` | meminfo；uptime 也可走 sysinfo | 节点存在，有真实 allocator/CPU 时间来源；待验 |
| BusyBox `vmstat`, `ip -s link` | vmstat；rtnetlink IFLA_STATS/IFLA_STATS64 | 当前镜像 vmstat applet 未编译；`ip -s link` 主机也报 usage（不是 guest 内核缺失的证据） |
| procps `ps aux`, `top -b -n 1` | stat/status/cmdline/statm/task, cpuinfo/meminfo/stat/loadavg/uptime, sys/kernel/{pid_max,osrelease}, tty/drivers；CPU possible、node cpumap | 主体存在；statm、tty/drivers 缺失；cpuinfo 只有 processor/flags，smaps 不完整 |
| procps `free -w`, `vmstat -s`, `uptime` | meminfo、vmstat、stat、loadavg、uptime | vmstat 缺失，其余存在；数值完整性尚未验收 |
| procps `pmap -x <pid>` | maps、smaps | 存在；smaps 只有 Size/Rss/Locked，缺共享/私有/Pss 等字段，不能称格式完整 |
| sysstat `pidstat`, `iostat`, `mpstat` | stat、uptime、diskstats、interrupts、softirqs；每 PID stat/status/io，CPU online/topology | 主机未安装这三个程序：**未跟踪、未验收**，这是待补工具清单，不是已跑过的结论 |
| `htop --readonly` | stat/meminfo/loadavg/uptime, PID task/stat/status/cmdline, cgroup/cpuset/ns/pid, smaps_rollup；可选 hwmon、hugepages、zram、GPU | 主机非交互启动退出 0（不证明交互功能）；smaps_rollup/statm 缺失；可选硬件节点只对真实设备发布，不伪造 |
| `lsblk -a -o NAME,SIZE,TYPE,RO,RM,PKNAME` | `/sys/{block,dev/block,class/block}`, 每设备 dev/uevent/size/ro/removable/partition/stat、queue、父子链接 | block 只有 loop 和磁盘目录；磁盘缺 dev/ro/removable/stat，class/block 和分区父子结构缺失 |
| `findmnt`, `mount`, `df -T` | self/mountinfo、mounts；statfs；设备辅助信息 | mountinfo/mounts 存在且绑定 mount view；待真实程序验收 |
| `lscpu` | cpuinfo、system/cpu/{online,possible,present,cpuN/topology,cpuN/cache}、system/node | CPU 列表/node 已有，拓扑/cache/cpuinfo 详情不全；调频/空闲另属 A |
| `lsns` | PID ns/*、stat、cmdline、mountinfo；nsfs ioctl | ns/* 已有，需验证真实工具而不只检查链接文本 |
| `lspci -vvv`, `-k`, `-t` | bus/pci/devices/BDF/{vendor,device,class,revision,subsystem_vendor,subsystem_device,config,resource,irq,driver,numa_node,modalias,msi_irqs}；drivers 和真实父级 | 只有显示/输入路径发布部分 PCI 身份；没有完整 PCI 枚举、只读配置空间、BAR 资源/driver 树 |
| `lsusb` | bus/usb/devices/*/{busnum,devnum,descriptors,product,speed,subsystem}；usbfs | 通用 USB sysfs 枚举缺失，不能用空目录假装支持 |
| 主机 iproute2 `ip -s link` | rtnetlink，不直接读取 proc/sys | 主机成功；需核对 guest rtnetlink 计数与 sysfs statistics 共用来源 |
| `ss -tanp` | SOCK_DIAG netlink，PID fd/链接/cmdline；回退 proc/net/tcp* | SOCK_DIAG 支持情况待验；PID fd 已有；net/tcp* 缺失 |
| `netstat -tunap` | proc/net/{tcp,tcp6,udp,udp6}、PID fd/cmdline | net 仅 dev；协议连接表缺失 |

主机上述命令除特别标注者均退出 0。项目 BusyBox 从已构建 rootfs 中提取后
只读运行，不替换主机工具；其 ps/top/free/uptime/netstat 均退出 0。
BusyBox 1.36.1 选项与 Alpine BusyBox 选项不能假定相同。

## 节点逐类审核

| 节点 | 起始状态 / 格式 / 真实来源 |
| --- | --- |
| interrupts、softirqs | 缺失；需每 CPU 入口计数，不能用 CPU tick 数冒充所有 IRQ |
| partitions、diskstats | 缺失；已有块设备几何、NVMe GPT 分区 wrapper；应共用设备 ID 和 I/O 完成统计 |
| vmstat、zoneinfo、buddyinfo | 缺失；已有 allocator 总量、PageCache/PageTable usage、回收/水位统计；无 Linux zone/buddy 实现，不应虚构 DMA/Normal 分区或高阶空闲块 |
| modules | 缺失；内核静态链接，无可加载模块；正确模块列表为空，不把内建驱动列为模块 |
| filesystems | 缺失；mount/fsopen 已有统一 FILESYSTEM_TYPES，应从该注册表导出 nodev 标记，不独立维护第二份名单 |
| pressure/{cpu,memory,io} | 缺失；已有 memory_pressure 是 TheKernel 自定义回收事件快照，**不是** PSI stall 时间/平均值。没有 PSI 统计时不要发布虚假的零 PSI |
| net/dev | 已有 Linux 两行表头、16 个统计字段；8 个收发计数来自当前 netns 的 device_stats，其余未细分项为 0 |
| net/{route,tcp,tcp6,udp,udp6,unix,snmp} | 缺失；应来自 netns 路由/连接/协议计数，不能仅加空表掩盖活跃 socket |
| PID stat/status/maps/mountinfo/ns | 已有；这些存在不代表每字段完整或容器工具通过 |
| PID statm/io/smaps_rollup | 缺失；PID fault/accounting 和 address-space resident 信息已存在，需原始来源适配 |
| class/net/IF/statistics/* | 缺失；已有 net device_stats 可用，但 sysfs 必须与 netns 可见设备一致 |
| sys/block、class/block、dev/block | 见 lsblk 行；size 单位应始终为 512 字节扇区，不能改成原生 NVMe LBA 单位 |
| bus/pci 与 bus/usb | 见工具行；设备配置只读，枚举不得触发 BAR 写探测或默认写 NVMe |

## 修补顺序与验收

先修直接缺文件/程序退出错误，再修错误输出，再补可选字段。
每项独立提交前跑相关主机测试和 lint；内核行为变化加完整 KVM guest；
系统调用语义变化加 ABI。真实工具另用独立 payload，不能挤入默认 rootfs。
每三项以及结束前按 COMMON.md 跑完整 host/guest 和两个平台 lint。

当前：盘点完成；**B1 节点修补及全工具 guest 验收尚未完成**。
后续每项在这里记录真实来源、零字段的依据以及已跑/未跑边界。

### B1.2a：filesystems / modules

filesystems 从 legacy mount/fsopen 的同一 provider 注册表枚举：块 provider
省略 nodev，非块 provider 使用 nodev，字段以 tab 分隔并以 LF 结束。
modules 为空：静态链接的驱动不是可加载模块。新增两个主机格式/来源测试、
默认 guest 的 proc-inventory 用例。未改变 mount 的行为或 provider 集合。

后续工具准备：已用 Alpine 3.24.1 自带 apk 和信任密钥（不使用
allow-untrusted，不执行安装脚本）在本用户缓存目录解包签名软件包。
在主机只读运行 sysstat 12.7.8：pidstat 读取 stat/status/schedstat、
stat/uptime、CPU 目录；iostat -x 读取各磁盘 stat、stat/uptime、CPU 目录；
mpstat -P ALL 读取 interrupts/softirqs/stat/uptime、CPU 目录。三个程序均
退出 0；guest 尚未运行。PID schedstat 也缺失，应补真实调度时间而非假值。

B1.2a 实测：两个新增主机测试通过，product lint 通过；完整 KVM guest
53/53 通过，无跳过且正常关机，包含 `PROC_INVENTORY_OK filesystems=19
loaded_modules=0`。第一次运行因 helper 误放在不被打包的目录而失败，修复
打包路径并重新构建、完整重跑后才记录通过。这不是所有工具的验收。

### B1.2b：中断观察

在 axhal IRQ dispatcher 进入点记录当前 CPU 的 x86 vector：固定数组，每 CPU
cacheline 对齐，单次 relaxed atomic 增量，无锁、无分配、无日志。包括外部
INTx 和 MSI/MSI-X 实际到达的向量，未观察到的普通向量不输出。TheKernel
的 IRQ 号就是 vector，**不是 Linux 动态分配的逻辑 IRQ 编号**。
LOC/SPU 来自 LAPIC 向量；ERR 为所有 CPU 的实际 APIC error 总量。没有 NMI
入口统计，不伪造 NMI 行。没有真实设备/处理器标签的普通行标记 x86-vector。

单个原始 IPI vector 与多原因 broker 不等价：IPI 行是实际硬件到达次数；
RES/CAL/TLB 是对应原因的实际分发次数，合并的请求不会被计成多次原始 IRQ。
这些计数不会改动设备启用、路由、CPU 功耗策略或中断确认流程。

softirqs 输出 Linux 的 10 个名称和每 CPU 列。内核**没有 Linux softirq
执行机制**，已有网络轮询/定时器/延后任务在 hard IRQ 或 task context 执行，
所以这十类软中断事件为零；不能用这些零推导没有网络活动或没有定时器。
该节点不是引入 ksoftirqd，也不是已实现 Linux softirq 子系统的声明。

同一向量观察还导出 `/proc/stat` 的 intr 总量/各向量字段；总量只加原始
IRQ，不再加 IPI 原因，防止重复。CPU 行补为 Linux 的十字段形状：未单独
跟踪的 iowait/irq/softirq/steal/guest/guest_nice 为 0；现有 idle/system
仍包含无法细分的等待/中断时间。没有虚拟机 vCPU steal 或 guest time
会计来源，不能把这些零当成性能证明。softirq 总量和十类事件也为 0。

B1.2b 实测：完整 host 退出 0，650 个 Python 测试（3 个环境 skip）、5995 个
Rust 测试通过（含 kernel 2580）；q35 和 n305 product lint 通过（各 785 条
既有 kernel 警告）；最终源码的完整 KVM guest 54/54 通过、无 skip、正常
关机。proc-interrupts 在 4 CPU guest 检查列数和真实 LOC 增长。MSI/MSI-X
目前只验证统一向量计数路径的主机注入，**没有真机 MSI 验收**。

### B1.3：独立真实工具 payload 与第一轮发现

`--toolchain inspect` 独立 160 MiB 镜像；76 个精确版本签名 APK，实际工具
staging 约 17 MiB。保留默认 init、BusyBox、账户；真实程序在
`/opt/thekernel-tools/bin`，动态库/硬件 ID 数据按发行版路径安装。
测试脚本 `/opt/thekernel-tools/inspect-tools.sh` 逐个运行上述工具，有失败则
最终返回非零；htop 经真实 PTY 渲染并发送应用自己的 q 键退出，超时失败。
该脚本目前用于诊断，**不是已通过的 B1 验收**。

第一轮 QEMU `shell-kmiusllq`：ps/free/uptime/pidstat 退出 0；ps START 显示
1970（proc/stat 缺 btime），pmap 退出 0 但没有映射明细，lsblk 退出 0 但
没有磁盘行；这些输出不能称合理。top 返回 1，vmstat 报无法创建 vmstat
结构；htop 返回 2（PTY smoke 保持失败）。findmnt 触发同任务递归 Mutex
获取 panic。后续程序尚未运行，不能把它们记成成功/失败。

已定位可疑来源：with_path_fs 持有当前 fs_struct mutex 做 VFS lookup，
而 /proc/self/mountinfo lookup 要再取目标 task 的 fs->root。须以回归实际
验证后才能确认修复；该问题不是格式节点注册的许可或硬件问题。

工具 payload 本身验证：签名解包/版本闭包验证和重复 staging 成功；65 个
相关 Python 测试（3 个既有环境 skip）通过，PTY runner 在主机 htop 实际
渲染/正常退出，product lint 通过；最终默认 guest 54/54、无 skip、正常关机。
这只证明打包与基线没有回归；第一轮真实工具审计的 panic/失败仍然存在，
没有宣称全工具通过。

路径查找修复候选后实际审计 `shell-2cc4yv3e` 完成遍历，无 panic；findmnt
和 mount 返回 0，并显示真实的根/伪文件系统挂载树。剩余 14 项失败：
`top/vmstat/htop/df/lsns/lsusb/netstat`，以及 net 下除 dev 外的七个缺失文件。
htop 明确报 `No btime in /proc/stat`。df 为本次工具 wrapper 的 basename
错误（alpine-busybox 没进入 BusyBox 的 multi-call dispatch），不是内核
缺陷；需独立修复。lspci 三种调用返回 0 但仍报 config/class/irq/resource
缺失、class ffff；lsblk 没有磁盘行、lscpu 核心/插槽数为 0，这些均未验收。
`iostat/mpstat/ip/ss/net-dev` 返回 0，仍需检查活跃设备/连接数值而不是仅看
退出码。B1 未完成；B2 尚未开始，不能把工具打包当成容器支持。

### findmnt 的路径查找重入修复

Linux 7.2.3 fs/namei.c 的 path_init/get_fs_root 使用保留的 fs->root 视图，
不会持有 fs_struct 锁跨越整个 provider 查找。TheKernel 的 with_path_fs
原先把 live FsContext mutex 保持到回调返回；proc self mountinfo/mounts
在 lookup 时保存目标进程 root，从而重新获取同一锁并 panic。

修复为先 clone FsContext（保留 root/cwd/umask 的一致视图），释放 live
锁后再做路径回调。真正修改进程 fs_struct 的 with_fs 路径仍保持原 writer
锁，不能借此把 chdir/chroot/umask 改成修改临时副本。26 个直接/间接使用
这一路径快照的 syscall contract 并发说明同步更新，状态/进度计数不变。

主机新增回归要求 provider 回调中可重取 live fs_struct，绝对路径忽略非法
目录 fd；完整 kernel 2581 测试通过。proc-path-lookup 在 Linux 主机和
TheKernel guest 都通过：绝对/dirfd 相对打开 proc self mountinfo/mounts、
stat，以及 readlink root/cwd。最终完整默认 KVM guest 55/55，无 skip 正常
关机（system-7179numk）。独立 Alpine findmnt/mount 的真实挂载树已验证；
全 ABI 差分仍在构建/运行时不记通过。Bison 数据目录问题已实测修正后重跑。

工具 df 的 wrapper basename 修复为独立提交 800182ab；真实 Alpine guest
已显示 /dev/vda 和三个 tmpfs 的容量，正常关机（shell-k7ewh3x4）。

ABI 验证结果：最初 Linux oracle 的 CONFIG_HZ=100，使既有 socket-provider
测试硬编码的 2000us 超时回读断言失败；这个失败发生在 Linux 对照程序，
不是 TheKernel 路径查找修复。测试改为先用 1us 请求观测真实量化单位，再
验证 1500us 请求向上整倍数量化；未改两个内核的时钟或 socket 实现，也没
改成接受任意正数。独立重跑完整差分 **257/257 contracts 在两个 guest
通过**（abi-2wdn18zu）。该测试修复与内核锁修复分开提交。

### B1：真实 btime

`/proc/stat` 新增整数秒 btime，来自平台 RTC epoch 与已有 realtime offset
的同一发布协议，而不是固定日期或两次前进时钟相减。按照 Linux 7.2.3
对读者 time namespace 的 boottime offset 反向平移。主机格式回归通过；
完整 kernel 2582 测试、product lint、完整 KVM guest 56/56 均通过，无 skip
正常关机（system-1tq88tjv）；新 guest 回归检查 btime+uptime 重构实际 realtime。
Linux 主机同一回归也通过。**time namespace 偏移在本项只核对源码规则，
未额外 guest 验收；不把它写成已测通过。**

真实 Alpine ps 的 STARTED 不再是 1970，显示此次 guest 的实际启动日期
（shell-6riw98rp）。top 仍失败，不能称跑通；htop 已越过缺 btime 的错误，
现在明确报无法初始化终端类型 vt100，需核对 payload 的 terminfo 数据路径。

terminfo 打包修复：Alpine ncurses-terminfo-base 把数据装在 `/etc/terminfo`，
并非 `/usr/share/terminfo`。独立 payload 增加这个已签名数据目录，不复制
发行版账户/系统配置。真实 htop 在 guest PTY 内完成渲染并按 q 正常退出
（shell-tezgq9hr，PTY_TOOL_OK）；这证明基本交互启动，不代表所有字段/面板
都已核对。最新完整 host Python655（3 环境 skip）+Rust5997、两个平台 lint、
KVM default guest56/56 均通过，正常关机（system-pyjsqlum）。top/vmstat 仍待修。

### 基础 vmstat：已测量字段，未测量字段不伪造

新增 allocator 实际 free/VirtMem/PageCache/PageTable 页数、CPU-local 累计
成功 minor/major fault 事件；已有后台回收 worker 的 scanned/reclaimed 页
事件映射到 pgscan_kswapd/pgsteal_kswapd。PSWP/CMA 确实没有对应机制，字段
为 0。没有把累计扫描遇到的 dirty/writeback/pinned 数冒充当前 gauge。
未跟踪的 LRU/dirty/writeback/分页 I/O 等字段暂未发布；**统计覆盖仍部分**。
当前 pgfault/pgmajfault 的来源是 TheKernel 的成功 fault 分类边界，Linux
PGFAULT 也计部分失败的 MM fault、PGMAJFAULT 有失败 I/O 的计数边界；这种
错误完成路径尚未对齐，不能声称负向事件统计已经完整。

主机两个新计数/格式回归以及完整 kernel 2584 通过，lint 通过；最新完整
KVM guest57/57 无 skip 正常关机（system-bxuaoq86）。新 probe 在 fork 后
实际写私有页制造 COW，证明累计 fault 增长，而不假定 mmap 初始写都 lazy。
Linux 主机同一 probe 通过；nr_anon 是 gauge，Linux 会批量折叠且主机有其它
进程，不能要求一次全局快照净增恰好 32。第一次探针的这个错误假设已修正
并完整重跑，不将那次失败记成通过。

真实 Alpine vmstat -s、vmstat 1 2、top -b -n 1 均已启动并输出
（shell-xa3o8qek）。top 不再直接退出，但 VIRT/RES/SHR 都为 0（缺 PID statm），
vmstat 的 context switches 等未接入字段仍为 0；**不是最终完整工具验收**。

### 主程序 ELF 元数据与 statm

statm 首轮回归抓到原有 exec 元数据问题：主程序 text=392194 页却只有
size=2318 页（shell-9swbofm9）。原 reset_mm_layout_for_exec 扫描所有 EXEC
VMA，把解释器/远处 trampoline 纳入跨度。现在从已验证的 **main ELF**
PT_LOAD 保留 initialized code/data 范围，移除这个全 VMA 近似。
Linux 7.2.3 binfmt_elf.c 的范围规则只作为格式/事实参考，Rust 为原创。
execve/execveat 两项 contract 状态描述同步更新，进度计数不变。

新回归从 /proc/self/exe 读取自身 ELF（含 ET_DYN 的 AT_ENTRY bias），直接
对照 /proc/self/stat 的 start/end code/data；Linux 主机、TheKernel guest
均通过。完整 kernel2587、lint、最终 KVM guest59/59 无 skip 正常关机
（system-vrz73etb）、全 ABI257/257（abi-41enjrnd）通过。

statm 七个 4KiB 页字段来自实际 VMA/PTE、main ELF 元数据、现有 data_vm 和
实际 growdown VMA 策略；legacy lib/dirty 字段按 Linux 固定为 0。私有 COW
页不是 file/shmem RSS，不因 fork 共享物理页就虚增第三字段。它是公开汇总
信息，不套用 maps/smaps 的 ptrace 权限；guest 专门验证降 UID 的子进程仍
可读父进程 statm。实际 top VIRT/RES 不再为 0，ps 显示正常启动时间/内存
（shell-yq44cxtn）；完整其他工具/设备/连接视图仍未完成。

第十二提交周期：最新 full host Python655（3 环境 skip）+Rust6002、q35/n305
lint（784 条既有 kernel 警告）、KVM guest59/59、ABI257/257 均通过；正常
关机，真实 top/ps 输出已核对。设备树/连接表/zone/buddy 和负向 fault 统计
仍待做，不能据此声称 B1 或容器支持全部完成。

### Scheduler totals

`/proc/stat` now publishes `ctxt`, `processes`, and `procs_running` from actual
scheduler transitions, successful runnable-task publication, and lock-free
ready/running non-idle snapshots. Per-CPU switch counters are cacheline isolated;
no-op yields, failed reservations, and queue-only migrations are not counted as
new switches/tasks. Initial unpublished boot/idle tasks are outside the
publication counter. `procs_blocked` is still absent: Linux counts I/O wait, not
all uninterruptible tasks; the existing D-state counter cannot substitute for it.

Host tk-axtask139 and kernel2588 tests, lint, and full KVM guest60/60 passed
(system-o6f3be00), with no guest skip and normal shutdown. The fork/pipe probe
observed increased publication/switch totals. Real Alpine `vmstat 1 2` now reports
nonzero context-switch rates and real runnable snapshots (shell-70ia2vsi);
its missing blocked-I/O accounting remains unaccepted.

### Registered block geometry and GPT topology

The block registry now retains parent/partition-number/start metadata from the
validated GPT parser. Sysfs exposes `dev`, actual registry `ro`, `removable=0`
(the registered fixed disks have no removable-media state machine), Linux
512-byte-sector `size`/partition `start`, partition `partition`/`uevent`, and
`subsystem`. Whole disks contain their partition directories; `/sys/class/block`
and `/sys/dev/block` resolve to that same tree, using the established devfs IDs.
`/proc/partitions` reports actual nonzero capacities in KiB, including attached
loop devices. Existing queue logical-block-size/DMA-alignment remain unchanged;
other queue fields and `stat`/`diskstats` are **not yet implemented**.

Driver49, axfs230, kernel2590 host tests, product lint, and full KVM guest61/61
passed (system-1j2wnemi), with no guest skip and normal shutdown. The guest probe
compares sysfs and proc capacities/read-only state against real block ioctls and
checks both aliases. Real Alpine `lsblk` now shows the 160MiB read-only boot disk
mounted at `/`, rather than an empty inventory (shell-3xbtpov0). That guest has
no NVMe GPT disk: the GPT parent/offset tests cover the host partition view and
formatters, **not a real guest partition tree or physical NVMe**.

Optional NVMe GPT acceptance now runs non-interactively using
`tests/guest/block-gpt-tools.sh` in the inspect payload. A disposable 16MiB image
under the state directory contains an 8192-sector partition at LBA2048. Actual
Alpine lsblk reports the 16MiB parent and 4MiB child, including PKNAME and nested
JSON children; sysfs class/dev aliases agree. The probe attempts a write only to
that disposable QEMU partition, with the default `nvme.allow_write` still off:
it must fail and the original sector must remain unchanged. Both success markers
were observed (shell-vkfdw3ma), followed by normal shutdown. This is QEMU NVMe
and GPT-tree acceptance, not physical NVMe evidence.

The first probe incorrectly expected O_RDWR open itself to fail. Linux 7.2.3
normal blkdev_open allows it; the stricter RO check in bdev_file_open_by_path is
for an internal kernel helper, not that userspace path. The probe now checks the
actual forbidden write. TheKernel's established RO write errno is EROFS, whereas
Linux blkdev_write_iter returns EPERM; this acceptance tolerates either only to
prove rejection, **not to claim errno ABI equivalence**.

Fifteenth-commit cycle: full host Python655 (3 environmental skips) and Rust6006,
q35/n305 lint (784 existing kernel warnings), latest-source default KVM guest61/61
(system-mjc03spd) passed; no guest skips and normal shutdown. Inspect-payload
build-input regression also passed (5 focused Python tests).

Removability correction: the USB BOT driver has not retained SCSI INQUIRY RMB,
so it cannot truthfully report fixed media. Block geometry now retains an
optional driver fact: fixed loop/boot-module/VirtIO/NVMe media report 0, while
USB/dynamic-driver unknown media return EOPNOTSUPP rather than a fabricated 0.
No additional USB commands or hardware configuration writes were introduced.
USB media-removability acceptance remains pending.

Correction validation: driver49/axfs230/kernel2591 host tests, lint, and full
KVM guest61/61 passed (system-n0ghiks9), with no guest skips and normal shutdown.
Host formatter tests distinguish true/false/unknown; the guest still verifies
actual fixed boot-media geometry/read-only state. USB RMB is unverified.

### PCI identity and read-only binary configuration

PCI boot inventory follows the existing bounded reachable-bus walk and uses the
already mapped firmware-selected ECAM segment/range. Unowned functions, including
bridges, are added to the same device registry as the existing DRM/input parents;
those parents receive the same live vendor/device/class/subsystem/revision fields.
No competing `/sys/bus/pci` tree shadows display/input descendants. NUMA affinity
has not been discovered, so `numa_node` reports Linux's unknown value -1.

`config` is a real read-only binary node, not a generated full-buffer text file:
reads touch only the admitted interval, using aligned byte/halfword/dword loads.
Its observed length is 256 or 4096 bytes after capability/reachability/alias
checks. Metadata length is independent of read permissions. The immutable opener
credential must have CAP_SYS_ADMIN in the initial user namespace for full reads;
otherwise access ends at byte64 (CardBus byte128). Writes/append/truncation never
modify configuration space. No new BAR probes, command changes, or driver resets
are introduced by these observations.

Generic publication is boot-only: later sysfs mounts cannot claim a newly
arrived input function ahead of the established input reconcile owner. Existing
input removal/publication remains owned by that subsystem. General PCI hotplug,
canonical bridge-parent paths, and registry exhaustion beyond its existing
64-object capacity are not accepted here. IRQ/resource/driver/enable attributes
remain pending, so this is not full `lspci -vvv/-k` acceptance yet.

PCI identity/config validation: driver52 and kernel2594 host tests, lint, and
latest KVM guest62/62 passed (system-dfoke4e8), with no guest skips and normal
shutdown. The guest observed 13 functions/3 bridges, compared config bytes with
identity/class files, checked full fstat length, rejected a config write, and
verified inherited privileged descriptors versus newly opened unprivileged
64-byte descriptors. Host tests prove requested interval/width bounds, malformed
capability-cycle termination, fixed metadata length, and formatter widths.

Actual Alpine lspci now identifies host/SATA/network/display/input functions and
renders all three downstream buses (shell-ay4nlznj). Config/class failures and
bogus ffff class output are gone. `-vvv` still reports missing IRQ/resource files;
`-k` has no real driver binding links yet. Those are the next PCI sub-item, not
accepted via the programs' exit statuses. Physical PCI/ECAM behavior is untested.

PCI `irq` now follows the Linux MSI-versus-MSI-X selection rule: enabled MSI
reports its actual primary message vector; MSI-X retains legacy INTx. Valid
INTx firmware lines use the established TheKernel x86 GSI+0x20 vector namespace;
no valid INTx reports 0. These are local CPU-vector identifiers, not Linux's
dynamic logical IRQ numbers. Invalid/reserved enabled-MSI vectors are rejected,
not fabricated. Host cases cover 32/64-bit MSI, disabled MSI, MSI-X and no route.

Eighteenth-commit cycle: full host Python655 (3 environmental skips)+Rust6014,
q35/n305 lint (784 existing kernel warnings), and KVM guest62/62 passed
(system-qa8vmi60), with no guest skips and normal shutdown. The guest checks
13 actual firmware INTx routes. Real lspci -vvv now shows valid IRQ vectors and
no missing-irq errors (shell-3j95o27s); missing resources remain unaccepted.

Resource acquisition gap: the existing startup code sizes ordinary endpoint
BARs but does not retain the results; it does not size bridge BARs or expansion
ROMs, and deliberately preserves the firmware GPU. Root-port BAR0 and the NIC
ROM have real nonzero addresses in QEMU config. Their exact sizes cannot be
derived from those addresses or MSI-X table offsets. Linux reads BAR masks by
writing config registers; adding that to a sysfs read, or guessing a size, is
not permitted. Capture existing probe results first; unknown firmware geometry
must remain explicitly unresolved unless a safe admitted acquisition path exists.

### Network namespace-scoped dev and IPv4 routes

`/proc/net` now has Linux's `self/net` symlink shape; `/proc/<pid>/net` and
`/proc/<pid>/task/<tid>/net` select the target task's current network namespace
when looking up a child file. Open files retain that namespace after unshare or
setns; an already opened process-net directory follows its target on a new child
lookup. The previous dev callback incorrectly used the reader's current
namespace each time. Its sole formatter now lives in the new proc_net module.

`route` uses the actual RouteInfo/InterfaceInfo snapshots: normalized IPv4
prefix/mask in x86 native-word hex, gateway, and UP/GATEWAY/HOST flags. TheKernel
has one routing table and no per-route priority/advmss/window/rtt metrics; these
and Linux's legacy RefCnt/Use fields are 0. IPv6 routes are excluded; an IPv6
next hop has no IPv4 gateway number, as in Linux. A concurrently removed
interface's route is omitted, not relabelled via a reused storage position.

Host kernel2596 tests, lint, latest KVM guest63/63 (system-ljmsqmen), and full
ABI257/257 (abi-kmni5ouv) passed; no guest skips and normal shutdown. Guest
checks route grammar plus namespace unshare/setns, old-file pinning, directory
follow behavior, and reading the parent's namespace from a child in another
namespace. Contract state descriptions for unshare/setns were updated; progress
counts are unchanged. ABI coverage remains the existing suite, not a paired
Linux execution of the new proc-net smoke test.

Actual Alpine netstat -rn now displays the real loopback/default routes
(shell-pzl8yk85). `ip -4 route` independently fails to send its dump request;
that netlink failure is not masked by this procfs node. Host strace of the exact
signed Alpine ip binary shows a 156-byte send buffer containing a 36-byte
RTM_GETROUTE request followed by a zeroed tail; examine Linux NLMSG_OK termination
rather than weakening tool validation. Socket tables/SNMP and active ss/netstat
acceptance remain pending; current SOCK_DIAG retains identity but hardcodes
unbound addresses/closed stream state, so an empty successful ss is not proof.
