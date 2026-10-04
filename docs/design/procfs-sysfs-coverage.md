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
