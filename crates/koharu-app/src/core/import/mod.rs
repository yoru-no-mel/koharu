use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context as _, Result, bail};
use image::{ImageFormat, ImageReader};
use rayon::prelude::*;
use strum::{EnumIter, EnumMessage, EnumString};

mod pdf;
mod rar;
mod zip;

#[derive(Clone, Copy, EnumIter, EnumMessage, EnumString)]
#[strum(ascii_case_insensitive)]
pub(crate) enum Format {
    #[strum(
        serialize = "png",
        serialize = "jpg",
        serialize = "jpeg",
        serialize = "webp"
    )]
    Raster,
    #[strum(serialize = "cbz", serialize = "zip")]
    Zip,
    #[strum(serialize = "rar")]
    Rar,
    #[strum(serialize = "pdf")]
    Pdf,
}

#[derive(Debug)]
pub(crate) struct EncodedPage {
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
}

/// One uploaded page container: a raster image, CBZ/ZIP, RAR, or PDF with
/// the file name it arrived under. The name drives format detection and
/// page ordering exactly like a path-based import would.
#[derive(Debug)]
pub struct PageFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

pub(crate) struct Page {
    pub(crate) name: String,
    pub(crate) bytes: Arc<[u8]>,
    pub(crate) format: ImageFormat,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

fn decode(label: &str, source: EncodedPage) -> Result<Page> {
    let EncodedPage { name, bytes } = source;
    let format = image::guess_format(&bytes)
        .with_context(|| format!("failed to identify imported image {name:?} from {label}"))?;
    let (width, height) = ImageReader::with_format(Cursor::new(bytes.as_slice()), format)
        .into_dimensions()
        .with_context(|| {
            format!("failed to read dimensions of imported image {name:?} from {label}")
        })?;
    Ok(Page {
        name,
        bytes: Arc::<[u8]>::from(bytes),
        format,
        width,
        height,
    })
}

pub(crate) fn import(mut paths: Vec<PathBuf>) -> Result<Vec<Page>> {
    alphanumeric_sort::sort_slice_by_os_str_key(&mut paths, |path| {
        path.file_name().unwrap_or_else(|| path.as_os_str())
    });
    let files = paths
        .into_par_iter()
        .map(|path| -> Result<PageFile> {
            Ok(PageFile {
                name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "page".to_owned()),
                bytes: fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    import_payloads(files)
}

/// Decodes uploaded page containers in natural file-name order. Containers
/// are expanded exactly like a path-based import: raster images pass
/// through, CBZ/ZIP/RAR archives yield their image members (skipping
/// metadata junk), PDFs render at 300 DPI.
pub(crate) fn import_payloads(mut files: Vec<PageFile>) -> Result<Vec<Page>> {
    alphanumeric_sort::sort_slice_by_os_str_key(&mut files, |file| {
        std::ffi::OsStr::new(file.name.as_str())
    });
    let mut groups = files
        .into_par_iter()
        .map(|file| -> Result<Vec<Page>> {
            let extension = Path::new(&file.name)
                .extension()
                .and_then(|extension| extension.to_str())
                .and_then(|extension| extension.parse::<Format>().ok());
            let encoded = match extension {
                Some(Format::Raster) => vec![EncodedPage {
                    name: file.name.clone(),
                    bytes: file.bytes,
                }],
                Some(Format::Zip) => zip::extract_bytes(&file.name, file.bytes)?,
                Some(Format::Rar) => rar::extract_bytes(&file.name, file.bytes)?,
                Some(Format::Pdf) => pdf::render_bytes(&file.name, file.bytes)?,
                None => bail!("unsupported page import file {}", file.name),
            };
            encoded
                .into_iter()
                .map(|source| decode(&file.name, source))
                .collect()
        })
        .collect::<Result<Vec<_>>>()?;
    let page_count = groups.iter().map(Vec::len).sum();
    let mut pages = Vec::with_capacity(page_count);
    for group in &mut groups {
        pages.append(group);
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(2, 3, image::Rgba([0, 0, 0, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("encode fixture");
        encoded.into_inner()
    }

    #[test]
    fn uploaded_files_are_naturally_sorted_and_decoded() {
        let png = sample_png();
        let pages = import_payloads(vec![
            PageFile {
                name: "page10.png".to_owned(),
                bytes: png.clone(),
            },
            PageFile {
                name: "page2.png".to_owned(),
                bytes: png.clone(),
            },
            PageFile {
                name: "cover.webp".to_owned(),
                bytes: png,
            },
        ])
        .expect("import uploads");
        assert_eq!(
            pages
                .iter()
                .map(|page| page.name.as_str())
                .collect::<Vec<_>>(),
            ["cover.webp", "page2.png", "page10.png"]
        );
        assert!(pages.iter().all(|page| (page.width, page.height) == (2, 3)));
    }

    #[test]
    fn unsupported_uploads_are_rejected_with_the_file_name() {
        let error = import_payloads(vec![PageFile {
            name: "notes.txt".to_owned(),
            bytes: b"hello".to_vec(),
        }])
        .err()
        .expect("unsupported file should be rejected");
        assert!(error.to_string().contains("notes.txt"));
    }

    #[test]
    fn top_level_paths_are_naturally_sorted() {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "koharu-import-order-{}-{timestamp}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create fixture directory");
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
        let mut encoded = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("encode fixture");
        let paths = ["page10.PNG", "page2.png", "page1.png"].map(|name| directory.join(name));
        for path in &paths {
            fs::write(path, encoded.get_ref()).expect("write fixture");
        }

        let pages = import(paths.into()).expect("import fixtures");
        fs::remove_dir_all(&directory).expect("remove fixture directory");
        assert_eq!(
            pages
                .iter()
                .map(|page| page.name.as_str())
                .collect::<Vec<_>>(),
            ["page1.png", "page2.png", "page10.PNG"]
        );
    }
}
