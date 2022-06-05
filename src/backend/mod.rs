#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(not(target_arch = "wasm32"))]
mod native;

#[cfg(target_arch = "wasm32")]
pub(crate) use crate::backend::wasm::*;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use crate::backend::native::*;