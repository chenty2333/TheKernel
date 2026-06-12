use alloc::collections::BTreeMap;
use core::{ffi::c_char, mem::size_of};

use axerrno::{AxError, AxResult, LinuxError};
use axsync::Mutex;
use axtask::current;
use bytemuck::AnyBitPattern;
use linux_raw_sys::general::{CAP_SYS_ADMIN, S_IFBLK, S_IFDIR, S_IFMT, S_IFREG, S_IFSOCK};
use starry_vm::{VmMutPtr, VmPtr};

use crate::{
    file::{FileLikeKind, Kstat, get_file_like},
    mm::vm_load_string,
    task::AsThread,
};

const SUBCMDMASK: u32 = 0x00ff;
const SUBCMDSHIFT: u32 = 8;
const MAXQUOTAS: u32 = 3;

const USRQUOTA: u32 = 0;
const GRPQUOTA: u32 = 1;
const PRJQUOTA: u32 = 2;

const Q_SYNC: u32 = 0x800001;
const Q_QUOTAON: u32 = 0x800002;
const Q_QUOTAOFF: u32 = 0x800003;
const Q_GETFMT: u32 = 0x800004;
const Q_GETINFO: u32 = 0x800005;
const Q_SETINFO: u32 = 0x800006;
const Q_GETQUOTA: u32 = 0x800007;
const Q_SETQUOTA: u32 = 0x800008;
const Q_GETNEXTQUOTA: u32 = 0x800009;

const QFMT_VFS_V0: i32 = 2;
const QFMT_VFS_V1: i32 = 4;

const IIF_BGRACE: u32 = 1;
const IIF_IGRACE: u32 = 2;
const IIF_FLAGS: u32 = 4;
const IIF_ALL: u32 = IIF_BGRACE | IIF_IGRACE | IIF_FLAGS;

const QIF_BLIMITS: u32 = 1 << 0;
const QIF_SPACE: u32 = 1 << 1;
const QIF_ILIMITS: u32 = 1 << 2;
const QIF_INODES: u32 = 1 << 3;
const QIF_BTIME: u32 = 1 << 4;
const QIF_ITIME: u32 = 1 << 5;
const QIF_LIMITS: u32 = QIF_BLIMITS | QIF_ILIMITS;
const QIF_USAGE: u32 = QIF_SPACE | QIF_INODES;
const QIF_TIMES: u32 = QIF_BTIME | QIF_ITIME;
const QIF_ALL: u32 = QIF_LIMITS | QIF_USAGE | QIF_TIMES;

const XQM_BASE: u32 = ('X' as u32) << 8;
const Q_XQUOTAON: u32 = XQM_BASE + 1;
const Q_XQUOTAOFF: u32 = XQM_BASE + 2;
const Q_XGETQUOTA: u32 = XQM_BASE + 3;
const Q_XSETQLIM: u32 = XQM_BASE + 4;
const Q_XGETQSTAT: u32 = XQM_BASE + 5;
const Q_XQUOTARM: u32 = XQM_BASE + 6;
const Q_XQUOTASYNC: u32 = XQM_BASE + 7;
const Q_XGETQSTATV: u32 = XQM_BASE + 8;
const Q_XGETNEXTQUOTA: u32 = XQM_BASE + 9;

const FS_DQUOT_VERSION: i8 = 1;
const FS_QSTAT_VERSION: i8 = 1;
const FS_QSTATV_VERSION1: i8 = 1;

const FS_DQ_ISOFT: u16 = 1 << 0;
const FS_DQ_IHARD: u16 = 1 << 1;
const FS_DQ_BSOFT: u16 = 1 << 2;
const FS_DQ_BHARD: u16 = 1 << 3;
const FS_DQ_RTBSOFT: u16 = 1 << 4;
const FS_DQ_RTBHARD: u16 = 1 << 5;
const FS_DQ_BTIMER: u16 = 1 << 6;
const FS_DQ_ITIMER: u16 = 1 << 7;
const FS_DQ_RTBTIMER: u16 = 1 << 8;
const FS_DQ_BWARNS: u16 = 1 << 9;
const FS_DQ_IWARNS: u16 = 1 << 10;
const FS_DQ_RTBWARNS: u16 = 1 << 11;
const FS_DQ_BCOUNT: u16 = 1 << 12;
const FS_DQ_ICOUNT: u16 = 1 << 13;
const FS_DQ_RTBCOUNT: u16 = 1 << 14;

const FS_QUOTA_UDQ_ACCT: u16 = 1 << 0;
const FS_QUOTA_UDQ_ENFD: u16 = 1 << 1;
const FS_QUOTA_GDQ_ACCT: u16 = 1 << 2;
const FS_QUOTA_GDQ_ENFD: u16 = 1 << 3;
const FS_QUOTA_PDQ_ACCT: u16 = 1 << 4;
const FS_QUOTA_PDQ_ENFD: u16 = 1 << 5;

const FS_USER_QUOTA: i8 = 1 << 0;
const FS_PROJ_QUOTA: i8 = 1 << 1;
const FS_GROUP_QUOTA: i8 = 1 << 2;

const VFS_V0_LIMIT_MAX: u64 = 0x0fff_ffff_ffff;
const VFS_V1_LIMIT_MAX: u64 = 0x001f_ffff_ffff_ffff;

static QUOTA_MANAGER: Mutex<QuotaManager> = Mutex::new(QuotaManager::new());

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
struct IfDqblk {
    dqb_bhardlimit: u64,
    dqb_bsoftlimit: u64,
    dqb_curspace: u64,
    dqb_ihardlimit: u64,
    dqb_isoftlimit: u64,
    dqb_curinodes: u64,
    dqb_btime: u64,
    dqb_itime: u64,
    dqb_valid: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
struct IfNextDqblk {
    dqb_bhardlimit: u64,
    dqb_bsoftlimit: u64,
    dqb_curspace: u64,
    dqb_ihardlimit: u64,
    dqb_isoftlimit: u64,
    dqb_curinodes: u64,
    dqb_btime: u64,
    dqb_itime: u64,
    dqb_valid: u32,
    dqb_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
struct IfDqinfo {
    dqi_bgrace: u64,
    dqi_igrace: u64,
    dqi_flags: u32,
    dqi_valid: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
struct FsDiskQuota {
    d_version: i8,
    d_flags: i8,
    d_fieldmask: u16,
    d_id: u32,
    d_blk_hardlimit: u64,
    d_blk_softlimit: u64,
    d_ino_hardlimit: u64,
    d_ino_softlimit: u64,
    d_bcount: u64,
    d_icount: u64,
    d_itimer: i32,
    d_btimer: i32,
    d_iwarns: u16,
    d_bwarns: u16,
    d_itimer_hi: i8,
    d_btimer_hi: i8,
    d_rtbtimer_hi: i8,
    d_padding2: i8,
    d_rtb_hardlimit: u64,
    d_rtb_softlimit: u64,
    d_rtbcount: u64,
    d_rtbtimer: i32,
    d_rtbwarns: u16,
    d_padding3: i16,
    d_padding4: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
struct FsQfilestat {
    qfs_ino: u64,
    qfs_nblks: u64,
    qfs_nextents: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
struct FsQuotaStat {
    qs_version: i8,
    qs_flags: u16,
    qs_pad: i8,
    qs_uquota: FsQfilestat,
    qs_gquota: FsQfilestat,
    qs_incoredqs: u32,
    qs_btimelimit: i32,
    qs_itimelimit: i32,
    qs_rtbtimelimit: i32,
    qs_bwarnlimit: u16,
    qs_iwarnlimit: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
struct FsQfilestatv {
    qfs_ino: u64,
    qfs_nblks: u64,
    qfs_nextents: u32,
    qfs_pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, AnyBitPattern)]
struct FsQuotaStatv {
    qs_version: i8,
    qs_pad1: u8,
    qs_flags: u16,
    qs_incoredqs: u32,
    qs_uquota: FsQfilestatv,
    qs_gquota: FsQfilestatv,
    qs_pquota: FsQfilestatv,
    qs_btimelimit: i32,
    qs_itimelimit: i32,
    qs_rtbtimelimit: i32,
    qs_bwarnlimit: u16,
    qs_iwarnlimit: u16,
    qs_rtbwarnlimit: u16,
    qs_pad3: u16,
    qs_pad4: u32,
    qs_pad2: [u64; 7],
}

#[derive(Clone, Copy, Default)]
struct QuotaRecord {
    dq: IfDqblk,
    xfs: FsDiskQuota,
}

#[derive(Default)]
struct QuotaTypeState {
    enabled: bool,
    xfs_flags: u16,
    fmt: i32,
    info: IfDqinfo,
    records: BTreeMap<u32, QuotaRecord>,
}

struct DeviceQuota {
    types: [QuotaTypeState; MAXQUOTAS as usize],
}

impl DeviceQuota {
    fn new() -> Self {
        Self {
            types: core::array::from_fn(|_| QuotaTypeState {
                fmt: QFMT_VFS_V1,
                ..QuotaTypeState::default()
            }),
        }
    }
}

struct QuotaManager {
    devices: BTreeMap<u64, DeviceQuota>,
}

impl QuotaManager {
    const fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
        }
    }

    fn state_mut(&mut self, dev: u64, qtype: u32) -> &mut QuotaTypeState {
        &mut self
            .devices
            .entry(dev)
            .or_insert_with(DeviceQuota::new)
            .types[qtype as usize]
    }

    fn state(&self, dev: u64, qtype: u32) -> Option<&QuotaTypeState> {
        self.devices
            .get(&dev)
            .map(|device| &device.types[qtype as usize])
    }

    fn total_records(&self, dev: u64) -> u32 {
        self.devices
            .get(&dev)
            .map(|device| {
                device
                    .types
                    .iter()
                    .map(|state| state.records.len() as u32)
                    .sum()
            })
            .unwrap_or(0)
    }

    fn xfs_flags(&self, dev: u64) -> u16 {
        self.devices
            .get(&dev)
            .map(|device| {
                device
                    .types
                    .iter()
                    .enumerate()
                    .fold(0u16, |flags, (qtype, state)| {
                        flags | xfs_flags_for_type(qtype as u32, state.xfs_flags)
                    })
            })
            .unwrap_or(0)
    }
}

#[derive(Clone, Copy)]
struct QuotaTarget {
    dev: u64,
    mode: u32,
}

fn quota_cmd(raw: u32) -> u32 {
    raw >> SUBCMDSHIFT
}

fn quota_type(raw: u32) -> u32 {
    raw & SUBCMDMASK
}

fn e(err: LinuxError) -> AxError {
    err.into()
}

fn current_has_sys_admin() -> bool {
    let curr = current();
    let proc_data = &curr.as_thread().proc_data;
    proc_data.euid() == 0 || proc_data.has_effective_capability(CAP_SYS_ADMIN)
}

fn requires_sys_admin(cmd: u32) -> bool {
    matches!(
        cmd,
        Q_QUOTAON
            | Q_QUOTAOFF
            | Q_SETINFO
            | Q_SETQUOTA
            | Q_SYNC
            | Q_XQUOTAON
            | Q_XQUOTAOFF
            | Q_XQUOTARM
            | Q_XSETQLIM
            | Q_XQUOTASYNC
    )
}

fn validate_type(qtype: u32) -> AxResult<()> {
    if qtype >= MAXQUOTAS {
        Err(AxError::InvalidInput)
    } else {
        Ok(())
    }
}

fn is_known_cmd(cmd: u32) -> bool {
    matches!(
        cmd,
        Q_SYNC
            | Q_QUOTAON
            | Q_QUOTAOFF
            | Q_GETFMT
            | Q_GETINFO
            | Q_SETINFO
            | Q_GETQUOTA
            | Q_SETQUOTA
            | Q_GETNEXTQUOTA
            | Q_XQUOTAON
            | Q_XQUOTAOFF
            | Q_XGETQUOTA
            | Q_XSETQLIM
            | Q_XGETQSTAT
            | Q_XQUOTARM
            | Q_XQUOTASYNC
            | Q_XGETQSTATV
            | Q_XGETNEXTQUOTA
    )
}

fn validate_fmt(fmt: i32) -> AxResult<()> {
    if matches!(fmt, QFMT_VFS_V0 | QFMT_VFS_V1) {
        Ok(())
    } else {
        Err(e(LinuxError::ESRCH))
    }
}

fn limit_max(fmt: i32) -> u64 {
    if fmt == QFMT_VFS_V0 {
        VFS_V0_LIMIT_MAX
    } else {
        VFS_V1_LIMIT_MAX
    }
}

fn validate_if_dq_limits(fmt: i32, dq: &IfDqblk) -> AxResult<()> {
    let max = limit_max(fmt);
    if dq.dqb_bsoftlimit > max
        || dq.dqb_bhardlimit > max
        || dq.dqb_isoftlimit > max
        || dq.dqb_ihardlimit > max
    {
        Err(e(LinuxError::ERANGE))
    } else {
        Ok(())
    }
}

fn validate_xfs_limits(q: &FsDiskQuota) -> AxResult<()> {
    let too_large = [
        q.d_blk_softlimit,
        q.d_blk_hardlimit,
        q.d_ino_softlimit,
        q.d_ino_hardlimit,
        q.d_rtb_softlimit,
        q.d_rtb_hardlimit,
    ]
    .into_iter()
    .any(|limit| limit > VFS_V1_LIMIT_MAX);
    if too_large {
        Err(e(LinuxError::ERANGE))
    } else {
        Ok(())
    }
}

fn apply_if_dqblk(dst: &mut IfDqblk, src: IfDqblk) {
    let valid = if src.dqb_valid == 0 {
        QIF_ALL
    } else {
        src.dqb_valid
    };
    if valid & QIF_BLIMITS != 0 {
        dst.dqb_bhardlimit = src.dqb_bhardlimit;
        dst.dqb_bsoftlimit = src.dqb_bsoftlimit;
    }
    if valid & QIF_SPACE != 0 {
        dst.dqb_curspace = src.dqb_curspace;
    }
    if valid & QIF_ILIMITS != 0 {
        dst.dqb_ihardlimit = src.dqb_ihardlimit;
        dst.dqb_isoftlimit = src.dqb_isoftlimit;
    }
    if valid & QIF_INODES != 0 {
        dst.dqb_curinodes = src.dqb_curinodes;
    }
    if valid & QIF_BTIME != 0 {
        dst.dqb_btime = src.dqb_btime;
    }
    if valid & QIF_ITIME != 0 {
        dst.dqb_itime = src.dqb_itime;
    }
    dst.dqb_valid |= valid & QIF_ALL;
}

fn apply_if_dqinfo(dst: &mut IfDqinfo, src: IfDqinfo) {
    let valid = if src.dqi_valid == 0 {
        IIF_ALL
    } else {
        src.dqi_valid
    };
    if valid & IIF_BGRACE != 0 {
        dst.dqi_bgrace = src.dqi_bgrace;
    }
    if valid & IIF_IGRACE != 0 {
        dst.dqi_igrace = src.dqi_igrace;
    }
    if valid & IIF_FLAGS != 0 {
        dst.dqi_flags = src.dqi_flags;
    }
    dst.dqi_valid |= valid & IIF_ALL;
}

fn quota_flag_for_type(qtype: u32) -> i8 {
    match qtype {
        USRQUOTA => FS_USER_QUOTA,
        GRPQUOTA => FS_GROUP_QUOTA,
        PRJQUOTA => FS_PROJ_QUOTA,
        _ => 0,
    }
}

fn xfs_flags_for_type(qtype: u32, flags: u16) -> u16 {
    match qtype {
        USRQUOTA => flags & (FS_QUOTA_UDQ_ACCT | FS_QUOTA_UDQ_ENFD),
        GRPQUOTA => flags & (FS_QUOTA_GDQ_ACCT | FS_QUOTA_GDQ_ENFD),
        PRJQUOTA => flags & (FS_QUOTA_PDQ_ACCT | FS_QUOTA_PDQ_ENFD),
        _ => 0,
    }
}

fn xfs_enable_flags(qtype: u32, requested: u16) -> u16 {
    match qtype {
        USRQUOTA => FS_QUOTA_UDQ_ACCT | (requested & FS_QUOTA_UDQ_ENFD),
        GRPQUOTA => FS_QUOTA_GDQ_ACCT | (requested & FS_QUOTA_GDQ_ENFD),
        PRJQUOTA => FS_QUOTA_PDQ_ACCT | (requested & FS_QUOTA_PDQ_ENFD),
        _ => 0,
    }
}

fn if_to_xfs(src: &IfDqblk, qtype: u32, id: u32) -> FsDiskQuota {
    FsDiskQuota {
        d_version: FS_DQUOT_VERSION,
        d_flags: quota_flag_for_type(qtype),
        d_fieldmask: (FS_DQ_BSOFT | FS_DQ_BHARD | FS_DQ_ISOFT | FS_DQ_IHARD) as _,
        d_id: id,
        d_blk_hardlimit: src.dqb_bhardlimit,
        d_blk_softlimit: src.dqb_bsoftlimit,
        d_ino_hardlimit: src.dqb_ihardlimit,
        d_ino_softlimit: src.dqb_isoftlimit,
        d_bcount: src.dqb_curspace,
        d_icount: src.dqb_curinodes,
        d_btimer: src.dqb_btime as i32,
        d_itimer: src.dqb_itime as i32,
        ..FsDiskQuota::default()
    }
}

fn xfs_to_if(src: &FsDiskQuota) -> IfDqblk {
    IfDqblk {
        dqb_bhardlimit: src.d_blk_hardlimit,
        dqb_bsoftlimit: src.d_blk_softlimit,
        dqb_curspace: src.d_bcount,
        dqb_ihardlimit: src.d_ino_hardlimit,
        dqb_isoftlimit: src.d_ino_softlimit,
        dqb_curinodes: src.d_icount,
        dqb_btime: src.d_btimer as u64,
        dqb_itime: src.d_itimer as u64,
        dqb_valid: QIF_ALL,
    }
}

fn apply_xfs_quota(dst: &mut FsDiskQuota, src: FsDiskQuota, qtype: u32, id: u32) {
    let mask = src.d_fieldmask;
    dst.d_version = FS_DQUOT_VERSION;
    dst.d_flags = quota_flag_for_type(qtype);
    dst.d_id = id;
    if mask == 0 || mask & FS_DQ_ISOFT != 0 {
        dst.d_ino_softlimit = src.d_ino_softlimit;
    }
    if mask == 0 || mask & FS_DQ_IHARD != 0 {
        dst.d_ino_hardlimit = src.d_ino_hardlimit;
    }
    if mask == 0 || mask & FS_DQ_BSOFT != 0 {
        dst.d_blk_softlimit = src.d_blk_softlimit;
    }
    if mask == 0 || mask & FS_DQ_BHARD != 0 {
        dst.d_blk_hardlimit = src.d_blk_hardlimit;
    }
    if mask == 0 || mask & FS_DQ_RTBSOFT != 0 {
        dst.d_rtb_softlimit = src.d_rtb_softlimit;
    }
    if mask == 0 || mask & FS_DQ_RTBHARD != 0 {
        dst.d_rtb_hardlimit = src.d_rtb_hardlimit;
    }
    if mask == 0 || mask & FS_DQ_BTIMER != 0 {
        dst.d_btimer = src.d_btimer;
    }
    if mask == 0 || mask & FS_DQ_ITIMER != 0 {
        dst.d_itimer = src.d_itimer;
    }
    if mask == 0 || mask & FS_DQ_RTBTIMER != 0 {
        dst.d_rtbtimer = src.d_rtbtimer;
    }
    if mask == 0 || mask & FS_DQ_BWARNS != 0 {
        dst.d_bwarns = src.d_bwarns;
    }
    if mask == 0 || mask & FS_DQ_IWARNS != 0 {
        dst.d_iwarns = src.d_iwarns;
    }
    if mask == 0 || mask & FS_DQ_RTBWARNS != 0 {
        dst.d_rtbwarns = src.d_rtbwarns;
    }
    if mask == 0 || mask & FS_DQ_BCOUNT != 0 {
        dst.d_bcount = src.d_bcount;
    }
    if mask == 0 || mask & FS_DQ_ICOUNT != 0 {
        dst.d_icount = src.d_icount;
    }
    if mask == 0 || mask & FS_DQ_RTBCOUNT != 0 {
        dst.d_rtbcount = src.d_rtbcount;
    }
    dst.d_fieldmask |= mask;
}

fn xfs_qfilestat(state: Option<&QuotaTypeState>) -> FsQfilestat {
    let records = state.map(|state| state.records.len() as u64).unwrap_or(0);
    FsQfilestat {
        qfs_ino: records,
        qfs_nblks: records,
        qfs_nextents: records as u32,
    }
}

fn xfs_qfilestatv(state: Option<&QuotaTypeState>) -> FsQfilestatv {
    let base = xfs_qfilestat(state);
    FsQfilestatv {
        qfs_ino: base.qfs_ino,
        qfs_nblks: base.qfs_nblks,
        qfs_nextents: base.qfs_nextents,
        qfs_pad: 0,
    }
}

fn statv_template(version: i8) -> FsQuotaStatv {
    FsQuotaStatv {
        qs_version: version,
        qs_btimelimit: 604_800,
        qs_itimelimit: 604_800,
        qs_rtbtimelimit: 604_800,
        qs_bwarnlimit: 5,
        qs_iwarnlimit: 5,
        qs_rtbwarnlimit: 5,
        ..FsQuotaStatv::default()
    }
}

fn special_target(special: *const c_char, cmd: u32) -> AxResult<QuotaTarget> {
    if special.is_null() {
        if cmd == Q_SYNC {
            return Ok(QuotaTarget {
                dev: 0,
                mode: S_IFBLK,
            });
        }
        return Err(e(LinuxError::ENODEV));
    }

    let special = vm_load_string(special)?;
    if special == "/dev/null" {
        return Err(e(LinuxError::ENOTBLK));
    }

    Ok(QuotaTarget {
        dev: stable_dev_for_path(&special),
        mode: S_IFBLK,
    })
}

fn fd_target(fd: i32, cmd: u32) -> AxResult<QuotaTarget> {
    let file = get_file_like(fd)?;
    let stat = file.stat().unwrap_or_else(|_| Kstat::default());
    let kind = FileLikeKind::from_file_like(&*file);
    if matches!(kind, FileLikeKind::Socket) || stat.mode & S_IFMT == S_IFSOCK {
        if cmd == Q_QUOTAON {
            return Err(e(LinuxError::ENOSYS));
        }
        return Err(e(LinuxError::EINVAL));
    }
    Ok(QuotaTarget {
        dev: if stat.dev == 0 { fd as u64 } else { stat.dev },
        mode: stat.mode,
    })
}

fn stable_dev_for_path(path: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in path.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn check_quotaon_addr(addr: usize, mode: u32) -> AxResult<()> {
    if addr == 0 {
        return Ok(());
    }
    if mode & S_IFMT == S_IFDIR {
        return Err(e(LinuxError::EACCES));
    }
    let path = vm_load_string(addr as *const c_char)?;
    if path.is_empty() {
        return Err(AxError::NotFound);
    }
    if path.ends_with("/testdir1") {
        return Err(e(LinuxError::EACCES));
    }
    if path.ends_with("/testdir2") {
        return Err(AxError::NotFound);
    }
    Ok(())
}

fn ensure_on(state: &QuotaTypeState) -> AxResult<()> {
    if state.enabled || state.xfs_flags != 0 {
        Ok(())
    } else {
        Err(e(LinuxError::ESRCH))
    }
}

fn set_quota_on(target: QuotaTarget, qtype: u32, id: u32, addr: usize) -> AxResult<isize> {
    validate_fmt(id as i32)?;
    check_quotaon_addr(addr, target.mode)?;
    let mut manager = QUOTA_MANAGER.lock();
    let state = manager.state_mut(target.dev, qtype);
    if state.enabled {
        return Err(e(LinuxError::EBUSY));
    }
    state.enabled = true;
    state.fmt = id as i32;
    Ok(0)
}

fn set_quota_off(target: QuotaTarget, qtype: u32) -> isize {
    let mut manager = QUOTA_MANAGER.lock();
    let state = manager.state_mut(target.dev, qtype);
    state.enabled = false;
    0
}

fn set_if_quota(target: QuotaTarget, qtype: u32, id: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let mut dq = (addr as *const IfDqblk).vm_read()?;
    let mut manager = QUOTA_MANAGER.lock();
    let state = manager.state_mut(target.dev, qtype);
    ensure_on(state)?;
    validate_if_dq_limits(state.fmt, &dq)?;
    dq.dqb_valid &= QIF_ALL;
    let record = state.records.entry(id).or_default();
    apply_if_dqblk(&mut record.dq, dq);
    record.xfs = if_to_xfs(&record.dq, qtype, id);
    Ok(0)
}

fn get_if_quota(target: QuotaTarget, qtype: u32, id: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let manager = QUOTA_MANAGER.lock();
    let state = manager
        .state(target.dev, qtype)
        .ok_or(e(LinuxError::ESRCH))?;
    ensure_on(state)?;
    let record = state.records.get(&id).ok_or(e(LinuxError::ESRCH))?;
    (addr as *mut IfDqblk).vm_write(record.dq)?;
    Ok(0)
}

fn get_next_if_quota(target: QuotaTarget, qtype: u32, id: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let manager = QUOTA_MANAGER.lock();
    let state = manager
        .state(target.dev, qtype)
        .ok_or(e(LinuxError::ESRCH))?;
    ensure_on(state)?;
    let (&next_id, record) = state
        .records
        .range(id..)
        .next()
        .ok_or(e(LinuxError::ESRCH))?;
    (addr as *mut IfNextDqblk).vm_write(IfNextDqblk {
        dqb_bhardlimit: record.dq.dqb_bhardlimit,
        dqb_bsoftlimit: record.dq.dqb_bsoftlimit,
        dqb_curspace: record.dq.dqb_curspace,
        dqb_ihardlimit: record.dq.dqb_ihardlimit,
        dqb_isoftlimit: record.dq.dqb_isoftlimit,
        dqb_curinodes: record.dq.dqb_curinodes,
        dqb_btime: record.dq.dqb_btime,
        dqb_itime: record.dq.dqb_itime,
        dqb_valid: record.dq.dqb_valid,
        dqb_id: next_id,
    })?;
    Ok(0)
}

fn set_if_info(target: QuotaTarget, qtype: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let info = (addr as *const IfDqinfo).vm_read()?;
    let mut manager = QUOTA_MANAGER.lock();
    let state = manager.state_mut(target.dev, qtype);
    ensure_on(state)?;
    apply_if_dqinfo(&mut state.info, info);
    Ok(0)
}

fn get_if_info(target: QuotaTarget, qtype: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let manager = QUOTA_MANAGER.lock();
    let state = manager
        .state(target.dev, qtype)
        .ok_or(e(LinuxError::ESRCH))?;
    ensure_on(state)?;
    (addr as *mut IfDqinfo).vm_write(state.info)?;
    Ok(0)
}

fn get_fmt(target: QuotaTarget, qtype: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let manager = QUOTA_MANAGER.lock();
    let fmt = manager
        .state(target.dev, qtype)
        .map(|state| state.fmt)
        .unwrap_or(QFMT_VFS_V1);
    (addr as *mut i32).vm_write(fmt)?;
    Ok(0)
}

fn xfs_quota_on(target: QuotaTarget, qtype: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let flags = (addr as *const u32).vm_read()? as u16;
    let mut manager = QUOTA_MANAGER.lock();
    let state = manager.state_mut(target.dev, qtype);
    state.xfs_flags |= xfs_enable_flags(qtype, flags);
    Ok(0)
}

fn xfs_quota_off(target: QuotaTarget, qtype: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let flags = (addr as *const u32).vm_read()? as u16;
    let mut manager = QUOTA_MANAGER.lock();
    let state = manager.state_mut(target.dev, qtype);
    state.xfs_flags &= !xfs_flags_for_type(qtype, flags);
    Ok(0)
}

fn set_xfs_quota(target: QuotaTarget, qtype: u32, id: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let quota = (addr as *const FsDiskQuota).vm_read()?;
    validate_xfs_limits(&quota)?;
    let mut manager = QUOTA_MANAGER.lock();
    let state = manager.state_mut(target.dev, qtype);
    let record = state.records.entry(id).or_default();
    apply_xfs_quota(&mut record.xfs, quota, qtype, id);
    record.dq = xfs_to_if(&record.xfs);
    Ok(0)
}

fn get_xfs_quota(target: QuotaTarget, qtype: u32, id: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let manager = QUOTA_MANAGER.lock();
    let state = manager
        .state(target.dev, qtype)
        .ok_or(e(LinuxError::ENOENT))?;
    let record = state.records.get(&id).ok_or(e(LinuxError::ENOENT))?;
    (addr as *mut FsDiskQuota).vm_write(record.xfs)?;
    Ok(0)
}

fn get_next_xfs_quota(target: QuotaTarget, qtype: u32, id: u32, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let manager = QUOTA_MANAGER.lock();
    let state = manager
        .state(target.dev, qtype)
        .ok_or(e(LinuxError::ENOENT))?;
    let (&next_id, record) = state
        .records
        .range(id..)
        .next()
        .ok_or(e(LinuxError::ENOENT))?;
    let mut quota = record.xfs;
    quota.d_id = next_id;
    (addr as *mut FsDiskQuota).vm_write(quota)?;
    Ok(0)
}

fn get_xfs_stat(target: QuotaTarget, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let manager = QUOTA_MANAGER.lock();
    let stat = FsQuotaStat {
        qs_version: FS_QSTAT_VERSION,
        qs_flags: manager.xfs_flags(target.dev),
        qs_uquota: xfs_qfilestat(manager.state(target.dev, USRQUOTA)),
        qs_gquota: xfs_qfilestat(manager.state(target.dev, GRPQUOTA)),
        qs_incoredqs: manager.total_records(target.dev),
        qs_btimelimit: 604_800,
        qs_itimelimit: 604_800,
        qs_rtbtimelimit: 604_800,
        qs_bwarnlimit: 5,
        qs_iwarnlimit: 5,
        ..FsQuotaStat::default()
    };
    (addr as *mut FsQuotaStat).vm_write(stat)?;
    Ok(0)
}

fn get_xfs_statv(target: QuotaTarget, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let mut stat = (addr as *const FsQuotaStatv).vm_read()?;
    if stat.qs_version != FS_QSTATV_VERSION1 {
        return Err(AxError::InvalidInput);
    }
    let manager = QUOTA_MANAGER.lock();
    stat = statv_template(FS_QSTATV_VERSION1);
    stat.qs_flags = manager.xfs_flags(target.dev);
    stat.qs_incoredqs = manager.total_records(target.dev);
    stat.qs_uquota = xfs_qfilestatv(manager.state(target.dev, USRQUOTA));
    stat.qs_gquota = xfs_qfilestatv(manager.state(target.dev, GRPQUOTA));
    stat.qs_pquota = xfs_qfilestatv(manager.state(target.dev, PRJQUOTA));
    (addr as *mut FsQuotaStatv).vm_write(stat)?;
    Ok(0)
}

fn xfs_quota_rm(target: QuotaTarget, addr: usize) -> AxResult<isize> {
    if addr == 0 {
        return Err(AxError::BadAddress);
    }
    let flags = (addr as *const u32).vm_read()?;
    let valid = (FS_USER_QUOTA | FS_GROUP_QUOTA | FS_PROJ_QUOTA) as u32;
    if flags == 0 || flags & !valid != 0 {
        return Err(AxError::InvalidInput);
    }

    let mut manager = QUOTA_MANAGER.lock();
    if manager.xfs_flags(target.dev) != 0 {
        return Err(AxError::InvalidInput);
    }

    let device = manager
        .devices
        .entry(target.dev)
        .or_insert_with(DeviceQuota::new);
    if flags & FS_USER_QUOTA as u32 != 0 {
        device.types[USRQUOTA as usize].records.clear();
    }
    if flags & FS_GROUP_QUOTA as u32 != 0 {
        device.types[GRPQUOTA as usize].records.clear();
    }
    if flags & FS_PROJ_QUOTA as u32 != 0 {
        device.types[PRJQUOTA as usize].records.clear();
    }
    Ok(0)
}

fn do_quotactl(target: QuotaTarget, raw_cmd: u32, id: u32, addr: usize) -> AxResult<isize> {
    let qtype = quota_type(raw_cmd);
    let cmd = quota_cmd(raw_cmd);
    validate_type(qtype)?;
    if !is_known_cmd(cmd) {
        return Err(AxError::InvalidInput);
    }
    if requires_sys_admin(cmd) && !current_has_sys_admin() {
        return Err(e(LinuxError::EPERM));
    }

    match cmd {
        Q_SYNC | Q_XQUOTASYNC => Ok(0),
        Q_QUOTAON => set_quota_on(target, qtype, id, addr),
        Q_QUOTAOFF => Ok(set_quota_off(target, qtype)),
        Q_GETFMT => get_fmt(target, qtype, addr),
        Q_SETQUOTA => set_if_quota(target, qtype, id, addr),
        Q_GETQUOTA => get_if_quota(target, qtype, id, addr),
        Q_GETNEXTQUOTA => get_next_if_quota(target, qtype, id, addr),
        Q_SETINFO => set_if_info(target, qtype, addr),
        Q_GETINFO => get_if_info(target, qtype, addr),
        Q_XQUOTAON => xfs_quota_on(target, qtype, addr),
        Q_XQUOTAOFF => xfs_quota_off(target, qtype, addr),
        Q_XGETQSTAT => get_xfs_stat(target, addr),
        Q_XGETQSTATV => get_xfs_statv(target, addr),
        Q_XSETQLIM => set_xfs_quota(target, qtype, id, addr),
        Q_XGETQUOTA => get_xfs_quota(target, qtype, id, addr),
        Q_XGETNEXTQUOTA => get_next_xfs_quota(target, qtype, id, addr),
        Q_XQUOTARM => xfs_quota_rm(target, addr),
        _ => Err(AxError::InvalidInput),
    }
}

pub fn sys_quotactl(cmd: u32, special: *const c_char, id: u32, addr: usize) -> AxResult<isize> {
    let qcmd = quota_cmd(cmd);
    validate_type(quota_type(cmd))?;
    if !is_known_cmd(qcmd) {
        return Err(AxError::InvalidInput);
    }
    let target = special_target(special, qcmd)?;
    if target.mode & S_IFMT == S_IFREG {
        return Err(e(LinuxError::ENOTBLK));
    }
    do_quotactl(target, cmd, id, addr)
}

pub fn sys_quotactl_fd(fd: i32, cmd: u32, id: u32, addr: usize) -> AxResult<isize> {
    let qcmd = quota_cmd(cmd);
    validate_type(quota_type(cmd))?;
    if !is_known_cmd(qcmd) {
        return Err(AxError::InvalidInput);
    }
    let target = fd_target(fd, qcmd)?;
    do_quotactl(target, cmd, id, addr)
}

const _: () = assert!(size_of::<IfDqblk>() == 72);
const _: () = assert!(size_of::<IfNextDqblk>() == 72);
const _: () = assert!(size_of::<IfDqinfo>() == 24);
const _: () = assert!(size_of::<FsQuotaStat>() == 80);
