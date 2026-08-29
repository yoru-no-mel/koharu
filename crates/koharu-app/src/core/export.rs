//! Page export, thumbnails, and rendered previews shared by the desktop
//! dialog flow, the agent tooling, and the HTTP API.

use anyhow::{Context as _, Result};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use image::{
    ExtendedColorType, ImageEncoder as _,
    codecs::jpeg::JpegEncoder,
    codecs::png::{CompressionType, FilterType, PngEncoder},
};
use koharu_psd::{PsdExportOptions, export_page};
use koharu_rasterizer::{Raster, RasterOptions, Rasterizer};
use koharu_renderer::Frame;
use koharu_scene::{AssetRole, EntityId, Snapshot};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::io::Cursor;
use std::sync::Arc;
use utoipa::ToSchema;

const THUMBNAIL_EDGE: u32 = 128;

// Lossy export quality. WebP at 90 keeps manga line art visually
// indistinguishable from PNG at a fraction of the size; JPEG needs a
// slightly higher setting to avoid ringing around thin strokes.
const JPEG_QUALITY: u8 = 92;
const WEBP_QUALITY: f32 = 90.0;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ToSchema, Type)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Png,
    Jpeg,
    Webp,
    Psd,
}

impl ExportFormat {
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Psd => "application/octet-stream",
        }
    }

    fn file_extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Psd => "psd",
        }
    }
}

impl super::App {
    /// Exports the given pages (empty means all pages) of the open project
    /// into `directory` as raster images or layered PSD documents.
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

    /// Renders one page at export quality and returns the encoded bytes —
    /// the download counterpart of directory-based export for API clients
    /// on other machines.
    pub async fn render_page(&self, page: EntityId, format: ExportFormat) -> Result<Vec<u8>> {
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
        encode_page(format, rasterizer, &snapshot, &frame).await
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
                let bytes = encode_page(format, rasterizer, &snapshot, &frame).await?;
                let path = directory.join(format!(
                    "{stem}.{}",
                    format.file_extension()
                ));
                tokio::fs::write(path, bytes).await?;
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

/// Renders and encodes one page in the requested format. Raster formats
/// share the rasterizer output; PSD bypasses it entirely.
async fn encode_page(
    format: ExportFormat,
    rasterizer: Arc<Rasterizer>,
    snapshot: &Snapshot,
    frame: &Frame,
) -> Result<Vec<u8>> {
    match format {
        ExportFormat::Psd => {
            export_page(rasterizer, snapshot, frame, &PsdExportOptions::default())
                .await
                .map_err(Into::into)
        }
        ExportFormat::Png | ExportFormat::Jpeg | ExportFormat::Webp => {
            let image = rasterize(rasterizer, frame, RasterOptions::default())
                .await?
                .image;
            tokio::task::spawn_blocking(move || encode_raster(format, image))
                .await
                .context("page encode worker stopped unexpectedly")?
        }
    }
}

fn encode_raster(format: ExportFormat, image: image::RgbaImage) -> Result<Vec<u8>> {
    match format {
        ExportFormat::Png => {
            let mut png = Vec::new();
            PngEncoder::new_with_quality(
                Cursor::new(&mut png),
                CompressionType::Best,
                FilterType::Adaptive,
            )
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                ExtendedColorType::Rgba8,
            )?;
            Ok(png)
        }
        ExportFormat::Jpeg => {
            let flat = image::DynamicImage::ImageRgba8(flatten_white(&image)).to_rgb8();
            let mut jpeg = Vec::new();
            JpegEncoder::new_with_quality(Cursor::new(&mut jpeg), JPEG_QUALITY).write_image(
                flat.as_raw(),
                flat.width(),
                flat.height(),
                ExtendedColorType::Rgb8,
            )?;
            Ok(jpeg)
        }
        ExportFormat::Webp => {
            let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
            Ok(encoder.encode(WEBP_QUALITY).to_vec())
        }
        ExportFormat::Psd => unreachable!("PSD never reaches the raster encoder"),
    }
}

/// JPEG has no alpha channel, so transparent pixels composite onto white
/// (manga paper) before encoding.
fn flatten_white(image: &image::RgbaImage) -> image::RgbaImage {
    let mut flat =
        image::RgbaImage::from_pixel(image.width(), image.height(), image::Rgba([255, 255, 255, 255]));
    image::imageops::overlay(&mut flat, image, 0, 0);
    flat
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
