use koharu_scene::EntityId;
use specta::Type;
use tauri::{Cef, State, WebviewWindow, ipc::IpcResponse};

use super::Error;
use crate::core::{SharedApp, export::ExportFormat};

#[derive(Type)]
#[specta(transparent)]
pub(crate) struct ThumbnailBytes(#[specta(type = Vec<u8>)] Vec<u8>);

impl IpcResponse for ThumbnailBytes {
    fn body(self) -> tauri::Result<tauri::ipc::InvokeResponseBody> {
        Ok(self.0.into())
    }
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "export",
    skip_all,
    fields(origin = "user", format = ?format),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn export_pages(
    window: WebviewWindow<Cef>,
    pages: Vec<EntityId>,
    format: ExportFormat,
    app: State<'_, SharedApp>,
) -> std::result::Result<(), Error> {
    let Some(directory) = rfd::AsyncFileDialog::new()
        .set_parent(&window)
        .pick_folder()
        .await
        .map(|directory| directory.path().to_owned())
    else {
        return Ok(());
    };
    app.export_pages(pages, format, directory)
        .await
        .map_err(Error::from)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_thumbnail(
    page: EntityId,
    app: State<'_, SharedApp>,
) -> std::result::Result<ThumbnailBytes, Error> {
    Ok(ThumbnailBytes(app.page_thumbnail(page).await?))
}
