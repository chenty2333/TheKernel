use core::{cell::UnsafeCell, ptr, sync::atomic::AtomicBool};

use crate::{
    AutoEnable, BoxedIrqHandler, ConcurrentBoxedIrqHandler, CpuId, CpuMask, IrqContext,
    IrqExecution, IrqRequest, IrqReturn, IrqScope, types::IrqHandler,
};

pub(crate) enum ActionHandler {
    NonReentrant(UnsafeCell<BoxedIrqHandler>),
    Concurrent(ConcurrentBoxedIrqHandler),
}

// SAFETY: callbacks are Send, and shared access is limited to the
// NonReentrant run guard or the Concurrent handler's Sync contract.
unsafe impl Send for ActionHandler {}
// SAFETY: callbacks are serialized or explicitly Sync according to the enum.
unsafe impl Sync for ActionHandler {}

pub(crate) struct Action {
    pub(crate) id: u64,
    pub(crate) handler: ActionHandler,
    pub(crate) scope: IrqScope,
    pub(crate) execution: IrqExecution,
    pub(crate) enabled: AtomicBool,
    pub(crate) detached: AtomicBool,
    pub(crate) running: AtomicBool,
    pending_enable: UnsafeCell<CpuMask>,
    pub(crate) next: *mut Action,
}

// SAFETY: the boxed callback is Send, and the framework only mutably accesses
// it after the NonReentrant run guard succeeds; shared state is atomic or
// protected by the registry metadata lock.
unsafe impl Send for Action {}
// SAFETY: callback access is serialized by its run guard; pending-enable
// mutation is serialized by the registry metadata lock.
unsafe impl Sync for Action {}

impl Action {
    pub(crate) fn new(id: u64, request: &mut IrqRequest) -> Self {
        let handler = match request
            .handler
            .take()
            .expect("IRQ handler was already consumed")
        {
            IrqHandler::NonReentrant(handler) => {
                ActionHandler::NonReentrant(UnsafeCell::new(handler))
            }
            IrqHandler::Concurrent(handler) => ActionHandler::Concurrent(handler),
        };
        Self {
            id,
            handler,
            scope: request.scope,
            execution: request.execution,
            enabled: AtomicBool::new(request.auto_enable == AutoEnable::Yes),
            detached: AtomicBool::new(false),
            running: AtomicBool::new(false),
            pending_enable: UnsafeCell::new(CpuMask::empty()),
            next: ptr::null_mut(),
        }
    }

    pub(crate) fn pending_enable_contains(&self, cpu: CpuId) -> bool {
        // SAFETY: callers hold the registry metadata lock while accessing this
        // non-atomic mask.
        unsafe { (&*self.pending_enable.get()).contains(cpu) }
    }

    pub(crate) fn insert_pending_enable(&self, cpu: CpuId) {
        // SAFETY: callers hold the registry metadata lock while accessing this
        // non-atomic mask.
        unsafe { (&mut *self.pending_enable.get()).insert(cpu) };
    }

    pub(crate) fn remove_pending_enable(&self, cpu: CpuId) {
        // SAFETY: callers hold the registry metadata lock while accessing this
        // non-atomic mask.
        unsafe { (&mut *self.pending_enable.get()).remove(cpu) };
    }

    pub(crate) fn clear_pending_enable_all(&self) {
        // SAFETY: callers hold the registry metadata lock while accessing this
        // non-atomic mask.
        unsafe { *self.pending_enable.get() = CpuMask::empty() };
    }

    pub(crate) fn call(&self, ctx: IrqContext) -> IrqReturn {
        match &self.handler {
            ActionHandler::NonReentrant(handler) => {
                // SAFETY: ActionRunGuard grants exactly one caller mutable
                // access to this callback for the duration of the dispatch.
                let handler = unsafe { &mut *handler.get() };
                handler(ctx)
            }
            ActionHandler::Concurrent(handler) => handler(ctx),
        }
    }
}
