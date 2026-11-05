//! `emu-common`: the pieces every emulator core in this workspace shares.
//!
//! It has no dependencies and no I/O, so it builds anywhere gb-core does,
//! including `wasm32`. Each core keeps its own state layout and recording
//! semantics; this crate holds the byte-level primitives they are built from
//! and the [`Core`] trait a frontend drives.
#![forbid(unsafe_code)]

pub mod color;
pub mod interface;
pub mod replay;
pub mod state;

pub use color::rgb555_to_rgb888;
pub use interface::{BatteryError, Button, Core, FrameRef, Pixels, System};
pub use state::{StateError, StateReader, StateWriter, check, fnv1a};
