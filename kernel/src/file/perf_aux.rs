//! Exact, opt-in perf AUX admission and publication state.
//!
//! AUX is deliberately not a fallback for the ordinary perf data ring.  A
//! request is accepted only on the committed Panther Lake product PMU, and
//! only after the platform has identified a concrete PT or BTS backend.

use axerrno::{AxError, AxResult};
use thekernel_linux_perf::{
    ATTR_EXCLUDE_KERNEL, ATTR_EXCLUDE_USER, ATTR_PRECISE_IP, PERF_AUX_ACTION_ALL,
    PERF_AUX_FLAG_OVERWRITE, PERF_AUX_FLAG_TRUNCATED, PERF_SAMPLE_AUX, PERF_SAMPLE_BRANCH_STACK,
    PerfEventAttr, PerfEventAttrV0,
};

/// Linux's perf mmap metadata locations.  They follow the ordinary data-ring
/// fields and are therefore safe to expose through the same metadata page
/// while the AUX bytes themselves live in a distinct mapping.
pub(crate) const AUX_HEAD: usize = 1056;
pub(crate) const AUX_TAIL: usize = 1064;
pub(crate) const AUX_OFFSET: usize = 1072;
pub(crate) const AUX_SIZE: usize = 1080;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuxMode {
    Snapshot,
    Overwrite,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuxBackend {
    IntelPt,
    Bts,
}

/// The V0-visible portion of a precise/AUX request.  Later attr fields are
/// admitted by the full attr path before this object is constructed; this
/// structure deliberately does not invent filter encodings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuxRequest {
    /// `attr.precise_ip` is a two-bit level, not a boolean.  Panther Lake's
    /// committed PEBS path promises only level 1.
    pub(crate) precise_ip: u8,
    pub(crate) branch_stack: bool,
    pub(crate) aux: bool,
    pub(crate) mode: AuxMode,
    pub(crate) branch_sample_type: u64,
    pub(crate) watermark: u32,
    pub(crate) sample_size: u32,
    pub(crate) action: u32,
    /// Intel PT's native `config` word.  `config1`/`config2` are retained
    /// explicitly so they can be rejected for PT/BTS instead of silently
    /// being mistaken for address filters (Linux address filters use ioctl).
    pub(crate) config: u64,
    pub(crate) config1: u64,
    pub(crate) config2: u64,
    pub(crate) trace_user: bool,
    pub(crate) trace_kernel: bool,
    /// A dynamic `intel_pt`/`intel_bts` PMU type is an explicit backend
    /// request.  Generic sampling events leave this unset and use the one
    /// CPUID-discovered transport.
    requested_backend: Option<AuxBackend>,
}

impl AuxRequest {
    /// MODIFY_ATTRIBUTES may change the record field selection, but it must
    /// not replace an AUX transport or its hardware configuration after the
    /// descriptor has been admitted.  A branch-stack bit by itself is an
    /// exact-capture request, not an AUX transport replacement.
    pub(crate) fn compatible_with_modify(self, candidate: Option<Self>) -> bool {
        match candidate {
            None => !self.aux && self.precise_ip == 0,
            Some(candidate) => {
                self.precise_ip == candidate.precise_ip
                    && self.aux == candidate.aux
                    && self.mode == candidate.mode
                    && self.branch_sample_type == candidate.branch_sample_type
                    && self.watermark == candidate.watermark
                    && self.sample_size == candidate.sample_size
                    && self.action == candidate.action
                    && self.config == candidate.config
                    && self.config1 == candidate.config1
                    && self.config2 == candidate.config2
                    && self.trace_user == candidate.trace_user
                    && self.trace_kernel == candidate.trace_kernel
                    && self.requested_backend == candidate.requested_backend
            }
        }
    }

    pub(crate) fn from_attr(attr: &PerfEventAttr, size: u32) -> Option<Self> {
        let mut request = Self::from_v0(&PerfEventAttrV0::from(*attr))?;
        if size >= 80 {
            request.branch_sample_type = attr.branch_sample_type;
        }
        if size >= 112 {
            request.watermark = attr.aux_watermark;
        }
        if size >= 120 {
            request.sample_size = attr.aux_sample_size;
            request.action = attr.aux_action;
        }
        request.config = attr.config;
        if size >= 64 {
            request.config1 = attr.config1;
        }
        if size >= 72 {
            request.config2 = attr.config2;
        }
        request.trace_user = attr.flags & ATTR_EXCLUDE_USER == 0;
        request.trace_kernel = attr.flags & ATTR_EXCLUDE_KERNEL == 0;
        request.requested_backend = requested_backend(attr.event_type);
        Some(request)
    }

    pub(crate) fn from_v0(attr: &PerfEventAttrV0) -> Option<Self> {
        let precise_ip = ((attr.flags & ATTR_PRECISE_IP) >> 15) as u8;
        let branch_stack = attr.sample_type & PERF_SAMPLE_BRANCH_STACK != 0;
        let aux = attr.sample_type & PERF_SAMPLE_AUX != 0;
        if precise_ip == 0 && !branch_stack && !aux {
            return None;
        }
        Some(Self {
            precise_ip,
            branch_stack,
            aux,
            // V0 has no AUX action word.  Its only truthful mode is the
            // non-overwrite/snapshot transport.
            mode: AuxMode::Snapshot,
            branch_sample_type: 0,
            watermark: 0,
            sample_size: 0,
            action: 0,
            config: attr.config,
            config1: attr.config1,
            config2: 0,
            trace_user: attr.flags & ATTR_EXCLUDE_USER == 0,
            trace_kernel: attr.flags & ATTR_EXCLUDE_KERNEL == 0,
            requested_backend: requested_backend(attr.event_type),
        })
    }

    /// Gate every exact facility separately.  In particular, a generic
    /// architectural PMU cannot use precise-IP merely because it can count.
    pub(crate) fn admit(self) -> AxResult<Option<AuxBackend>> {
        #[cfg(all(feature = "pmu", target_os = "none"))]
        {
            if self.action & !PERF_AUX_ACTION_ALL != 0 {
                return Err(AxError::InvalidInput);
            }
            if self.precise_ip > 1 {
                return Err(AxError::OperationNotSupported);
            }
            if self.precise_ip == 1 && axhal::perf_precise_aux::precise_ip_admitted(true).is_err() {
                return Err(AxError::OperationNotSupported);
            }
            if self.branch_stack && !axhal::perf_precise_aux::lbr_supported() {
                return Err(AxError::OperationNotSupported);
            }
            if self.aux {
                let backend = axhal::perf_precise_aux::discover_aux_backend()
                    .map_err(|_| AxError::OperationNotSupported)?;
                let backend = match (self.requested_backend, backend) {
                    (Some(AuxBackend::IntelPt), axhal::perf_precise_aux::AuxBackend::IntelPt) => {
                        axhal::perf_precise_aux::AuxBackend::IntelPt
                    }
                    (Some(AuxBackend::Bts), axhal::perf_precise_aux::AuxBackend::Bts) => {
                        axhal::perf_precise_aux::AuxBackend::Bts
                    }
                    (Some(_), _) => return Err(AxError::OperationNotSupported),
                    (None, backend) => backend,
                };
                match backend {
                    axhal::perf_precise_aux::AuxBackend::IntelPt => {
                        axhal::perf_precise_aux::validate_pt_attr(
                            self.config,
                            self.config1,
                            self.config2,
                            self.trace_user,
                            self.trace_kernel,
                        )
                        .map_err(|error| match error {
                            axhal::perf_precise_aux::Error::InvalidBuffer => AxError::InvalidInput,
                            _ => AxError::OperationNotSupported,
                        })?;
                        return Ok(Some(AuxBackend::IntelPt));
                    }
                    axhal::perf_precise_aux::AuxBackend::Bts => {
                        // BTS has no Intel PT config word, address ranges, or
                        // privilege/context control MSRs.  No nonzero config
                        // can be truthfully honored.
                        if self.config != 0 || self.config1 != 0 || self.config2 != 0 {
                            return Err(AxError::OperationNotSupported);
                        }
                        return Ok(Some(AuxBackend::Bts));
                    }
                }
            }
            Ok(None)
        }
        #[cfg(not(all(feature = "pmu", target_os = "none")))]
        {
            let _ = self;
            Err(AxError::OperationNotSupported)
        }
    }
}

fn requested_backend(event_type: u32) -> Option<AuxBackend> {
    #[cfg(feature = "pmu")]
    {
        match crate::pmu_registry::dynamic_pmu(event_type) {
            Some(crate::pmu_registry::DynamicPmu::IntelPt) => Some(AuxBackend::IntelPt),
            Some(crate::pmu_registry::DynamicPmu::IntelBts) => Some(AuxBackend::Bts),
            _ => None,
        }
    }
    #[cfg(not(feature = "pmu"))]
    {
        let _ = event_type;
        None
    }
}

/// A data-ring-independent publication descriptor.  It maps one-for-one to
/// `PERF_RECORD_AUX`, so publication can remain in task context and reuse the
/// ordinary ring producer's loss accounting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AuxPublication {
    pub(crate) offset: u64,
    pub(crate) size: u64,
    pub(crate) flags: u64,
}

impl AuxPublication {
    pub(crate) const fn from_completion(
        offset: u64,
        size: usize,
        mode: AuxMode,
        truncated: bool,
    ) -> Self {
        let mut flags = if matches!(mode, AuxMode::Overwrite) {
            PERF_AUX_FLAG_OVERWRITE
        } else {
            0
        };
        if truncated {
            flags |= PERF_AUX_FLAG_TRUNCATED;
        }
        Self {
            offset,
            size: size as u64,
            flags,
        }
    }
}

/// Proves that an AUX mapping does not overlap the metadata/data mapping.
/// This is intentionally pure, allowing host tests to exercise geometry even
/// though host builds cannot claim an Intel PT device.
pub(crate) fn aux_mapping_offset(data_size: usize, aux_size: usize) -> AxResult<u64> {
    const PAGE: usize = 4096;
    if data_size == 0
        || aux_size == 0
        || !data_size.is_power_of_two()
        || !data_size.is_multiple_of(PAGE)
        || !aux_size.is_multiple_of(PAGE)
    {
        return Err(AxError::InvalidInput);
    }
    PAGE.checked_add(data_size)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(AxError::InvalidInput)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aux_mapping_begins_after_metadata_and_data() {
        assert_eq!(aux_mapping_offset(4096, 4096), Ok(8192));
        assert_eq!(aux_mapping_offset(8192, 4096), Ok(12288));
    }

    #[test]
    fn aux_mapping_rejects_non_ring_geometry() {
        assert_eq!(aux_mapping_offset(4097, 4096), Err(AxError::InvalidInput));
        assert_eq!(aux_mapping_offset(4096, 17), Err(AxError::InvalidInput));
    }

    #[test]
    fn aux_publication_preserves_overwrite_and_truncation() {
        assert_eq!(
            AuxPublication::from_completion(4, 16, AuxMode::Overwrite, true).flags,
            PERF_AUX_FLAG_OVERWRITE | PERF_AUX_FLAG_TRUNCATED
        );
    }
}
