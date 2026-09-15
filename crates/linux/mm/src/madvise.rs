//! Pure `madvise(2)` advice-validity tables.
//!
//! Mirrors Linux v7.2.3 `mm/madvise.c:madvise_behavior_valid()`,
//! `process_madvise_remote_valid()`, `mm/ksm.c:ksm_madvise()` and
//! `mm/madvise.c:madvise_inject_error()`.

/// Whether an advice value is accepted by this kernel at all.
///
/// Linux compiles several advice values out of `madvise_behavior_valid()`
/// entirely, and an advice value that is compiled out returns `-EINVAL` — it is
/// not a silent no-op.  This kernel has no KSM and no page-migration-based soft
/// offline, so the corresponding advice values must be refused with that same
/// errno instead of being reported as successful.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdviceAvailability {
    /// Linux accepts the value and this kernel implements its effect.
    Impl,
    /// Linux accepts the value only with a configuration option this kernel
    /// does not provide, so the syscall returns `-EINVAL`.
    Unavailable,
}

/// Linux `madvise` advice values, including the ones this kernel must refuse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Advice {
    /// `MADV_NORMAL`
    Normal,
    /// `MADV_RANDOM`
    Random,
    /// `MADV_SEQUENTIAL`
    Sequential,
    /// `MADV_WILLNEED`
    WillNeed,
    /// `MADV_DONTNEED`
    DontNeed,
    /// `MADV_FREE`
    Free,
    /// `MADV_REMOVE`
    Remove,
    /// `MADV_DONTFORK`
    DontFork,
    /// `MADV_DOFORK`
    DoFork,
    /// `MADV_MERGEABLE` (`CONFIG_KSM`)
    Mergeable,
    /// `MADV_UNMERGEABLE` (`CONFIG_KSM`)
    Unmergeable,
    /// `MADV_HUGEPAGE` (`CONFIG_TRANSPARENT_HUGEPAGE`)
    HugePage,
    /// `MADV_NOHUGEPAGE` (`CONFIG_TRANSPARENT_HUGEPAGE`)
    NoHugePage,
    /// `MADV_DONTDUMP`
    DontDump,
    /// `MADV_DODUMP`
    DoDump,
    /// `MADV_WIPEONFORK`
    WipeOnFork,
    /// `MADV_KEEPONFORK`
    KeepOnFork,
    /// `MADV_COLD`
    Cold,
    /// `MADV_PAGEOUT`
    PageOut,
    /// `MADV_POPULATE_READ`
    PopulateRead,
    /// `MADV_POPULATE_WRITE`
    PopulateWrite,
    /// `MADV_DONTNEED_LOCKED`
    DontNeedLocked,
    /// `MADV_COLLAPSE` (`CONFIG_TRANSPARENT_HUGEPAGE`)
    Collapse,
    /// `MADV_GUARD_INSTALL`
    GuardInstall,
    /// `MADV_GUARD_REMOVE`
    GuardRemove,
    /// `MADV_HWPOISON` (`CONFIG_MEMORY_FAILURE`)
    HwPoison,
    /// `MADV_SOFT_OFFLINE` (`CONFIG_MEMORY_FAILURE`)
    SoftOffline,
}

macro_rules! advice_constants {
    ($($name:ident => $value:expr),* $(,)?) => {
        $(pub const $name: u32 = $value;)*
    };
}

advice_constants! {
    MADV_NORMAL => 0,
    MADV_RANDOM => 1,
    MADV_SEQUENTIAL => 2,
    MADV_WILLNEED => 3,
    MADV_DONTNEED => 4,
    MADV_FREE => 8,
    MADV_REMOVE => 9,
    MADV_DONTFORK => 10,
    MADV_DOFORK => 11,
    MADV_MERGEABLE => 12,
    MADV_UNMERGEABLE => 13,
    MADV_HUGEPAGE => 14,
    MADV_NOHUGEPAGE => 15,
    MADV_DONTDUMP => 16,
    MADV_DODUMP => 17,
    MADV_WIPEONFORK => 18,
    MADV_KEEPONFORK => 19,
    MADV_COLD => 20,
    MADV_PAGEOUT => 21,
    MADV_POPULATE_READ => 22,
    MADV_POPULATE_WRITE => 23,
    MADV_DONTNEED_LOCKED => 24,
    MADV_COLLAPSE => 25,
    MADV_HWPOISON => 100,
    MADV_SOFT_OFFLINE => 101,
    MADV_GUARD_INSTALL => 102,
    MADV_GUARD_REMOVE => 103,
}

impl Advice {
    /// Decodes a raw advice value.  Returns `None` exactly where Linux's
    /// `madvise_behavior_valid()` falls through to `default: return false`.
    pub const fn from_raw(advice: u32) -> Option<Self> {
        Some(match advice {
            MADV_NORMAL => Self::Normal,
            MADV_RANDOM => Self::Random,
            MADV_SEQUENTIAL => Self::Sequential,
            MADV_WILLNEED => Self::WillNeed,
            MADV_DONTNEED => Self::DontNeed,
            MADV_FREE => Self::Free,
            MADV_REMOVE => Self::Remove,
            MADV_DONTFORK => Self::DontFork,
            MADV_DOFORK => Self::DoFork,
            MADV_MERGEABLE => Self::Mergeable,
            MADV_UNMERGEABLE => Self::Unmergeable,
            MADV_HUGEPAGE => Self::HugePage,
            MADV_NOHUGEPAGE => Self::NoHugePage,
            MADV_DONTDUMP => Self::DontDump,
            MADV_DODUMP => Self::DoDump,
            MADV_WIPEONFORK => Self::WipeOnFork,
            MADV_KEEPONFORK => Self::KeepOnFork,
            MADV_COLD => Self::Cold,
            MADV_PAGEOUT => Self::PageOut,
            MADV_POPULATE_READ => Self::PopulateRead,
            MADV_POPULATE_WRITE => Self::PopulateWrite,
            MADV_DONTNEED_LOCKED => Self::DontNeedLocked,
            MADV_COLLAPSE => Self::Collapse,
            MADV_HWPOISON => Self::HwPoison,
            MADV_SOFT_OFFLINE => Self::SoftOffline,
            MADV_GUARD_INSTALL => Self::GuardInstall,
            MADV_GUARD_REMOVE => Self::GuardRemove,
            _ => return None,
        })
    }

    /// Raw Linux advice value.
    pub const fn raw(self) -> u32 {
        match self {
            Self::Normal => MADV_NORMAL,
            Self::Random => MADV_RANDOM,
            Self::Sequential => MADV_SEQUENTIAL,
            Self::WillNeed => MADV_WILLNEED,
            Self::DontNeed => MADV_DONTNEED,
            Self::Free => MADV_FREE,
            Self::Remove => MADV_REMOVE,
            Self::DontFork => MADV_DONTFORK,
            Self::DoFork => MADV_DOFORK,
            Self::Mergeable => MADV_MERGEABLE,
            Self::Unmergeable => MADV_UNMERGEABLE,
            Self::HugePage => MADV_HUGEPAGE,
            Self::NoHugePage => MADV_NOHUGEPAGE,
            Self::DontDump => MADV_DONTDUMP,
            Self::DoDump => MADV_DODUMP,
            Self::WipeOnFork => MADV_WIPEONFORK,
            Self::KeepOnFork => MADV_KEEPONFORK,
            Self::Cold => MADV_COLD,
            Self::PageOut => MADV_PAGEOUT,
            Self::PopulateRead => MADV_POPULATE_READ,
            Self::PopulateWrite => MADV_POPULATE_WRITE,
            Self::DontNeedLocked => MADV_DONTNEED_LOCKED,
            Self::Collapse => MADV_COLLAPSE,
            Self::HwPoison => MADV_HWPOISON,
            Self::SoftOffline => MADV_SOFT_OFFLINE,
            Self::GuardInstall => MADV_GUARD_INSTALL,
            Self::GuardRemove => MADV_GUARD_REMOVE,
        }
    }

    /// Whether this kernel accepts the advice value.
    ///
    /// Linux `mm/madvise.c:madvise_behavior_valid()` compiles advice values out
    /// of its accepted table with `#ifdef`s, and a compiled-out value reaches
    /// `do_madvise()`'s
    ///
    /// ```c
    /// 	if (!madvise_behavior_valid(behavior))
    /// 		return -EINVAL;
    /// ```
    ///
    /// so it is `-EINVAL`, never a silent success.  The reference oracle runs
    /// with `CONFIG_KSM=n`, `CONFIG_TRANSPARENT_HUGEPAGE=n` and
    /// `CONFIG_MEMORY_FAILURE=n`, so the compiled-out set is exactly:
    ///
    /// ```c
    /// #ifdef CONFIG_KSM
    /// 	case MADV_MERGEABLE:
    /// 	case MADV_UNMERGEABLE:
    /// #endif
    /// #ifdef CONFIG_TRANSPARENT_HUGEPAGE
    /// 	case MADV_HUGEPAGE:
    /// 	case MADV_NOHUGEPAGE:
    /// 	case MADV_COLLAPSE:
    /// #endif
    /// #ifdef CONFIG_MEMORY_FAILURE
    /// 	case MADV_SOFT_OFFLINE:
    /// 	case MADV_HWPOISON:
    /// #endif
    /// ```
    ///
    /// The stubs those configurations leave behind are unreachable from
    /// `madvise(2)`: `mm/ksm.h`'s `!CONFIG_KSM` `ksm_madvise()` and
    /// `mm/madvise.c`'s `!CONFIG_MEMORY_FAILURE` `madvise_inject_error()` both
    /// return 0, but neither is called because the behavior table already
    /// rejected the value first.  Reporting success here would be the
    /// dangerous half of that pair: a caller that believed `MADV_HWPOISON` had
    /// injected an error would mis-attribute a later `SIGBUS`, and one that
    /// believed `MADV_SOFT_OFFLINE` had preserved a page would lose the
    /// contents this kernel's only local primitive (retire the resident pages)
    /// destroys.
    pub const fn availability(self) -> AdviceAvailability {
        match self {
            Self::Mergeable
            | Self::Unmergeable
            | Self::HugePage
            | Self::NoHugePage
            | Self::Collapse
            | Self::HwPoison
            | Self::SoftOffline => AdviceAvailability::Unavailable,
            _ => AdviceAvailability::Impl,
        }
    }

    /// Whether Linux accepts the advice value at all.
    pub const fn valid(self) -> bool {
        !matches!(self.availability(), AdviceAvailability::Unavailable)
    }
}

/// Linux `madvise_behavior_valid()` for a raw value.
pub const fn advice_valid(advice: u32) -> bool {
    match Advice::from_raw(advice) {
        Some(advice) => advice.valid(),
        None => false,
    }
}

/// Linux v7.2.3 `mm/madvise.c:process_madvise_remote_valid()`:
///
/// ```c
/// static bool process_madvise_remote_valid(int behavior)
/// {
/// 	switch (behavior) {
/// 	case MADV_COLD:
/// 	case MADV_PAGEOUT:
/// 	case MADV_WILLNEED:
/// 	case MADV_COLLAPSE:
/// 		return true;
/// 	default:
/// 		return false;
/// 	}
/// }
/// ```
///
/// Consulted only when the target mm is not the caller's own mm, in which case
/// a value outside this table fails with `-EINVAL` before the `CAP_SYS_NICE`
/// check.
pub const fn process_madvise_remote_valid(advice: u32) -> bool {
    matches!(
        advice,
        MADV_COLD | MADV_PAGEOUT | MADV_WILLNEED | MADV_COLLAPSE
    )
}
