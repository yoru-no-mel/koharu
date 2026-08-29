//! HTTP adapter over the shared [`koharu_app::core::App`].
//!
//! This crate owns the headless transport: REST routes under `/api/v1`, an
//! SSE event stream, and the OpenAPI document. It holds no application state
//! of its own — every handler receives the same `Arc<App>` the desktop
//! commands use, so both frontends stay behaviorally identical.

use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, header::CONTENT_TYPE};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use tokio::net::TcpListener;
use utoipa::OpenApi;

pub mod error;
pub mod events;
pub mod routes;

pub use error::ApiError;
use koharu_app::SharedApp;

/// Ceiling for one multipart upload request; a full chapter of webp pages
/// fits comfortably, and the route is loopback/LAN oriented.
const UPLOAD_BODY_LIMIT: usize = 1024 * 1024 * 1024;

/// Function mapping a URL path (e.g. `"/index.html"`) to `(bytes, mime)`.
/// Returning `None` signals a 404 fall-through. The headless entry point
/// supplies the prebuilt web UI bundle; development builds may omit it.
pub type AssetResolver = Arc<dyn Fn(&str) -> Option<(Vec<u8>, String)> + Send + Sync>;

/// API routes under `/api/v1` with readiness gating and the SSE stream.
pub fn router(app: SharedApp) -> Router {
    Router::new()
        .route("/projects", get(routes::list_projects).post(routes::create_project))
        .route("/projects/{name}", delete(routes::delete_project))
        .route("/projects/{name}/open", post(routes::open_project))
        .route("/project", get(routes::get_project))
        .route("/pages", get(routes::get_pages))
        .route("/pages/{id}", get(routes::get_page))
        .route("/pages/{id}/select", post(routes::select_page))
        .route("/pages/{id}/thumbnail", get(routes::get_thumbnail))
        .route("/pages/import", post(routes::import_pages))
        .route(
            "/pages/upload",
            post(routes::import_uploaded)
                .layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/pages/{id}/export", get(routes::export_page))
        .route("/process", post(routes::process))
        .route("/process/{job}/stop", post(routes::stop_job))
        .route("/export", post(routes::export_pages))
        .route("/fonts", get(routes::get_fonts))
        .route("/fonts/{family}/preview", get(routes::get_font_preview))
        .route(
            "/preferences",
            get(routes::get_preferences).post(routes::save_preferences),
        )
        .route("/translation/models", get(routes::get_translation_models))
        .route("/events", get(events::stream))
        .layer(middleware::from_fn_with_state(
            app.clone(),
            require_ready,
        ))
        .with_state(app)
}

/// Full application router: versioned API, OpenAPI document, and the static
/// UI fallback when an asset resolver is supplied.
pub fn router_with_assets(app: SharedApp, assets: AssetResolver) -> Router {
    router(app)
        .route("/openapi.json", get(openapi_json))
        .fallback(move |request: Request| {
            let assets = Arc::clone(&assets);
            async move { serve_asset(assets, request).await }
        })
}

/// Serves the API on `listener` until the process ends.
pub async fn serve(app: SharedApp, listener: TcpListener) -> Result<()> {
    axum::serve(listener, router(app).route("/openapi.json", get(openapi_json))).await?;
    Ok(())
}

/// Serves the API plus the static web UI from `assets` on `listener`.
pub async fn serve_with_assets(
    app: SharedApp,
    listener: TcpListener,
    assets: AssetResolver,
) -> Result<()> {
    axum::serve(listener, router_with_assets(app, assets)).await?;
    Ok(())
}

/// Rejects requests until the ML runtime finished initializing so clients
/// observe a deliberate 503 instead of hanging or timing out mid-startup.
async fn require_ready(
    State(app): State<SharedApp>,
    request: Request,
    next: Next,
) -> Response {
    if !app.is_ready() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the runtime is still initializing",
        )
            .into_response();
    }
    next.run(request).await
}

async fn openapi_json() -> Response {
    let spec = ApiDoc::openapi()
        .to_pretty_json()
        .expect("the OpenAPI document is serializable");
    let mut response = Response::new(Body::from(spec));
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn serve_asset(assets: AssetResolver, request: Request) -> Response {
    if request.method() != axum::http::Method::GET {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }
    let path = request.uri().path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some((bytes, mime)) = assets(path)
        && let Ok(header) = HeaderValue::from_str(&mime)
    {
        let mut response = Response::new(Body::from(bytes));
        response.headers_mut().insert(CONTENT_TYPE, header);
        return response;
    }
    (StatusCode::NOT_FOUND, "not found").into_response()
}

#[derive(OpenApi)]
#[openapi(
    info(title = "Koharu Headless API", version = env!("CARGO_PKG_VERSION")),
    paths(
        routes::list_projects,
        routes::create_project,
        routes::delete_project,
        routes::open_project,
        routes::get_project,
        routes::get_pages,
        routes::get_page,
        routes::select_page,
        routes::get_thumbnail,
        routes::import_pages,
        routes::import_uploaded,
        routes::export_page,
        routes::process,
        routes::stop_job,
        routes::export_pages,
        routes::get_fonts,
        routes::get_font_preview,
        routes::get_preferences,
        routes::save_preferences,
        routes::get_translation_models,
    ),
    components(schemas(
        // Payloads of the /events SSE stream; each frame carries one tagged
        // `Event` variant serialized as JSON data.
        koharu_app::core::downloads::DeviceResources,
        koharu_app::core::downloads::Download,
        koharu_app::core::downloads::DownloadState,
        koharu_app::core::downloads::ModelResources,
        koharu_app::core::jobs::Job,
        koharu_app::core::jobs::JobState,
    ))
)]
pub struct ApiDoc;
