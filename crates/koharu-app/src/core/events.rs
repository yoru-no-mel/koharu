//! Transport-agnostic application events shared by every frontend adapter.
//!
//! Adapters subscribe through [`EventBus::subscribe`] and translate the typed
//! [`Event`] stream into their own delivery mechanism (Tauri channels, SSE
//! frames). Events mirror the previous per-frontend channel slots so payload
//! types stay identical for the generated UI protocol.

use std::fmt;

use koharu_desktop::CanvasState;
use serde::Serialize;
use tokio::sync::broadcast;

use super::{
    downloads::{Download, ModelResources},
    jobs::Job,
    project::ProjectInfo,
};

/// Maximum buffered events per subscriber before the subscriber is marked
/// lagged. Frontends resynchronize through full-state commands after a lag.
const EVENT_CAPACITY: usize = 256;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Canvas(CanvasState),
    Job(Job),
    Download(Download),
    Resources(ModelResources),
    Project(Option<ProjectInfo>),
}

impl fmt::Display for Event {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Canvas(_) => "canvas",
            Self::Job(_) => "job",
            Self::Download(_) => "download",
            Self::Resources(_) => "resources",
            Self::Project(_) => "project",
        };
        formatter.write_str(name)
    }
}

/// Process-wide broadcast hub. Every state mutation publishes here; adapters
/// forward to their transports. A send only fails when no adapter currently
/// listens, which is normal for headless runs without an events consumer.
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl Default for EventBus {
    fn default() -> Self {
        let (sender, _) = broadcast::channel(EVENT_CAPACITY);
        Self { sender }
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&self, event: Event) {
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}
