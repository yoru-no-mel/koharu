use parking_lot::Mutex;
use tauri::{State, ipc::Channel};

use super::Error;
use crate::core::{
    SharedApp,
    jobs::{Job, JobId},
};

#[derive(Default)]
pub(crate) struct JobChannel {
    pub(crate) channel: Mutex<Option<Channel<Job>>>,
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn process(
    app: State<'_, SharedApp>,
    scope: koharu_pipeline::Scope,
    operation: koharu_pipeline::Operation,
) -> std::result::Result<JobId, Error> {
    Ok(app.inner().clone().process(scope, operation).await?)
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "pipeline_stop",
    skip_all,
    fields(state = "requested")
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn stop_job(
    job: JobId,
    app: State<'_, SharedApp>,
) -> std::result::Result<(), Error> {
    app.stop_job(job)?;
    Ok(())
}
