//! Transport-agnostic application core.
//!
//! `App` owns the project session, the project library, processing jobs, the
//! desktop scene state, and the agent. Tauri commands and HTTP handlers are
//! thin adapters over these methods; event delivery flows through [`EventBus`],
//! never through transport-specific channels.

use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result};
use koharu_desktop::Desktop;
use koharu_scene::{AssetInput, AssetMetadata, AssetRole, At, Commit, EntityId, PageDraft};
use tokio::sync::Mutex as AsyncMutex;

use crate::commands::agent::AgentState;

use super::{
    downloads::{Download, DownloadState},
    events::{Event, EventBus},
    jobs::{JobId, Processing},
    preferences::Preferences,
    project::{Page, PageSelection, PageSummary, Project, ProjectInfo, ProjectLibrary, ProjectSummary},
};

/// Aggregate application state shared by all frontends.
///
/// The desktop adapter manages this type through `tauri::Manager::manage` and
/// the HTTP layer holds it as Axum state; neither constructs the inner fields
/// directly. The agent is attached after construction because its tool host
/// holds a handle back to this aggregate.
pub struct App {
    pub(crate) project: AsyncMutex<Option<Project>>,
    pub(crate) library: ProjectLibrary,
    pub(crate) processing: Processing,
    pub(crate) desktop: Desktop,
    pub(crate) pipeline: OnceLock<koharu_pipeline::Pipeline>,
    pub(crate) events: EventBus,
    pub(crate) initialization: Initialization,
    pub(crate) agent: OnceLock<AgentState>,
}

/// Gate between process start and the ML runtime being usable. Frontends
/// reject work until [`App::wait_ready`] resolves.
pub(crate) struct Initialization {
    ready: tokio::sync::watch::Sender<bool>,
}

impl Default for Initialization {
    fn default() -> Self {
        let (ready, _) = tokio::sync::watch::channel(false);
        Self { ready }
    }
}

impl Initialization {
    fn ready(&self) {
        self.ready.send_replace(true);
    }

    fn is_ready(&self) -> bool {
        *self.ready.borrow()
    }

    async fn wait(&self) -> Result<()> {
        let mut ready = self.ready.subscribe();
        while !*ready.borrow_and_update() {
            ready
                .changed()
                .await
                .context("startup state closed before initialization completed")?;
        }
        Ok(())
    }
}

impl App {
    pub fn new() -> Result<Self> {
        Ok(Self {
            project: AsyncMutex::new(None),
            library: ProjectLibrary::new()?,
            processing: Processing::default(),
            desktop: Desktop::new()?,
            pipeline: OnceLock::new(),
            events: EventBus::default(),
            initialization: Initialization::default(),
            agent: OnceLock::new(),
        })
    }

    /// Shared startup path for every launch mode: brings up the ML runtime,
    /// attaches the pipeline, starts the download/resource event producers,
    /// and restores the current project's page onto the canvas. Desktop and
    /// headless must not fork this sequence.
    #[tracing::instrument(
        target = "koharu_metrics",
        name = "app_started",
        skip_all,
        fields(phase = "initialization")
    )]
    pub async fn initialize(self: &Arc<Self>) -> Result<()> {
        koharu_ml::init()
            .await
            .context("failed to initialize the ML runtime")?;
        let device = koharu_ml::device(false);
        koharu_metrics::context(serde_json::json!({
            "compute_backend": device.backend.to_string().to_ascii_lowercase(),
            "device_type": format!("{:?}", device.device_type).to_ascii_lowercase(),
            "gpu_model": device.description.clone(),
            "vram_bytes": device.memory_total,
        }));
        self.attach_pipeline(koharu_pipeline::Pipeline::load(device)?);
        self.spawn_resource_watcher();
        self.spawn_download_watcher();

        let project = self.project.lock().await.as_ref().map(|project| {
            (
                project.snapshot(),
                project.active_page(),
            )
        });
        if let Some((snapshot, page)) = project {
            self.desktop.show_page(&snapshot, page).await?;
        } else {
            self.desktop.clear().await;
        }
        Ok(())
    }

    /// Publishes model-resource snapshots onto the event bus for the whole
    /// process lifetime.
    fn spawn_resource_watcher(self: &Arc<Self>) {
        let mut resources = self.pipeline().subscribe_resources();
        let app = Arc::clone(self);
        drop(tokio::spawn(async move {
            while resources.changed().await.is_ok() {
                let snapshot = resources.borrow_and_update().clone();
                app.events.publish(Event::Resources(snapshot.into()));
            }
        }));
    }

    /// Forwards runtime download progress onto the event bus for the whole
    /// process lifetime.
    fn spawn_download_watcher(self: &Arc<Self>) {
        use koharu_runtime::download;
        let mut downloads = download::subscribe();
        let app = Arc::clone(self);
        drop(tokio::spawn(async move {
            loop {
                match downloads.recv().await {
                    Ok(event) => {
                        let download_event = match event {
                            download::Event::Started { id, name } => {
                                tracing::info!(
                                    target: "koharu_metrics",
                                    metric = "download_start",
                                    resource = "runtime",
                                );
                                Download {
                                    id,
                                    state: DownloadState::Running,
                                    name: Some(name),
                                    completed: 0,
                                    total: 0,
                                    error: None,
                                }
                            }
                            download::Event::Progress {
                                id,
                                name,
                                completed,
                                total,
                            } => {
                                tracing::info!(
                                    target: "koharu_metrics",
                                    metric = "download_progress",
                                    resource = "runtime",
                                    used_bytes = completed,
                                    total_bytes = total,
                                );
                                Download {
                                    id,
                                    state: DownloadState::Running,
                                    name: Some(name),
                                    completed,
                                    total,
                                    error: None,
                                }
                            }
                            download::Event::Finished { id } => {
                                tracing::info!(
                                    target: "koharu_metrics",
                                    metric = "download_result",
                                    resource = "runtime",
                                    outcome = "completed",
                                );
                                Download {
                                    id,
                                    state: DownloadState::Finished,
                                    name: None,
                                    completed: 0,
                                    total: 0,
                                    error: None,
                                }
                            }
                            download::Event::Failed { id, name, error } => {
                                tracing::info!(
                                    target: "koharu_metrics",
                                    metric = "download_result",
                                    resource = "runtime",
                                    outcome = "failed",
                                );
                                Download {
                                    id,
                                    state: DownloadState::Failed,
                                    name: Some(name),
                                    completed: 0,
                                    total: 0,
                                    error: Some(error),
                                }
                            }
                        };
                        app.events.publish(Event::Download(download_event));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "download channel fell behind");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }));
    }

    /// Attaches the pipeline after asynchronous ML-runtime initialization.
    /// Processing commands block on this via [`App::wait_ready`].
    pub(crate) fn attach_pipeline(&self, pipeline: koharu_pipeline::Pipeline) {
        let _ = self.pipeline.set(pipeline);
    }

    /// Returns the attached pipeline. Callers must only invoke this after
    /// initialization marked the runtime ready.
    pub(crate) fn pipeline(&self) -> &koharu_pipeline::Pipeline {
        self.pipeline
            .get()
            .expect("pipeline is attached during initialization")
    }

    /// Attaches the agent tooling to this aggregate. Called once per process;
    /// later calls fail silently because the agent is optional tooling.
    #[allow(dead_code)]
    pub(crate) fn attach_agent(&self, agent: AgentState) {
        let _ = self.agent.set(agent);
    }

    /// Returns the attached agent, if agent tooling was wired up.
    #[allow(dead_code)]
    pub(crate) fn agent(&self) -> Option<&AgentState> {
        self.agent.get()
    }

    pub fn events(&self) -> &EventBus {
        &self.events
    }

    pub fn mark_ready(&self) {
        self.initialization.ready();
    }

    /// Non-blocking readiness probe for transport layers that must reject
    /// work (HTTP 503) instead of awaiting startup.
    pub fn is_ready(&self) -> bool {
        self.initialization.is_ready()
    }

    pub async fn wait_ready(&self) -> Result<()> {
        self.initialization.wait().await
    }

    pub fn preferences(&self) -> Result<Preferences> {
        Preferences::load()
    }

    pub async fn project_info(&self) -> Result<Option<ProjectInfo>> {
        Ok(self.project.lock().await.as_ref().map(Project::info))
    }

    pub async fn pages(&self) -> Result<Vec<PageSummary>> {
        let snapshot = self
            .project
            .lock()
            .await
            .as_ref()
            .context("no project is open")?
            .snapshot();
        Ok(Project::pages(&snapshot)?)
    }

    pub async fn page(&self) -> Result<Option<Page>> {
        let current = {
            let project = self.project.lock().await;
            project
                .as_ref()
                .map(|project| (project.snapshot(), project.active_page()))
        };
        Ok(current
            .and_then(|(snapshot, page)| page.map(|page| (snapshot, page)))
            .map(|(snapshot, page)| Project::page(&snapshot, page))
            .transpose()?)
    }

    pub async fn list_projects(&self) -> Result<Vec<ProjectSummary>> {
        self.library.list()
    }

    async fn replace_project(&self, opened: Project) -> Result<()> {
        let snapshot = opened.snapshot();
        let page = opened.active_page();
        let info = opened.info();

        if let Some(agent) = self.agent.get() {
            agent.reset().await;
        }
        self.cancel_processing();

        let previous = {
            let mut current = self.project.lock().await;
            current.replace(opened)
        };

        self.desktop.show_page(&snapshot, page).await?;
        let canvas = self.desktop.canvas_state();
        drop(previous);
        self.publish_canvas(canvas);
        self.events.publish(Event::Project(Some(info)));
        Ok(())
    }

    pub async fn create_project(&self, name: &str) -> Result<()> {
        let opened = self.library.create(name).await?;
        self.replace_project(opened).await?;
        Ok(())
    }

    pub async fn open_project(&self, name: &str) -> Result<()> {
        let opened = self.library.open(name).await?;
        self.replace_project(opened).await?;
        Ok(())
    }

    pub async fn close_project(&self) -> Result<()> {
        if let Some(agent) = self.agent.get() {
            agent.reset().await;
        }
        self.cancel_processing();
        let previous = {
            let mut current = self.project.lock().await;
            current.take()
        };
        self.desktop.clear().await;
        let canvas = self.desktop.canvas_state();
        drop(previous);
        self.publish_canvas(canvas);
        self.events.publish(Event::Project(None));
        Ok(())
    }

    pub async fn delete_project(&self, name: &str) -> Result<()> {
        let active = self
            .project
            .lock()
            .await
            .as_ref()
            .is_some_and(|project| project.name == name);
        if active {
            self.close_project().await?;
        }
        tokio::task::spawn_blocking({
            let library = self.library.clone();
            let name = name.to_owned();
            move || library.delete(&name)
        })
        .await
        .context("project deletion worker stopped unexpectedly")??;
        Ok(())
    }

    /// Imports page images from explicit paths. Dialog-based frontends resolve
    /// paths first, then call this; headless frontends pass request paths.
    pub async fn import_pages(&self, files: Vec<std::path::PathBuf>) -> Result<Commit> {
        if !self.processing.stops.lock().is_empty() {
            anyhow::bail!("pages cannot be imported while processing is running");
        }
        let pages = tokio::task::spawn_blocking(move || super::import::import(files))
            .await
            .context("page import worker stopped unexpectedly")??;
        let page_count = pages.len();

        let (commit, page) = {
            let mut project = self.project.lock().await;
            let project = project.as_mut().context("no project is open")?;
            let source = AssetRole::new("source")?;
            let patch = project.snapshot().patch(|edit| {
                for imported in pages {
                    let page = edit.add_page(
                        PageDraft::new(
                            imported.name,
                            f64::from(imported.width),
                            f64::from(imported.height),
                        ),
                        At::End,
                    )?;
                    edit.set_asset(
                        page,
                        &source,
                        AssetInput::new(
                            imported.bytes,
                            imported.format.to_mime_type(),
                            AssetMetadata {
                                width: Some(imported.width),
                                height: Some(imported.height),
                                attributes: Default::default(),
                            },
                        ),
                    )?;
                }
                Ok(())
            })?;
            let commit = project.session.commit(patch).await?;
            project.record(vec![commit.revision]);
            project.reconcile_page();
            let page = project.active_page();
            (commit, page)
        };
        self.desktop
            .synchronize(&commit.snapshot, page, &commit)
            .await?;
        self.publish_canvas(self.desktop.canvas_state());
        tracing::info!(target: "koharu_metrics", metric = "page_imported", page_count);
        Ok(commit)
    }

    pub async fn select_page(&self, page: EntityId) -> Result<PageSelection> {
        let (snapshot, project_info, selected_page) = {
            let mut project = self.project.lock().await;
            let project = project.as_mut().context("no project is open")?;
            project.select_page(page)?;
            let snapshot = project.snapshot();
            let project_info = project.info();
            let selected_page = Project::page(&snapshot, page)?;
            (snapshot, project_info, selected_page)
        };
        if self.desktop.show_page(&snapshot, Some(page)).await? {
            self.publish_canvas(self.desktop.canvas_state());
        }
        Ok(PageSelection {
            project: project_info,
            page: selected_page,
        })
    }

    /// Fetches one page's full view by id, regardless of which page is active.
    pub async fn page_view(&self, page: EntityId) -> Result<Page> {
        let snapshot = self
            .project
            .lock()
            .await
            .as_ref()
            .context("no project is open")?
            .snapshot();
        snapshot.page(page)?;
        Ok(Project::page(&snapshot, page)?)
    }

    pub fn stop_job(&self, job: JobId) -> Result<()> {
        let stops = self.processing.stops.lock();
        let stop = stops
            .get(&job)
            .with_context(|| format!("job {job} is not running"))?;
        stop.stop();
        Ok(())
    }

    /// Loads the given stages' models into memory (downloading weights when
    /// missing) without processing pages. Headless startup calls this with
    /// its configured warmup list before marking the application ready.
    pub async fn warm(
        &self,
        stages: impl IntoIterator<Item = koharu_pipeline::Stage>,
    ) -> Result<()> {
        self.pipeline().warm(stages).await
    }

    /// Cancels every running job and clears job bookkeeping.
    pub(crate) fn cancel_processing(&self) {
        for stop in self.processing.stops.lock().values() {
            stop.stop();
        }
        self.processing.stops.lock().clear();
        self.processing.jobs.lock().clear();
    }

    pub(crate) fn publish_canvas(&self, canvas: koharu_desktop::CanvasState) {
        self.events.publish(Event::Canvas(canvas));
    }
}

/// Shared handle used by spawned tasks and agent tooling.
pub type SharedApp = Arc<App>;
