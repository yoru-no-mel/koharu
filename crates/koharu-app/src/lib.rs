//! Koharu's application state, commands, and lifecycle.

mod app;
mod commands;
pub mod core;

pub use app::run;
pub use commands::bindings;
pub use core::{App, SharedApp};
