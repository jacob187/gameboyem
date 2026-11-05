//! `emu-common`: the pieces every emulator core in this workspace shares.
//!
//! It has no dependencies and no I/O, so it builds anywhere gb-core does,
//! including `wasm32`. Each core keeps its own state layout and recording
//! semantics; this crate holds the byte-level primitives they are built from.
#![forbid(unsafe_code)]

pub mod state;

pub use state::{StateError, StateReader, StateWriter, check, fnv1a};
