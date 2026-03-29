#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://bevy.org/assets/icon.png",
    html_favicon_url = "https://bevy.org/assets/icon.png"
)]
#![no_std]

#[cfg(feature = "std")]
extern crate std;

mod bridge;
mod plugin;
mod system_state;
mod wake_signal;
mod world;

pub use plugin::AsyncPlugin;
pub use world::{async_world_sync_point, AsyncSystemState, AsyncWorld};

pub mod prelude {
    #[doc(hidden)]
    pub use crate::{
        async_world_sync_point, AsyncPlugin, AsyncSystemState, AsyncWorld, EcsAccessError,
    };
}

#[derive(thiserror::Error, Debug)]
pub enum EcsAccessError {
    #[error(transparent)]
    SystemParamValidation(bevy_ecs::system::SystemParamValidationError),
    #[error("World no longer exists")]
    WorldDropped,
}
