//! Processing job bookkeeping and the shared pipeline-run task.
//!
//! Both the desktop command and the HTTP handler start jobs through
//! [`App::process`]; progress and completion flow through the [`EventBus`]
//! as [`Event::Job`](super::events::Event::Job), never through transport
//! channels.

use std::{collections::HashMap, fmt, sync::Arc};

use anyhow::{Context as _, Result};
use koharu_pipeline::{Committer, Progress, RunStatus, StageOutput, StopToken};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use specta::Type;
use utoipa::ToSchema;
use uuid::Uuid;

use super::events::Event;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, ToSchema, Type)]
#[serde(transparent)]
#[schema(value_type = uuid::Uuid)]
pub struct JobId(Uuid);

impl JobId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for JobId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

#[derive(Clone, Debug, Serialize, ToSchema, Type)]
pub struct Job {
    pub id: JobId,
    pub state: JobState,
    #[schema(value_type = f64)]
    #[specta(type = f64)]
    pub completed: usize,
    #[schema(value_type = f64)]
    #[specta(type = f64)]
    pub total: usize,
    #[schema(value_type = Option<uuid::Uuid>)]
    pub page: Option<koharu_scene::EntityId>,
    #[schema(value_type = Option<String>)]
    pub stage: Option<koharu_pipeline::Stage>,
    pub model: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ToSchema, Type)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Finished,
    Failed,
    Stopped,
}

#[derive(Default)]
pub(crate) struct Processing {
    pub(crate) stops: Mutex<HashMap<JobId, StopToken>>,
    pub(crate) jobs: Mutex<HashMap<JobId, Job>>,
    pub(crate) inpainting_mask: Mutex<Option<koharu_pipeline::InpaintingMask>>,
}

impl super::App {
    /// Starts a pipeline run over `scope` with `operation` and returns the
    /// job id immediately; the run continues in the background and reports
    /// through the event bus. Only one run may be active at a time.
    pub async fn process(
        self: Arc<Self>,
        scope: koharu_pipeline::Scope,
        operation: koharu_pipeline::Operation,
    ) -> Result<JobId> {
        let snapshot = self
            .project
            .lock()
            .await
            .as_ref()
            .context("no project is open")?
            .snapshot();
        let id = JobId::new();
        let stop = StopToken::default();
        {
            let mut stops = self.processing.stops.lock();
            if !stops.is_empty() {
                anyhow::bail!("another process is already running");
            }
            stops.insert(id, stop.clone());
        }
        let job = Job {
            id,
            state: JobState::Running,
            completed: 0,
            total: 0,
            page: None,
            stage: None,
            model: None,
            error: None,
        };
        self.processing.jobs.lock().insert(id, job.clone());
        self.events.publish(Event::Job(job));

        let pipeline = self.pipeline().clone();
        let task_app = self.clone();
        let inpainting_mask = self.processing.inpainting_mask.lock().take();
        drop(tokio::spawn(async move {
            let progress = Arc::new(Mutex::new((0_usize, 0_usize)));
            let progress_app = task_app.clone();
            let mut request = koharu_pipeline::Request {
                operation,
                scope,
                stop: stop.clone(),
                progress: None,
                inpainting_mask,
            };
            request.progress = Some(Arc::new(move |event| {
                let update = match event {
                    Progress::Started { pages, stages } => {
                        tracing::info!(
                            target: "koharu_metrics",
                            metric = "pipeline_start",
                            page_count = pages.len(),
                            stage_count = stages.len(),
                        );
                        let mut progress = progress.lock();
                        *progress = (0, pages.len().saturating_mul(stages.len()));
                        Some((0, progress.1, None, None, None))
                    }
                    Progress::Loading { page, stage, model } => {
                        tracing::info!(
                            target: "koharu_metrics",
                            metric = "stage_loading",
                            stage = %stage,
                            model,
                        );
                        let progress = progress.lock();
                        Some((progress.0, progress.1, Some(page), Some(stage), Some(model)))
                    }
                    Progress::Finished {
                        page,
                        stage,
                        model,
                        elapsed,
                    } => {
                        if stage != koharu_pipeline::Stage::Translation {
                            tracing::info!(
                                target: "koharu_metrics",
                                metric = "model_run",
                                stage = %stage,
                                model,
                                duration_ms = elapsed.as_secs_f64() * 1000.0,
                            );
                        }
                        let mut progress = progress.lock();
                        progress.0 = progress.0.saturating_add(1).min(progress.1);
                        Some((progress.0, progress.1, Some(page), Some(stage), Some(model)))
                    }
                    Progress::Skipped { page, stage } => {
                        tracing::info!(
                            target: "koharu_metrics",
                            metric = "stage_skip",
                            stage = %stage,
                        );
                        let mut progress = progress.lock();
                        progress.0 = progress.0.saturating_add(1).min(progress.1);
                        Some((progress.0, progress.1, Some(page), Some(stage), None))
                    }
                    Progress::Running { stage, model, .. } => {
                        tracing::info!(
                            target: "koharu_metrics",
                            metric = "stage_running",
                            stage = %stage,
                            model,
                        );
                        None
                    }
                };
                if let Some((completed, total, page, stage, model)) = update {
                    let job = {
                        let processing = &progress_app.processing;
                        let mut jobs = processing.jobs.lock();
                        jobs.get_mut(&id).map(|job| {
                            job.completed = completed;
                            job.total = total;
                            job.page = page;
                            job.stage = stage;
                            job.model = model;
                            job.clone()
                        })
                    };
                    if let Some(job) = job {
                        progress_app.events.publish(Event::Job(job));
                    }
                }
            }));

            struct PipelineCommitter {
                app: Arc<super::App>,
            }

            #[async_trait::async_trait]
            impl Committer for PipelineCommitter {
                async fn commit(&mut self, output: StageOutput) -> Result<koharu_scene::Snapshot> {
                    let (commit, page) = {
                        let mut projects = self.app.project.lock().await;
                        let project = projects.as_mut().context("no project is open")?;
                        let Some(commit) = project.commit_rebased(output.patch).await? else {
                            return Ok(project.snapshot());
                        };
                        project.record_commit(&commit);
                        let page = project.active_page();
                        (commit, page)
                    };
                    let snapshot = commit.snapshot.clone();
                    self.app
                        .desktop
                        .synchronize(&commit.snapshot, page, &commit)
                        .await?;
                    self.app.publish_canvas(self.app.desktop.canvas_state());
                    Ok(snapshot)
                }
            }

            let mut committer = PipelineCommitter {
                app: task_app.clone(),
            };
            let result = pipeline.execute(snapshot, request, &mut committer).await;
            let (stopped, error) = match result {
                Ok(report) => (report.status == RunStatus::Stopped, None),
                Err(error) => {
                    tracing::error!(stage = ?error.stage, %error, "processing failed");
                    (false, Some(format!("{error:#}")))
                }
            };
            tracing::info!(
                target: "koharu_metrics",
                metric = "pipeline_result",
                outcome = if stopped {
                    "stopped"
                } else if error.is_some() {
                    "failed"
                } else {
                    "completed"
                },
            );
            task_app.processing.stops.lock().remove(&id);
            let job = task_app
                .processing
                .jobs
                .lock()
                .remove(&id)
                .map(|mut job| {
                    job.state = if stopped {
                        JobState::Stopped
                    } else if error.is_some() {
                        JobState::Failed
                    } else {
                        JobState::Finished
                    };
                    job.error = error;
                    job
                });
            if let Some(job) = job {
                task_app.events.publish(Event::Job(job));
            }
        }));
        Ok(id)
    }
}
