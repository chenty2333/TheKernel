// SPDX-License-Identifier: MIT
// Copyright © 2015 Intel Corporation.
// Linux 7.2.3 drivers/gpu/drm/i915/display/intel_atomic.c translation.
// DRM object/state allocation, property blobs, tunnels, and HDCP remain hooks.
#![allow(dead_code, clippy::too_many_arguments)]

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DigitalState {
    pub force_audio: u64,
    pub broadcast_rgb: u64,
    pub colorspace: u32,
    pub picture_aspect_ratio: u32,
    pub content_type: u32,
    pub scaling_mode: u32,
    pub privacy_screen_sw_state: u32,
    pub hdr_metadata: u64,
    pub crtc: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcState {
    pub id: u64,
    pub mode_changed: bool,
    pub active: bool,
    pub color_blobs: [Option<u64>; 5],
    pub tunnel: Option<u64>,
    pub update_pipe: bool,
    pub update_m_n: bool,
    pub update_lrr: bool,
    pub disable_cxsr: bool,
    pub update_wm_pre: bool,
    pub update_wm_post: bool,
    pub fifo_changed: bool,
    pub preload_luts: bool,
    pub wm_need_postvbl_update: bool,
    pub do_async_flip: bool,
    pub fb_bits: u32,
    pub update_planes: u32,
    pub dsb_color: Option<u64>,
    pub dsb_commit: Option<u64>,
    pub use_dsb: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Connector {
    pub id: u32,
    pub force_audio_property: u32,
    pub broadcast_rgb_property: u32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConnectorRef {
    pub id: u32,
    pub old: DigitalState,
    pub new: DigitalState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CrtcRef {
    pub id: u32,
    pub old: CrtcState,
    pub new: CrtcState,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AtomicState {
    pub handle: u64,
    pub dpll_set: bool,
    pub modeset: bool,
    /// `internal` intentionally survives `intel_atomic_state_clear()`.
    pub internal: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicError {
    Invalid,
    NoMemory,
    Framework(i32),
}

/// Operations owned by DRM, HDCP, tunnel refcounts, and kernel allocation.
pub trait AtomicIo {
    fn warn_unreleased_dsb(&mut self, _color: bool) {}
    fn debug_unknown_property(&mut self, _property: u32) {}
    fn hdcp_atomic_check(&mut self, _connector: u32, _old: DigitalState, _new: DigitalState) {}
    fn hdr_metadata_equal(&mut self, old: u64, new: u64) -> bool { old == new }
    fn crtc_needs_modeset(&mut self, old: CrtcState, new: CrtcState) -> bool {
        old.active != new.active || old.mode_changed || new.mode_changed
    }
    fn dup_connector_state(&mut self, state: DigitalState) -> Result<DigitalState, AtomicError> { Ok(state) }
    fn dup_crtc_state(&mut self, state: CrtcState) -> Result<CrtcState, AtomicError> { Ok(state) }
    fn blob_get(&mut self, _blob: u64) {}
    fn blob_put(&mut self, _blob: u64) {}
    fn tunnel_get(&mut self, _tunnel: u64) {}
    fn tunnel_put(&mut self, _tunnel: u64) {}
    fn crtc_destroy_helper(&mut self, _state: u64) {}
    fn alloc_atomic(&mut self) -> Result<u64, AtomicError> { Err(AtomicError::NoMemory) }
    fn atomic_init(&mut self, _id: u64) -> Result<(), AtomicError> { Ok(()) }
    fn atomic_default_release(&mut self, _id: u64) {}
    fn free_global_objects(&mut self, _id: u64) {}
    fn free_atomic(&mut self, _id: u64) {}
    fn atomic_clear(&mut self, _id: u64) {}
    fn clear_global_state(&mut self, _id: u64) {}
    fn cleanup_inherited_tunnel_state(&mut self, _id: u64) {}
    fn get_connector_state(&mut self, _atomic: u64, _connector: u32) -> Result<DigitalState, AtomicError> { Err(AtomicError::Invalid) }
    fn get_crtc_state(&mut self, _atomic: u64, _crtc: u32) -> Result<CrtcState, AtomicError> { Err(AtomicError::Invalid) }
}

// upstream: intel_atomic.c intel_digital_connector_atomic_get_property()
pub fn intel_digital_connector_atomic_get_property<I: AtomicIo>(io: &mut I, connector: Connector, state: DigitalState, property: u32) -> Result<u64, AtomicError> {
    if property == connector.force_audio_property { return Ok(state.force_audio); }
    if property == connector.broadcast_rgb_property { return Ok(state.broadcast_rgb); }
    io.debug_unknown_property(property);
    Err(AtomicError::Invalid)
}

// upstream: intel_atomic.c intel_digital_connector_atomic_set_property()
pub fn intel_digital_connector_atomic_set_property<I: AtomicIo>(io: &mut I, connector: Connector, state: &mut DigitalState, property: u32, value: u64) -> Result<(), AtomicError> {
    if property == connector.force_audio_property { state.force_audio = value; return Ok(()); }
    if property == connector.broadcast_rgb_property { state.broadcast_rgb = value; return Ok(()); }
    io.debug_unknown_property(property);
    Err(AtomicError::Invalid)
}

// upstream: intel_atomic.c intel_digital_connector_atomic_check()
pub fn intel_digital_connector_atomic_check<I: AtomicIo>(io: &mut I, connector: u32, old: DigitalState, new: DigitalState, crtc: Option<&mut CrtcState>) {
    io.hdcp_atomic_check(connector, old, new);
    let Some(crtc) = crtc else { return; };
    if new.force_audio != old.force_audio || new.broadcast_rgb != old.broadcast_rgb ||
       new.colorspace != old.colorspace || new.picture_aspect_ratio != old.picture_aspect_ratio ||
       new.content_type != old.content_type || new.scaling_mode != old.scaling_mode ||
       new.privacy_screen_sw_state != old.privacy_screen_sw_state ||
       !io.hdr_metadata_equal(old.hdr_metadata, new.hdr_metadata) {
        crtc.mode_changed = true;
    }
}

// upstream: intel_atomic.c intel_digital_connector_duplicate_state()
pub fn intel_digital_connector_duplicate_state<I: AtomicIo>(io: &mut I, state: DigitalState) -> Result<DigitalState, AtomicError> {
    io.dup_connector_state(state)
}

// upstream: intel_atomic.c intel_connector_needs_modeset()
pub fn intel_connector_needs_modeset<I: AtomicIo>(io: &mut I, old: DigitalState, new: DigitalState, crtc: Option<(CrtcState, CrtcState)>) -> bool {
    old.crtc != new.crtc || crtc.is_some_and(|(old, new)| io.crtc_needs_modeset(old, new))
}

// upstream: intel_atomic.c intel_any_crtc_needs_modeset()
pub fn intel_any_crtc_needs_modeset<I: AtomicIo>(io: &mut I, crtcs: &[CrtcRef]) -> bool {
    crtcs.iter().any(|state| io.crtc_needs_modeset(state.old, state.new))
}

// upstream: intel_atomic.c intel_atomic_get_digital_connector_state()
pub fn intel_atomic_get_digital_connector_state<I: AtomicIo>(io: &mut I, atomic: u64, connector: Connector) -> Result<DigitalState, AtomicError> {
    io.get_connector_state(atomic, connector.id)
}

// upstream: intel_atomic.c intel_crtc_duplicate_state()
pub fn intel_crtc_duplicate_state<I: AtomicIo>(io: &mut I, old: CrtcState) -> Result<CrtcState, AtomicError> {
    let mut state = io.dup_crtc_state(old)?;
    for blob in state.color_blobs.into_iter().flatten() { io.blob_get(blob); }
    if let Some(tunnel) = state.tunnel { io.tunnel_get(tunnel); }
    state.update_pipe = false;
    state.update_m_n = false;
    state.update_lrr = false;
    state.disable_cxsr = false;
    state.update_wm_pre = false;
    state.update_wm_post = false;
    state.fifo_changed = false;
    state.preload_luts = false;
    state.wm_need_postvbl_update = false;
    state.do_async_flip = false;
    state.fb_bits = 0;
    state.update_planes = 0;
    state.dsb_color = None;
    state.dsb_commit = None;
    state.use_dsb = false;
    Ok(state)
}

// upstream: intel_atomic.c intel_crtc_put_color_blobs()
pub fn intel_crtc_put_color_blobs<I: AtomicIo>(io: &mut I, state: &mut CrtcState) {
    for blob in &mut state.color_blobs { if let Some(id) = blob.take() { io.blob_put(id); } }
}

// upstream: intel_atomic.c intel_crtc_free_hw_state()
pub fn intel_crtc_free_hw_state<I: AtomicIo>(io: &mut I, state: &mut CrtcState) { intel_crtc_put_color_blobs(io, state); }

// upstream: intel_atomic.c intel_crtc_destroy_state()
pub fn intel_crtc_destroy_state<I: AtomicIo>(io: &mut I, state: &mut CrtcState) -> Result<(), AtomicError> {
    if state.dsb_color.is_some() { io.warn_unreleased_dsb(true); }
    if state.dsb_commit.is_some() { io.warn_unreleased_dsb(false); }
    io.crtc_destroy_helper(state.id);
    intel_crtc_free_hw_state(io, state);
    if let Some(tunnel) = state.tunnel.take() { io.tunnel_put(tunnel); }
    Ok(())
}

// upstream: intel_atomic.c intel_atomic_state_alloc()
pub fn intel_atomic_state_alloc<I: AtomicIo>(io: &mut I) -> Result<AtomicState, AtomicError> {
    let state = io.alloc_atomic()?;
    if let Err(error) = io.atomic_init(state) { io.free_atomic(state); return Err(error); }
    Ok(AtomicState { handle: state, ..AtomicState::default() })
}

// upstream: intel_atomic.c intel_atomic_state_free()
pub fn intel_atomic_state_free<I: AtomicIo>(io: &mut I, state: AtomicState) {
    io.atomic_default_release(state.handle);
    io.free_global_objects(state.handle);
    io.free_atomic(state.handle);
}

// upstream: intel_atomic.c intel_atomic_state_clear()
pub fn intel_atomic_state_clear<I: AtomicIo>(io: &mut I, state: &mut AtomicState) {
    io.atomic_clear(state.handle);
    io.clear_global_state(state.handle);
    state.dpll_set = false;
    state.modeset = false;
    // state.internal intentionally survives clear, as in i915.
    io.cleanup_inherited_tunnel_state(state.handle);
}

// upstream: intel_atomic.c intel_atomic_get_crtc_state()
pub fn intel_atomic_get_crtc_state<I: AtomicIo>(io: &mut I, atomic: u64, crtc: u32) -> Result<CrtcState, AtomicError> {
    io.get_crtc_state(atomic, crtc)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)] struct Mock;
    impl AtomicIo for Mock {}
    #[test] fn connector_properties_and_fastset_changes_match_source() {
        let c = Connector { id: 1, force_audio_property: 4, broadcast_rgb_property: 5 };
        let mut state = DigitalState::default();
        intel_digital_connector_atomic_set_property(&mut Mock, c, &mut state, 4, 2).unwrap();
        assert_eq!(intel_digital_connector_atomic_get_property(&mut Mock, c, state, 4), Ok(2));
        let mut crtc = CrtcState::default();
        intel_digital_connector_atomic_check(&mut Mock, 1, state, DigitalState { force_audio: 1, ..state }, Some(&mut crtc));
        assert!(crtc.mode_changed);
    }
    #[test] fn duplicated_crtc_clears_commit_deltas_and_drops_blobs() {
        let mut io = Mock;
        let old = CrtcState { color_blobs: [Some(1), None, Some(3), None, None], update_pipe: true, use_dsb: true, ..CrtcState::default() };
        let state = intel_crtc_duplicate_state(&mut io, old).unwrap();
        assert_eq!(state.color_blobs, old.color_blobs);
        assert!(!state.update_pipe && !state.use_dsb);
    }
    #[test] fn atomic_clear_resets_dpll_modeset_but_preserves_internal() {
        let mut io = Mock;
        let mut state = AtomicState { handle: 9, dpll_set: true, modeset: true, internal: true };
        intel_atomic_state_clear(&mut io, &mut state);
        assert!(!state.dpll_set && !state.modeset && state.internal);
    }
}
