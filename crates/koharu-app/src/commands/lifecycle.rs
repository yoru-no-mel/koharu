use koharu_desktop::CanvasState;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use specta::Type;
use strum::{EnumMessage as _, IntoEnumIterator as _};
use tauri::{AppHandle, Cef, Manager as _, State, WebviewWindow, ipc::Channel};
use walkdir::WalkDir;

use super::{
    Error,
    canvas::CanvasChannel,
    processing::JobChannel,
};
use crate::core::{
    SharedApp,
    downloads::{Download, ModelResources},
    import,
    jobs::Job,
    preferences::Preferences,
    project::{Page, PageSelection, PageSummary, ProjectInfo, ProjectSummary},
};

#[derive(Clone, Debug, Serialize, Type)]
pub struct StartupState {
    pub preferences: Preferences,
    pub jobs: Vec<Job>,
    pub canvas: CanvasState,
}

#[derive(Clone, Copy, Debug, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum PageImportSource {
    Files,
    Folder,
}

#[derive(Default)]
pub(crate) struct DownloadChannel {
    pub(crate) channel: Mutex<Option<Channel<Download>>>,
}

#[derive(Default)]
pub(crate) struct ResourceChannel {
    pub(crate) channel: Mutex<Option<Channel<ModelResources>>>,
}

#[derive(Default)]
pub(crate) struct ProjectChannel {
    pub(crate) channel: Mutex<Option<Channel<Option<ProjectInfo>>>>,
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn subscribe(
    app: State<'_, SharedApp>,
    handle: AppHandle<Cef>,
    on_canvas: Channel<CanvasState>,
    on_job: Channel<Job>,
    on_download: Channel<Download>,
    on_resources: Channel<ModelResources>,
    on_project: Channel<Option<ProjectInfo>>,
) -> std::result::Result<StartupState, Error> {
    app.wait_ready().await?;

    *handle.state::<CanvasChannel>().channel.lock() = Some(on_canvas);
    *handle.state::<JobChannel>().channel.lock() = Some(on_job);
    *handle.state::<DownloadChannel>().channel.lock() = Some(on_download);
    *handle.state::<ResourceChannel>().channel.lock() = Some(on_resources);
    *handle.state::<ProjectChannel>().channel.lock() = Some(on_project);

    let canvas = app.desktop.canvas_state();
    let preferences = app.preferences()?;
    Ok(StartupState {
        preferences,
        jobs: app
            .processing
            .jobs
            .lock()
            .values()
            .cloned()
            .collect(),
        canvas,
    })
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_project(
    app: State<'_, SharedApp>,
) -> std::result::Result<Option<ProjectInfo>, Error> {
    Ok(app.project_info().await?)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_pages(
    app: State<'_, SharedApp>,
) -> std::result::Result<Vec<PageSummary>, Error> {
    Ok(app.pages().await?)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_page(
    app: State<'_, SharedApp>,
) -> std::result::Result<Option<Page>, Error> {
    Ok(app.page().await?)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn list_projects(
    app: State<'_, SharedApp>,
) -> std::result::Result<Vec<ProjectSummary>, Error> {
    Ok(app.list_projects().await?)
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "project_created",
    skip_all,
    fields(origin = "user")
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn create_project(
    name: String,
    app: State<'_, SharedApp>,
) -> std::result::Result<(), Error> {
    app.create_project(&name).await?;
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "project_opened",
    skip_all,
    fields(origin = "user")
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn open_project(
    name: String,
    app: State<'_, SharedApp>,
) -> std::result::Result<(), Error> {
    app.open_project(&name).await?;
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "project_closed",
    skip_all,
    fields(origin = "user")
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn close_project(
    app: State<'_, SharedApp>,
) -> std::result::Result<(), Error> {
    app.close_project().await?;
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "project_deleted",
    skip_all,
    fields(origin = "user")
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn delete_project(
    name: String,
    app: State<'_, SharedApp>,
) -> std::result::Result<(), Error> {
    app.delete_project(&name).await?;
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "import",
    skip_all,
    fields(origin = "user", method = ?source),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn import_pages(
    source: PageImportSource,
    window: WebviewWindow<Cef>,
    app: State<'_, SharedApp>,
) -> std::result::Result<(), Error> {
    let extensions = import::Format::iter()
        .flat_map(|format| format.get_serializations())
        .collect::<Vec<_>>();
    let dialog = rfd::AsyncFileDialog::new()
        .add_filter("Images, archives, and PDF", &extensions)
        .set_parent(&window);
    let files = match source {
        PageImportSource::Files => dialog.pick_files().await.map(|files| {
            files
                .into_iter()
                .map(|file| file.path().to_owned())
                .collect::<Vec<_>>()
        }),
        PageImportSource::Folder => dialog.pick_folder().await.map(|folder| {
            WalkDir::new(folder.path())
                .follow_links(false)
                .into_iter()
                .filter_map(|entry| match entry {
                    Ok(entry) if entry.file_type().is_file() => Some(entry.into_path()),
                    Ok(_) => None,
                    Err(error) => {
                        tracing::warn!(%error, "could not inspect an import directory entry");
                        None
                    }
                })
                .filter(|path| {
                    path.extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.parse::<import::Format>().is_ok())
                })
                .collect::<Vec<_>>()
        }),
    };
    let Some(files) = files else {
        return Ok(());
    };
    if files.is_empty() {
        return Err(anyhow::anyhow!("no supported images were found in the selection").into());
    }
    app.import_pages(files).await?;
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "page_selected",
    skip_all,
    fields(origin = "user")
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn select_page(
    app: State<'_, SharedApp>,
    page: koharu_scene::EntityId,
) -> std::result::Result<PageSelection, Error> {
    Ok(app.select_page(page).await?)
}
