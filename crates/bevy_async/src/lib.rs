#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://bevy.org/assets/icon.png",
    html_favicon_url = "https://bevy.org/assets/icon.png"
)]
#![no_std]

#[cfg(feature = "std")]
extern crate std;

mod access;
mod bridge;
mod guarded_latch;
mod plugin;
mod request;
mod system_state_cell;

pub use crate::access::{AsyncAccessError, AsyncSystemHandle};
pub use crate::bridge::{tick_async_bridge, AsyncBridge};
pub use crate::plugin::AsyncPlugin;

pub mod prelude {
    #[doc(hidden)]
    pub use crate::{
        tick_async_bridge, AsyncAccessError, AsyncBridge, AsyncPlugin, AsyncSystemHandle,
    };
}
