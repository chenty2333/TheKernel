//! Device driver prelude that includes some traits and types.

pub use axdriver_base::{BaseDriverOps, DevError, DevResult, DeviceType};
#[cfg(feature = "block")]
pub use {
    crate::structs::AxBlockDevice,
    axdriver_block::{
        BlockAsyncOp, BlockCapabilities, BlockCompletion, BlockCompletionAvailability,
        BlockCompletionDrain, BlockCompletionNotifier, BlockCompletionOwner, BlockCompletionStatus,
        BlockCompletionTerminalNotifier, BlockDriverOps, BlockGeometry,
        BlockPhysicalCompletionRoute, BlockPhysicalRequest, BlockPhysicalSegment,
        BlockPhysicalSgOutcome, BlockQueueCaps, BlockQueueRequest, BlockRange, BlockRequestHandle,
        BlockResetOutcome, BlockSegment, BlockSegmentDirection, BlockSubmitReport,
    },
};
#[cfg(feature = "display")]
pub use {
    crate::structs::AxDisplayDevice,
    axdriver_display::{DisplayDriverOps, DisplayInfo},
};
#[cfg(feature = "input")]
pub use {
    crate::structs::AxInputDevice,
    axdriver_input::{Event, EventType, InputDeviceId, InputDriverOps},
};
#[cfg(feature = "net")]
pub use {
    crate::structs::AxNetDevice,
    axdriver_net::{NetBufPtr, NetDriverOps},
};
#[cfg(feature = "vsock")]
pub use {
    crate::structs::AxVsockDevice,
    axdriver_vsock::{VsockAddr, VsockConnId, VsockDriverEvent, VsockDriverOps},
};
