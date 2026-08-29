use anyhow::Context as _;
use koharu_desktop::{CanvasState, Desktop};
use koharu_scene::EntityId;
use tauri::State;

use super::Error;
use crate::core::{
    SharedApp,
    project::{GeometryUpdate, Page, Project, TypographyUpdate},
};

async fn synchronize_canvas(
    desktop: &Desktop,
    commit: &koharu_scene::Commit,
    page: Option<EntityId>,
) -> anyhow::Result<CanvasState> {
    desktop.synchronize(&commit.snapshot, page, commit).await?;
    Ok(desktop.canvas_state())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "page_renamed",
    skip_all,
    fields(origin = "user", character_count = label.chars().count(), empty = label.is_empty()),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn rename_page(
    page: EntityId,
    label: String,
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.rename_page(page, label).await?;
        project.record_commit(&commit);
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "pages_deleted",
    skip_all,
    fields(origin = "user", entity_count = pages.len()),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn delete_pages(
    pages: Vec<EntityId>,
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.delete_pages(pages).await?;
        project.record_commit(&commit);
        project.reconcile_page();
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "page_moved",
    skip_all,
    fields(origin = "user", page_number = index + 1),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn move_page(
    page: EntityId,
    index: u32,
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.move_page(page, index as usize).await?;
        project.record_commit(&commit);
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "source_text_edited",
    skip_all,
    fields(origin = "user", character_count = text.chars().count(), empty = text.is_empty()),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn set_source_text(
    layer: EntityId,
    text: String,
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.set_source_text(layer, text).await?;
        project.record_commit(&commit);
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "translation_edited",
    skip_all,
    fields(
        origin = "user",
        character_count = text.as_ref().map_or(0, |text| text.chars().count()),
        empty = text.as_ref().is_none_or(String::is_empty),
    ),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn set_translation(
    layer: EntityId,
    text: Option<String>,
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.set_translation(layer, text).await?;
        project.record_commit(&commit);
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "typography_edited",
    skip_all,
    fields(origin = "user", entity_count = updates.len()),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn set_typography(
    updates: Vec<TypographyUpdate>,
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.set_typography(updates).await?;
        project.record_commit(&commit);
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "geometry_edited",
    skip_all,
    fields(origin = "user", entity_count = updates.len()),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn set_geometry(
    updates: Vec<GeometryUpdate>,
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.set_geometry(updates).await?;
        project.record_commit(&commit);
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "visibility_edited",
    skip_all,
    fields(origin = "user", entity_count = layers.len()),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn set_visibility(
    layers: Vec<EntityId>,
    visible: Option<bool>,
    opacity: Option<f32>,
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.set_visibility(layers, visible, opacity).await?;
        project.record_commit(&commit);
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "layers_deleted",
    skip_all,
    fields(origin = "user", entity_count = layers.len()),
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn delete_layers(
    layers: Vec<EntityId>,
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.delete_layers(layers).await?;
        project.record_commit(&commit);
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "layer_moved",
    skip_all,
    fields(origin = "user", entity_count = 1_u64)
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn move_layer(
    layer: EntityId,
    parent: EntityId,
    index: u32,
    app: State<'_, SharedApp>,
) -> Result<Page, Error> {
    let (commit, page, view) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.move_layer(layer, parent, index as usize).await?;
        project.record_commit(&commit);
        let page = project.active_page().context("no active page")?;
        let view = Project::page(&commit.snapshot, page)?;
        (commit, page, view)
    };
    app.desktop
        .synchronize(&commit.snapshot, Some(page), &commit)
        .await?;
    app.publish_canvas(app.desktop.canvas_state());
    Ok(view)
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "undo",
    skip_all,
    fields(origin = "user")
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn undo(
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.undo().await?;
        project.reconcile_page();
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}

#[tracing::instrument(
    target = "koharu_metrics",
    name = "redo",
    skip_all,
    fields(origin = "user")
)]
#[tauri::command]
#[specta::specta]
pub(crate) async fn redo(
    app: State<'_, SharedApp>,
) -> Result<(), Error> {
    let (commit, page) = {
        let mut project = app.project.lock().await;
        let project = project.as_mut().context("no project is open")?;
        let commit = project.redo().await?;
        project.reconcile_page();
        (commit, project.active_page())
    };
    let canvas = synchronize_canvas(&app.desktop, &commit, page).await?;
    app.publish_canvas(canvas);
    Ok(())
}
