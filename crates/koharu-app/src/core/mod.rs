//! Transport-agnostic application core shared by the desktop commands and the
//! headless HTTP API. DTO types and workflows live here; `commands/` holds
//! only the Tauri adapters.

pub mod app;
pub mod downloads;
pub mod events;
pub mod export;
pub mod fonts;
pub mod import;
pub mod jobs;
pub mod preferences;
pub mod project;

pub use app::{App, SharedApp};
