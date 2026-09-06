#!/usr/bin/env python3
"""Self-contained unit tests for the Linux v7.2.3 static routing gate."""

from __future__ import annotations

import tempfile
from tests.support import test_tmpdir
import unittest
from pathlib import Path

from tests.support import load_script_module, repo_root

ROOT = repo_root()
gate = load_script_module("linux_abi_gate", "scripts/ci/linux_abi_gate.py")


class GateTests(unittest.TestCase):
    def manifest(self) -> Path:
        return ROOT / "config/linux-abi.toml"

    def source(self, root: Path) -> tuple[Path, dict[int, str]]:
        manifest = gate.load_manifest(self.manifest())
        values = set()
        for key in ("ordinary_explicit", "explicit_enosys", "native_fallback"):
            values |= gate.numbers(manifest["routing_inventory"][key], key)
        entries = {number: f"sys_{number}" for number in values}
        entries.update({61:"wait4",247:"waitid",154:"modify_ldt",157:"prctl",158:"arch_prctl",334:"rseq",435:"clone3",448:"process_mrelease",166:"umount2",73:"flock",280:"utimensat",285:"fallocate",187:"readahead",163:"acct",167:"swapon",168:"swapoff",175:"init_module",313:"finit_module",176:"delete_module",246:"kexec_load",320:"kexec_file_load",248:"add_key",249:"request_key",250:"keyctl",298:"perf_event_open",101:"ptrace",109:"setpgid",111:"getpgrp",112:"setsid",121:"getpgid",124:"getsid",113:"setreuid",114:"setregid",117:"setresuid",119:"setresgid",125:"capget",126:"capset",237:"mbind",238:"set_mempolicy",239:"get_mempolicy",256:"migrate_pages",279:"move_pages",450:"set_mempolicy_home_node",272:"unshare",308:"setns",312:"kcmp",459:"lsm_get_self_attr",460:"lsm_set_self_attr",461:"lsm_list_modules",24:"sched_yield",35:"nanosleep",203:"sched_setaffinity",204:"sched_getaffinity",309:"getcpu",142:"sched_setparam",144:"sched_setscheduler",143:"sched_getparam",145:"sched_getscheduler",148:"sched_rr_get_interval",146:"sched_get_priority_max",147:"sched_get_priority_min",140:"getpriority",141:"setpriority",230:"clock_nanosleep",251:"ioprio_set",252:"ioprio_get",314:"sched_setattr",315:"sched_getattr",173:"ioperm",172:"iopl",133:"mknod",253:"inotify_init",254:"inotify_add_watch",255:"inotify_rm_watch",294:"inotify_init1",282:"signalfd",289:"signalfd4",283:"timerfd_create",286:"timerfd_settime",287:"timerfd_gettime",300:"fanotify_init",301:"fanotify_mark",179:"quotactl",443:"quotactl_fd",165:"mount",155:"pivot_root",428:"open_tree",429:"move_mount",430:"fsopen",431:"fsconfig",432:"fsmount",433:"fspick",457:"statmount",458:"listmount",442:"mount_setattr",2:"open",257:"openat",437:"openat2",303:"name_to_handle_at",304:"open_by_handle_at",3:"close",436:"close_range",32:"dup",33:"dup2",292:"dup3",72:"fcntl",139:"sysfs",80:"chdir",81:"fchdir",161:"chroot",83:"mkdir",258:"mkdirat",259:"mknodat",78:"getdents",217:"getdents64",86:"link",265:"linkat",87:"unlink",263:"unlinkat",84:"rmdir",88:"symlink",266:"symlinkat",89:"readlink",267:"readlinkat",92:"chown",94:"lchown",93:"fchown",260:"fchownat",90:"chmod",91:"fchmod",268:"fchmodat",452:"fchmodat2",132:"utime",235:"utimes",261:"futimesat",82:"rename",264:"renameat",316:"renameat2",162:"sync",153:"vhangup",306:"syncfs",79:"getcwd",169:"reboot",0:"read",1:"write",19:"readv",20:"writev",8:"lseek",17:"pread64",18:"pwrite64",295:"preadv",296:"pwritev",40:"sendfile",275:"splice",326:"copy_file_range",74:"fsync",75:"fdatasync",76:"truncate",77:"ftruncate",221:"fadvise64",277:"sync_file_range",327:"preadv2",328:"pwritev2",276:"tee",278:"vmsplice",206:"io_setup",209:"io_submit",208:"io_getevents",333:"io_pgetevents",207:"io_destroy",210:"io_cancel",22:"pipe",293:"pipe2",434:"pidfd_open",438:"pidfd_getfd",424:"pidfd_send_signal",451:"cachestat",56:"clone",57:"fork",58:"vfork",59:"execve",322:"execveat",25:"mremap",26:"msync",149:"mlock",325:"mlock2",150:"munlock",151:"mlockall",152:"munlockall",216:"remap_file_pages",273:"set_robust_list",274:"get_robust_list",7:"poll",271:"ppoll",329:"pkey_mprotect",331:"pkey_free",440:"process_madvise",453:"map_shadow_stack",213:"epoll_create",291:"epoll_create1",232:"epoll_wait",281:"epoll_pwait",441:"epoll_pwait2",39:"getpid",110:"getppid",186:"gettid",218:"set_tid_address",13:"rt_sigaction",14:"rt_sigprocmask",15:"rt_sigreturn",34:"pause",62:"kill",127:"rt_sigpending",128:"rt_sigtimedwait",129:"rt_sigqueueinfo",130:"rt_sigsuspend",131:"sigaltstack",200:"tkill",234:"tgkill",297:"rt_tgsigqueueinfo",36:"getitimer",37:"alarm",38:"setitimer",96:"gettimeofday",100:"times",159:"adjtimex",164:"settimeofday",222:"timer_create",223:"timer_settime",224:"timer_gettime",225:"timer_getoverrun",226:"timer_delete",227:"clock_settime",228:"clock_gettime",229:"clock_getres",305:"clock_adjtime",103:"syslog",219:"restart_syscall",318:"getrandom",240:"mq_open",241:"mq_unlink",242:"mq_timedsend",243:"mq_timedreceive",244:"mq_notify",245:"mq_getsetattr",68:"msgget",69:"msgsnd",70:"msgrcv",71:"msgctl",64:"semget",65:"semop",66:"semctl",220:"semtimedop",29:"shmget",30:"shmat",31:"shmctl",67:"shmdt",102:"getuid",107:"geteuid",118:"getresuid",104:"getgid",108:"getegid",120:"getresgid",105:"setuid",106:"setgid",122:"setfsuid",123:"setfsgid",115:"getgroups",116:"setgroups",63:"uname",170:"sethostname",171:"setdomainname",135:"personality",99:"sysinfo",302:"prlimit64",160:"setrlimit",97:"getrlimit",98:"getrusage",41:"socket",53:"socketpair",49:"bind",42:"connect",51:"getsockname",52:"getpeername",50:"listen",43:"accept",288:"accept4",48:"shutdown",44:"sendto",46:"sendmsg",307:"sendmmsg",45:"recvfrom",47:"recvmsg",299:"recvmmsg",55:"getsockopt",54:"setsockopt",60:"exit",231:"exit_group",4:"stat",5:"fstat",6:"lstat",262:"newfstatat",21:"access",269:"faccessat",439:"faccessat2",136:"ustat",137:"statfs",138:"fstatfs",332:"statx",188:"setxattr",189:"lsetxattr",190:"fsetxattr",191:"getxattr",192:"lgetxattr",193:"fgetxattr",194:"listxattr",195:"llistxattr",196:"flistxattr",197:"removexattr",198:"lremovexattr",199:"fremovexattr",330:"pkey_alloc",10:"mprotect",11:"munmap",27:"mincore",310:"process_vm_readv",311:"process_vm_writev",462:"mseal",12:"brk",233:"epoll_ctl",202:"futex",449:"futex_waitv",454:"futex_wake",455:"futex_wait",456:"futex_requeue",324:"membarrier",425:"io_uring_setup",426:"io_uring_enter",427:"io_uring_register",9:"mmap",28:"madvise",23:"select",270:"pselect6",317:"seccomp",319:"memfd_create",323:"userfaultfd",447:"memfd_secret",444:"landlock_create_ruleset",445:"landlock_add_rule",446:"landlock_restrict_self",16: "ioctl", 85: "creat", 284: "eventfd", 290: "eventfd2", 95: "umask", 201: "time", 134: "uselib", 156: "_sysctl", 174: "create_module", 177: "get_kernel_syms", 178: "query_module", 180: "nfsservctl", 181: "getpmsg", 182: "putpmsg", 183: "afs_syscall", 184: "tuxcall", 185: "security", 205: "set_thread_area", 211: "get_thread_area", 212: "lookup_dcookie", 214: "epoll_ctl_old", 215: "epoll_wait_old", 236: "vserver", 321: "bpf", 335: "uretprobe", 336: "uprobe", 463: "setxattrat", 464: "getxattrat", 465: "listxattrat", 466: "removexattrat", 467: "open_tree_attr", 468: "file_getattr", 469: "file_setattr", 470: "listns", 471: "rseq_slice_yield"})
        table = root / manifest["linux"]["table"]
        table.parent.mkdir(parents=True)
        table.write_text("\n".join(f"{number} common {name}" for number, name in sorted(entries.items())) + "\n512 x32 ignored\n", encoding="utf-8")
        return root, entries

    def dispatch(self, path: Path, entries: dict[int, str], fallback: bool = False, expression: str = "sys_call()") -> None:
        manifest = gate.load_manifest(self.manifest())
        inventory = manifest["routing_inventory"]
        ordinary = gate.numbers(inventory["ordinary_explicit"], "ordinary_explicit")
        if fallback: ordinary |= gate.numbers(inventory["native_fallback"], "native_fallback")
        ni = gate.numbers(inventory["explicit_enosys"], "explicit_enosys")
        arms = [f"Sysno::{entries[number]} => {expression}," for number in sorted(ordinary)]
        arms.append(" | ".join(f"Sysno::{entries[number]}" for number in sorted(ni - {470, 471})) + " => sys_ni_syscall(),")
        arms.append("_ => Err(AxError::Unsupported),")
        path.write_text("fn dispatch_new_syscall(number: usize) -> Option<AxResult<isize>> { match number { 470 | 471 => Some(sys_ni_syscall()), _ => None, } }\nfn dispatch_syscall(sysno: Sysno) { match sysno {\n" + "\n".join(arms) + "\n} }", encoding="utf-8")

    def contract_dispatch(self, path: Path, entries: dict[int, str]) -> None:
        self.dispatch(path, entries)
        routes = {
            61: "Sysno::wait4 => sys_waitpid(0),",
            247: "Sysno::waitid => sys_waitid(0),",
            154: "Sysno::modify_ldt => sys_modify_ldt(0),",
            157: "Sysno::prctl => sys_prctl(0),",
            158: "Sysno::arch_prctl => sys_arch_prctl(0),",
            334: "Sysno::rseq => sys_rseq(0),",
            435: "Sysno::clone3 => sys_clone3(0),",
            448: "Sysno::process_mrelease => sys_process_mrelease(0),",

            166: "Sysno::umount2 => sys_umount2(0),",
            73: "Sysno::flock => sys_flock(0),",
            280: "Sysno::utimensat => sys_utimensat(0),",
            285: "Sysno::fallocate => sys_fallocate(0),",
            187: "Sysno::readahead => sys_readahead(0),",

            163: "Sysno::acct => sys_acct(0),",
            167: "Sysno::swapon => sys_swapon(0),",
            168: "Sysno::swapoff => sys_swapoff(0),",
            175: "Sysno::init_module => sys_init_module(0),",
            313: "Sysno::finit_module => sys_finit_module(0),",
            176: "Sysno::delete_module => sys_delete_module(0),",
            246: "Sysno::kexec_load => sys_kexec_load(0),",
            320: "Sysno::kexec_file_load => sys_kexec_file_load(0),",
            248: "Sysno::add_key => sys_add_key(0),",
            249: "Sysno::request_key => sys_request_key(0),",
            250: "Sysno::keyctl => sys_keyctl(0),",
            298: "Sysno::perf_event_open => sys_perf_event_open(0),",

            101: "Sysno::ptrace => sys_ptrace(0),",

            109: "Sysno::setpgid => sys_setpgid(0),",
            111: "Sysno::getpgrp => compat_getpgrp(0),",
            112: "Sysno::setsid => sys_setsid(0),",
            121: "Sysno::getpgid => sys_getpgid(0),",
            124: "Sysno::getsid => sys_getsid(0),",
            113: "Sysno::setreuid => sys_setreuid(0),",
            114: "Sysno::setregid => sys_setregid(0),",
            117: "Sysno::setresuid => sys_setresuid(0),",
            119: "Sysno::setresgid => sys_setresgid(0),",
            125: "Sysno::capget => sys_capget(0),",
            126: "Sysno::capset => sys_capset(0),",
            237: "Sysno::mbind => sys_mbind(0),",
            238: "Sysno::set_mempolicy => sys_set_mempolicy(0),",
            239: "Sysno::get_mempolicy => sys_get_mempolicy(0),",
            256: "Sysno::migrate_pages => sys_migrate_pages(0),",
            279: "Sysno::move_pages => sys_move_pages(0),",
            450: "Sysno::set_mempolicy_home_node => sys_set_mempolicy_home_node(0),",
            272: "Sysno::unshare => sys_unshare(0),",
            308: "Sysno::setns => sys_setns(0),",
            312: "Sysno::kcmp => sys_kcmp(0),",
            459: "Sysno::lsm_get_self_attr => sys_lsm_get_self_attr(0),",
            460: "Sysno::lsm_set_self_attr => sys_lsm_set_self_attr(0),",
            461: "Sysno::lsm_list_modules => sys_lsm_list_modules(0),",

            24: "Sysno::sched_yield => sys_sched_yield(0),",
            35: "Sysno::nanosleep => sys_nanosleep(0),",
            203: "Sysno::sched_setaffinity => sys_sched_setaffinity(0),",
            204: "Sysno::sched_getaffinity => sys_sched_getaffinity(0),",
            309: "Sysno::getcpu => sys_getcpu(0),",
            142: "Sysno::sched_setparam => sys_sched_setparam(0),",
            144: "Sysno::sched_setscheduler => sys_sched_setscheduler(0),",
            143: "Sysno::sched_getparam => sys_sched_getparam(0),",
            145: "Sysno::sched_getscheduler => sys_sched_getscheduler(0),",
            148: "Sysno::sched_rr_get_interval => sys_sched_rr_get_interval(0),",
            146: "Sysno::sched_get_priority_max => sys_sched_get_priority_max(0),",
            147: "Sysno::sched_get_priority_min => sys_sched_get_priority_min(0),",
            140: "Sysno::getpriority => sys_getpriority(0),",
            141: "Sysno::setpriority => sys_setpriority(0),",
            230: "Sysno::clock_nanosleep => sys_clock_nanosleep(0),",
            251: "Sysno::ioprio_set => sys_ioprio_set(0),",
            252: "Sysno::ioprio_get => sys_ioprio_get(0),",
            314: "Sysno::sched_setattr => sys_sched_setattr(0),",
            315: "Sysno::sched_getattr => sys_sched_getattr(0),",
            173: "Sysno::ioperm => sys_ioperm(0),",
            172: "Sysno::iopl => sys_iopl(0),",

            133: "Sysno::mknod => compat_mknod(0),",
            253: "Sysno::inotify_init => compat_inotify_init(0),",
            254: "Sysno::inotify_add_watch => sys_inotify_add_watch(0),",
            255: "Sysno::inotify_rm_watch => sys_inotify_rm_watch(0),",
            294: "Sysno::inotify_init1 => sys_inotify_init1(0),",
            282: "Sysno::signalfd => compat_signalfd(0),",
            289: "Sysno::signalfd4 => sys_signalfd4(0),",
            283: "Sysno::timerfd_create => sys_timerfd_create(0),",
            286: "Sysno::timerfd_settime => sys_timerfd_settime(0),",
            287: "Sysno::timerfd_gettime => sys_timerfd_gettime(0),",
            300: "Sysno::fanotify_init => sys_fanotify_init(0),",
            301: "Sysno::fanotify_mark => sys_fanotify_mark(0),",
            179: "Sysno::quotactl => sys_quotactl(0),",
            443: "Sysno::quotactl_fd => sys_quotactl_fd(0),",
            165: "Sysno::mount => sys_mount(0),",
            155: "Sysno::pivot_root => sys_pivot_root(0),",
            428: "Sysno::open_tree => sys_open_tree(0),",
            429: "Sysno::move_mount => sys_move_mount(0),",
            430: "Sysno::fsopen => sys_fsopen(0),",
            431: "Sysno::fsconfig => sys_fsconfig(0),",
            432: "Sysno::fsmount => sys_fsmount(0),",
            433: "Sysno::fspick => sys_fspick(0),",
            457: "Sysno::statmount => sys_statmount(0),",
            458: "Sysno::listmount => sys_listmount(0),",
            442: "Sysno::mount_setattr => sys_mount_setattr(0),",

            2: "Sysno::open => sys_open(0),",
            257: "Sysno::openat => sys_openat(0),",
            437: "Sysno::openat2 => sys_openat2(0),",
            303: "Sysno::name_to_handle_at => sys_name_to_handle_at(0),",
            304: "Sysno::open_by_handle_at => sys_open_by_handle_at(0),",
            3: "Sysno::close => sys_close(0),",
            436: "Sysno::close_range => sys_close_range(0),",
            32: "Sysno::dup => sys_dup(0),",
            33: "Sysno::dup2 => sys_dup2(0),",
            292: "Sysno::dup3 => sys_dup3(0),",
            72: "Sysno::fcntl => sys_fcntl(0),",
            139: "Sysno::sysfs => sys_sysfs(0),",
            80: "Sysno::chdir => sys_chdir(0),",
            81: "Sysno::fchdir => sys_fchdir(0),",
            161: "Sysno::chroot => sys_chroot(0),",
            83: "Sysno::mkdir => sys_mkdir(0),",
            258: "Sysno::mkdirat => sys_mkdirat(0),",
            259: "Sysno::mknodat => sys_mknodat(0),",
            78: "Sysno::getdents => sys_getdents(0),",
            217: "Sysno::getdents64 => sys_getdents64(0),",
            86: "Sysno::link => sys_link(0),",
            265: "Sysno::linkat => sys_linkat(0),",
            87: "Sysno::unlink => sys_unlink(0),",
            263: "Sysno::unlinkat => sys_unlinkat(0),",
            84: "Sysno::rmdir => sys_rmdir(0),",
            88: "Sysno::symlink => sys_symlink(0),",
            266: "Sysno::symlinkat => sys_symlinkat(0),",
            89: "Sysno::readlink => sys_readlink(0),",
            267: "Sysno::readlinkat => sys_readlinkat(0),",
            92: "Sysno::chown => sys_chown(0),",
            94: "Sysno::lchown => sys_lchown(0),",
            93: "Sysno::fchown => sys_fchown(0),",
            260: "Sysno::fchownat => sys_fchownat(0),",
            90: "Sysno::chmod => sys_chmod(0),",
            91: "Sysno::fchmod => sys_fchmod(0),",
            268: "Sysno::fchmodat => sys_fchmodat(0),",
            452: "Sysno::fchmodat2 => sys_fchmodat(0),",
            132: "Sysno::utime => sys_utime(0),",
            235: "Sysno::utimes => sys_utimes(0),",
            261: "Sysno::futimesat => sys_futimesat(0),",
            82: "Sysno::rename => sys_rename(0),",
            264: "Sysno::renameat => sys_renameat(0),",
            316: "Sysno::renameat2 => sys_renameat2(0),",
            162: "Sysno::sync => sys_sync(0),",
            153: "Sysno::vhangup => sys_vhangup(0),",
            306: "Sysno::syncfs => sys_syncfs(0),",
            79: "Sysno::getcwd => sys_getcwd(0),",
            169: "Sysno::reboot => sys_reboot(0),",
            0: "Sysno::read => sys_read(0),",
            1: "Sysno::write => sys_write(0),",
            19: "Sysno::readv => sys_readv(0),",
            20: "Sysno::writev => sys_writev(0),",
            8: "Sysno::lseek => sys_lseek(0),",
            17: "Sysno::pread64 => sys_pread64(0),",
            18: "Sysno::pwrite64 => sys_pwrite64(0),",
            295: "Sysno::preadv => sys_preadv(0),",
            296: "Sysno::pwritev => sys_pwritev(0),",
            40: "Sysno::sendfile => sys_sendfile(0),",
            275: "Sysno::splice => sys_splice(0),",
            326: "Sysno::copy_file_range => sys_copy_file_range(0),",
            74: "Sysno::fsync => sys_fsync(0),",
            75: "Sysno::fdatasync => sys_fdatasync(0),",
            76: "Sysno::truncate => sys_truncate(0),",
            77: "Sysno::ftruncate => sys_ftruncate(0),",
            221: "Sysno::fadvise64 => sys_fadvise64(0),",
            277: "Sysno::sync_file_range => sys_sync_file_range(0),",
            327: "Sysno::preadv2 => sys_preadv2(0),",
            328: "Sysno::pwritev2 => sys_pwritev2(0),",
            276: "Sysno::tee => sys_tee(0),",
            278: "Sysno::vmsplice => sys_vmsplice(0),",
            206: "Sysno::io_setup => sys_io_setup(0),",
            209: "Sysno::io_submit => sys_io_submit(0),",
            208: "Sysno::io_getevents => sys_io_getevents(0),",
            333: "Sysno::io_pgetevents => sys_io_pgetevents(0),",
            207: "Sysno::io_destroy => sys_io_destroy(0),",
            210: "Sysno::io_cancel => sys_io_cancel(0),",
            22: "Sysno::pipe => sys_pipe2(0),",
            293: "Sysno::pipe2 => sys_pipe2(0),",
            434: "Sysno::pidfd_open => sys_pidfd_open(0),",
            438: "Sysno::pidfd_getfd => sys_pidfd_getfd(0),",
            424: "Sysno::pidfd_send_signal => sys_pidfd_send_signal(0),",
            451: "Sysno::cachestat => sys_cachestat(0),",

            56: "Sysno::clone => sys_clone(0),",
            57: "Sysno::fork => sys_fork(0),",
            58: '#[cfg(target_arch = "x86_64")] Sysno::vfork => sys_vfork(0),',
            59: "Sysno::execve => sys_execve(0),",
            322: "Sysno::execveat => sys_execveat(0),",

            25: "Sysno::mremap => sys_mremap(0),",
            26: "Sysno::msync => sys_msync(0),",
            149: "Sysno::mlock => sys_mlock(0),",
            325: "Sysno::mlock2 => sys_mlock2(0),",
            150: "Sysno::munlock => sys_munlock(0),",
            151: "Sysno::mlockall => sys_mlockall(0),",
            152: "Sysno::munlockall => sys_munlockall(0),",
            216: "Sysno::remap_file_pages => sys_remap_file_pages(0),",
            273: "Sysno::set_robust_list => sys_set_robust_list(0),",
            274: "Sysno::get_robust_list => sys_get_robust_list(0),",
            7: "Sysno::poll => sys_poll(0),",
            271: "Sysno::ppoll => sys_ppoll(0),",
            329: "Sysno::pkey_mprotect => sys_pkey_mprotect(0),",
            331: "Sysno::pkey_free => sys_pkey_free(0),",
            440: "Sysno::process_madvise => sys_process_madvise(0),",
            453: "Sysno::map_shadow_stack => sys_map_shadow_stack(0),",
            213: "Sysno::epoll_create => compat_epoll_create(0),",
            291: "Sysno::epoll_create1 => sys_epoll_create1(0),",
            232: "Sysno::epoll_wait => sys_epoll_wait(0),",
            281: "Sysno::epoll_pwait => sys_epoll_pwait(0),",
            441: "Sysno::epoll_pwait2 => sys_epoll_pwait2(0),",

            39: "Sysno::getpid => sys_getpid(0),",
            110: "Sysno::getppid => sys_getppid(0),",
            186: "Sysno::gettid => sys_gettid(0),",
            218: "Sysno::set_tid_address => sys_set_tid_address(0),",
            13: "Sysno::rt_sigaction => sys_rt_sigaction(0),",
            14: "Sysno::rt_sigprocmask => sys_rt_sigprocmask(0),",
            15: "Sysno::rt_sigreturn => sys_rt_sigreturn(0),",
            34: "Sysno::pause => sys_pause(0),",
            62: "Sysno::kill => sys_kill(0),",
            127: "Sysno::rt_sigpending => sys_rt_sigpending(0),",
            128: "Sysno::rt_sigtimedwait => sys_rt_sigtimedwait(0),",
            129: "Sysno::rt_sigqueueinfo => sys_rt_sigqueueinfo(0),",
            130: "Sysno::rt_sigsuspend => sys_rt_sigsuspend(0),",
            131: "Sysno::sigaltstack => sys_sigaltstack(0),",
            200: "Sysno::tkill => sys_tkill(0),",
            234: "Sysno::tgkill => sys_tgkill(0),",
            297: "Sysno::rt_tgsigqueueinfo => sys_rt_tgsigqueueinfo(0),",

            36: "Sysno::getitimer => sys_getitimer(0),",
            37: "Sysno::alarm => sys_alarm(0),",
            38: "Sysno::setitimer => sys_setitimer(0),",
            96: "Sysno::gettimeofday => sys_gettimeofday(0),",
            100: "Sysno::times => sys_times(0),",
            159: "Sysno::adjtimex => sys_adjtimex(0),",
            164: "Sysno::settimeofday => sys_settimeofday(0),",
            222: "Sysno::timer_create => sys_timer_create(0),",
            223: "Sysno::timer_settime => sys_timer_settime(0),",
            224: "Sysno::timer_gettime => sys_timer_gettime(0),",
            225: "Sysno::timer_getoverrun => sys_timer_getoverrun(0),",
            226: "Sysno::timer_delete => sys_timer_delete(0),",
            227: "Sysno::clock_settime => sys_clock_settime(0),",
            228: "Sysno::clock_gettime => sys_clock_gettime(0),",
            229: "Sysno::clock_getres => sys_clock_getres(0),",
            305: "Sysno::clock_adjtime => sys_clock_adjtime(0),",
            103: "Sysno::syslog => sys_syslog(0),",
            219: "Sysno::restart_syscall => sys_restart_syscall(0),",
            318: "Sysno::getrandom => sys_getrandom(0),",

            240: "Sysno::mq_open => sys_mq_open(0),",
            241: "Sysno::mq_unlink => sys_mq_unlink(0),",
            242: "Sysno::mq_timedsend => sys_mq_timedsend(0),",
            243: "Sysno::mq_timedreceive => sys_mq_timedreceive(0),",
            244: "Sysno::mq_notify => sys_mq_notify(0),",
            245: "Sysno::mq_getsetattr => sys_mq_getsetattr(0),",

            68: "Sysno::msgget => sys_msgget(0),",
            69: "Sysno::msgsnd => sys_msgsnd(0),",
            70: "Sysno::msgrcv => sys_msgrcv(0),",
            71: "Sysno::msgctl => sys_msgctl(0),",
            64: "Sysno::semget => sys_semget(0),",
            65: "Sysno::semop => sys_semop(0),",
            66: "Sysno::semctl => sys_semctl(0),",
            220: "Sysno::semtimedop => sys_semtimedop(0),",
            29: "Sysno::shmget => sys_shmget(0),",
            30: "Sysno::shmat => sys_shmat(0),",
            31: "Sysno::shmctl => sys_shmctl(0),",
            67: "Sysno::shmdt => sys_shmdt(0),",

            102: "Sysno::getuid => sys_getuid(0),",
            107: "Sysno::geteuid => sys_geteuid(0),",
            118: "Sysno::getresuid => sys_getresuid(0),",
            104: "Sysno::getgid => sys_getgid(0),",
            108: "Sysno::getegid => sys_getegid(0),",
            120: "Sysno::getresgid => sys_getresgid(0),",
            105: "Sysno::setuid => sys_setuid(0),",
            106: "Sysno::setgid => sys_setgid(0),",
            122: "Sysno::setfsuid => sys_setfsuid(0),",
            123: "Sysno::setfsgid => sys_setfsgid(0),",
            115: "Sysno::getgroups => sys_getgroups(0),",
            116: "Sysno::setgroups => sys_setgroups(0),",
            63: "Sysno::uname => sys_uname(0),",
            170: "Sysno::sethostname => sys_sethostname(0),",
            171: "Sysno::setdomainname => sys_setdomainname(0),",
            135: "Sysno::personality => sys_personality(0),",
            99: "Sysno::sysinfo => sys_sysinfo(0),",
            302: "Sysno::prlimit64 => sys_prlimit64(0),",
            160: "Sysno::setrlimit => sys_setrlimit(0),",
            97: "Sysno::getrlimit => sys_getrlimit(0),",
            98: "Sysno::getrusage => sys_getrusage(0),",

            41: "Sysno::socket => sys_socket(0),",
            53: "Sysno::socketpair => sys_socketpair(0),",
            49: "Sysno::bind => sys_bind(0),",
            42: "Sysno::connect => sys_connect(0),",
            51: "Sysno::getsockname => sys_getsockname(0),",
            52: "Sysno::getpeername => sys_getpeername(0),",
            50: "Sysno::listen => sys_listen(0),",
            43: "Sysno::accept => sys_accept(0),",
            288: "Sysno::accept4 => sys_accept4(0),",
            48: "Sysno::shutdown => sys_shutdown(0),",
            44: "Sysno::sendto => sys_sendto(0),",
            46: "Sysno::sendmsg => sys_sendmsg(0),",
            307: "Sysno::sendmmsg => sys_sendmmsg(0),",
            45: "Sysno::recvfrom => sys_recvfrom(0),",
            47: "Sysno::recvmsg => sys_recvmsg(0),",
            299: "Sysno::recvmmsg => sys_recvmmsg(0),",
            55: "Sysno::getsockopt => sys_getsockopt(0),",
            54: "Sysno::setsockopt => sys_setsockopt(0),",

            60: "Sysno::exit => sys_exit(0),",
            231: "Sysno::exit_group => sys_exit_group(0),",

            4: "Sysno::stat => sys_stat(0),",
            5: "Sysno::fstat => sys_fstat(0),",
            6: "Sysno::lstat => sys_lstat(0),",
            262: "Sysno::newfstatat => sys_fstatat(0),",
            21: "Sysno::access => sys_access(0),",
            269: "Sysno::faccessat => sys_faccessat(0),",
            439: "Sysno::faccessat2 => sys_faccessat2(0),",
            136: "Sysno::ustat => sys_ustat(0),",
            137: "Sysno::statfs => sys_statfs(0),",
            138: "Sysno::fstatfs => sys_fstatfs(0),",
            332: "Sysno::statx => sys_statx(0),",

            188: "Sysno::setxattr => sys_setxattr(0),",
            189: "Sysno::lsetxattr => sys_lsetxattr(0),",
            190: "Sysno::fsetxattr => sys_fsetxattr(0),",
            191: "Sysno::getxattr => sys_getxattr(0),",
            192: "Sysno::lgetxattr => sys_lgetxattr(0),",
            193: "Sysno::fgetxattr => sys_fgetxattr(0),",
            194: "Sysno::listxattr => sys_listxattr(0),",
            195: "Sysno::llistxattr => sys_llistxattr(0),",
            196: "Sysno::flistxattr => sys_flistxattr(0),",
            197: "Sysno::removexattr => sys_removexattr(0),",
            198: "Sysno::lremovexattr => sys_lremovexattr(0),",
            199: "Sysno::fremovexattr => sys_fremovexattr(0),",

            330: "Sysno::pkey_alloc => sys_pkey_alloc(0,0),",
            10: "Sysno::mprotect => sys_mprotect(0),",
            11: "Sysno::munmap => sys_munmap(0),",
            27: "Sysno::mincore => sys_mincore(0),",
            310: "Sysno::process_vm_readv => sys_process_vm_readv(0),",
            311: "Sysno::process_vm_writev => sys_process_vm_writev(0),",
            462: "Sysno::mseal => sys_mseal(0),",

            12: "Sysno::brk => sys_brk(0),",
            233: "Sysno::epoll_ctl => sys_epoll_ctl(0),",
            202: "Sysno::futex => sys_futex(0),",
            449: "Sysno::futex_waitv => sys_futex_waitv(0),",
            454: "Sysno::futex_wake => sys_futex_wake(0),",
            455: "Sysno::futex_wait => sys_futex_wait(0),",
            456: "Sysno::futex_requeue => sys_futex_requeue(0),",
            324: "Sysno::membarrier => sys_membarrier(0),",

            425: "Sysno::io_uring_setup => sys_io_uring_setup(0),",
            426: "Sysno::io_uring_enter => sys_io_uring_enter(0),",
            427: "Sysno::io_uring_register => sys_io_uring_register(0),",

            9: "Sysno::mmap => sys_mmap(0),",
            28: "Sysno::madvise => sys_madvise(0),",
            23: "Sysno::select => sys_select(0),",
            270: "Sysno::pselect6 => sys_pselect6(0),",

            317: "Sysno::seccomp => sys_seccomp(memory,0,0,0),",
            319: "Sysno::memfd_create => sys_memfd_create(0),",
            323: "Sysno::userfaultfd => sys_userfaultfd(0),",
            447: "Sysno::memfd_secret => sys_memfd_secret(0),",

            444: "Sysno::landlock_create_ruleset => sys_landlock_create_ruleset(0),",
            445: "Sysno::landlock_add_rule => sys_landlock_add_rule(0),",
            446: "Sysno::landlock_restrict_self => sys_landlock_restrict_self(0),",

            16: "Sysno::ioctl => sys_ioctl(context, 0, 0, 0),",
            85: "Sysno::creat => sys_creat(capability, 0, 0),",
            284: "Sysno::eventfd => compat_eventfd(0),",
            290: "Sysno::eventfd2 => sys_eventfd2(0, 0),",
            95: "Sysno::umask => sys_umask(0),",
            201: "Sysno::time => sys_time(memory, 0),",
            321: '#[cfg(feature = "bpf")] Sysno::bpf => super::bpf::sys_bpf(memory, 0, 0, 0),',
            335: "Sysno::uretprobe => super::task::sys_uretprobe(uctx),",
            336: "Sysno::uprobe => super::task::sys_uprobe(uctx),",
            463: "Sysno::setxattrat => super::fs::sys_setxattrat(memory),",
            464: "Sysno::getxattrat => super::fs::sys_getxattrat(memory),",
            465: "Sysno::listxattrat => super::fs::sys_listxattrat(memory),",
            466: "Sysno::removexattrat => super::fs::sys_removexattrat(memory),",
            467: "Sysno::open_tree_attr => super::fs::sys_open_tree_attr(memory),",
            468: "Sysno::file_getattr => super::fs::sys_file_getattr(memory),",
            469: "Sysno::file_setattr => super::fs::sys_file_setattr(memory),",
        }
        text = path.read_text(encoding="utf-8")
        for number, route in routes.items():
            text = text.replace(f"Sysno::{entries[number]} => sys_call(),", route)
        path.write_text(text, encoding="utf-8")

    def contracts(self, root: Path, change: tuple[str, str] | None = None) -> Path:
        text = (ROOT / "config/linux-contracts.toml").read_text(encoding="utf-8")
        if change is not None:
            text = text.replace(*change)
        path = root / "contracts.toml"; path.write_text(text, encoding="utf-8")
        return path

    def test_pin_and_terminal_are_fixed(self) -> None:
        manifest = gate.load_manifest(self.manifest())
        self.assertEqual(manifest["linux"]["tag"], "v7.2.3")
        self.assertEqual(manifest["terminal"], {"ordinary_explicit": 366, "explicit_enosys": 19, "native_fallback": 0})

    def test_new_release_routes_cannot_fall_through(self) -> None:
        with test_tmpdir() as temp:
            root = Path(temp)
            _, entries = self.source(root / "linux")
            path = root / "dispatch.rs"
            self.dispatch(path, entries)
            path.write_text(path.read_text().replace("470 | 471", "470 | 472"))
            with self.assertRaisesRegex(gate.GateError, "raw-number ENOSYS"):
                gate.routes(path, set(entries.values()), gate.WITNESS)

    def test_inventory_accepts_canonical_decimal_strings_only(self) -> None:
        self.assertEqual(gate.numbers(["179", "180-181"], "ordinary_explicit"), {179, 180, 181})
        for value in (
            "0179",
            "+179",
            " 179",
            "179 ",
            "-1",
            "1_79",
            "0179-180",
            "179-0180",
            "179-",
            "179-180-181",
        ):
            with self.subTest(value=value):
                with self.assertRaisesRegex(gate.GateError, "invalid number/range"):
                    gate.numbers([value], "ordinary_explicit")

    def test_x32_is_excluded(self) -> None:
        with test_tmpdir() as temporary:
            source, _ = self.source(Path(temporary) / "linux")
            self.assertEqual(len(gate.parse_table(source / gate.load_manifest(self.manifest())["linux"]["table"])), 385)

    def test_inventory_passes(self) -> None:
        with test_tmpdir() as temporary:
            source, entries = self.source(Path(temporary) / "linux")
            dispatch = Path(temporary) / "dispatch.rs"; self.dispatch(dispatch, entries)
            gate.inventory(self.manifest(), source, dispatch)

    def test_literals_and_comments_cannot_invent_routes(self) -> None:
        with test_tmpdir() as temporary:
            path = Path(temporary) / "dispatch.rs"
            path.write_text('/* fn dispatch_syscall(x) { match sysno { Sysno::fake => {} } } */\nconst X: &str = r#"Sysno::fake => {}"#; const Y: u8 = b\'}\';\nfn dispatch_syscall(sysno: Sysno) { match sysno { Sysno::real => { let _ = "},"; ok() }, Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } }', encoding="utf-8")
            found, ni, _ = gate.routes(path, {"real", "ni"}, gate.WITNESS)
        self.assertEqual(found, {"real", "ni"}); self.assertEqual(ni, {"ni"})

    def test_pattern_comment_cannot_invent_route(self) -> None:
        with test_tmpdir() as temporary:
            path = Path(temporary) / "dispatch.rs"
            path.write_text("fn dispatch_new_syscall(number: usize) -> Option<AxResult<isize>> { match number { 470 | 471 => Some(sys_ni_syscall()), _ => None, } }\nfn dispatch_syscall(sysno: Sysno) { match sysno { Sysno::real /* Sysno::fake */ => ok(), Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } }", encoding="utf-8")
            found, _, _ = gate.routes(path, {"real", "fake", "ni"}, gate.WITNESS)
        self.assertEqual(found, {"real", "ni"})

    def test_macro_dispatch_decoy_is_not_selected(self) -> None:
        with test_tmpdir() as temporary:
            path = Path(temporary) / "dispatch.rs"
            path.write_text("macro_rules! decoy { () => { fn dispatch_syscall(sysno: Sysno) { match sysno { Sysno::fake => ok(), Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } } }; }\nfn dispatch_syscall(sysno: Sysno) { match sysno { Sysno::real => ok(), Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } }", encoding="utf-8")
            found, ni, _ = gate.routes(path, {"real", "ni"}, gate.WITNESS)
        self.assertEqual(found, {"real", "ni"})
        self.assertEqual(ni, {"ni"})

    def test_guard_and_false_cfg_are_rejected(self) -> None:
        for arm, message in (("Sysno::real if false => ok(),", "match guard"), ("#[cfg(any())] Sysno::real => ok(),", "conditional attribute")):
            with self.subTest(arm=arm), test_tmpdir() as temporary:
                path = Path(temporary) / "dispatch.rs"
                path.write_text(f"fn dispatch_syscall(sysno: Sysno) {{ match sysno {{ {arm} Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), }} }}", encoding="utf-8")
                with self.assertRaisesRegex(gate.GateError, message): gate.routes(path, {"real", "ni"}, gate.WITNESS)

    def test_exact_bpf_witness(self) -> None:
        with test_tmpdir() as temporary:
            path = Path(temporary) / "dispatch.rs"
            path.write_text('#[allow(dead_code)] fn dispatch_syscall(sysno: Sysno) { match sysno { #[cfg(feature = "bpf")] Sysno::bpf => ok(), Sysno::ni => sys_ni_syscall(), _ => Err(AxError::Unsupported), } }', encoding="utf-8")
            found, _, _ = gate.routes(path, {"bpf", "ni"}, gate.WITNESS)
            self.assertEqual(found, {"bpf", "ni"})

    def test_contract_cells_are_individual_and_honest(self) -> None:
        with test_tmpdir() as temporary:
            root = Path(temporary); source, entries = self.source(root / "linux")
            cells = gate.contract_cells(self.contracts(root), entries)
            self.assertEqual(set(cells), {61,247,154,157,158,334,435,448,166,73,280,285,187,163,167,168,175,313,176,246,320,248,249,250,298,101,109,111,112,121,124,113,114,117,119,125,126,237,238,239,256,279,450,272,308,312,459,460,461,24,35,203,204,309,142,144,143,145,148,146,147,140,141,230,251,252,314,315,173,172,133,253,254,255,294,282,289,283,286,287,300,301,179,443,165,155,428,429,430,431,432,433,457,458,442,2,257,437,303,304,3,436,32,33,292,72,139,80,81,161,83,258,259,78,217,86,265,87,263,84,88,266,89,267,92,94,93,260,90,91,268,452,132,235,261,82,264,316,162,153,306,79,169,0,1,19,20,8,17,18,295,296,40,275,326,74,75,76,77,221,277,327,328,276,278,206,209,208,333,207,210,22,293,434,438,424,451,56,57,58,59,322,25,26,149,325,150,151,152,216,273,274,7,271,329,331,440,453,213,291,232,281,441,39,110,186,218,13,14,15,34,62,127,128,129,130,131,200,234,297,36,37,38,96,100,159,164,222,223,224,225,226,227,228,229,305,103,219,318,240,241,242,243,244,245,68,69,70,71,64,65,66,220,29,30,31,67,102,107,118,104,108,120,105,106,122,123,115,116,63,170,171,135,99,302,160,97,98,41,53,49,42,51,52,50,43,288,48,44,46,307,45,47,299,55,54,60,231,4,5,6,262,21,269,439,136,137,138,332,188,189,190,191,192,193,194,195,196,197,198,199,330,10,11,27,310,311,462,12,233,202,449,454,455,456,324,425,426,427,9,28,23,270,317,319,323,447,444,445,446,16, 85, 284, 290, 95, 201, 134, 156, 174, 177, 178, 180, 181, 182, 183, 184, 185, 205, 211, 212, 214, 215, 236, 321, 335, 336, 463, 464, 465, 466, 467, 468, 469, 470, 471})
            self.assertTrue(all(cell["handler"].endswith(":sys_ni_syscall") for cell in cells.values() if cell["status"] == "explicit-enosys"))
            self.assertEqual(cells[321]["conditional"], "bpf")

    def test_final_acceptance_rejects_unknown_and_unvalidated_cells(self) -> None:
        with test_tmpdir() as temporary:
            root = Path(temporary); source, entries = self.source(root / "linux")
            dispatch = root / "dispatch.rs"; self.contract_dispatch(dispatch, entries)
            with self.assertRaisesRegex(gate.GateError, "final ABI static prerequisites incomplete"):
                gate.schema(self.manifest(), self.contracts(root), source, dispatch, final=True)

    def test_missing_test_symbol_and_hidden_validation_gap_are_rejected(self) -> None:
        with test_tmpdir() as temporary:
            root = Path(temporary); _, entries = self.source(root / "linux")
            contracts = self.contracts(root, ("native-ni-differential.c:main", "native-ni-differential.c:missing_test"))
            with self.assertRaisesRegex(gate.GateError, "test symbol does not exist"):
                gate.contract_cells(contracts, entries)
            contracts = self.contracts(root)
            text = contracts.read_text().replace(
                'validation_gaps = ["uretprobe has a source-reviewed contract but no bound Linux 7.2.3 guest behavior test"]',
                'validation_gaps = ["explicit-none"]')
            contracts.write_text(text)
            with self.assertRaisesRegex(gate.GateError, "without tests must report"):
                gate.contract_cells(contracts, entries)

    def test_all_declared_non_ni_cells_bind_dispatch_handlers(self) -> None:
        with test_tmpdir() as temporary:
            root = Path(temporary); source, entries = self.source(root / "linux")
            dispatch = root / "dispatch.rs"; self.contract_dispatch(dispatch, entries)
            gate.contract_cells(self.contracts(root), entries, dispatch)
            text = dispatch.read_text(encoding="utf-8").replace("super::bpf::sys_bpf(memory, 0, 0, 0)", "other_real_handler(memory)")
            dispatch.write_text(text, encoding="utf-8")
            with self.assertRaisesRegex(gate.GateError, "non-NI cell is not bound"):
                gate.contract_cells(self.contracts(root), entries, dispatch)
            self.contract_dispatch(dispatch, entries)
            text = dispatch.read_text(encoding="utf-8").replace("super::fs::sys_setxattrat(memory)", "other_module::sys_setxattrat(memory)")
            dispatch.write_text(text, encoding="utf-8")
            with self.assertRaisesRegex(gate.GateError, "non-NI cell is not bound"):
                gate.contract_cells(self.contracts(root), entries, dispatch)

    def test_static_schema_accepts_contracts(self) -> None:
        with test_tmpdir() as temporary:
            root = Path(temporary); source, entries = self.source(root / "linux")
            dispatch = root / "dispatch.rs"; self.contract_dispatch(dispatch, entries)
            gate.schema(self.manifest(), self.contracts(root), source, dispatch)

    def test_graph_fields_reject_empty_or_handler_defined_payloads(self) -> None:
        for value in (["flag:"], ["errno:handler-defined"]):
            with self.subTest(value=value):
                with self.assertRaisesRegex(gate.GateError, "typed grammar|placeholder"):
                    gate.graph_field(value, "linux-test", "flags" if value[0].startswith("flag:") else "errno_order")

    def test_ordinary_function_cannot_be_declared_a_rust_test(self) -> None:
        with test_tmpdir() as temporary:
            root = Path(temporary); _, entries = self.source(root / "linux")
            contracts = self.contracts(root, (
                "tests/guest/tools/cpu-smoke.c:pku_state",
                "kernel/src/syscall/mm/mmap.rs:sys_pkey_alloc",
            ))
            with self.assertRaisesRegex(gate.GateError, r"must have #\[test\]"):
                gate.contract_cells(contracts, entries)

    def test_unrelated_portable_test_cannot_clear_a_validation_gap(self) -> None:
        with test_tmpdir() as temporary:
            root = Path(temporary); _, entries = self.source(root / "linux")
            contracts = self.contracts(root, (
                "tests/guest/portable/time-differential.c:main",
                "tests/guest/portable/creat-differential.c:main",
            ))
            with self.assertRaisesRegex(gate.GateError, "without a registered differential case"):
                gate.contract_cells(contracts, entries)

    def test_handler_cfg_must_match_cell_conditional(self) -> None:
        with tempfile.TemporaryDirectory(dir=ROOT) as temporary:
            path = Path(temporary) / "handler.rs"
            path.write_text('#[cfg(feature = "other")]\nfn handler() {}\n', encoding="utf-8")
            with self.assertRaisesRegex(gate.GateError, "cfg does not match"):
                gate.rust_function(path, "handler", "bpf")

    def test_handler_array_semicolon_is_not_a_declaration(self) -> None:
        with tempfile.TemporaryDirectory(dir=ROOT) as temporary:
            path = Path(temporary) / "handler.rs"
            path.write_text("fn handler(value: [i32; 2]) -> [u8; 4] { [0; 4] }\n", encoding="utf-8")
            gate.rust_function(path, "handler", "explicit-none")
            path.write_text("fn handler(value: [i32; 2]) -> [u8; 4];\n", encoding="utf-8")
            with self.assertRaisesRegex(gate.GateError, "no function body"):
                gate.rust_function(path, "handler", "explicit-none")

    def test_handler_must_be_a_top_level_rust_item(self) -> None:
        with tempfile.TemporaryDirectory(dir=ROOT) as temporary:
            path = Path(temporary) / "handler.rs"
            path.write_text("macro_rules! decoy { () => { fn macro_handler() {} }; }\nfn outer() {\n    fn nested_handler() {}\n}\n", encoding="utf-8")
            for symbol in ("macro_handler", "nested_handler"):
                with self.subTest(symbol=symbol):
                    with self.assertRaisesRegex(gate.GateError, "top-level match"):
                        gate.rust_function(path, symbol, "explicit-none")

    def test_contract_rejects_placeholder_unbound_handler_and_duplicate_cell(self) -> None:
        with test_tmpdir() as temporary:
            root = Path(temporary); _, entries = self.source(root / "linux")
            for change, message in (
                (('errno_order = ["errno:ENOSYS"]', 'errno_order = ["Linux syscall-specific order"]'), "placeholder"),
                (("handler = \"kernel/src/syscall/dispatch.rs:sys_ni_syscall\"", "handler = \"kernel/src/syscall/dispatch.rs:not_a_function\""), "function definition"),
                (("number = 156\nname = \"_sysctl\"", "number = 134\nname = \"uselib\""), "duplicate"),
            ):
                with self.subTest(change=change):
                    with self.assertRaisesRegex(gate.GateError, message):
                        gate.contract_cells(self.contracts(root, change), entries)


if __name__ == "__main__": unittest.main()
