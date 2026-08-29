use koharu_pipeline::PipelineConfig;
use koharu_renderer::TypesettingConfig;

use super::Error;
use crate::core::preferences::{self, Preferences, ProviderPreferences};

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_preferences() -> std::result::Result<Preferences, Error> {
    Ok(Preferences::load()?)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn save_preferences(
    pipeline: PipelineConfig,
    providers: ProviderPreferences,
    typesetting: TypesettingConfig,
) -> std::result::Result<Preferences, Error> {
    Ok(preferences::save(pipeline, providers, typesetting)?)
}

#[tauri::command]
#[specta::specta]
pub(crate) async fn get_translation_models(
) -> std::result::Result<Vec<koharu_translator::Model>, Error> {
    Ok(preferences::translation_models().await?)
}
