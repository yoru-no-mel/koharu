use specta::Type;
use tauri::{State, ipc::IpcResponse};

use super::Error;
use crate::core::SharedApp;

#[derive(Type)]
#[specta(transparent)]
pub(crate) struct FontPreviewBytes(#[specta(type = Vec<u8>)] Vec<u8>);

impl IpcResponse for FontPreviewBytes {
    fn body(self) -> tauri::Result<tauri::ipc::InvokeResponseBody> {
        Ok(self.0.into())
    }
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_fonts(
    app: State<'_, SharedApp>,
) -> std::result::Result<Vec<crate::core::fonts::FontFamily>, Error> {
    Ok(app.list_fonts().await?)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_font_preview(
    family_name: String,
    app: State<'_, SharedApp>,
) -> std::result::Result<FontPreviewBytes, Error> {
    Ok(FontPreviewBytes(app.font_preview(&family_name).await?))
}
