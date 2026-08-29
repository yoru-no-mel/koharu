//! Page export, thumbnails, and rendered previews shared by the desktop
//! dialog flow, the agent tooling, and the HTTP API.

use anyhow::{Context as _, Result};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use image::{
    ExtendedColorType, ImageEncoder as _,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use koharu_psd::{PsdExportOptions, export_page};
use koharu_rasterizer::{Raster, RasterOptions, Rasterizer};
use koharu_renderer::Frame;
use koharu_scene::{AssetRole, EntityId, Snapshot};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::Arc;
use utoipa::ToSchema;

const THUMBNAIL_EDGE: u32 = 128;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema, Type)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Png,
    Psd,
}

impl super::App {
    /// Exports the given pages (empty means all pages) of the open project
    /// into `directory` as PNG images or layered PSD documents.
    #[tracing::instrument(
        target = "koharu_metrics",
        name = "export",
        skip_all,
        fields(format = ?format),
    )]
    pub async fn export_pages(
        &self,
        pages: Vec<EntityId>,
        format: ExportFormat,
        directory: std::path::PathBuf,
    ) -> Result<()> {
        let snapshot = {
            let project = self.project.lock().await;
            let project = project.as_ref().context("no project is open")?;
            project.snapshot()
        };
        export_snapshot_pages(&self.desktop, &snapshot, pages, format, directory).await
    }

    /// WebP thumbnail of a page's source image.
    pub async fn page_thumbnail(&self, page: EntityId) -> Result<Vec<u8>> {
        let snapshot = self
            .project
            .lock()
            .await
            .as_ref()
            .context("no project is open")?
            .snapshot();
        snapshot.page(page)?;
        let blob = snapshot
            .asset(page, &AssetRole::new("source")?)?
            .with_context(|| format!("page {page} has no source image"))?
            .blob;
        let bytes = snapshot.read_blob(blob).await?;
        tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
            let image = image::load_from_memory(&bytes).context("failed to decode source image")?;
            if image.width() == 0 || image.height() == 0 {
                return Err(anyhow::anyhow!("source image is empty"));
            }
            let image = image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE).to_rgba8();
            let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
            Ok(encoder.encode(80.0).to_vec())
        })
        .await
        .context("thumbnail worker stopped unexpectedly")?
    }

    /// Rendered WebP preview of a page at review resolution, used by the
    /// agent's view tooling.
    pub async fn rendered_preview(&self, page: EntityId) -> Result<Vec<u8>> {
        let snapshot = self
            .project
            .lock()
            .await
            .as_ref()
            .context("no project is open")?
            .snapshot();
        snapshot.page(page)?;
        let renderer = self.desktop.renderer();
        let rasterizer = self.desktop.rasterizer().await?;
        let frame = renderer.render(&snapshot, page).await?;
        let image = rasterize(rasterizer, &frame, RasterOptions::default())
            .await?
            .image;
        tokio::task::spawn_blocking(move || {
            let image = image::DynamicImage::ImageRgba8(image)
                .resize(1024, 1024, image::imageops::FilterType::Lanczos3)
                .to_rgba8();
            let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
            Ok::<_, anyhow::Error>(encoder.encode(85.0).to_vec())
        })
        .await
        .context("preview encode worker stopped unexpectedly")?
    }
}

/// Path-parameterized export shared by the desktop dialog flow and the
/// headless HTTP handler.
#[tracing::instrument(
    target = "koharu_metrics",
    name = "export",
    skip_all,
    fields(format = ?format),
)]
pub(crate) async fn export_snapshot_pages(
    desktop: &koharu_desktop::Desktop,
    snapshot: &Snapshot,
    pages: Vec<EntityId>,
    format: ExportFormat,
    directory: std::path::PathBuf,
) -> Result<()> {
    let pages = if pages.is_empty() {
        snapshot.pages().map(|page| page.id()).collect()
    } else {
        pages
    };
    if pages.is_empty() {
        return Err(anyhow::anyhow!("there are no pages to export").into());
    }
    let renderer = desktop.renderer();
    let rasterizer = desktop.rasterizer().await?;
    let jobs = pages
        .into_iter()
        .enumerate()
        .map(|(index, page_id)| {
            let page = snapshot.page(page_id)?.page()?;
            let name = page
                .label
                .trim()
                .trim_end_matches(|character: char| character == '.' || character.is_whitespace());
            let name = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
            let name = name
                .chars()
                .map(|character| {
                    if matches!(
                        character,
                        '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                    ) {
                        '_'
                    } else {
                        character
                    }
                })
                .collect::<String>();
            let stem = format!(
                "{:04}_{}",
                index + 1,
                if name.is_empty() { "page" } else { &name }
            );
            Ok::<_, anyhow::Error>((page_id, stem))
        })
        .collect::<Result<Vec<_>>>()?;
    stream::iter(jobs)
        .map(|(page_id, stem)| {
            let renderer = renderer.clone();
            let rasterizer = Arc::clone(&rasterizer);
            let snapshot = snapshot.clone();
            let directory = directory.clone();
            async move {
                let frame = renderer.render(&snapshot, page_id).await?;
                match format {
                    ExportFormat::Png => {
                        let image =
                            rasterize(Arc::clone(&rasterizer), &frame, RasterOptions::default())
                                .await?
                                .image;
                        tokio::task::spawn_blocking(move || -> Result<()> {
                            let file =
                                std::fs::File::create(directory.join(format!("{stem}.png")))?;
                            PngEncoder::new_with_quality(
                                file,
                                CompressionType::Best,
                                FilterType::Adaptive,
                            )
                            .write_image(
                                image.as_raw(),
                                image.width(),
                                image.height(),
                                ExtendedColorType::Rgba8,
                            )?;
                            Ok(())
                        })
                        .await
                        .context("PNG export worker stopped unexpectedly")??;
                    }
                    ExportFormat::Psd => {
                        let bytes = export_page(
                            Arc::clone(&rasterizer),
                            &snapshot,
                            &frame,
                            &PsdExportOptions::default(),
                        )
                        .await?;
                        tokio::fs::write(directory.join(format!("{stem}.psd")), bytes).await?;
                    }
                }
                tracing::info!(
                    target: "koharu_metrics",
                    metric = "page_exported",
                    format = ?format,
                );
                Ok::<_, anyhow::Error>(())
            }
        })
        .buffer_unordered(4)
        .try_collect::<Vec<_>>()
        .await?;
    Ok(())
}

async fn rasterize(
    rasterizer: Arc<Rasterizer>,
    frame: &Frame,
    options: RasterOptions,
) -> Result<Raster> {
    let frame = frame.raster_frame()?;
    tokio::task::spawn_blocking(move || rasterizer.rasterize(&frame, options))
        .await
        .context("rasterizer worker stopped unexpectedly")?
        .map_err(Into::into)
}
