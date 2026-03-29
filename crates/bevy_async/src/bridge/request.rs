use super::wake_signal::WakeSignal;
use crate::system_state::ErasedSystemStateCell;
use bevy_platform::sync::Arc;
use core::task::Waker;

pub(crate) struct BridgeRequest {
    pub(super) waker: Waker,
    pub(super) wake_signal: WakeSignal,
    pub(super) system_state: Arc<dyn ErasedSystemStateCell>,
}

impl BridgeRequest {
    pub(crate) fn wake(self) -> WokenBridgeRequest {
        self.waker.wake();
        WokenBridgeRequest {
            wake_signal: self.wake_signal,
            system_state: self.system_state,
        }
    }
}

pub(crate) struct WokenBridgeRequest {
    wake_signal: WakeSignal,
    system_state: Arc<dyn ErasedSystemStateCell>,
}

impl WokenBridgeRequest {
    pub(crate) fn wait(self) -> Arc<dyn ErasedSystemStateCell> {
        self.wake_signal.wait();
        self.system_state
    }
}
