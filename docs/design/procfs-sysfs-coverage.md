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
| `ss -tanp` | SOCK_DIAG netlink，PID fd/链接/cmdline；回退 proc/net/tcp* | TCP基本SOCK_DIAG真实端点/队列/PID/inode已验；高级extensions未验；net/tcp* 缺失 |
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
事件映射到 pgscan_kswapd/pgsteal_kswapd。CMA 无对应池。最初把 PSWP 也假定
不存在而填0是错误，后续实际软件 swap 回归纠正此判断，见文末 swap I/O events。没有把累计扫描遇到的 dirty/writeback/pinned 数冒充当前 gauge。
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

### Netlink dump allocation padding

The exact signed Alpine ip sends 156 bytes for a 36-byte RTM_GETROUTE request,
followed by a zeroed allocation tail. Ordinary kernel netlink receiver framing
now follows Linux NLMSG_OK termination: the first incomplete/invalid next header
ends dispatch, including a short final alignment tail. Successful writes report
the submitted length, not the trimmed prefix. The special uevent envelope and
the current transactional nfnetlink preflight remain strict; no version-specific
iproute2 workaround or new mutation fallback is introduced.

The same padded C request succeeds on host Linux (read-only dump) and guest;
host kernel2599, lint, KVM guest64/64 (system-ngy3ngg8), and full ABI257/257
(abi-9xqnyodt) passed, with no guest skips and normal shutdown. Two old length-
admission tests incorrectly assumed a zero header must later fail EINVAL; their
EMSGSIZE/import-once assertions remain, while the verified parsing result is now
an ignored message and successful byte count. Write/writev/sendto/sendmsg state
notes were updated without changing progress counts.

Real ip -4 route now receives/prints actual rows (shell-jh5z8xjc), but then reports
`DONE truncated`/`Dump terminated`: the preexisting multipart completion has only
a header, missing Linux's signed 32-bit completion status. This is a distinct
next wire-format fix. Do not mark the real ip command fully accepted yet.

### Multipart completion status

NLMSG_DONE now includes the successful signed 32-bit status after its header,
with the original sequence/port identity and MULTI flag. Host wire test and the
same C dump probe on Linux/guest require that payload, not merely the message
type. Real Alpine ip -4 route now completes without `DONE truncated` or
`Dump terminated` (shell-ar6wexmo); netstat -rn also displays the actual routes.
The netlink loopback destination still carries host bits (127.0.0.1/8) whereas
proc route is normalized (127.0.0.0/8); fix that independent existing formatter
before claiming every route field is correct.

Twenty-first cycle: full host Python655 (3 environmental skips)+Rust6020,
q35/n305 lint (784 existing kernel warnings), KVM guest64/64 (system-p838_dz4),
and full ABI257/257 (abi-ej44bbkj) passed, with no guest skips and normal shutdown.
Read/readv/recvfrom/recvmsg state descriptions now mention the status payload;
progress counts remain unchanged. No physical acceptance or performance claim.

Route dump destination prefixes now exclude configured host bits for both IP
families; this only fixes serialization, not routing state or lookup. Host tests
cover IPv4, non-byte IPv6 prefixes, and /0,/128 boundaries. The guest parses
actual RTA_DST values and rejects host bits. Kernel2601, lint, guest64/64
(system-slewxdie), and ABI257/257 (abi-hjsd4mfs) passed with normal shutdown and
no guest skips. Real ip -4 route and netstat -rn now agree on 127.0.0.0/8 and the
actual default gateway (shell-rzzrswu8). Socket tables/real ss snapshots remain
unimplemented; this is route-view acceptance only.

### Live TCP diagnostic base records

The SOCK_DIAG provider now observes actual TCP sockets rather than emitting a
canonical CLOSED/unbound placeholder for every registered inet OFD. The socket
retains a lifetime token; the registry and token hold weak references only.
Registration is attached after Arc publication, including accepted inet sockets.
The dump copies namespace-local tokens, releases the registry, then observes
transports under socket-set -> listener-entry ordering. It does not poll,
acknowledge, or consume queues. NOWAIT uses try-locks for registry, owner,
UID-map and transport observations before any reply enqueue. A concurrent wrapper
transition is omitted by a blocking dump, or reports WouldBlock to NOWAIT.

Linux 7.2.3 inet_diag/tcp_diag and tcp_states are the behavioral references:
state masks use `1 << state` (zero selects nothing). Bound inactive sockets are
selected by pseudo-state 13 but emit TCP_CLOSE=7; anonymous unbound TCP_CLOSE
OFDs are not enumerated. IPv4/IPv6 endpoint bytes and network-order ports, actual
SO_BINDTODEVICE index, TCP state, receive/send queue bytes, listener completed
accept entries/backlog limit, UID mapped through the requesting user namespace,
and actual socket pseudo-inode are serialized in the 72-byte base record.
The lifetime cookie is distinct from the inode; a dump does not use request
cookie/address/interface fields as exact-lookup filters. Queue capacity is the existing
transport's admitted bound, not a fabricated Linux default.

Partial: this does not cover orphaned/TIME-WAIT endpoints after final OFD close,
SYN-queue child records, TCP diagnostic bytecode, exact non-dump lookup (both explicitly rejected), timer/
retransmission base fields or optional extensions. At item23 timer/retransmission fields were still zero/unaccepted; the later
TCP control-timer observation section replaces those placeholders with actual
provider observations. UDP/raw/DCCP/SCTP dumps now reject EOPNOTSUPP rather than publish
fake endpoints. /proc socket tables and SNMP remain pending. No physical network
or performance acceptance is claimed.

The same socket-diag C regression passes on host Linux: IPv4/IPv6 LISTEN and
ESTABLISHED, bound-inactive selection, adjacent/zero state masks, actual fstat
inode/owner, pending accept count, unread byte count and nonconsumption, queue
drain and closed-listener retirement. It also exercises a UID1000 creator when
running as root. Guest KTAP and signed Alpine ss -tanpe acceptance are pending;
these are not marked passed merely because host formatting tests passed.

Final TCP base-record validation: net crate213 (one preexisting ignored test),
kernel2603, lint (784 existing warnings), KVM guest65/65 without skips and normal
shutdown (system-ss1gvq3z), and full ABI257/257 (abi-szushf_d) passed. The accepted
connection regression caught and repaired a new snapshot error: use the real
connection tuple before optional bind-admission metadata, which is empty for
accepted children. The actual Alpine ss also caught an old dump-cookie filter:
it submits zero cookies, and Linux does not use them for dump exact selection.
The C probe now tests zero and arbitrary dump cookies on both Linux and guest.

Actual signed Alpine ss -tanpe displays IPv4/IPv6 LISTEN and ESTAB, the server's
16 unread bytes, admitted listener backlog4, and the parent's actual PID/fd and
inode (shell-k0mycr4k, DIAG_TOOLS_RC=0). This is active TCP base-record acceptance,
not acceptance of missing proc TCP/UDP tables, unsupported diagnostic transports,
or timers/advanced filters/orphan states. Contract progress counts are unchanged.

### Common network counter source

Linux 7.2.3 net/core/net-sysfs.c, fs/sysfs/mount.c and the if_link UAPI are
behavior/layout references. New /sys/class/net/<if>/statistics/ core counters
retain the sysfs mount's network namespace; ordinary readers changing netns
cannot silently change that filesystem's view. Boot uses the already-registered
init-net because sysfs is mounted before the init userspace Thread exists.
Interface names are dynamically enumerated, and opened attributes retain the
stable ifindex so removal/name reuse cannot relabel another device's counters.
A newly created legacy sysfs mount captures its caller's network namespace.
Modern fsopen context-time namespace capture is not newly accepted here.

Eight decimal/newline counters come from the actual DeviceStats under the
existing service lock (RX/TX bytes, packets, errors, drops); no poll/receive or
hardware configuration occurs on read. The same stable-ifindex observation
supplies RTM_NEWLINK's Linux-shaped IFLA_STATS and IFLA_STATS64. Wide counters
are preserved; narrow counters use the Linux low-word conversion. Detailed
error/multicast/collision/compression/nohandler/otherhost classifications are
not maintained by this device provider and their wire fields remain0, as for
an unimplemented optional driver counter; they are not newly accepted real
hardware measurements. Only the eight observed counters are exposed in sysfs.

The old /proc/net/dev device-name snapshot used the immutable transport name
rather than the router's renamed link; it now uses the control-plane name.
Canonical sysfs device ancestry, device/subsystem symlinks, uevent publication,
and other netdev configuration attributes remain incomplete; these class
counter directories are not a claim of complete Linux netdev sysfs topology.

Host Linux C probe passes: active loopback UDP traffic advances actual packet/
byte counters and sysfs/proc/rtnetlink agree, including narrow/wide conversion.
Final period24 validation passed: Python655 (three environmental skips),
Rust6029 (one preexisting ignored test; net214 and kernel2605), q35/n305 lint
(784 existing kernel warnings), KVM guest66/66 with no skips/normal shutdown
(system-k5c6vjnm), and full ABI257/257 (abi-cvcea69d). Guest verifies old sysfs
retains its mount namespace after NEWNET, a newly mounted sysfs sees the new
namespace, and actual configured-new-loopback traffic advances only the new
view. Linux correctly creates lo down/unconfigured in NEWNET; an initial test
omitted that setup and failed EADDRNOTAVAIL, then was repaired with explicit
in-guest netlink address/UP setup rather than changing kernel defaults.

Signed Alpine iproute2 ip -s link and BusyBox ifconfig both display the actual
five packets/2700 bytes per direction and agree with sysfs/proc/netlink
(shell-zi60qhmz, NET_STATS_TOOLS_RC=0). The exact Alpine BusyBox1.37 ip applet
rejects -s on host Linux too (usage, exit1); it lacks that option, so BusyBox
ip -s is not accepted or worked around with a wrapper. An initial shell probe
correctly failed on this tool limitation rather than accepting runner shutdown
as program success. Host stable-ifindex tests also cover actual traffic,
control-plane rename and removal/name reuse without counter aliasing. No
physical network or concurrent-load performance acceptance is claimed.

### Per-VMA swap observations and pmap mapping rows

Actual Alpine pmap -x exits0 but emits only totals (shell-e58g_u3x), despite
valid mapping headers and real Rss. The procps-ng4.0.6 upstream pmap.c consumer
prints its mapping row when it sees the numeric Swap field; missing that field
suppresses every row. Behavioral consumer source:
[procps-ng4.0.6 pmap source](https://gitlab.com/procps-ng/procps/-/raw/v4.0.6/src/pmap.c). Linux7.2.3
fs/proc/task_mmu.c supplies the field grammar/order, not implementation code.

Add Swap from the mm-owned software-PTE BTreeMap within each page-aligned VMA,
not by guessing that every hardware PTE hole is swapped. Empty software maps
produce real0; demand holes, PROT_NONE and unrelated VMAs cannot inflate it.
Keep MM snapshot locking separate from pathname/VFS resolution. Size/Rss/Swap/
Locked formatting is in proc_task_memory, rather than adding a new formatter
body to the shared proc builder. Dirty/private/shared/PSS accounting remains
incomplete; displaying mapping rows does not make pmap's Dirty column verified.

The same base C parser passes host Linux on an actually touched anonymous VMA.
Guest-only --swap creates an explicit temporary RAM-filesystem fixture, activates
it, exercises MADV_PAGEOUT/page-in with content checks, and removes it; it never
runs swapon on the host or touches any block device. Such a RAM-backed TheKernel
fixture is a software-PTE observation test, not Linux tmpfs swapon acceptance or
physical swap-I/O evidence. Kernel2607, lint (784 existing warnings), and
KVM guest67/67 with no skips/normal shutdown passed (system-wcabpb9i). The
RAM-backed guest fixture observes actual Swap0 ->64 ->0kB with corresponding
resident transitions, preserved page contents and successful swapoff/unlink.
Actual signed Alpine pmap -x now shows the held anonymous mapping as64kB/RSS64
and prints all mapping rows, not just totals (shell-dmf5effw, PMAP_TOOLS_RC=0).
Dirty/private/shared/PSS remain incomplete; only mapping/RSS/swap are accepted.
No syscall errno/admission behavior changed; the last full ABI257/257 and full
period host validation are from item24, not claimed as rerun for this item.

While verifying the source, found that the existing kernel does support software
swap leaves, contradicting the earlier vmstat formatter's assumed absence of
swap. Its pswpin/pswpout constant0 must be replaced by actual successful swap-I/O
counters in a follow-up; absence of an active swap device in the baseline guest
is not proof that these fields are always0. Do not claim those counters correct
under active swap until repaired and measured.


### File-swap I/O events

Replace vmstat pswpin/pswpout constant0 with actual regular-file swap events.
Linux7.2.3 mm/page_io.c counts PSWPOUT when file writes are submitted (including
later I/O failure), but PSWPIN only on full successful file-read completion.
TheKernel's existing swap engine uses regular files: increment at those matching
boundaries in swap.rs, after slot/entry admission and never for invalid input,
missing capacity or dead slot references. Read/short-I/O failure does not add a
page-in event; slot/VM ownership behavior is unchanged. Events are4KiB page
counts, not physical disk I/O, cache-miss claims or a new swap implementation.

Local counter/format tests cover full vs short completion and actual nonzero
wire values. Extend the existing host VFS swap test with admitted/rejected I/O
counter boundaries; guest RAM fixture must show real16-page output then input
increments. Final kernel2608 and lint (784 existing warnings) passed, including
the actual VFS host swap testcase's invalid/admitted/retired-slot boundaries.
Guest67/67 passes without skips and with normal shutdown (system-cw2cjjjo): the
RAM fixture reports16 submitted output pages and16 completed input pages, while
smaps observes0->64->0kB and contents survive. Signed Alpine vmstat -s displays
16 pages swapped in/out after fixture cleanup (shell-jzjy5qwh, SWAP_EVENTS_RC=0).
A host-test-only nested-module reference error was repaired and host tests rerun;
production event behavior was unchanged by that test repair. No host/device
swapon, physical I/O, or new errno/admission behavior is claimed.

Also found meminfo SwapTotal/Free and proc/swaps
still report empty/zero despite active swap; those real registry gauges need a
separate follow-up rather than silently claiming free/vmstat complete.


### Active swap capacity and inventory

Publish active regular-file swap registry rows and actual meminfo SwapTotal/
SwapFree; the formerly header-only/constant0 views hid usable swap. Maintain
used_slots at zero/nonzero reference transitions so each shared software slot
is counted once and proc reads do not scan every slot under the global swap
mutex. Snapshot locations/size/usage/priority under that lock, then resolve byte
paths after unlock. Preserve raw non-UTF8 names and escape space/tab/LF/backslash
as Linux7.2.3 mm/swapfile.c does; large path rows always separate filename/type.
Linux swap header pages are excluded from usable capacity.

Free/total availability excludes draining areas; table entries retain backing
files until withdrawal. TheKernel does not newly serialize table reads across
an entire swapoff using Linux's swapon_mutex, so transient draining observations
remain a concurrency difference, not a newly accepted exact Linux snapshot.
No new block-device swap support or hardware I/O is introduced. Slot retain/
release semantics are otherwise unchanged, including the existing internal
retain API; used_slots tracks whatever that API actually owns.

CommitLimit adds active usable swap bytes to the existing RAM-ratio estimate;
this also fixes the corresponding strict-overcommit admission threshold. It
does not claim complete Linux committed_as/HugeTLB-reservation accounting,
which remains limited by the existing model. That admission change requires
full ABI, in addition to period27 host/guest/q35+n305 lint.

Extend the guest RAM fixture to check capacity2044kB (511 usable4KiB slots),
used64kB after pageout, restored free slots/page-in, inventory priority/type and
final disappearance on swapoff, plus CommitLimit capacity deltas. Signed Alpine
free is run while swap is active, not after cleanup. Host range/format/VFS slot-
refcount tests pass. Period27: Python655 (three environmental skips), Rust6033
(one existing ignored test; kernel2609), KVM guest67/67 without skips and with
normal shutdown (system-q2e2dbru), full ABI257/257 (abi-oi6ws1a7), and q35/n305
lint pass. A new redundant error-type conversion lint warning was removed;
that identity conversion cleanup does not change runtime behavior. Final q35/
n305 lint reports784 existing warnings and related kernel2609 tests pass after
that cleanup.

Guest verifies activation and cleanup add/remove2044kB in SwapTotal/Free and
CommitLimit, unique occupancy64kB after16-page pageout, inventory file/priority
and disappearance. Signed Alpine free -k while the fixture is active displays
Swap2044 total/64 used/1980 free (shell-j47n82gu, SWAP_CAPACITY_RC=0). Data and
16 in/out events survive; no host/device swapon or physical acceptance is
claimed. This is the existing TheKernel regular-file RAM fixture, not Linux
acceptance of tmpfs swap.

### Public per-mm VmSwap

PID/status VmSwap was still constant0 after active swap became observable in
smaps/meminfo. Linux7.2.3 fs/proc/array.c/task_mmu.c status reports mm swap-entry
occupancy; use the same mm-owned software registry under the existing status
memory snapshot lock. CLONE_VM shares one mm observation; fork duplicates its
PTE owners while the global swap area's unique occupied-slot count stays fixed.
Status aggregate access remains public under the current default procfs view;
do not add the maps/smaps ptrace gate or change reader credential mapping.

Extend the existing controlled guest fixture to compare VmSwap before/pageout/
page-in, let an UID1000 child read its root parent's aggregate status, and check
fork slot references do not multiply global usage. Host Linux base parser checks
the numeric status grammar but does not activate host swap. Kernel2609, lint
(784 existing warnings), and KVM guest67/67 without skips/normal shutdown pass
(system-a8dngfao). The controlled RAM fixture verifies VmSwap's actual64kB
increase and restoration, UID1000 access to its root parent's64kB aggregate,
and unchanged unique global slot usage across that fork. Actual signed Alpine
free remains correct (shell-j_rkmh6n, SWAP_CAPACITY_RC=0). No new errno/admission
behavior, host/device swap, or broader Dirty/PSS/status completeness claim;
full period/ABI validation remains the item27 run.

### TCP control-timer observations

Before publishing proc TCP tables, replace diagnostic timer/retransmission zeros
with real transport observations. smoltcp already has active retransmit,
keepalive, zero-window, TIME-WAIT and delayed-ACK timers. A read-only typed
observation samples those fields with the same clock the existing service uses;
it never polls, emits, consumes, acknowledges, or changes deadlines. Numeric
Linux7.2.3 inet_diag timer kinds0..5 and millisecond expiry are encoded at the
kernel boundary, not baked into transport behavior.

Add observation-only counters to actual protocol events: RTO expiry increments
retry timeouts (not fast retransmit), valid acknowledgement progress resets
them; accepted keepalive/zero-window probe emission increments probes, and valid
inbound progress clears probes. Reset clears both. This does not change send,
ACK, timer, congestion, socket-option or admission behavior. Linux's base wire
record selects retry vs probe counters according to timer kind; its byte field
is bounded when the native count exceeds255. Proc-oriented full counts and
RTO/ACK delay are retained for the next table formatter.

Linux7.2.3 net/ipv4/inet_diag.c, tcp_ipv4.c and UAPI inet_diag.h are behavioral
references. The previous diagnostic base timers are no longer accepted as
constant0 when active. Orphan/TIME-WAIT after final OFD close, SYN-queue records,
bytecode and optional extensions remain incomplete. Kernel/transport host tests
pass: smol631+7 doctests, net214 (one existing ignored test), kernel2609 and
lint784 existing warnings. Guest67/67 without skips/normal shutdown passes
(system-0oi5i21n), and full ABI257/257 passes (abi-q0edryik). The same C keepalive
probe on host Linux and guest requires actual timer2 and positive expiry; it
does not assume a Linux default interval for this transport.

Signed Alpine ss -tanpeo displays the live retransmit and keepalive deadlines
for both families (shell-2bq15nfl, DIAG_TOOLS_RC=0). This package labels Linux7.2.3
DELACK kind5 as unknown, but the numeric kind/expiry are correct; do not remap
that kernel UAPI to work around a tool-version label. An initial host assertion
was incorrectly placed after a subsequent keepalive emission: it now checks
reset immediately after ACK and count1 after the next emitted probe. Related
host/lint were rerun after that test-only repair; production behavior remained
unchanged. No physical network or real packet-loss acceptance is claimed.

### Live proc TCP consumer prefix

Publish tcp/tcp6 in the existing task-selected net directory; an opened file
pins that target network namespace and its opener's user-namespace credential.
Reuse the live readonly TCP provider, not a second endpoint registry or an empty
fallback. Preserve full64-bit socket inode for proc (inet_diag's UAPI remains
32-bit). Bound inactive TCP_CLOSE is not in proc TCP's listen/established walk.
Addresses use native32-bit word hex on x86_64, with the correct IPv4/IPv6 headers;
IPv4 records retain the Linux minimum line width. Listener proc transmit queue
is0, unlike inet_diag's listener backlog capacity. Proc ignores delayed-ACK
control timers, while inet_diag exposes Linux7.2.3 kind5; other active deadlines
are translated to USER_HZ100 units. Retry/probe/UID/inode/queues come from actual
observations, with no polling or queue consumption.

Linux7.2.3 tcp_ipv4.c/tcp_ipv6.c supply the mandatory consumer-prefix grammar.
Native sock-reference/pointer/congestion-tail observations are not yet exposed;
those trailing Linux per-state fields are omitted rather than fabricated0.
Thus this is not complete Linux7.2.3 row-field coverage. Orphan/TIME-WAIT after
final OFD close and SYN-queue child rows also remain incomplete. UDP/UDP6/UNIX/
SNMP are still missing and netstat -tunap is not yet accepted as a whole.

The same C probe on host Linux validates active IPv4/IPv6 endpoints, listener
and unread queues, addresses/UID/inode against inet_diag, closed retirement and
inactive omission. Guest-only namespace mode will check old-file namespace
pinning vs a fresh view and setns restoration; no host namespace is changed.
Period30 passes: Python655 (three environmental skips), Rust6036 (one existing
ignored test; kernel2611), KVM guest67/67 without skips/normal shutdown
(system-h4vj1n9c), full ABI257/257 (abi-jfcfyisi), q35/n305 lint. The new constant-
chunk style warning was repaired using typed4-byte chunks; related kernel2611
and both lints were rerun, leaving784 existing warnings with unchanged runtime
address conversion. Guest verifies old-file namespace pinning, fresh-new-net
view, setns restoration, inactive omission and live mandatory column values.

Actual signed Alpine netstat -tanp displays IPv4/IPv6 LISTEN/ESTABLISHED and
actual16-byte receive queues (shell-xjgff18k, DIAG_TOOLS_RC=0). Most PID/program
labels are shown, but that run's first IPv4 listener shows '-' despite ss finding
its PID/fd/inode; this separate ownership-label anomaly needs diagnosis and is
not declared passed. Netstat -tunap/UDP/UNIX/SNMP and complete trailing TCP rows
remain unaccepted; no physical network acceptance is claimed.

### B1 follow-up31: distinguish net-tools and BusyBox ownership labels

The inspect payload's `netstat` is the signed net-tools2.10-r3 standalone
program, not the BusyBox applet. Read-only diagnosis of fstat, lstat, readlink
and readdir succeeds for the first listener's legitimate `socket:[3]` link.
[Upstream net-tools v2.10 parser](https://github.com/ecki/net-tools/blob/v2.10/netstat.c)
rejects a one-digit inode because its socket-link minimum length assumes at
least two digits. This is a consumer restriction, not an observed descriptor
loss; no inode renumbering or patched tool is used to hide it.

The optional C `--tools` mode now runs both signed consumers and explicitly
requires the BusyBox LISTEN row's own PID/program. Actual KVM shell-t7c9u73i
passes DIAG_TOOLS_RC=0: BusyBox1.37 correctly labels inode3; net-tools retains
its '-' for that inode and labels the other endpoints. Both show real IPv4/IPv6
rows and unread queues. This corrects the earlier provider assumption without
claiming net-tools' one-digit-inode PID display passes. Host Linux C endpoint
probe, kernel2611 tests and q35 lint pass (784 existing warnings). This item
changes acceptance tests/documentation only; runtime behavior stays unchanged.

### B1 follow-up32: live UDP/UDP6 observation

UDP adds read-only diagnostic snapshots using cork → transition → socket-set
lock order, matching send/autobind. It does not poll, drain datagrams or take
pending asynchronous errors. Unbound endpoints are omitted; bound/unconnected
state7 and connected state1 use Linux state masks, not TCP's inactive bit13.
Namespace/open-credential UID mapping and real socket inode are reused. UDP
has no TCP timer/retry state, so the corresponding Linux columns are zero.
`/proc/net/udp{,6}` exposes the mandatory endpoint/queue/owner columns, and
inet_diag admits UDP dumps; exact lookup and bytecode remain unsupported.

Native UDP queue observation is occupied payload-ring bytes (including actual
ring wrap padding) plus corked transmit payload length. It is deliberately not
a guessed Linux skb truesize. Zero-length datagrams can therefore have zero
native payload queue bytes; no socket-object memory or skb overhead is invented.
Linux reference/pointer/drop tail fields are not yet supplied, despite preserving
the Linux header labels; full row coverage is not claimed. UNIX/SNMP remain
missing and full netstat -tunap remains unaccepted. Paired C probes cover bound/
connected masks, all unread datagrams, non-consuming repeated reads, real cork,
retirement and open-file namespace pinning. Validation results will be appended.

Follow-up32 validation: axnet216 tests (one existing ignore), kernel2614 tests,
KVM guest67/67 without skips/normal shutdown (system-bnr0ao5n), ABI257/257 on
both guests (abi-35ffdyy8), and actual signed Alpine ss/net-tools/BusyBox UDP
views (shell-qakyurkw, DIAG_TOOLS_RC=0) pass. IPv4/IPv6 show real19 occupied
payload bytes for the two queued datagrams and their owning PID. Cork and
post-observation receive content are asserted, not inferred from exit alone.

Review caught cross-protocol coupling before commit: the initial all-transport
snapshot could wait for unrelated UDP cork locks during a TCP query. Proc files
now select their protocol before endpoint observation; netlink preflights the
supported dump protocols in the whole valid datagram prefix before any reply
is queued. Host regression holds a UDP owner lock: TCP NOWAIT succeeds while
UDP NOWAIT correctly returns WouldBlock. Mixed/padded/invalid request selection
is covered. The runtime/ABI/tool runs above include this fix. A later unused
argument deletion and UDP-header helper extraction are behavior-equivalent;
related kernel/formatter tests and lint are rerun after those cleanups. UDP's
proc slot is the native registry iteration index, not a claim of Linux hash
bucket identity. Full UDP tail and UNIX/SNMP coverage remain incomplete.

Final equivalent-cleanup checks: kernel2615 tests, including UDP header/row
formatter coverage, and q35 lint pass; unused argument removal restores784
existing warnings. No runtime protocol behavior changed in this cleanup.

### B1 follow-up33: namespace relationship queries blocking lsns (pending)

Fresh actual signed-tool sweep after32 completes (shell-__l_zry9) with four
nonzero probes: lsns, lsusb, net-unix and net-snmp. The sweep remains diagnostic,
not whole-B1 acceptance. iostat still has no disk rows; its exit0 is not storage
statistics acceptance. The aggregate CPU field interpretation differs between
its output and mpstat and must be checked separately, not dismissed as timing.

Actual LSNS_DEBUG=all reports get_ns_inos rc=-22 on PID1 (shell-_y9g3fok).
[Upstream util-linux2.42.3 lsns](https://github.com/util-linux/util-linux/blob/v2.42.3/sys-utils/lsns.c)
accepts EPERM for an inaccessible user parent but aborts on EINVAL. Linux7.2.3
fs/nsfs.c and kernel/user_namespace.c use the same ancestry rule for user-parent
and namespace-owner queries. Initial user namespaces have no parent; a caller
cannot see an owner outside its own user-namespace subtree. No extra SYS_ADMIN
gate is involved for this owner traversal. The native user-parent ioctl was
unconditionally EINVAL and the owner ioctl lacked the ancestry check; both are
now routed through an original, pointer-identity ancestry helper before any
namespace descriptor is created. Nonhierarchical parent EINVAL remains intact.
This does not implement new mount namespaces or claim complete nsfs metadata.

The C probe's base relationship/errno cases pass on host Linux without changing
host namespaces. Guest-only fixture will create a user namespace, verify caller
scope against inherited old namespace FDs, check an ancestor's returned parent
identity, and drop SYS_ADMIN in an isolated worker to prove no extra gate.
Guest/tool/full-period33 validation is still pending.

Initial actual guest fixture succeeds (shell-y4aimmk3, NS_REL_TOOLS_RC=0):
ancestor/child identity, inherited-FD denial and no-extra-SYS_ADMIN checks pass.
lsns now returns0 and lists the eight initial namespace types plus the live
child user namespace, but emits three Unsupported ioctl NS_GET_NSTYPE warnings.
These are not declared whole-lsns acceptance: the existing namespace files share
procfs's device ID with ordinary proc files, so the tool misclassifies proc FDs
as namespace FDs. Device/fs identity remains a separate next correction.
Linux7.2.3 open_namespace also requires CLOEXEC; the existing related-FD helper
used false. Corrected to true and added paired C descriptor-flag assertions;
full-period33/runtime/ABI validation must be run after that change.

Final period33 passes: Python655 (three environmental skips), Rust6043 (one
existing ignore; kernel2616), q35/n305 lint (784 existing warnings), KVM guest
68/68 without skips/normal shutdown (system-rq5zr3br), ABI257/257 on both guests
(abi-3pwcs2ib), and actual relationship/CLOEXEC/child-scope/no-extra-SYS_ADMIN
fixture (shell-ue926kv_, NS_REL_TOOLS_RC=0). Actual lsns lists correct namespace
rows but still emits the three procfs-device-misclassification warnings, so
no-error whole-tool acceptance remains pending. No host namespace or physical
hardware configuration was changed. Contract progress counters stay unchanged.

### B1 follow-up34: actual namespace filesystem identity and clean lsns

See [nsfs design](nsfs.md). Proc namespace source nodes are now actual dynamic
magic symlinks, while followed/opened targets belong to one real private nsfs
filesystem with a VFS-allocated device identity. No fake NSTYPE is returned for
ordinary proc files and no guessed device number is substituted. READ_FSCREDS
image checks are reused before observing the live target; an opened target pins
its namespace. Generic pathwalk jumps to an actual Location, preserving magic/
symlink and cross-mount policy. Namespace readlink observes one complete label.

Actual signed Alpine lsns now lists all eight initial types and a live child
user namespace **without diagnostics** (shell-lq89w57m, NS_REL_TOOLS_RC=0).
The same fixture asserts inode/device/statfs/mode/source-label/truncation,
openat2 no-magic/no-symlink/no-cross-device errors, a retained dynamic O_PATH
source across unshare, an opened target surviving creator exit, parent/owner
scope, no extra SYS_ADMIN gate and CLOEXEC. Host Linux base passes without host
namespace changes. Related axfs177/VFS27/kernel2618 tests, q35 lint, KVM guest
68/68 (system-1oroyxtd) and ABI257/257 (abi-zshngyku) pass. Initial failed probes
and their repairs are documented in nsfs.md; no failures were counted as passes.

This is basic lsns/namespace-file acceptance, not complete nsfs ioctl/export/
inode-attribute behavior or container acceptance. Broader filesystem UID user-
namespace projection is still incomplete. mount-tree namespace construction
is still absent; its contracts now acknowledge that the nsfs descriptor provider
exists rather than repeating the earlier missing-provider statement.

### B1 follow-up35: usbutils hardware-name database payload

The signed usbutils019 lsusb is a standalone ELF, using eudev's compiled hardware
name database rather than the usb.ids text alone. [Upstream names_init](https://github.com/gregkh/usbutils/blob/v019/names.c) creates
udev_hwdb; read-only host strace of the same signed executable confirms an open
of host /etc/udev/hwdb.bin. The guest payload lacked that file, explaining its
initial hardware-name initialization diagnostic without proving a kernel USB
enumeration fault.

The exact signed closure now has81 packages, adding eudev/eudev-hwids and their
three new dependencies (five packages total). APK scripts/ownership changes remain disabled.
After exact closure validation, the pinned staging udevadm performs only offline
hwdb compilation with --root STAGING; input directories and output file remain
under that isolated root. No udev daemon, trigger/control operation or host
configuration change is involved. Copy only the generated database to
payload/etc/udev/hwdb.bin. Source data and licensing notices remain in the
runtime/data trees. The inspector includes a noninteractive binary-header check.

Two fresh signed builds pass closure validation and database generation. Six
related Python tests and q35 lint pass (784 existing warnings). Actual KVM
shell-ft284xb7 sees the9.3MiB database; lsusb no longer emits the name database
initialization diagnostic. **Full USB acceptance still fails**: default lsusb
returns1 with no devices, and lsusb -t explicitly reports missing
/sys/bus/usb/devices. These results do not assert real USB inventory or physical
support. No kernel runtime changed in this packaging item.

Final follow-up35 checks after adding the noninteractive header probe: six
Python tests and q35 lint pass; actual KVM shell-0wjdb87d reports
HWDB_HEADER_RC=0 and no name-initialization diagnostic. LSUSB_RC remains1,
not accepted as USB enumeration. No kernel/runtime source changed; latest
full guest/ABI runtime checks remain follow-up34.

### B1 follow-up36: namespace-local live Unix OFD table

`/proc/net/unix` is now wired through the existing target-task network namespace
provider, with the namespace retained at open. Stream/datagram/seqpacket socket
creation, socketpair and both accept paths register a weak observation token;
the shared preparation helper attaches the actual file owner. Registry locks
are released before endpoint observation. Reads do not poll, accept or consume
queued messages, and the registry does not keep an OFD alive after final close.

The seven-field prefix follows Linux 7.2.3 `net/unix/af_unix.c`'s proc format.
Type, connected/listening state, real pseudo-inode and raw bound-address bytes
come from the endpoint. Abstract leading/embedded NULs become `@`; non-UTF-8
bytes are retained. Protocol is zero (Unix has no IP protocol); the opaque
pointer column is always zero as a deliberate native pointer-hiding policy.
RefCount measures actual **native socket-wrapper Arc owners**, excluding the
observation's own temporary reference. It is not Linux's `sk_refcnt`, and is
not presented as an equivalent transport reference count. An OFD-backed
connected socket has state 03, all other OFD-backed sockets state 01; a listening
stream/seqpacket endpoint sets flag 00010000.

Coverage is explicitly partial: the native registry is OFD-backed, not a Linux
Unix sock hash-table walk. Unaccepted listener-queue children and orphan
transports surviving final OFD close are not represented. Unix SOCK_DIAG is
still unsupported; no empty successful netlink response is substituted.
SNMP and full TCP/UDP trailing fields remain separate work.

The original regression runs unchanged on host Linux (no host namespace or
container creation), comparing headers/mandatory grammar, three socket types,
raw abstract bytes, pathname/listen/accept states, inode/dup/final-close
lifetime and non-consumption of actual queued data. Guest additionally tests
namespace pin/isolation and runs both signed Alpine net-tools and BusyBox
`netstat -xanp` against a live pathname listener. The host base regression and the actual signed Alpine net-tools/BusyBox
listener/connected/PID rows passed (`shell-mzhcic74`, tools result 0).
The existing net-tools single-digit inode ownership-parser limitation is not
worked around by renumbering kernel inodes. BusyBox ownership is asserted.

Required regression-image headroom: adding this static guest regression exposed
that the default 96 MiB ext4 image had only 307 free 4 KiB blocks (about 1.2 MiB)
while reserving 1228 blocks. `e2fsck -fn` found a clean image, not corrupt metadata.
Two complete native suites failed only the direct-I/O fixture's initial fsync;
the same failure reproduced before running the Unix test in a fresh shell guest.
The Linux 7.2.3 oracle independently failed several non-root filesystem fixtures
with ENOSPC. Keeping this exact kernel/content and expanding only a disposable
copy to 128 MiB made the complete isolated direct-I/O program pass, including
fragmented physical SG, fixed-buffer lifetimes and queued-close completion.
The baseline allocation and both standalone builder defaults are therefore
128 MiB now, with a host test keeping them in sync. This is capacity for the
existing regression corpus, not a new payload or an alternate validation path.
The optional inspect payload remains 160 MiB; container tools are still separate.
Native ext4's insufficient-space fsync errno of EINVAL was observed but is not
claimed repaired by this capacity change. No filesystem errno was weakened to
make the test pass. Freshly rebuilt 128 MiB baseline: complete host suite passed (657 Python cases,
3 environment skips; 6049 Rust cases, 1 existing ignored case; kernel 2619).
KVM guest passed 69/69 with no skips and normal shutdown (`system-y9iunctn`);
full Linux/native ABI comparison passed 257/257 (`abi-ufpkqjni`). q35 and n305
lint passed with the same 784 existing kernel warnings. The fresh baseline has
4915 free 4 KiB blocks, not the prior 307. No physical hardware acceptance.

## 已知差异（CONTINUE-B 收口范围，2026-10-05）

以下属于字段级/非目标用法差异，按续做要求登记后不再继续扩展：
- net-tools 的单数字 inode PID 标签解析差异；不为用户态解析缺陷重编号。
- Unix RefCount 是 native OFD wrapper 引用，未接受的队列子连接及孤儿
  transport 不在此表；Unix SOCK_DIAG 是否影响 `ss -x` 的实际使用仍待验。
- TCP/UDP 尾字段、skb truesize、引用/drop 细节与 SNMP 等高级统计未完整。
- smaps 的 Dirty/PSS/私有共享精确分摊、buddy/zone 高阶表、PSI stall
  时间、精确 iowait/blocked/per-PID I/O 与部分 fault 边界未完整；不伪造数值。
- `/proc/stat` aggregate 行少一个空格，sysstat 的固定偏移解析可导致
  iostat CPU 字段错位（与退出失败不同）；本轮不扩展 CPU 字段级工作。
- PCI 未观测 BAR/ROM/GPU 资源尺寸、USB native 公共 API 缺真实地址/速度/
  拓扑/raw config；若影响指定工具基本使用，在收口时间内单独修阻塞。
- native ext4 在测试镜像空间耗尽时 fsync 曾返回 EINVAL；扩大回归镜像
  只修容量不足，不声称修复该 errno。未来 ENOSPC 边界另行处理。

收口优先顺序是 lspci → ss/netstat → net statistics → diskstats/iostat →
lsusb → lsns；剩余细节不阻止进入 B2，但实际工具失败不能标为通过。

### B1 close37: eliminate lspci's missing-resource diagnostic

The fresh USB-backed QEMU sweep (`shell-v9pe1q4o`) showed that all three lspci
calls exit 0, but `-vvv` prints `pcilib: Cannot open .../resource` for each
function. This is a direct tool diagnostic, not a BAR-size cosmetic difference.
The native PCI configuration owner already sizes standard-function BARs twice;
it now retains the results of those **existing** probes. No probe/configuration
write is added, and observation failure/cache exhaustion does not change driver
admission. Removal invalidates observations; reads check current identity and
all six raw BAR words before using a cached range.

The read-only `resource` file exports the measured six-BAR prefix with Linux
three-column hex records. A measured zero-size BAR and a 64-bit BAR's upper slot
are genuinely absent resources, so their rows are zero. Unmeasured functions
export no rows; unmeasured ROM/bridge windows are omitted at EOF, not filled with
invented zero-sized resources. This is **not the full Linux resource array**.
pciutils' native sysfs reader treats a missing suffix as unknown and falls back
to read-only config for that class of fields; this preserves actual config
addresses without new hardware sizing. No cache/file read enables a device.

The first sweep also confirmed clean basic lsns (8 real namespace rows), empty
ss Unix exit 0 (not yet an active socket acceptance), missing diskstats and
empty iostat disks, and lsusb exit 1 with three QEMU USB input devices present.
The next item after lspci is active ss Unix; field-level driver/ROM/bridge
resource details stay in “已知差异”, not another expansive implementation.

Close37 validation: related driver host tests 55/55 and kernel 2620/2620 passed;
KVM guest 69/69 passed (`system-we0f6x6y`). Actual signed Alpine lspci -vvv/-k/-t
all returned 0 without pcilib diagnostics (`shell-4yjhlt52`), and the GPU resource
prefix contained actual 4 KiB and 16 KiB memory ranges. The guest PCI test checks
prefix grammar and each measured start against raw config while retaining its
existing read-only/credential checks. Final lint-only iterator cleanup uses the
same fixed four-byte chunks, with related host tests/lint rerun; no new runtime
semantics or syscall contract changes, so no redundant full ABI run.
