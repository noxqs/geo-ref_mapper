//! Resizing, cropping and format conversion.
//!
//! Pure logic: nothing here touches the UI, and nothing here decides *which*
//! files to work on. That separation is deliberate — the batch runner can then
//! be tested without a window, and these operations can be tested without a
//! filesystem.
//!
//! **Nothing in this module overwrites or deletes anything.** [`write_output`]
//! refuses to write over a file that already exists unless told to in as many
//! words, and no path here removes or truncates a source. Losing an original to
//! a batch conversion is not a recoverable mistake, so it is made hard rather
//! than merely discouraged.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use image::DynamicImage;

/// What to encode as. Kept separate from the decode side because the set of
/// things worth *writing* is much smaller than the set worth reading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// Lossless, universally readable. The safe default.
    Png,
    /// Lossy, but the only sensible choice for photographs at a small size.
    Jpeg,
    /// Lossless or lossy; here always lossless, since a conversion that quietly
    /// degrades is worse than a larger file.
    WebP,
    /// Lossless and fast, though little else reads it.
    Qoi,
    /// Lossless, and the right answer for anything with more than 8 bits.
    Tiff,
    /// Keeps the source bytes exactly. Used when only a rename or a copy into
    /// an output folder is wanted.
    Original,
}

impl Target {
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::WebP => "WebP",
            Self::Qoi => "QOI",
            Self::Tiff => "TIFF",
            Self::Original => "Unchanged",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
            Self::Qoi => "qoi",
            Self::Tiff => "tif",
            Self::Original => "",
        }
    }

    /// True when encoding to this format discards information.
    pub fn is_lossy(self) -> bool {
        matches!(self, Self::Jpeg)
    }

    /// Whether an alpha channel survives. Flattening onto white without saying
    /// so would silently ruin a cut-out image.
    pub fn keeps_alpha(self) -> bool {
        !matches!(self, Self::Jpeg)
    }

    pub const ALL: &'static [Target] = &[
        Target::Png,
        Target::Jpeg,
        Target::WebP,
        Target::Qoi,
        Target::Tiff,
        Target::Original,
    ];
}

/// How to change the dimensions.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Resize {
    #[default]
    None,
    /// Fit inside a box, keeping the aspect ratio. Never enlarges.
    Fit { width: u32, height: u32 },
    /// Longest edge becomes this. Never enlarges.
    LongestEdge(u32),
    /// A percentage of the original, where 100 is unchanged.
    Percent(u32),
    /// Exactly these dimensions, aspect ratio be damned. Not offered in the UI,
    /// where distorting an image by accident is easier than doing it on purpose,
    /// but kept because a batch driven from elsewhere may genuinely want it.
    #[allow(dead_code)]
    Exact { width: u32, height: u32 },
}

/// A rectangle to keep, in source pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Crop {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Everything one operation does, in the order it happens.
#[derive(Clone, Debug)]
pub struct Recipe {
    pub crop: Option<Crop>,
    pub resize: Resize,
    pub target: Target,
    /// 1-100. Ignored by the lossless targets.
    pub quality: u8,
    /// Applied after cropping, as EXIF or the container asked.
    pub orientation: crate::exif::Orientation,
}

impl Default for Recipe {
    fn default() -> Self {
        Self {
            crop: None,
            resize: Resize::None,
            target: Target::Png,
            // High enough that the loss is hard to see, low enough to be worth
            // doing at all.
            quality: 92,
            orientation: crate::exif::Orientation::Normal,
        }
    }
}

impl Recipe {
    /// True when this would rewrite pixels rather than just re-containing them.
    pub fn changes_pixels(&self) -> bool {
        self.crop.is_some()
            || self.resize != Resize::None
            || self.orientation != crate::exif::Orientation::Normal
    }
}

/// Works out the size an image becomes, without doing the work. Used by the UI
/// to show the result before anything is written.
pub fn resized_dimensions(width: u32, height: u32, resize: Resize) -> (u32, u32) {
    let (width, height) = (width.max(1), height.max(1));
    match resize {
        Resize::None => (width, height),
        Resize::Exact {
            width: w,
            height: h,
        } => (w.max(1), h.max(1)),
        Resize::Percent(percent) => {
            let scale = f64::from(percent.clamp(1, 1000)) / 100.0;
            (
                ((width as f64 * scale).round() as u32).max(1),
                ((height as f64 * scale).round() as u32).max(1),
            )
        }
        Resize::LongestEdge(edge) => {
            let edge = edge.max(1);
            if width.max(height) <= edge {
                // Enlarging invents detail, so shrinking is all this does.
                (width, height)
            } else {
                let [w, h] = crate::loader::fit_within(width, height, edge);
                (w.max(1), h.max(1))
            }
        }
        Resize::Fit {
            width: box_width,
            height: box_height,
        } => {
            let (box_width, box_height) = (box_width.max(1), box_height.max(1));
            if width <= box_width && height <= box_height {
                return (width, height);
            }
            // Scale by whichever axis is more constrained.
            let scale = (f64::from(box_width) / f64::from(width))
                .min(f64::from(box_height) / f64::from(height));
            (
                ((width as f64 * scale).round() as u32).max(1),
                ((height as f64 * scale).round() as u32).max(1),
            )
        }
    }
}

/// Applies a recipe to a decoded image. Cropping first, then resizing, then
/// orientation — doing it in that order means the crop rectangle is in the
/// coordinates the user was looking at.
pub fn apply(image: &DynamicImage, recipe: &Recipe, threads: usize) -> Result<DynamicImage> {
    let mut working = match recipe.crop {
        Some(crop) => {
            let (width, height) = (image.width(), image.height());
            if crop.x >= width || crop.y >= height {
                bail!(
                    "crop starts at {},{} which is outside a {width} x {height} image",
                    crop.x,
                    crop.y
                );
            }
            // Clamp rather than fail: a rectangle dragged past the edge is a
            // normal thing to do.
            let w = crop.width.min(width - crop.x).max(1);
            let h = crop.height.min(height - crop.y).max(1);
            image.crop_imm(crop.x, crop.y, w, h)
        }
        None => image.clone(),
    };

    let (width, height) = (working.width(), working.height());
    let (target_width, target_height) = resized_dimensions(width, height, recipe.resize);
    if (target_width, target_height) != (width, height) {
        working = if target_width <= width && target_height <= height {
            // Downscaling: the parallel area filter, which is both faster and
            // less prone to aliasing than a general resample.
            crate::loader::downscale_to(&working, target_width, target_height, threads)
        } else {
            // Enlarging genuinely needs interpolation.
            working.resize_exact(
                target_width,
                target_height,
                image::imageops::FilterType::CatmullRom,
            )
        };
    }

    Ok(recipe.orientation.apply(working))
}

/// Encodes to the target format's bytes.
pub fn encode(image: &DynamicImage, target: Target, quality: u8) -> Result<Vec<u8>> {
    use image::ImageEncoder as _;

    let mut out = Vec::new();
    match target {
        Target::Original => bail!("Target::Original has no encoding of its own"),
        Target::Png => {
            // Best compression available without an extra dependency; this is
            // the archival target, so size matters more than encode time.
            let encoder = image::codecs::png::PngEncoder::new_with_quality(
                std::io::Cursor::new(&mut out),
                image::codecs::png::CompressionType::Best,
                image::codecs::png::FilterType::Adaptive,
            );
            let rgba = image.to_rgba8();
            encoder.write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )?;
        }
        Target::Jpeg => {
            // JPEG has no alpha, so anything transparent has to go somewhere.
            // White is the least surprising, and it is stated in the docs rather
            // than left for the user to discover.
            let rgb = flatten_onto_white(image);
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                std::io::Cursor::new(&mut out),
                quality.clamp(1, 100),
            );
            encoder.encode(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )?;
        }
        Target::WebP => {
            // The `image` crate only writes lossless WebP, which is what we
            // want anyway for a conversion.
            let encoder =
                image::codecs::webp::WebPEncoder::new_lossless(std::io::Cursor::new(&mut out));
            let rgba = image.to_rgba8();
            encoder.write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )?;
        }
        Target::Qoi => {
            let rgba = image.to_rgba8();
            let encoder = image::codecs::qoi::QoiEncoder::new(std::io::Cursor::new(&mut out));
            encoder.write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )?;
        }
        Target::Tiff => {
            let rgba = image.to_rgba8();
            let encoder = image::codecs::tiff::TiffEncoder::new(std::io::Cursor::new(&mut out));
            encoder.write_image(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )?;
        }
    }
    Ok(out)
}

/// Composites onto white, for targets with no alpha channel.
fn flatten_onto_white(image: &DynamicImage) -> image::RgbImage {
    match image {
        // Already opaque: no compositing needed, and no copy beyond the convert.
        DynamicImage::ImageRgb8(buffer) => buffer.clone(),
        other => {
            let rgba = other.to_rgba8();
            let mut rgb = image::RgbImage::new(rgba.width(), rgba.height());
            for (source, destination) in rgba.pixels().zip(rgb.pixels_mut()) {
                let alpha = source.0[3] as u32;
                for channel in 0..3 {
                    let over = source.0[channel] as u32 * alpha;
                    let under = 255 * (255 - alpha);
                    destination.0[channel] = ((over + under) / 255) as u8;
                }
            }
            rgb
        }
    }
}

/// Where a converted file should go, given a source and an output folder.
///
/// Keeps the stem and swaps the extension, so `holiday.psd` becomes
/// `holiday.png`. Never returns a path equal to the source.
pub fn output_path(
    source: &Path,
    output_dir: &Path,
    target: Target,
    suffix: &str,
) -> Result<PathBuf> {
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .context("source has no usable file name")?;

    let name = match target {
        Target::Original => {
            let extension = source.extension().and_then(|e| e.to_str()).unwrap_or("bin");
            format!("{stem}{suffix}.{extension}")
        }
        _ => format!("{stem}{suffix}.{}", target.extension()),
    };

    let destination = output_dir.join(name);
    if destination == source {
        bail!(
            "that would write over the original at {} — choose another folder or a suffix",
            source.display()
        );
    }
    Ok(destination)
}

/// What happened to one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Written {
        path: PathBuf,
        bytes: u64,
    },
    /// The destination already existed and overwriting was not permitted.
    Skipped {
        path: PathBuf,
    },
}

/// Writes bytes to `destination`, refusing to clobber anything.
///
/// The refusal is the point. A batch job over a folder of originals is exactly
/// the situation where a mistake is unrecoverable, so overwriting has to be
/// asked for explicitly and per job.
pub fn write_output(destination: &Path, bytes: &[u8], overwrite: bool) -> Result<Outcome> {
    if destination.exists() && !overwrite {
        return Ok(Outcome::Skipped {
            path: destination.to_path_buf(),
        });
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(destination, bytes)
        .with_context(|| format!("writing {}", destination.display()))?;

    Ok(Outcome::Written {
        path: destination.to_path_buf(),
        bytes: bytes.len() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32) -> DynamicImage {
        DynamicImage::ImageRgba8(image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        }))
    }

    #[test]
    fn fit_never_enlarges_and_keeps_the_aspect_ratio() {
        // 4000x3000 into a 1000x1000 box: constrained by width.
        assert_eq!(
            resized_dimensions(
                4000,
                3000,
                Resize::Fit {
                    width: 1000,
                    height: 1000
                }
            ),
            (1000, 750)
        );
        // Already smaller than the box: untouched.
        assert_eq!(
            resized_dimensions(
                400,
                300,
                Resize::Fit {
                    width: 1000,
                    height: 1000
                }
            ),
            (400, 300)
        );
        // Constrained by height instead.
        assert_eq!(
            resized_dimensions(
                1000,
                4000,
                Resize::Fit {
                    width: 900,
                    height: 800
                }
            ),
            (200, 800)
        );
    }

    #[test]
    fn longest_edge_shrinks_but_does_not_grow() {
        assert_eq!(
            resized_dimensions(4000, 2000, Resize::LongestEdge(1000)),
            (1000, 500)
        );
        assert_eq!(
            resized_dimensions(2000, 4000, Resize::LongestEdge(1000)),
            (500, 1000)
        );
        assert_eq!(
            resized_dimensions(300, 200, Resize::LongestEdge(1000)),
            (300, 200),
            "already small enough"
        );
    }

    #[test]
    fn percentages_and_exact_sizes_behave() {
        assert_eq!(
            resized_dimensions(1000, 500, Resize::Percent(50)),
            (500, 250)
        );
        assert_eq!(
            resized_dimensions(1000, 500, Resize::Percent(100)),
            (1000, 500)
        );
        assert_eq!(
            resized_dimensions(1000, 500, Resize::Percent(200)),
            (2000, 1000)
        );
        assert_eq!(
            resized_dimensions(
                1000,
                500,
                Resize::Exact {
                    width: 64,
                    height: 64
                }
            ),
            (64, 64)
        );
    }

    #[test]
    fn no_resize_setting_ever_collapses_an_axis_to_zero() {
        for resize in [
            Resize::Percent(1),
            Resize::LongestEdge(1),
            Resize::Fit {
                width: 1,
                height: 1,
            },
            Resize::Exact {
                width: 0,
                height: 0,
            },
        ] {
            let (width, height) = resized_dimensions(4000, 3, resize);
            assert!(
                width >= 1 && height >= 1,
                "{resize:?} gave {width}x{height}"
            );
        }
    }

    #[test]
    fn cropping_then_resizing_happens_in_that_order() {
        let source = gradient(100, 100);
        let recipe = Recipe {
            crop: Some(Crop {
                x: 20,
                y: 20,
                width: 40,
                height: 40,
            }),
            resize: Resize::Exact {
                width: 10,
                height: 10,
            },
            ..Default::default()
        };
        let out = apply(&source, &recipe, 2).unwrap();
        assert_eq!((out.width(), out.height()), (10, 10));
    }

    #[test]
    fn a_crop_running_off_the_edge_is_clamped_not_refused() {
        let source = gradient(50, 50);
        let recipe = Recipe {
            crop: Some(Crop {
                x: 40,
                y: 40,
                width: 999,
                height: 999,
            }),
            ..Default::default()
        };
        let out = apply(&source, &recipe, 1).expect("should clamp");
        assert_eq!((out.width(), out.height()), (10, 10));
    }

    #[test]
    fn a_crop_starting_outside_the_image_is_an_error() {
        let source = gradient(50, 50);
        let recipe = Recipe {
            crop: Some(Crop {
                x: 60,
                y: 0,
                width: 10,
                height: 10,
            }),
            ..Default::default()
        };
        assert!(apply(&source, &recipe, 1).is_err());
    }

    #[test]
    fn rotation_is_applied_and_swaps_the_axes() {
        let source = gradient(40, 10);
        let recipe = Recipe {
            orientation: crate::exif::Orientation::Rotate90,
            ..Default::default()
        };
        let out = apply(&source, &recipe, 1).unwrap();
        assert_eq!((out.width(), out.height()), (10, 40));
    }

    #[test]
    fn lossless_targets_round_trip_pixels_exactly() {
        let source = gradient(64, 48);
        for target in [Target::Png, Target::WebP, Target::Qoi, Target::Tiff] {
            let bytes = encode(&source, target, 100).unwrap_or_else(|e| panic!("{target:?}: {e}"));
            let decoded = image::load_from_memory(&bytes)
                .unwrap_or_else(|e| panic!("{target:?} did not decode: {e}"));
            assert_eq!(
                decoded.to_rgba8().as_raw(),
                source.to_rgba8().as_raw(),
                "{target:?} claims to be lossless but changed the pixels"
            );
        }
    }

    #[test]
    fn jpeg_is_lossy_but_close_and_flattens_transparency() {
        let source = gradient(64, 48);
        let bytes = encode(&source, Target::Jpeg, 95).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (64, 48));

        // Every pixel should be close, even though none need be exact.
        let source_rgba = source.to_rgba8();
        let worst = decoded
            .pixels()
            .zip(source_rgba.pixels())
            .map(|(a, b)| {
                (0..3)
                    .map(|c| (a.0[c] as i32 - b.0[c] as i32).abs())
                    .max()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0);
        assert!(
            worst < 24,
            "quality 95 should stay close, worst channel was {worst}"
        );
        assert!(Target::Jpeg.is_lossy());
        assert!(!Target::Jpeg.keeps_alpha());
    }

    #[test]
    fn transparency_is_flattened_onto_white_rather_than_black() {
        let transparent = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            8,
            8,
            image::Rgba([255, 0, 0, 0]),
        ));
        let rgb = flatten_onto_white(&transparent);
        assert_eq!(rgb.get_pixel(0, 0).0, [255, 255, 255]);

        let half = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            4,
            4,
            image::Rgba([0, 0, 0, 128]),
        ));
        // Half-transparent black over white lands near mid grey.
        assert!((flatten_onto_white(&half).get_pixel(0, 0).0[0] as i32 - 127).abs() <= 2);
    }

    #[test]
    fn output_paths_swap_the_extension_and_keep_the_stem() {
        let source = Path::new("/photos/holiday.psd");
        let out = Path::new("/exports");
        assert_eq!(
            output_path(source, out, Target::Png, "").unwrap(),
            PathBuf::from("/exports/holiday.png")
        );
        assert_eq!(
            output_path(source, out, Target::Jpeg, "_small").unwrap(),
            PathBuf::from("/exports/holiday_small.jpg")
        );
        // Original keeps whatever it was.
        assert_eq!(
            output_path(source, out, Target::Original, "_copy").unwrap(),
            PathBuf::from("/exports/holiday_copy.psd")
        );
    }

    /// The rule the whole module exists to guarantee.
    #[test]
    fn an_output_that_would_land_on_the_source_is_refused() {
        let source = Path::new("/photos/holiday.png");
        let same = Path::new("/photos");
        assert!(
            output_path(source, same, Target::Png, "").is_err(),
            "writing a PNG next to a PNG of the same name must be refused"
        );
        // A suffix makes it safe again.
        assert!(output_path(source, same, Target::Png, "_v2").is_ok());
    }

    #[test]
    fn writing_never_clobbers_unless_told_to() {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-ops-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.png");

        let first = write_output(&path, b"original", false).unwrap();
        assert!(matches!(first, Outcome::Written { .. }));

        // Second write must leave the first alone.
        let second = write_output(&path, b"replacement", false).unwrap();
        assert_eq!(second, Outcome::Skipped { path: path.clone() });
        assert_eq!(std::fs::read(&path).unwrap(), b"original");

        // And only go through when explicitly permitted.
        let third = write_output(&path, b"replacement", true).unwrap();
        assert!(matches!(third, Outcome::Written { .. }));
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Converts one file, so the result can be diffed against another tool.
    /// Correctness of a decoder is not something unit tests can establish on
    /// their own — an independent implementation has to agree.
    ///
    /// `GEO_REF_MAPPER_IN=a.psd GEO_REF_MAPPER_OUT=/tmp/out.png cargo test --release convert_one -- --ignored --nocapture`
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_IN and GEO_REF_MAPPER_OUT"]
    fn convert_one_file() {
        let (Ok(input), Ok(output)) = (
            std::env::var("GEO_REF_MAPPER_IN"),
            std::env::var("GEO_REF_MAPPER_OUT"),
        ) else {
            eprintln!("set GEO_REF_MAPPER_IN and GEO_REF_MAPPER_OUT");
            return;
        };

        let decoded =
            crate::loader::decode_still(std::path::Path::new(&input), 8192).expect("decoding");
        println!(
            "  decoded {} -> {} x {}",
            input,
            decoded.width(),
            decoded.height()
        );

        let encoded = encode(&decoded, Target::Png, 100).expect("encoding");
        // Overwrite here is fine: the caller named the destination explicitly.
        std::fs::write(&output, &encoded).expect("writing");
        println!("  wrote {output} ({} bytes)", encoded.len());
    }

    #[test]
    fn a_recipe_knows_whether_it_touches_pixels() {
        assert!(
            !Recipe::default().changes_pixels(),
            "a plain convert does not"
        );
        assert!(
            Recipe {
                resize: Resize::Percent(50),
                ..Default::default()
            }
            .changes_pixels()
        );
        assert!(
            Recipe {
                crop: Some(Crop {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                }),
                ..Default::default()
            }
            .changes_pixels()
        );
    }
}
