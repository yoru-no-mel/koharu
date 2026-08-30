//! HTTP handlers: thin adapters from REST requests onto the shared `App`.
//!
//! Every handler mirrors a Tauri command body — the same `core::App` method
//! calls, the same DTO types — so the two transports cannot drift. Request
//! body structs are local because headless replaces native file dialogs
//! with explicit paths in JSON.

use anyhow::Context as _;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::Json;
use koharu_app::SharedApp;
use koharu_app::core::export::ExportFormat;
use koharu_app::core::import::PageFile;
use koharu_app::core::fonts::FontFamily;
use koharu_app::core::jobs::JobId;
use koharu_app::core::preferences::{Preferences, ProviderPreferences};
use koharu_app::core::project::{
    Page, PageSelection, PageSummary, PageText, ProjectInfo, ProjectSummary,
};
use koharu_pipeline::{Operation, Scope};
use serde::Deserialize;
use utoipa::ToSchema;

use crate::error::ApiError;

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct CreateProject {
    pub name: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/projects",
    responses((status = 200, description = "All known projects, most recently used first", body = [ProjectSummary]))
)]
pub(crate) async fn list_projects(
    State(app): State<SharedApp>,
) -> Result<Json<Vec<ProjectSummary>>, ApiError> {
    Ok(Json(app.list_projects().await?))
}

#[utoipa::path(
    post,
    path = "/api/v1/projects",
    request_body = CreateProject,
    responses((status = 201, description = "Project created and opened", body = ProjectInfo))
)]
pub(crate) async fn create_project(
    State(app): State<SharedApp>,
    Json(request): Json<CreateProject>,
) -> Result<(StatusCode, Json<ProjectInfo>), ApiError> {
    app.create_project(&request.name).await?;
    let info = app
        .project_info()
        .await?
        .context("the created project is not the active project")?;
    Ok((StatusCode::CREATED, Json(info)))
}

#[utoipa::path(
    delete,
    path = "/api/v1/projects/{name}",
    params(("name" = String, Path, description = "Project name")),
    responses((status = 204, description = "Project deleted; closes it when active"))
)]
pub(crate) async fn delete_project(
    State(app): State<SharedApp>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    app.delete_project(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/api/v1/projects/{name}/open",
    params(("name" = String, Path, description = "Project name")),
    responses((status = 200, description = "Project opened", body = ProjectInfo))
)]
pub(crate) async fn open_project(
    State(app): State<SharedApp>,
    Path(name): Path<String>,
) -> Result<Json<ProjectInfo>, ApiError> {
    app.open_project(&name).await?;
    let info = app
        .project_info()
        .await?
        .context("the opened project is not the active project")?;
    Ok(Json(info))
}

#[utoipa::path(
    get,
    path = "/api/v1/project",
    responses((status = 200, description = "The active project, if any", body = Option<ProjectInfo>))
)]
pub(crate) async fn get_project(
    State(app): State<SharedApp>,
) -> Result<Json<Option<ProjectInfo>>, ApiError> {
    Ok(Json(app.project_info().await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/pages",
    responses((status = 200, description = "Pages of the active project", body = [PageSummary]))
)]
pub(crate) async fn get_pages(
    State(app): State<SharedApp>,
) -> Result<Json<Vec<PageSummary>>, ApiError> {
    Ok(Json(app.pages().await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/pages/{id}",
    params(("id" = uuid::Uuid, Path, description = "Page id")),
    responses((status = 200, description = "Full page view with layers and regions", body = Page))
)]
pub(crate) async fn get_page(
    State(app): State<SharedApp>,
    Path(page): Path<koharu_scene::EntityId>,
) -> Result<Json<Page>, ApiError> {
    Ok(Json(app.page_view(page).await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/texts",
    responses((status = 200, description = "Original and translated text of every page in the active project, in page order", body = [PageText]))
)]
pub(crate) async fn get_texts(
    State(app): State<SharedApp>,
) -> Result<Json<Vec<PageText>>, ApiError> {
    Ok(Json(app.texts().await?))
}

#[utoipa::path(
    post,
    path = "/api/v1/pages/{id}/select",
    params(("id" = uuid::Uuid, Path, description = "Page id")),
    responses((status = 200, description = "Page selected and shown on the canvas", body = PageSelection))
)]
pub(crate) async fn select_page(
    State(app): State<SharedApp>,
    Path(page): Path<koharu_scene::EntityId>,
) -> Result<Json<PageSelection>, ApiError> {
    Ok(Json(app.select_page(page).await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/pages/{id}/thumbnail",
    params(("id" = uuid::Uuid, Path, description = "Page id")),
    responses((status = 200, description = "WebP thumbnail of the page's source image", content_type = "image/webp", body = Vec<u8>))
)]
pub(crate) async fn get_thumbnail(
    State(app): State<SharedApp>,
    Path(page): Path<koharu_scene::EntityId>,
) -> Result<Response, ApiError> {
    let bytes = app.page_thumbnail(page).await?;
    Ok(([(header::CONTENT_TYPE, "image/webp")], bytes).into_response())
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ImportPages {
    #[schema(value_type = Vec<String>)]
    pub paths: Vec<std::path::PathBuf>,
}

#[utoipa::path(
    post,
    path = "/api/v1/pages/import",
    request_body = ImportPages,
    responses((status = 204, description = "Pages appended to the active project"))
)]
pub(crate) async fn import_pages(
    State(app): State<SharedApp>,
    Json(request): Json<ImportPages>,
) -> Result<StatusCode, ApiError> {
    if request.paths.is_empty() {
        return Err(ApiError::bad_request("no import paths were provided"));
    }
    app.import_pages(request.paths).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ProcessRequest {
    #[schema(value_type = Object)]
    pub scope: Scope,
    #[schema(value_type = Object)]
    pub operation: Operation,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExportQuery {
    pub format: Option<ExportFormat>,
}

#[utoipa::path(
    post,
    path = "/api/v1/pages/upload",
    request_body(
        content_type = "multipart/form-data",
        description = "Page containers (raster images, CBZ/ZIP, RAR, PDF) as repeated file fields. The client-supplied file names decide container format and page order."
    ),
    responses((status = 204, description = "Pages appended to the active project"))
)]
pub(crate) async fn import_uploaded(
    State(app): State<SharedApp>,
    mut multipart: Multipart,
) -> Result<StatusCode, ApiError> {
    let mut files = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| ApiError::bad_request(format!("invalid multipart upload: {error}")))?
    {
        let Some(name) = field.file_name().map(str::to_owned) else {
            continue;
        };
        let bytes = field.bytes().await.map_err(|error| {
            ApiError::bad_request(format!("failed to read uploaded file {name:?}: {error}"))
        })?;
        files.push(PageFile {
            name,
            bytes: bytes.into(),
        });
    }
    if files.is_empty() {
        return Err(ApiError::bad_request("no files were uploaded"));
    }
    app.import_uploaded_pages(files).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/v1/pages/{id}/export",
    params(
        ("id" = uuid::Uuid, Path, description = "Page id"),
        ("format" = ExportFormat, Query, description = "Output format; defaults to png")
    ),
    responses(
        (status = 200, description = "Rendered page at export quality", content_type = "image/png", body = Vec<u8>),
        (status = 404, description = "Page not found", body = serde_json::Value),
    )
)]
pub(crate) async fn export_page(
    State(app): State<SharedApp>,
    Path(page): Path<koharu_scene::EntityId>,
    Query(query): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    let format = query.format.unwrap_or(ExportFormat::Png);
    let bytes = app.render_page(page, format).await?;
    Ok(([(header::CONTENT_TYPE, format.content_type())], bytes).into_response())
}

#[utoipa::path(
    post,
    path = "/api/v1/process",
    request_body = ProcessRequest,
    responses((status = 200, description = "Processing job started; progress arrives on the event stream", body = JobId))
)]
pub(crate) async fn process(
    State(app): State<SharedApp>,
    Json(request): Json<ProcessRequest>,
) -> Result<Json<JobId>, ApiError> {
    Ok(Json(app.process(request.scope, request.operation).await?))
}

#[utoipa::path(
    post,
    path = "/api/v1/process/{job}/stop",
    params(("job" = uuid::Uuid, Path, description = "Job id returned by the process endpoint")),
    responses((status = 204, description = "Stop requested; the terminal job state arrives on the event stream"))
)]
pub(crate) async fn stop_job(
    State(app): State<SharedApp>,
    Path(job): Path<JobId>,
) -> Result<StatusCode, ApiError> {
    app.stop_job(job)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct ExportRequest {
    #[schema(value_type = Vec<uuid::Uuid>)]
    pub pages: Vec<koharu_scene::EntityId>,
    pub format: ExportFormat,
    #[schema(value_type = String)]
    pub directory: std::path::PathBuf,
}

#[utoipa::path(
    post,
    path = "/api/v1/export",
    request_body = ExportRequest,
    responses((status = 204, description = "Pages exported into the given directory"))
)]
pub(crate) async fn export_pages(
    State(app): State<SharedApp>,
    Json(request): Json<ExportRequest>,
) -> Result<StatusCode, ApiError> {
    app.export_pages(request.pages, request.format, request.directory)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/api/v1/fonts",
    responses((status = 200, description = "Installed fonts with metadata and faces", body = [FontFamily]))
)]
pub(crate) async fn get_fonts(
    State(app): State<SharedApp>,
) -> Result<Json<Vec<FontFamily>>, ApiError> {
    Ok(Json(app.list_fonts().await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/fonts/{family}/preview",
    params(("family" = String, Path, description = "Font family name")),
    responses((status = 200, description = "WebP preview image of the family", content_type = "image/webp", body = Vec<u8>))
)]
pub(crate) async fn get_font_preview(
    State(app): State<SharedApp>,
    Path(family): Path<String>,
) -> Result<Response, ApiError> {
    let bytes = app.font_preview(&family).await?;
    Ok(([(header::CONTENT_TYPE, "image/webp")], bytes).into_response())
}

#[utoipa::path(
    get,
    path = "/api/v1/preferences",
    responses((status = 200, description = "Current pipeline, provider, and typesetting preferences", body = Preferences))
)]
pub(crate) async fn get_preferences() -> Result<Json<Preferences>, ApiError> {
    Ok(Json(Preferences::load()?))
}

#[derive(Debug, Deserialize, ToSchema)]
pub(crate) struct SavePreferences {
    #[schema(value_type = Object)]
    pub pipeline: koharu_pipeline::PipelineConfig,
    pub providers: ProviderPreferences,
    #[schema(value_type = Object)]
    pub typesetting: koharu_renderer::TypesettingConfig,
}

#[utoipa::path(
    post,
    path = "/api/v1/preferences",
    request_body = SavePreferences,
    responses((status = 200, description = "Preferences saved; returns the reloaded view", body = Preferences))
)]
pub(crate) async fn save_preferences(
    Json(request): Json<SavePreferences>,
) -> Result<Json<Preferences>, ApiError> {
    Ok(Json(koharu_app::core::preferences::save(
        request.pipeline,
        request.providers,
        request.typesetting,
    )?))
}

#[utoipa::path(
    get,
    path = "/api/v1/translation/models",
    responses((status = 200, description = "Translation models available to the local runtime", body = Vec<serde_json::Value>))
)]
pub(crate) async fn get_translation_models() -> Result<Json<Vec<koharu_translator::Model>>, ApiError>
{
    Ok(Json(koharu_app::core::preferences::translation_models().await?))
}
