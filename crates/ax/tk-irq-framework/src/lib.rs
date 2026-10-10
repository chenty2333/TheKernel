//! Dynamic IRQ registration and dispatch primitives.
//!
//! Adapted from the Apache-2.0 `irq-framework` component in `rcore-os/tgoskits`.

#![no_std]

extern crate alloc;

mod action;
mod descriptor;
mod lock;
mod registry;
mod types;

pub use registry::Registry;
pub use types::{
    AcpiGsiController, AcpiGsiRoute, AcpiIrqPolarity, AcpiIrqTrigger, AutoEnable, BoxedIrqHandler,
    ConcurrentBoxedIrqHandler, CpuId, CpuMask, CpuMaskIter, HwIrq, IrqAffinity, IrqContext,
    IrqDomainId, IrqError, IrqExecution, IrqHandle, IrqId, IrqOps, IrqOrigin, IrqOutcome,
    IrqRequest, IrqReturn, IrqScope, IrqSource, IrqStatus, IrqTrigger, ShareMode, TrapVector,
};
