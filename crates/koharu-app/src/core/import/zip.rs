use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

use super::{EncodedPage, Format};

pub(crate) fn extract_bytes(label: &str, bytes: Vec<u8>) -> Result<Vec<EncodedPage>> {
    let mut archive = ::zip::ZipArchive::new(Cursor::new(bytes))
        .with_context(|| format!("failed to read ZIP/CBZ container {label}"))?;
    let mut images = Vec::new();

    for index in 0..archive.len() {
        let mut member = archive
            .by_index(index)
            .with_context(|| format!("failed to read member {index} from ZIP/CBZ container {label}"))?;
        if member.is_dir() {
            continue;
        }
        let name = member.name().to_owned();
        let member_path = Path::new(&name);
        let supported = member_path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| matches!(extension.parse(), Ok(Format::Raster)));
        let metadata = member_path
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .is_none_or(|file_name| {
                file_name.starts_with("._")
                    || [".ds_store", "thumbs.db", "desktop.ini"]
                        .iter()
                        .any(|candidate| file_name.eq_ignore_ascii_case(candidate))
            })
            || member_path.components().any(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|component| component.eq_ignore_ascii_case("__macosx"))
            });
        if !supported || metadata {
            continue;
        }
        let mut bytes = Vec::new();
        member
            .read_to_end(&mut bytes)
            .with_context(|| {
                format!("failed to read image member {name:?} from ZIP/CBZ container {label}")
            })?;
        images.push(EncodedPage { name, bytes });
    }

    alphanumeric_sort::sort_slice_by_os_str_key(&mut images, |image| {
        std::ffi::OsStr::new(&image.name)
    });
    if images.is_empty() {
        bail!("ZIP/CBZ container {label} contains no supported raster images");
    }
    Ok(images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageFormat;
    use std::io::Write as _;

    fn sample_png() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(2, 3, image::Rgba([255, 0, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encode PNG");
        bytes.into_inner()
    }

    fn archive_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = ::zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = ::zip::write::SimpleFileOptions::default();
        for (name, bytes) in entries {
            archive.start_file(*name, options).expect("start member");
            archive.write_all(bytes).expect("write member");
        }
        archive.finish().expect("finish archive").into_inner()
    }

    #[test]
    fn reads_images_in_natural_member_order_and_skips_metadata() {
        let png = sample_png();
        let bytes = archive_bytes(&[
            ("ComicInfo.xml", b"<ComicInfo />"),
            ("pages/page10.png", &png),
            ("pages/page2.png", &png),
            ("__MACOSX/._page3.png", &png),
        ]);
        let images = extract_bytes("chapter.cbz", bytes).expect("read archive");
        assert_eq!(
            images
                .iter()
                .map(|image| image.name.as_str())
                .collect::<Vec<_>>(),
            ["pages/page2.png", "pages/page10.png"]
        );
    }

    #[test]
    fn rejects_archive_without_supported_images() {
        let bytes = archive_bytes(&[("ComicInfo.xml", b"<ComicInfo />")]);
        let error = extract_bytes("chapter.cbz", bytes).expect_err("archive should be rejected");
        assert!(error.to_string().contains("no supported raster images"));
    }
}
