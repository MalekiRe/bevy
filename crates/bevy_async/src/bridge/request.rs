use super::guarded_latch::LatchWaiter;
use crate::system_state::ErasedSystemStateCell;
use bevy_platform::sync::Arc;
use core::task::Waker;

pub(crate) struct BridgeRequest {
    pub(super) waker: Waker,
    pub(super) latch: LatchWaiter,
    pub(super) system_state: Arc<dyn ErasedSystemStateCell>,
}

impl BridgeRequest {
    pub(crate) fn wake(self) -> WokenBridgeRequest {
        self.waker.wake();
        WokenBridgeRequest {
            latch: self.latch,
            system_state: self.system_state,
        }
    }
}

pub(crate) struct WokenBridgeRequest {
    latch: LatchWaiter,
    system_state: Arc<dyn ErasedSystemStateCell>,
}

impl WokenBridgeRequest {
    pub(crate) fn wait(self) -> Arc<dyn ErasedSystemStateCell> {
        self.latch.wait();
        self.system_state
    }
}
