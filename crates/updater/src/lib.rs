//! Community configuration and HTTPS self-update before game startup.
//!
//! Native builds use `native` (files next to the executable, TLS, binary
//! replacement). The browser build has no executable to replace and no
//! community descriptors, so `web` exposes the same surface with nothing
//! configured.

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::*;
