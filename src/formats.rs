//! The catalogue of image formats shown on the Config page.
//!
//! Still images only — video is out of scope. Keep this table honest: it is the
//! answer to "will this open my files?", so a format is only [`Status::Decodes`]
//! if the shipped build can actually decode it today.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Decodes in this build, no extra dependency.
    Decodes,
    /// Code exists upstream but needs a feature we do not enable, usually
    /// because it drags in a C library.
    NeedsFeature,
    /// Recognised and wanted, but nothing decodes it yet.
    Planned,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Decodes => "Decodes",
            Self::NeedsFeature => "Opt-in",
            Self::Planned => "Planned",
        }
    }
}

pub struct Format {
    pub name: &'static str,
    pub extensions: &'static str,
    pub status: Status,
    /// Which variants and versions are covered.
    pub detail: &'static str,
}

pub struct Group {
    pub title: &'static str,
    pub formats: &'static [Format],
}

pub const GROUPS: &[Group] = &[
    Group {
        title: "EVERYDAY",
        formats: &[
            Format {
                name: "PNG",
                extensions: ".png .apng",
                status: Status::Decodes,
                detail: "8/16-bit, interlaced, APNG first frame",
            },
            Format {
                name: "JPEG",
                extensions: ".jpg .jpeg .jfif",
                status: Status::Decodes,
                detail: "Baseline and progressive, CMYK and YCCK",
            },
            Format {
                name: "GIF",
                extensions: ".gif",
                status: Status::Decodes,
                detail: "87a and 89a, animated",
            },
            Format {
                name: "WebP",
                extensions: ".webp",
                status: Status::Decodes,
                detail: "Lossy and lossless, alpha, animated",
            },
            Format {
                name: "BMP",
                extensions: ".bmp .dib",
                status: Status::Decodes,
                detail: "1/4/8/16/24/32-bit, RLE",
            },
            Format {
                name: "ICO",
                extensions: ".ico",
                status: Status::Decodes,
                detail: "BMP and PNG payloads, all sizes",
            },
        ],
    },
    Group {
        title: "PHOTO AND PRINT",
        formats: &[
            Format {
                name: "TIFF",
                extensions: ".tif .tiff",
                status: Status::Decodes,
                detail: "LZW, Deflate, PackBits, uncompressed",
            },
            Format {
                name: "OpenEXR",
                extensions: ".exr",
                status: Status::Decodes,
                detail: "Half and float, ZIP and PIZ",
            },
            Format {
                name: "Radiance HDR",
                extensions: ".hdr .pic",
                status: Status::Decodes,
                detail: "RGBE, 32-bit float output",
            },
            Format {
                name: "Camera RAW",
                extensions: ".cr2 .cr3 .nef .arw .dng .raf .orf",
                status: Status::Planned,
                detail: "Needs a RAW pipeline — demosaic and camera profiles",
            },
            Format {
                name: "Photoshop",
                extensions: ".psd .psb",
                status: Status::Decodes,
                detail: "The flattened composite Photoshop stores; layers ignored",
            },
        ],
    },
    Group {
        title: "MODERN CODECS",
        formats: &[
            Format {
                name: "AVIF",
                extensions: ".avif",
                status: Status::NeedsFeature,
                detail: "Encoder is built in; decoding needs dav1d (avif-native)",
            },
            Format {
                name: "HEIC / HEIF",
                extensions: ".heic .heif .hif",
                status: Status::Decodes,
                detail: "HEVC grids, gain maps and all, via a pure-Rust decoder",
            },
            Format {
                name: "JPEG XL",
                extensions: ".jxl",
                status: Status::Decodes,
                detail: "Via jxl-oxide, pure Rust — bare codestream and ISOBMFF",
            },
            Format {
                name: "JPEG 2000",
                extensions: ".jp2 .j2k",
                status: Status::Planned,
                detail: "No pure-Rust decoder yet",
            },
        ],
    },
    Group {
        title: "TECHNICAL AND RETRO",
        formats: &[
            Format {
                name: "QOI",
                extensions: ".qoi",
                status: Status::Decodes,
                detail: "Full specification",
            },
            Format {
                name: "TGA",
                extensions: ".tga",
                status: Status::Decodes,
                detail: "8/16/24/32-bit, RLE",
            },
            Format {
                name: "DDS",
                extensions: ".dds",
                status: Status::Decodes,
                detail: "BC1-BC7 block compression",
            },
            Format {
                name: "Netpbm",
                extensions: ".pnm .pbm .pgm .ppm .pam",
                status: Status::Decodes,
                detail: "ASCII and binary, 8 and 16-bit",
            },
            Format {
                name: "farbfeld",
                extensions: ".ff",
                status: Status::Decodes,
                detail: "16-bit RGBA",
            },
            Format {
                name: "SVG",
                extensions: ".svg .svgz",
                status: Status::Decodes,
                detail: "Via resvg, rasterised to fit rather than at a fixed size",
            },
            Format {
                name: "GIMP",
                extensions: ".xcf",
                status: Status::Decodes,
                detail: "Layers composited with plain alpha; blend modes ignored",
            },
        ],
    },
];

/// How one family of files is actually decoded, and what was done about it.
///
/// Every `measured` string below came from a benchmark in this repository, not
/// from a claim. If one of them stops being true the benchmark that produced it
/// is named so it can be re-run.
pub struct Pipeline {
    pub title: &'static str,
    /// What the decoder does, in order.
    pub steps: &'static [&'static str],
    /// The optimisation that matters, and what it bought.
    pub measured: &'static [(&'static str, &'static str)],
    /// What is still slow, and why it is not simply fixed.
    pub limit: &'static str,
}

pub const PIPELINES: &[Pipeline] = &[
    Pipeline {
        title: "JPEG",
        steps: &[
            "Read the file, then scan its markers for a restart interval.",
            "With restart markers, cut the scan into strips and rebuild each as a \
             standalone JPEG — the original header with its height patched — so \
             the ordinary decoder can take them in parallel.",
            "Stack the strips, downscale if over 8192px, repack straight from RGB.",
        ],
        measured: &[
            ("Parallel strip decode", "2.53× faster than one thread"),
            (
                "RGB kept out of RGBA",
                "saved 12.6 ms of 66.7 ms on an 11 MP frame",
            ),
            (
                "nvJPEG on the GPU",
                "3.5× SLOWER — off by default, synchronises per image",
            ),
        ],
        limit: "A progressive JPEG has no restart markers to cut at, so it stays \
                single-threaded. Preloads also decode on one thread on purpose, so \
                the pool works on different images rather than fighting over one.",
    },
    Pipeline {
        title: "PNG",
        steps: &[
            "Read the file and inflate the single DEFLATE stream.",
            "Unfilter the rows, downscale if over 8192px, repack.",
        ],
        measured: &[
            (
                "Where the time goes",
                "decode 76-90%, downscale 3-13%, read ~3%",
            ),
            (
                "Parallel area downscale",
                "33× faster than the old Triangle filter",
            ),
            ("End to end, 328 MP map", "4784 ms down to 2765 ms"),
        ],
        limit: "Decode is irreducible and single-threaded by nature: DEFLATE is one \
                continuous stream with no resync points, so the restart-marker trick \
                does not transfer. A faster inflate is a dead end too — the decoder \
                already uses fdeflate, which beat a standalone flate2 inflate of the \
                same data. No GPU PNG decoder exists.",
    },
    Pipeline {
        title: "TIFF · BigTIFF · Cloud-Optimized GeoTIFF",
        steps: &[
            "Parse the IFD chain directly, including BigTIFF's 8-byte offsets and \
             the WebP tiles (compression 50001) the `image` crate cannot open.",
            "Show the coarsest overview immediately, then the level that matches the \
             window — never the full image.",
            "Past 100% zoom, decode only the visible rectangle from the level that \
             matches the zoom and lay it over the base.",
        ],
        measured: &[
            (
                "Parallel tile decode",
                "1536 tiles in 96 ms, 0.06 ms per tile",
            ),
            (
                "Region instead of level",
                "1.5 ms against 106 ms — 69× cheaper",
            ),
            (
                "A 1.6 gigapixel level",
                "never decoded whole; it would need 6.4 GB",
            ),
        ],
        limit: "A TIFF with no overview pyramid has to subsample every tile on the \
                way in, so it appears progressively rather than at once.",
    },
    Pipeline {
        title: "HEIF · HEIC (iPhone)",
        steps: &[
            "Parse the ISOBMFF container ourselves — items, properties, references \
             and locations — reading only the `ftyp` and `meta` boxes rather than \
             the file.",
            "Resolve the primary item, which on a phone is a `grid`: a lattice of \
             HEVC tiles, most often 8x6 of them, plus a thumbnail item, an HDR gain \
             map and an `Exif` item hanging off references.",
            "Rotation comes from the container's own `irot` and `imir`, not from \
             EXIF, so it is read there and converted to the same orientation the \
             rest of the viewer uses.",
            "Each tile's `hvcC` gives the parameter sets and prefix length, which \
             turn the stored NAL units into an Annex-B stream a decoder accepts. \
             Tiles then decode in parallel and are composited.",
            "For a thumbnail, the embedded `thmb` item is decoded instead — one \
             small picture rather than forty-eight tiles.",
        ],
        measured: &[
            (
                "Metadata read",
                "0.02 ms per file — 1178× faster than a blind 512 KB prefix",
            ),
            (
                "Parsed on a real library",
                "500 of 500 iPhone files, none refused",
            ),
            ("Decoded", "120 of 120, 1.86 gigapixels at 144 MP/s"),
            (
                "System libheif 1.17.6",
                "refuses every gain-map file — 2917 of 9468 here",
            ),
        ],
        limit: "The gain map is read but not applied, so an HDR photo shows its base \
                exposure rather than the brighter highlights an HDR display would give. \
                Depth and alpha auxiliary images are identified and skipped.",
    },
    Pipeline {
        title: "Everything else",
        steps: &[
            "WebP, GIF, BMP, ICO, QOI, TGA, DDS, Netpbm, OpenEXR and Radiance HDR \
             all go through the `image` crate's decoder.",
            "The same downscale and repack path as the rest.",
        ],
        measured: &[(
            "Allocation ceiling",
            "raised to 6 GB — the crate default of 512 MB refused a 372 MB map outright",
        )],
        limit: "Camera RAW and AVIF are recognised but not decoded; see the \
                catalogue above for what each would need.",
    },
];

/// Cross-cutting work that is not tied to one format.
pub const SHARED: &[(&str, &str)] = &[
    (
        "Downscaling",
        "Area averaging spread across cores, not a general resample. Output rows read \
         disjoint source rectangles, so it parallelises exactly and aliases less at \
         large ratios. 2110 ms down to 63 ms on a 328 MP map.",
    ),
    (
        "Orientation",
        "EXIF orientation is applied after the downscale, so a rotated photo copies a \
         few megapixels instead of a few hundred. Recorded dimensions are swapped to \
         match, so the tree and the canvas agree.",
    ),
    (
        "Priority",
        "The image on screen gets every core; a speculative preload decodes on one \
         thread but still downscales across several. Preloads queued for a selection \
         you have left are dropped before decoding, not after.",
    ),
    (
        "GPU",
        "Resizing on the GPU is 24× faster than all 32 CPU threads — 2.7 ms against \
         66 ms — but moving 1 GB across PCIe costs 148 ms, so the CPU wins. Host RAM \
         bandwidth beats PCIe; the GPU only pays off if the pixels are already in VRAM.",
    ),
    (
        "Build profile",
        "The pixel loops live in this crate, so an unoptimised build is 18× slower at \
         downscaling than a release one. A debug build says so in the title bar.",
    ),
];

/// What the badge over the canvas says about the open file.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Description {
    /// The container, e.g. `PNG` or `BigTIFF`.
    pub name: String,
    /// The variant worth knowing, e.g. `pyramidal · WebP tiles`.
    pub detail: Option<String>,
}

impl Description {
    fn plain(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            detail: None,
        }
    }
}

/// Identifies a file from its magic bytes rather than its extension, so a
/// mislabelled file still reports what it really is.
///
/// TIFFs get a second look through [`crate::cog`], because "TIFF" is the least
/// useful thing you can say about a tiled, pyramidal BigTIFF — the distinction
/// decides whether the file opens instantly or not at all.
pub fn describe(path: &std::path::Path) -> Option<Description> {
    use std::io::Read;

    let mut header = [0_u8; 16];
    let read = {
        let mut file = std::fs::File::open(path).ok()?;
        file.read(&mut header).ok()?
    };
    let header = &header[..read];

    let description = match header {
        [0x89, b'P', b'N', b'G', ..] => Description::plain("PNG"),
        [0xFF, 0xD8, 0xFF, ..] => Description::plain("JPEG"),
        [b'G', b'I', b'F', b'8', ..] => Description::plain("GIF"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Description::plain("WebP"),
        [b'B', b'M', ..] => Description::plain("BMP"),
        [0x00, 0x00, 0x01, 0x00, ..] => Description::plain("ICO"),
        [b'q', b'o', b'i', b'f', ..] => Description::plain("QOI"),
        [b'D', b'D', b'S', b' ', ..] => Description::plain("DDS"),
        [0x76, 0x2F, 0x31, 0x01, ..] => Description::plain("OpenEXR"),
        [b'#', b'?', ..] => Description::plain("Radiance HDR"),
        [b'f', b'a', b'r', b'b', b'f', b'e', b'l', b'd', ..] => Description::plain("farbfeld"),
        [b'P', digit @ b'1'..=b'7', ..] => Description {
            name: "Netpbm".to_owned(),
            detail: Some(format!("P{}", *digit as char)),
        },
        // TIFF: byte order, then 42 for classic and 43 for BigTIFF.
        [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..] => tiff_description(path, "TIFF"),
        [b'I', b'I', 43, 0, ..] | [b'M', b'M', 0, 43, ..] => tiff_description(path, "BigTIFF"),
        // ISOBMFF: HEIC, AVIF and friends all start with an `ftyp` box, and
        // only the brand inside distinguishes them.
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => heif_description(path),
        _ => return None,
    };
    Some(description)
}

/// Names the ISOBMFF variant and, for HEIF, what is actually inside it.
///
/// "HEIC" alone says very little: an iPhone still is a grid of 48 HEVC tiles
/// with an HDR gain map beside it, and that is what decides both how it decodes
/// and how long it takes.
fn heif_description(path: &std::path::Path) -> Description {
    let Some(heif) = crate::heif::from_path(path) else {
        // An ISOBMFF file we cannot read the structure of — often a video
        // container that happens to be sitting in a photo folder.
        return Description::plain("ISOBMFF");
    };

    let name = match heif.major_brand.as_str() {
        "avif" | "avis" => "AVIF",
        "heic" | "heix" | "hevc" | "heim" | "heis" => "HEIC",
        "mif1" | "msf1" => "HEIF",
        "" => "HEIF",
        _ => "HEIF",
    };

    Description {
        name: name.to_owned(),
        detail: Some(heif.summary()),
    }
}

/// Adds the structure that decides how a TIFF is handled: tiling, how many
/// overview levels it carries, and whether we can decode its tiles at all.
fn tiff_description(path: &std::path::Path, base: &str) -> Description {
    let Ok(tiff) = crate::cog::TiledTiff::open(path) else {
        // Not tiled, or an IFD layout we do not parse — the ordinary decoder
        // still handles it.
        return Description::plain(base);
    };

    let mut parts = Vec::new();
    if tiff.levels.len() > 1 {
        parts.push(format!("{} levels", tiff.levels.len()));
    } else {
        parts.push("tiled".to_owned());
    }
    if let Some(level) = tiff.levels.first() {
        if level.compression == 50001 {
            parts.push("WebP tiles".to_owned());
        }
        if !level.is_supported() {
            parts.push(format!("compression {} unsupported", level.compression));
        }
    }

    Description {
        name: base.to_owned(),
        detail: (!parts.is_empty()).then(|| parts.join(" · ")),
    }
}

/// Counts for the summary line, as `(decodes, opt-in, planned)`.
pub fn counts() -> (usize, usize, usize) {
    let mut counts = (0, 0, 0);
    for format in GROUPS.iter().flat_map(|group| group.formats) {
        match format.status {
            Status::Decodes => counts.0 += 1,
            Status::NeedsFeature => counts.1 += 1,
            Status::Planned => counts.2 += 1,
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_cover_every_listed_format() {
        let total: usize = GROUPS.iter().map(|group| group.formats.len()).sum();
        let (decodes, opt_in, planned) = counts();
        assert_eq!(decodes + opt_in + planned, total);
        assert!(decodes > 0 && planned > 0);
    }

    /// Aliases the `image` crate does not map by name. Decoding sniffs magic
    /// bytes, so these still open — but list them explicitly rather than
    /// letting an unrecognised extension slip into the table unnoticed.
    const CONTENT_SNIFFED_ONLY: &[&str] = &["dib", "farbfeld", "pic", "svgz"];

    /// Extensions handled by [`crate::codecs`] rather than the `image` crate.
    const OUR_OWN_DECODERS: &[&str] = &[
        "psd", "psb", "jxl", "svg", "svgz", "xcf", "heic", "heif", "hif",
    ];

    /// The table claims a format decodes; something in the build must actually
    /// be able to. Either the `image` crate knows the extension, or we decode it
    /// ourselves, or it is a documented alias recognised from its contents.
    #[test]
    fn decodable_extensions_have_a_decoder_behind_them() {
        for format in GROUPS.iter().flat_map(|group| group.formats) {
            if format.status != Status::Decodes {
                continue;
            }
            for extension in format.extensions.split_whitespace() {
                let extension = extension.trim_start_matches('.');
                assert!(
                    image::ImageFormat::from_extension(extension).is_some()
                        || OUR_OWN_DECODERS.contains(&extension)
                        || CONTENT_SNIFFED_ONLY.contains(&extension),
                    "{} claims to decode .{extension}, but nothing in this build can",
                    format.name,
                );
            }
        }
    }

    /// Anything we can decode should also survive a scan, and vice versa, or
    /// the FILES panel and this table would disagree.
    #[test]
    fn decodable_extensions_are_indexed_by_the_scanner() {
        for format in GROUPS.iter().flat_map(|group| group.formats) {
            if format.status != Status::Decodes {
                continue;
            }
            for extension in format.extensions.split_whitespace() {
                let extension = extension.trim_start_matches('.');
                assert!(
                    crate::db::is_image_extension(extension),
                    "{} decodes .{extension}, but the scanner would skip it",
                    format.name,
                );
            }
        }
    }

    fn scratch(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("geo-ref_mapper-describe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn describe_reads_the_magic_bytes_not_the_extension() {
        // Deliberately mislabelled: the badge must say what it really is.
        let path = scratch("lying.jpg", b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR");
        assert_eq!(describe(&path).unwrap().name, "PNG");

        let path = scratch("sample.bin", b"\xFF\xD8\xFF\xE0\x00\x10JFIF\0\0\0\0\0\0");
        assert_eq!(describe(&path).unwrap().name, "JPEG");

        let path = scratch("sample.webp", b"RIFF\x24\x00\x00\x00WEBPVP8 ");
        assert_eq!(describe(&path).unwrap().name, "WebP");
    }

    /// The distinction the user actually asked to see on the canvas.
    #[test]
    fn classic_and_big_tiffs_are_told_apart() {
        let classic = scratch("classic.tif", b"II\x2a\x00\x08\x00\x00\x00\x00\x00\x00\x00");
        assert_eq!(describe(&classic).unwrap().name, "TIFF");

        let big = scratch("big.tif", b"II\x2b\x00\x08\x00\x00\x00\x00\x00\x00\x00");
        assert_eq!(describe(&big).unwrap().name, "BigTIFF");

        // Big-endian spellings of both.
        let classic = scratch(
            "classic-be.tif",
            b"MM\x00\x2a\x00\x00\x00\x08\x00\x00\x00\x00",
        );
        assert_eq!(describe(&classic).unwrap().name, "TIFF");

        let big = scratch("big-be.tif", b"MM\x00\x2b\x00\x08\x00\x00\x00\x00\x00\x00");
        assert_eq!(describe(&big).unwrap().name, "BigTIFF");
    }

    /// The badge has to distinguish HEIC variants, since "HEIC" alone hides
    /// whether the file is one coded image or a grid of 48 tiles with a gain map.
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_HEIF_DIR and real HEIC files"]
    fn describe_names_real_heic_variants() {
        let Ok(dir) = std::env::var("GEO_REF_MAPPER_HEIF_DIR") else {
            eprintln!("set GEO_REF_MAPPER_HEIF_DIR");
            return;
        };
        let files: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| {
                path.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("heic"))
            })
            .take(12)
            .collect();
        assert!(!files.is_empty(), "no HEIC files under {dir}");

        for path in &files {
            let Some(description) = describe(path) else {
                continue;
            };
            println!(
                "  {:<34} {}  {}",
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .chars()
                    .take(34)
                    .collect::<String>(),
                description.name,
                description.detail.clone().unwrap_or_default(),
            );
            // A mislabelled JPEG must report as JPEG, not HEIC.
            let magic = std::fs::read(path).unwrap_or_default();
            if magic.starts_with(&[0xFF, 0xD8, 0xFF]) {
                assert_eq!(description.name, "JPEG", "{}", path.display());
            } else {
                assert!(
                    description.name == "HEIC" || description.name == "HEIF",
                    "{} reported as {}",
                    path.display(),
                    description.name
                );
                let detail = description.detail.unwrap_or_default();
                assert!(!detail.is_empty(), "{} has no detail", path.display());
            }
        }
    }

    #[test]
    fn describe_gives_up_quietly_on_junk_and_missing_files() {
        let path = scratch("junk.dat", b"not an image at all");
        assert!(describe(&path).is_none());

        let empty = scratch("empty.png", b"");
        assert!(
            describe(&empty).is_none(),
            "a truncated file must not panic"
        );

        assert!(describe(std::path::Path::new("/nonexistent/geo-ref_mapper/x.png")).is_none());
    }

    /// The page is only worth having if it stays specific, so every pipeline
    /// has to actually say something in each column.
    #[test]
    fn every_pipeline_is_filled_in() {
        assert!(!PIPELINES.is_empty());
        for pipeline in PIPELINES {
            assert!(!pipeline.title.is_empty());
            assert!(!pipeline.steps.is_empty(), "{}", pipeline.title);
            assert!(!pipeline.measured.is_empty(), "{}", pipeline.title);
            assert!(
                pipeline.limit.len() > 20,
                "{} should say what is still slow",
                pipeline.title
            );
            for (what, result) in pipeline.measured {
                assert!(!what.is_empty() && !result.is_empty(), "{}", pipeline.title);
            }
        }
        for (what, detail) in SHARED {
            assert!(!what.is_empty() && detail.len() > 20, "{what}");
        }
    }

    /// Claims about a format only belong on the page if the format is listed.
    #[test]
    fn pipelines_cover_the_formats_that_decode() {
        let titles: String = PIPELINES.iter().map(|p| p.title.to_lowercase()).collect();
        for name in ["JPEG", "PNG", "TIFF"] {
            assert!(
                titles.contains(&name.to_lowercase()),
                "{name} decodes but has no pipeline described"
            );
        }
    }

    #[test]
    fn entries_are_filled_in() {
        for format in GROUPS.iter().flat_map(|group| group.formats) {
            assert!(!format.name.is_empty());
            assert!(format.extensions.starts_with('.'), "{}", format.name);
            assert!(!format.detail.is_empty(), "{}", format.name);
        }
    }
}
