//! Reader for tiled, pyramidal TIFFs — including BigTIFF and the WebP-tiled
//! variant that GDAL writes for Cloud-Optimized GeoTIFFs.
//!
//! The `tiff` crate stops short of both: it cannot open BigTIFF's WebP tiles
//! (compression 50001), and it has no notion of picking an overview level. For
//! a 114944 × 98304 aerial photograph that matters enormously — decoding the
//! full image would need 45 GB of RGBA, while the same file already stores a
//! 225 × 192 overview that decodes instantly.
//!
//! So this parses the IFD chain directly and decodes only the tiles of only the
//! level asked for. Every level is a complete picture of the whole image, which
//! is what makes progressive display possible: show the coarsest level at once,
//! then refine.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use image::RgbaImage;

/// Compression tags we can decode. The 5xxxx values are GDAL extensions.
const COMPRESSION_NONE: u16 = 1;
const COMPRESSION_JPEG: u16 = 7;
const COMPRESSION_DEFLATE: u16 = 8;
const COMPRESSION_DEFLATE_OLD: u16 = 32946;
const COMPRESSION_WEBP: u16 = 50001;

// TIFF tag numbers.
const TAG_IMAGE_WIDTH: u16 = 256;
const TAG_IMAGE_LENGTH: u16 = 257;
const TAG_COMPRESSION: u16 = 259;
const TAG_SAMPLES_PER_PIXEL: u16 = 277;
const TAG_TILE_WIDTH: u16 = 322;
const TAG_TILE_LENGTH: u16 = 323;
const TAG_TILE_OFFSETS: u16 = 324;
const TAG_TILE_BYTE_COUNTS: u16 = 325;
const TAG_PREDICTOR: u16 = 317;

/// No prediction: the stored bytes are the samples.
const PREDICTOR_NONE: u16 = 1;
/// Horizontal differencing: each sample is stored as its difference from the
/// one to its left, which compresses far better and is common in Deflate and
/// LZW TIFFs. Undoing it is not optional — the deltas of an RGB image are small
/// and similar across channels, so treating them as colours gives a picture
/// that looks almost greyscale.
const PREDICTOR_HORIZONTAL: u16 = 2;

/// One resolution in the pyramid. Level 0 is full size; each subsequent level
/// is roughly half the linear dimensions.
#[derive(Clone, Debug)]
pub struct Level {
    pub width: u32,
    pub height: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub compression: u16,
    pub samples: u16,
    /// TIFF tag 317. Deflate and LZW tiles are usually stored with horizontal
    /// differencing, which has to be undone before the bytes mean anything.
    pub predictor: u16,
    offsets: Vec<u64>,
    byte_counts: Vec<u64>,
}

impl Level {
    #[allow(dead_code)]
    pub fn megapixels(&self) -> f64 {
        self.width as f64 * self.height as f64 / 1e6
    }

    pub fn tiles_across(&self) -> u32 {
        self.width.div_ceil(self.tile_width.max(1))
    }

    #[allow(dead_code)]
    pub fn tiles_down(&self) -> u32 {
        self.height.div_ceil(self.tile_height.max(1))
    }

    pub fn tile_count(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_supported(&self) -> bool {
        matches!(
            self.compression,
            COMPRESSION_NONE
                | COMPRESSION_JPEG
                | COMPRESSION_DEFLATE
                | COMPRESSION_DEFLATE_OLD
                | COMPRESSION_WEBP
        )
    }
}

pub struct TiledTiff {
    file: File,
    pub levels: Vec<Level>,
    /// Reported for diagnostics; classic and BigTIFF are read the same way.
    #[allow(dead_code)]
    pub big_tiff: bool,
}

impl TiledTiff {
    /// Opens a tiled TIFF and reads its pyramid, without decoding any pixels.
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;

        let mut header = [0_u8; 16];
        file.read_exact(&mut header)?;
        let little_endian = match &header[0..2] {
            b"II" => true,
            b"MM" => false,
            _ => bail!("not a TIFF: bad byte-order mark"),
        };

        let magic = read_u16(&header[2..4], little_endian);
        let (big_tiff, first_ifd) = match magic {
            42 => (false, read_u32(&header[4..8], little_endian) as u64),
            43 => {
                let offset_size = read_u16(&header[4..6], little_endian);
                if offset_size != 8 {
                    bail!("BigTIFF with an unsupported offset size of {offset_size}");
                }
                (true, read_u64(&header[8..16], little_endian))
            }
            other => bail!("not a TIFF: magic {other}"),
        };

        let levels = read_levels(&mut file, first_ifd, little_endian, big_tiff)?;
        if levels.is_empty() {
            bail!("no tiled levels found — this reader only handles tiled TIFFs");
        }

        Ok(Self {
            file,
            levels,
            big_tiff,
        })
    }

    /// Index of the smallest level whose longest edge still covers `target`.
    /// Falls back to the finest level when none is large enough.
    pub fn level_for(&self, target: u32) -> usize {
        // Levels are ordered finest first, so walk backwards from the coarsest.
        let mut chosen = 0;
        for (index, level) in self.levels.iter().enumerate() {
            if !level.is_supported() {
                continue;
            }
            if level.width.max(level.height) >= target {
                chosen = index;
            }
        }
        chosen
    }

    /// The coarsest usable level — the one to show first.
    pub fn coarsest_level(&self) -> Option<usize> {
        (0..self.levels.len())
            .rev()
            .find(|index| self.levels[*index].is_supported())
    }

    /// Decodes an entire level by reading and decoding each of its tiles.
    pub fn decode_level(&mut self, index: usize) -> Result<RgbaImage> {
        let level = self
            .levels
            .get(index)
            .with_context(|| format!("level {index} does not exist"))?
            .clone();

        if !level.is_supported() {
            bail!("tile compression {} is not supported", level.compression);
        }

        // Pull every tile off disk in one pass. Seeking per tile from several
        // threads would fight over a single handle and turn what the drive sees
        // as a sequential read into a random one.
        let mut compressed: Vec<Vec<u8>> = Vec::with_capacity(level.tile_count());
        for tile in 0..level.tile_count() {
            let length = level.byte_counts[tile] as usize;
            let mut buffer = vec![0_u8; length];
            if length > 0 {
                self.file.seek(SeekFrom::Start(level.offsets[tile]))?;
                self.file.read_exact(&mut buffer)?;
            }
            compressed.push(buffer);
        }

        // Decode and composite across threads. A whole row of tiles is a
        // contiguous band of the canvas, so each thread owns a disjoint slice
        // and no locking is needed. This is the expensive half — a display
        // level of a nine-level COG is several hundred WebP tiles.
        let across = level.tiles_across();
        let width = level.width as usize;
        let band_bytes = level.tile_height as usize * width * 4;
        let mut buffer = vec![0_u8; width * level.height as usize * 4];

        std::thread::scope(|scope| {
            for (row, band) in buffer.chunks_mut(band_bytes).enumerate() {
                let level = &level;
                let compressed = &compressed;
                scope.spawn(move || {
                    let band_rows = band.len() / (width * 4);
                    for column in 0..across as usize {
                        let index = row * across as usize + column;
                        let Some(bytes) = compressed.get(index) else {
                            break;
                        };
                        if bytes.is_empty() {
                            continue; // A sparse tile: leave it transparent.
                        }
                        // One unreadable tile should leave a hole, not fail the
                        // whole level.
                        let Ok(tile) = decode_tile(bytes, level) else {
                            continue;
                        };
                        blit_band(
                            band,
                            band_rows,
                            width,
                            &tile,
                            column * level.tile_width as usize,
                        );
                    }
                });
            }
        });

        RgbaImage::from_raw(level.width, level.height, buffer)
            .context("the canvas buffer was sized for these dimensions")
    }
}

/// Finest level whose pixels are no denser than `screen_pixels` across the
/// visible `image_fraction` of the image.
///
/// This is what keeps zoomed-in pixels sharp: level 0 of a nine-level COG is
/// 1.6 gigapixels, but the part of it under a 2000-pixel-wide window at 100%
/// zoom is only 2000 pixels wide, so it can be read directly.
///
/// Takes level sizes rather than a [`TiledTiff`] so the canvas, which caches
/// only the sizes, applies exactly the same rule as the decoder.
pub fn level_for_zoom(levels: &[(u32, u32)], screen_pixels: u32, image_fraction: f64) -> usize {
    let wanted = (screen_pixels as f64 / image_fraction.max(f64::MIN_POSITIVE)).max(1.0);
    // Levels run finest first, so walk to the coarsest that still has at least
    // as many pixels as we can show.
    let mut best = 0;
    for (index, (width, _)) in levels.iter().enumerate() {
        if (*width as f64) >= wanted {
            best = index;
        }
    }
    best
}

/// A rectangle of a level, in that level's own pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }
}

impl TiledTiff {
    /// Finest level whose pixels are no denser than `screen_pixels` across the
    /// visible `image_fraction` of the image.
    ///
    /// This is what keeps zoomed-in pixels sharp: level 0 of a nine-level COG
    /// is 1.6 gigapixels, but the part of it under a 2000-pixel-wide window at
    /// 100% zoom is only 2000 pixels wide, so it can be read directly.
    /// Level sizes, with anything we cannot decode reported as zero so it is
    /// never chosen. This is what the canvas caches.
    pub fn level_sizes(&self) -> Vec<(u32, u32)> {
        self.levels
            .iter()
            .map(|level| {
                if level.is_supported() {
                    (level.width, level.height)
                } else {
                    (0, 0)
                }
            })
            .collect()
    }

    /// Decodes just the tiles covering `region`, returning exactly that
    /// rectangle. Cost is set by the region, not by the level, which is what
    /// makes reading from a gigapixel level affordable at all.
    /// How many squares [`decode_region`](Self::decode_region) would read for
    /// this rectangle.
    ///
    /// Reported so the interface can say what a view actually cost, rather than
    /// the whole layer's square count — which is what it used to show, and
    /// which made a cheap read look like an expensive one.
    pub fn squares_for(&self, index: usize, region: Region) -> usize {
        let Some(level) = self.levels.get(index) else {
            return 0;
        };
        let x0 = region.x.min(level.width);
        let y0 = region.y.min(level.height);
        let x1 = (region.x + region.width).min(level.width);
        let y1 = (region.y + region.height).min(level.height);
        if x1 <= x0 || y1 <= y0 {
            return 0;
        }
        let columns = (x1 - 1) / level.tile_width - x0 / level.tile_width + 1;
        let rows = (y1 - 1) / level.tile_height - y0 / level.tile_height + 1;
        columns as usize * rows as usize
    }

    pub fn decode_region(&mut self, index: usize, region: Region) -> Result<RgbaImage> {
        let level = self
            .levels
            .get(index)
            .with_context(|| format!("level {index} does not exist"))?
            .clone();
        if !level.is_supported() {
            bail!("tile compression {} is not supported", level.compression);
        }

        // Clamp so a viewport hanging off the edge of the image is not an error.
        let x0 = region.x.min(level.width);
        let y0 = region.y.min(level.height);
        let x1 = (region.x + region.width).min(level.width);
        let y1 = (region.y + region.height).min(level.height);
        if x1 <= x0 || y1 <= y0 {
            return Ok(RgbaImage::new(1, 1));
        }

        let first_column = x0 / level.tile_width;
        let last_column = (x1 - 1) / level.tile_width;
        let first_row = y0 / level.tile_height;
        let last_row = (y1 - 1) / level.tile_height;
        let across = level.tiles_across();

        // Read every needed tile in one pass, as `decode_level` does.
        let mut jobs = Vec::new();
        for row in first_row..=last_row {
            for column in first_column..=last_column {
                let tile = (row * across + column) as usize;
                let (Some(offset), Some(length)) =
                    (level.offsets.get(tile), level.byte_counts.get(tile))
                else {
                    continue;
                };
                if *length == 0 {
                    continue; // Sparse: leave it transparent.
                }
                let mut bytes = vec![0_u8; *length as usize];
                self.file.seek(SeekFrom::Start(*offset))?;
                self.file.read_exact(&mut bytes)?;
                jobs.push((column, row, bytes));
            }
        }

        let width = (x1 - x0) as usize;
        let height = (y1 - y0) as usize;
        let mut buffer = vec![0_u8; width * height * 4];

        // Tiles can overlap the same output rows once cropped, so unlike
        // `decode_level` the bands here are not disjoint. Decode in parallel and
        // composite serially — decoding is the expensive part by far.
        let decoded: Vec<(u32, u32, RgbaImage)> = {
            let mut out = Vec::with_capacity(jobs.len());
            std::thread::scope(|scope| {
                let level = &level;
                let handles: Vec<_> = jobs
                    .into_iter()
                    .map(|(column, row, bytes)| {
                        scope.spawn(move || {
                            decode_tile(&bytes, level)
                                .ok()
                                .map(|tile| (column, row, tile))
                        })
                    })
                    .collect();
                for handle in handles {
                    if let Ok(Some(tile)) = handle.join() {
                        out.push(tile);
                    }
                }
            });
            out
        };

        for (column, row, tile) in decoded {
            let tile_x = column * level.tile_width;
            let tile_y = row * level.tile_height;
            for ty in 0..tile.height() {
                let image_y = tile_y + ty;
                if image_y < y0 || image_y >= y1 {
                    continue;
                }
                let run_start = x0.max(tile_x);
                let run_end = x1.min(tile_x + tile.width());
                if run_end <= run_start {
                    continue;
                }
                let run = (run_end - run_start) as usize;
                let destination = ((image_y - y0) as usize * width + (run_start - x0) as usize) * 4;
                let source =
                    (ty as usize * tile.width() as usize + (run_start - tile_x) as usize) * 4;
                buffer[destination..destination + run * 4]
                    .copy_from_slice(&tile.as_raw()[source..source + run * 4]);
            }
        }

        RgbaImage::from_raw(width as u32, height as u32, buffer)
            .context("the region buffer was sized for these dimensions")
    }
}

/// Smallest power-of-two factor that brings `width`/`height` within `target`.
pub fn subsample_for(width: u32, height: u32, target: u32) -> u32 {
    let mut factor = 1_u32;
    while width.max(height) / factor > target.max(1) && factor < 1024 {
        factor *= 2;
    }
    factor
}

/// Where a tile lands on a subsampled canvas, and how big it is once scaled.
/// Computed by dividing the *absolute* pixel bounds so neighbouring tiles tile
/// exactly — scaling each tile independently would leave seams.
fn placement(column: u32, row: u32, level: &Level, factor: u32) -> (u32, u32, u32, u32, u32, u32) {
    let x0 = column * level.tile_width;
    let y0 = row * level.tile_height;
    let source_width = level.tile_width.min(level.width.saturating_sub(x0));
    let source_height = level.tile_height.min(level.height.saturating_sub(y0));

    let destination_x = x0 / factor;
    let destination_y = y0 / factor;
    let destination_width = (x0 + source_width)
        .div_ceil(factor)
        .saturating_sub(destination_x);
    let destination_height = (y0 + source_height)
        .div_ceil(factor)
        .saturating_sub(destination_y);

    (
        destination_x,
        destination_y,
        destination_width.max(1),
        destination_height.max(1),
        source_width,
        source_height,
    )
}

impl TiledTiff {
    /// Decodes a level straight into a reduced-size canvas.
    ///
    /// Each tile is downscaled *before* it is composited, so peak memory is the
    /// subsampled canvas rather than the full image: a 431 MP scan at factor 8
    /// needs about 27 MB instead of 1.7 GB. `on_progress` is called as rows of
    /// tiles land, which is what lets the viewer paint while this is still
    /// running — the only way to be progressive when a file has no pyramid.
    pub fn decode_level_subsampled(
        &mut self,
        index: usize,
        factor: u32,
        mut on_progress: impl FnMut(&RgbaImage, f32),
    ) -> Result<RgbaImage> {
        let level = self
            .levels
            .get(index)
            .with_context(|| format!("level {index} does not exist"))?
            .clone();
        if !level.is_supported() {
            bail!("tile compression {} is not supported", level.compression);
        }

        let factor = factor.max(1);
        let mut canvas = RgbaImage::new(
            level.width.div_ceil(factor).max(1),
            level.height.div_ceil(factor).max(1),
        );
        let across = level.tiles_across();
        let total = level.tile_count();

        // Tiles are read sequentially from one handle, then decoded in
        // parallel: decode dominates, and this keeps the file access simple.
        const BATCH: usize = 64;
        let mut done = 0_usize;
        let mut last_report = std::time::Instant::now();

        for start in (0..total).step_by(BATCH) {
            let end = (start + BATCH).min(total);
            let mut batch = Vec::with_capacity(end - start);

            for tile in start..end {
                let length = level.byte_counts[tile] as usize;
                if length == 0 {
                    batch.push((tile, Vec::new()));
                    continue;
                }
                let mut bytes = vec![0_u8; length];
                self.file.seek(SeekFrom::Start(level.offsets[tile]))?;
                self.file.read_exact(&mut bytes)?;
                batch.push((tile, bytes));
            }

            let decoded: Vec<(usize, Option<RgbaImage>)> = std::thread::scope(|scope| {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|(tile, bytes)| {
                        let level = &level;
                        scope.spawn(move || {
                            if bytes.is_empty() {
                                return (*tile, None);
                            }
                            let column = *tile as u32 % across;
                            let row = *tile as u32 / across;
                            let (_, _, width, height, source_width, source_height) =
                                placement(column, row, level, factor);

                            let tile_image = match decode_tile(bytes, level) {
                                Ok(image) => image,
                                Err(_) => return (*tile, None),
                            };
                            // Drop the padding on edge tiles before scaling, or
                            // the blank margin would bleed into the result.
                            let cropped = if source_width < tile_image.width()
                                || source_height < tile_image.height()
                            {
                                image::imageops::crop_imm(
                                    &tile_image,
                                    0,
                                    0,
                                    source_width.min(tile_image.width()).max(1),
                                    source_height.min(tile_image.height()).max(1),
                                )
                                .to_image()
                            } else {
                                tile_image
                            };

                            let scaled = if factor > 1 {
                                image::imageops::thumbnail(&cropped, width, height)
                            } else {
                                cropped
                            };
                            (*tile, Some(scaled))
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().unwrap())
                    .collect()
            });

            for (tile, scaled) in decoded {
                done += 1;
                let Some(scaled) = scaled else { continue };
                let column = tile as u32 % across;
                let row = tile as u32 / across;
                let (x, y, ..) = placement(column, row, &level, factor);
                blit(&mut canvas, &scaled, x, y);
            }

            // Report at a human rate, not once per batch.
            if last_report.elapsed() >= std::time::Duration::from_millis(150) {
                last_report = std::time::Instant::now();
                on_progress(&canvas, done as f32 / total.max(1) as f32);
            }
        }

        Ok(canvas)
    }
}

/// Decodes one tile's bytes into RGBA.
fn decode_tile(compressed: &[u8], level: &Level) -> Result<RgbaImage> {
    let image = match level.compression {
        COMPRESSION_WEBP => {
            image::load_from_memory_with_format(compressed, image::ImageFormat::WebP)?
        }
        COMPRESSION_JPEG => {
            image::load_from_memory_with_format(compressed, image::ImageFormat::Jpeg)?
        }
        COMPRESSION_DEFLATE | COMPRESSION_DEFLATE_OLD => {
            let mut raw = inflate(compressed)?;
            undo_predictor(&mut raw, level)?;
            return raw_tile(raw, level);
        }
        COMPRESSION_NONE => {
            let mut raw = compressed.to_vec();
            undo_predictor(&mut raw, level)?;
            return raw_tile(raw, level);
        }
        other => bail!("unsupported tile compression {other}"),
    };
    Ok(image.to_rgba8())
}

/// Builds a tile from uncompressed samples.
/// Reverses horizontal differencing in place, per row and per channel.
///
/// Deflate and LZW TIFFs are almost always written this way — the encoder
/// stores each sample as its difference from the one to its left, because
/// neighbouring pixels are similar and small numbers compress better. Skipping
/// this step does not fail loudly: the deltas of a photograph are small and
/// roughly equal across R, G and B, so the picture comes out looking very
/// nearly greyscale. That is exactly how the bug presented.
fn undo_predictor(raw: &mut [u8], level: &Level) -> Result<()> {
    match level.predictor {
        PREDICTOR_NONE | 0 => return Ok(()),
        PREDICTOR_HORIZONTAL => {}
        // 3 is floating-point differencing, which needs a byte-shuffle as well
        // and only appears in 32-bit scientific rasters.
        other => bail!("unsupported TIFF predictor {other}"),
    }

    let samples = level.samples.max(1) as usize;
    // Tiles are always stored full size, padded at the image edges, so the row
    // stride comes from the tile width rather than the image width.
    let stride = level.tile_width as usize * samples;
    if stride == 0 {
        return Ok(());
    }

    for row in raw.chunks_mut(stride) {
        // Each sample adds the one a whole pixel behind it, not the previous
        // byte — interleaved channels must not bleed into each other.
        for index in samples..row.len() {
            row[index] = row[index].wrapping_add(row[index - samples]);
        }
    }
    Ok(())
}

fn raw_tile(raw: Vec<u8>, level: &Level) -> Result<RgbaImage> {
    let (width, height) = (level.tile_width, level.tile_height);
    let expected = width as usize * height as usize * level.samples as usize;
    if raw.len() < expected {
        bail!("tile is {} bytes, expected {expected}", raw.len());
    }

    let mut tile = RgbaImage::new(width, height);
    for (index, pixel) in tile.pixels_mut().enumerate() {
        let base = index * level.samples as usize;
        *pixel = match level.samples {
            1 => image::Rgba([raw[base], raw[base], raw[base], 255]),
            3 => image::Rgba([raw[base], raw[base + 1], raw[base + 2], 255]),
            _ => image::Rgba([raw[base], raw[base + 1], raw[base + 2], raw[base + 3]]),
        };
    }
    Ok(tile)
}

fn inflate(compressed: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(compressed)
        .read_to_end(&mut out)
        .context("inflating a Deflate tile")?;
    Ok(out)
}

/// Copies a tile into the canvas, clipping at the right and bottom edges where
/// the last tile in a row or column runs past the image.
/// Copies a tile into one horizontal band of the canvas, a row run at a time.
/// Clips at the right and bottom edges, which the last tile of each row and the
/// last band always need.
fn blit_band(band: &mut [u8], band_rows: usize, canvas_width: usize, tile: &RgbaImage, x: usize) {
    if x >= canvas_width {
        return;
    }
    let run = (tile.width() as usize).min(canvas_width - x);
    let raw = tile.as_raw();
    for row in 0..(tile.height() as usize).min(band_rows) {
        let destination = (row * canvas_width + x) * 4;
        let source = row * tile.width() as usize * 4;
        band[destination..destination + run * 4].copy_from_slice(&raw[source..source + run * 4]);
    }
}

fn blit(canvas: &mut RgbaImage, tile: &RgbaImage, x: u32, y: u32) {
    let width = tile.width().min(canvas.width().saturating_sub(x));
    let height = tile.height().min(canvas.height().saturating_sub(y));
    for row in 0..height {
        for column in 0..width {
            canvas.put_pixel(x + column, y + row, *tile.get_pixel(column, row));
        }
    }
}

// ------------------------------------------------------------- IFD traversal

fn read_levels(
    file: &mut File,
    first_ifd: u64,
    little_endian: bool,
    big_tiff: bool,
) -> Result<Vec<Level>> {
    let mut levels = Vec::new();
    let mut next = first_ifd;
    // A malformed file could otherwise loop forever.
    let mut guard = 0;

    while next != 0 && guard < 64 {
        guard += 1;
        file.seek(SeekFrom::Start(next))?;

        let entry_count = if big_tiff {
            let mut buffer = [0_u8; 8];
            file.read_exact(&mut buffer)?;
            read_u64(&buffer, little_endian)
        } else {
            let mut buffer = [0_u8; 2];
            file.read_exact(&mut buffer)?;
            read_u16(&buffer, little_endian) as u64
        };

        let entry_size = if big_tiff { 20 } else { 12 };
        let mut entries = vec![0_u8; entry_count as usize * entry_size];
        file.read_exact(&mut entries)?;

        let mut following = [0_u8; 8];
        if big_tiff {
            file.read_exact(&mut following)?;
            next = read_u64(&following, little_endian);
        } else {
            file.read_exact(&mut following[..4])?;
            next = read_u32(&following[..4], little_endian) as u64;
        }

        if let Some(level) = parse_ifd(file, &entries, entry_size, little_endian, big_tiff)? {
            levels.push(level);
        }
    }

    Ok(levels)
}

fn parse_ifd(
    file: &mut File,
    entries: &[u8],
    entry_size: usize,
    little_endian: bool,
    big_tiff: bool,
) -> Result<Option<Level>> {
    let mut width = 0_u32;
    let mut height = 0_u32;
    let mut tile_width = 0_u32;
    let mut tile_height = 0_u32;
    let mut compression = COMPRESSION_NONE;
    let mut predictor = PREDICTOR_NONE;
    let mut samples = 1_u16;
    let mut offsets = Vec::new();
    let mut byte_counts = Vec::new();

    for chunk in entries.chunks_exact(entry_size) {
        let tag = read_u16(&chunk[0..2], little_endian);
        let field_type = read_u16(&chunk[2..4], little_endian);
        let count = if big_tiff {
            read_u64(&chunk[4..12], little_endian)
        } else {
            read_u32(&chunk[4..8], little_endian) as u64
        };
        let value = &chunk[if big_tiff { 12 } else { 8 }..];

        match tag {
            TAG_IMAGE_WIDTH => width = scalar(value, field_type, little_endian) as u32,
            TAG_IMAGE_LENGTH => height = scalar(value, field_type, little_endian) as u32,
            TAG_TILE_WIDTH => tile_width = scalar(value, field_type, little_endian) as u32,
            TAG_TILE_LENGTH => tile_height = scalar(value, field_type, little_endian) as u32,
            TAG_COMPRESSION => compression = scalar(value, field_type, little_endian) as u16,
            TAG_PREDICTOR => predictor = scalar(value, field_type, little_endian) as u16,
            TAG_SAMPLES_PER_PIXEL => samples = scalar(value, field_type, little_endian) as u16,
            TAG_TILE_OFFSETS => {
                offsets = read_array(file, value, field_type, count, little_endian, big_tiff)?
            }
            TAG_TILE_BYTE_COUNTS => {
                byte_counts = read_array(file, value, field_type, count, little_endian, big_tiff)?
            }
            _ => {}
        }
    }

    // Stripped images are out of scope; the `tiff` crate handles those.
    if tile_width == 0 || tile_height == 0 || offsets.is_empty() {
        return Ok(None);
    }
    if byte_counts.len() != offsets.len() {
        bail!(
            "tile offsets ({}) and byte counts ({}) disagree",
            offsets.len(),
            byte_counts.len()
        );
    }

    Ok(Some(Level {
        width,
        height,
        tile_width,
        tile_height,
        compression,
        samples,
        predictor,
        offsets,
        byte_counts,
    }))
}

/// Reads a scalar value out of an IFD entry's inline value field.
fn scalar(value: &[u8], field_type: u16, little_endian: bool) -> u64 {
    match field_type {
        3 => read_u16(&value[0..2], little_endian) as u64,
        4 => read_u32(&value[0..4], little_endian) as u64,
        16 => read_u64(&value[0..8], little_endian),
        _ => 0,
    }
}

/// Reads an array field, which lives inline when small and out-of-line
/// otherwise — the usual TIFF arrangement.
fn read_array(
    file: &mut File,
    value: &[u8],
    field_type: u16,
    count: u64,
    little_endian: bool,
    big_tiff: bool,
) -> Result<Vec<u64>> {
    let element = match field_type {
        3 => 2_usize,
        4 => 4,
        16 => 8,
        other => bail!("unexpected array field type {other}"),
    };

    let inline_capacity = if big_tiff { 8 } else { 4 };
    let total = element * count as usize;

    let bytes = if total <= inline_capacity {
        value[..total].to_vec()
    } else {
        let offset = if big_tiff {
            read_u64(&value[0..8], little_endian)
        } else {
            read_u32(&value[0..4], little_endian) as u64
        };
        let here = file.stream_position()?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buffer = vec![0_u8; total];
        file.read_exact(&mut buffer)?;
        file.seek(SeekFrom::Start(here))?;
        buffer
    };

    Ok(bytes
        .chunks_exact(element)
        .map(|chunk| match element {
            2 => read_u16(chunk, little_endian) as u64,
            4 => read_u32(chunk, little_endian) as u64,
            _ => read_u64(chunk, little_endian),
        })
        .collect())
}

fn read_u16(bytes: &[u8], little_endian: bool) -> u16 {
    let array = [bytes[0], bytes[1]];
    if little_endian {
        u16::from_le_bytes(array)
    } else {
        u16::from_be_bytes(array)
    }
}

fn read_u32(bytes: &[u8], little_endian: bool) -> u32 {
    let array = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if little_endian {
        u32::from_le_bytes(array)
    } else {
        u32::from_be_bytes(array)
    }
}

fn read_u64(bytes: &[u8], little_endian: bool) -> u64 {
    let mut array = [0_u8; 8];
    array.copy_from_slice(&bytes[0..8]);
    if little_endian {
        u64::from_le_bytes(array)
    } else {
        u64::from_be_bytes(array)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(width: u32, height: u32, compression: u16) -> Level {
        Level {
            width,
            height,
            tile_width: 256,
            tile_height: 256,
            compression,
            samples: 4,
            predictor: PREDICTOR_NONE,
            offsets: vec![0],
            byte_counts: vec![0],
        }
    }

    #[test]
    fn endian_readers_agree_with_the_standard_library() {
        assert_eq!(read_u16(&[0x01, 0x02], true), 0x0201);
        assert_eq!(read_u16(&[0x01, 0x02], false), 0x0102);
        assert_eq!(read_u32(&[1, 0, 0, 0], true), 1);
        assert_eq!(read_u64(&[1, 0, 0, 0, 0, 0, 0, 0], true), 1);
    }

    #[test]
    fn tile_grid_covers_partial_edge_tiles() {
        let level = level(700, 500, COMPRESSION_WEBP);
        assert_eq!(level.tiles_across(), 3, "700 / 256 rounds up to 3");
        assert_eq!(level.tiles_down(), 2);
    }

    #[test]
    fn webp_and_deflate_are_supported_but_jpeg2000_is_not() {
        assert!(level(1, 1, COMPRESSION_WEBP).is_supported());
        assert!(level(1, 1, COMPRESSION_DEFLATE).is_supported());
        assert!(level(1, 1, COMPRESSION_NONE).is_supported());
        assert!(!level(1, 1, 34712).is_supported(), "JPEG 2000");
    }

    fn nine_level_pyramid() -> TiledTiff {
        // The shape of the user's 1958.tif: 48896 x 32768 down to 191 x 128.
        let mut levels = Vec::new();
        let (mut width, mut height) = (48896_u32, 32768_u32);
        for _ in 0..9 {
            levels.push(level(width, height, COMPRESSION_WEBP));
            width = (width / 2).max(1);
            height = (height / 2).max(1);
        }
        TiledTiff {
            file: File::open("/dev/null").unwrap(),
            big_tiff: false,
            levels,
        }
    }

    #[test]
    fn zoom_level_gets_finer_as_you_zoom_in() {
        let tiff = nine_level_pyramid();

        // Whole image across a 2000px window: a coarse level is plenty.
        let sizes = tiff.level_sizes();
        let zoomed_out = level_for_zoom(&sizes, 2000, 1.0);
        // Ten percent of the image across the same window needs ten times the
        // pixel density, so a strictly finer level.
        let zoomed_in = level_for_zoom(&sizes, 2000, 0.1);
        // One percent, finer still.
        let very_close = level_for_zoom(&sizes, 2000, 0.01);

        assert!(
            zoomed_in < zoomed_out,
            "zooming in must choose a finer level ({zoomed_in} vs {zoomed_out})"
        );
        assert!(
            very_close < zoomed_in,
            "zooming further must go finer again ({very_close} vs {zoomed_in})"
        );
        // 1% of a 48896-wide image under a 2000px window wants 200k pixels of
        // detail, which is past the finest level, so level 0 is the right answer.
        assert_eq!(very_close, 0);

        // The zoomed-out case must still be a real level, not the finest one.
        assert!(
            zoomed_out > 0 && zoomed_out < tiff.levels.len(),
            "level {zoomed_out} should be a mid pyramid level"
        );
    }

    #[test]
    fn zoom_level_never_runs_off_either_end_of_the_pyramid() {
        let tiff = nine_level_pyramid();
        // Absurdly deep zoom cannot ask for a level that does not exist.
        let sizes = tiff.level_sizes();
        assert_eq!(level_for_zoom(&sizes, 4000, 0.000_001), 0);
        // A tiny window over the whole image picks the coarsest usable level,
        // and in any case stays inside the pyramid.
        let coarse = level_for_zoom(&sizes, 1, 1.0);
        assert!(coarse < tiff.levels.len());
    }

    #[test]
    fn an_empty_or_off_image_region_is_not_an_error() {
        assert!(Region::default().is_empty());
        assert!(
            !Region {
                x: 0,
                y: 0,
                width: 4,
                height: 4
            }
            .is_empty()
        );
    }

    #[test]
    fn level_selection_picks_the_smallest_that_still_covers_the_target() {
        let tiff = TiledTiff {
            file: File::open("/dev/null").unwrap(),
            big_tiff: true,
            levels: vec![
                level(114944, 98304, COMPRESSION_WEBP),
                level(57472, 49152, COMPRESSION_WEBP),
                level(7184, 6144, COMPRESSION_WEBP),
                level(1796, 1536, COMPRESSION_WEBP),
                level(225, 192, COMPRESSION_WEBP),
            ],
        };

        // A 1500px viewport is covered by the 1796px level, not the 225px one.
        assert_eq!(tiff.level_for(1500), 3);
        // Asking for more than the finest level gives the finest.
        assert_eq!(tiff.level_for(200_000), 0);
        // The coarsest is what we show first.
        assert_eq!(tiff.coarsest_level(), Some(4));
    }

    #[test]
    fn unsupported_levels_are_skipped_when_choosing() {
        let tiff = TiledTiff {
            file: File::open("/dev/null").unwrap(),
            big_tiff: true,
            levels: vec![
                level(8000, 8000, COMPRESSION_WEBP),
                level(500, 500, 34712), // JPEG 2000: unusable
            ],
        };
        assert_eq!(tiff.coarsest_level(), Some(0));
    }

    #[test]
    fn blit_clips_at_the_canvas_edge() {
        let mut canvas = RgbaImage::new(300, 300);
        let tile = RgbaImage::from_pixel(256, 256, image::Rgba([9, 8, 7, 255]));
        // Second tile starts at 256 and runs 212px past the edge.
        blit(&mut canvas, &tile, 256, 0);
        assert_eq!(canvas.get_pixel(299, 0)[0], 9);
        assert_eq!(
            canvas.get_pixel(0, 299)[3],
            0,
            "untouched stays transparent"
        );
    }

    #[test]
    fn subsample_factor_is_the_smallest_power_of_two_that_fits() {
        assert_eq!(subsample_for(8000, 6000, 8192), 1, "already fits");
        assert_eq!(subsample_for(26697, 19750, 8192), 4, "26697/4 = 6674");
        assert_eq!(subsample_for(114944, 98304, 8192), 16);
    }

    #[test]
    fn tile_placements_tile_the_canvas_without_gaps_or_overlap() {
        // 700x500 image, 256px tiles, subsampled by 4 -> a 175x125 canvas.
        let level = level(700, 500, COMPRESSION_WEBP);
        let factor = 4;
        let canvas_width = 700_u32.div_ceil(factor);
        let mut covered = vec![0_u32; canvas_width as usize * 125];

        for row in 0..level.tiles_down() {
            for column in 0..level.tiles_across() {
                let (x, y, width, height, ..) = placement(column, row, &level, factor);
                for dy in 0..height {
                    for dx in 0..width {
                        let index = (y + dy) as usize * canvas_width as usize + (x + dx) as usize;
                        if index < covered.len() {
                            covered[index] += 1;
                        }
                    }
                }
            }
        }
        assert!(
            covered.iter().all(|count| *count == 1),
            "every canvas pixel must be written by exactly one tile"
        );
    }

    fn deflate_level(tile_width: u32, samples: u16, predictor: u16) -> Level {
        Level {
            width: tile_width,
            height: 2,
            tile_width,
            tile_height: 2,
            compression: COMPRESSION_DEFLATE,
            samples,
            predictor,
            offsets: vec![0],
            byte_counts: vec![0],
        }
    }

    /// The bug that made a colour map look black and white: horizontal
    /// differencing left undone.
    #[test]
    fn horizontal_differencing_is_reversed_per_channel() {
        let level = deflate_level(3, 3, PREDICTOR_HORIZONTAL);
        // One row of three RGB pixels, stored as deltas from the left.
        // Absolute: (10,20,30) (15,18,40) (35,18,20)
        let mut raw = vec![
            10, 20, 30, // first pixel is absolute
            5, 254, 10, // +5, -2, +10
            20, 0, 236, // +20, 0, -20
            // second row, so the stride handling is exercised
            1, 2, 3, 1, 1, 1, 1, 1, 1,
        ];
        undo_predictor(&mut raw, &level).unwrap();

        assert_eq!(&raw[..9], &[10, 20, 30, 15, 18, 40, 35, 18, 20]);
        // The second row restarts from its own first pixel — differencing never
        // carries across a row boundary.
        assert_eq!(&raw[9..], &[1, 2, 3, 2, 3, 4, 3, 4, 5]);
    }

    #[test]
    fn a_predictor_of_none_leaves_the_bytes_alone() {
        let level = deflate_level(3, 3, PREDICTOR_NONE);
        let original = vec![
            10, 20, 30, 5, 254, 10, 20, 0, 236, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let mut raw = original.clone();
        undo_predictor(&mut raw, &level).unwrap();
        assert_eq!(raw, original);

        // Some writers leave the tag out entirely, which means the same thing.
        let mut raw = original.clone();
        undo_predictor(&mut raw, &deflate_level(3, 3, 0)).unwrap();
        assert_eq!(raw, original);
    }

    #[test]
    fn greyscale_and_rgba_strides_are_both_handled() {
        // One channel: every byte adds the one before it.
        let mut grey = vec![10, 5, 5, 100, 1, 1];
        undo_predictor(&mut grey, &deflate_level(3, 1, PREDICTOR_HORIZONTAL)).unwrap();
        assert_eq!(grey, vec![10, 15, 20, 100, 101, 102]);

        // Four channels: alpha must not pick up red's delta.
        let mut rgba = vec![10, 20, 30, 255, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        undo_predictor(&mut rgba, &deflate_level(2, 4, PREDICTOR_HORIZONTAL)).unwrap();
        assert_eq!(&rgba[..8], &[10, 20, 30, 255, 11, 21, 31, 255]);
    }

    #[test]
    fn an_unknown_predictor_is_refused_rather_than_silently_wrong() {
        // Floating-point differencing needs a byte shuffle we do not do, and
        // guessing would produce a plausible-looking but wrong picture.
        let mut raw = vec![0_u8; 12];
        assert!(undo_predictor(&mut raw, &deflate_level(3, 1, 3)).is_err());
    }

    #[test]
    fn blit_band_clips_at_the_right_edge_and_the_band_bottom() {
        // Canvas 300 wide, band 256 tall; the second tile runs 212px past the
        // right edge and the tile is taller than the band has room for.
        let canvas_width = 300_usize;
        let band_rows = 100_usize;
        let mut band = vec![0_u8; canvas_width * band_rows * 4];
        let tile = RgbaImage::from_pixel(256, 256, image::Rgba([9, 8, 7, 255]));

        blit_band(&mut band, band_rows, canvas_width, &tile, 256);

        // Only x in 256..300 should be written, and only for band_rows rows.
        let pixel = |x: usize, y: usize| -> [u8; 4] {
            let at = (y * canvas_width + x) * 4;
            band[at..at + 4].try_into().unwrap()
        };
        assert_eq!(pixel(256, 0), [9, 8, 7, 255], "first written column");
        assert_eq!(
            pixel(299, band_rows - 1),
            [9, 8, 7, 255],
            "last written pixel"
        );
        assert_eq!(pixel(255, 0), [0, 0, 0, 0], "left of the tile is untouched");
        assert_eq!(band.len(), canvas_width * band_rows * 4, "no growth");
    }

    #[test]
    fn blit_band_ignores_a_tile_that_starts_past_the_canvas() {
        let mut band = vec![0_u8; 100 * 10 * 4];
        let tile = RgbaImage::from_pixel(16, 16, image::Rgba([1, 2, 3, 4]));
        blit_band(&mut band, 10, 100, &tile, 100);
        assert!(
            band.iter().all(|byte| *byte == 0),
            "nothing should be written"
        );
    }

    /// The banded path must lay tiles down exactly where the sequential one did.
    #[test]
    fn blit_band_agrees_with_the_sequential_blit() {
        let canvas_width = 300_u32;
        let tile_height = 128_usize;
        let mut reference = RgbaImage::new(canvas_width, 256);
        let mut banded = vec![0_u8; canvas_width as usize * 256 * 4];

        for (row, band) in banded
            .chunks_mut(tile_height * canvas_width as usize * 4)
            .enumerate()
        {
            for column in 0..2_usize {
                let shade = (row * 2 + column + 1) as u8 * 40;
                let tile = RgbaImage::from_pixel(256, 128, image::Rgba([shade, 1, 2, 255]));
                blit(
                    &mut reference,
                    &tile,
                    column as u32 * 256,
                    row as u32 * tile_height as u32,
                );
                blit_band(
                    band,
                    tile_height,
                    canvas_width as usize,
                    &tile,
                    column * 256,
                );
            }
        }

        assert_eq!(banded, *reference.as_raw());
    }

    #[test]
    fn edge_tiles_report_only_their_valid_source_region() {
        let level = level(700, 500, COMPRESSION_WEBP);
        // Last column starts at 512 and only 188 pixels are real.
        let (_, _, _, _, source_width, _) = placement(2, 0, &level, 1);
        assert_eq!(source_width, 188);
        // A fully interior tile is the whole 256.
        let (_, _, _, _, interior, _) = placement(1, 0, &level, 1);
        assert_eq!(interior, 256);
    }

    /// Times the two levels the viewer actually decodes for a pyramidal file:
    /// the coarse one it shows immediately, then the display-resolution one.
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_COG_PATH"]
    fn bench_pyramid_levels() {
        use std::time::Instant;

        let Ok(path) = std::env::var("GEO_REF_MAPPER_COG_PATH") else {
            eprintln!("set GEO_REF_MAPPER_COG_PATH");
            return;
        };
        let mut tiff = TiledTiff::open(Path::new(&path)).unwrap();
        println!(
            "\n{} — BigTIFF: {}, {} levels",
            path,
            tiff.big_tiff,
            tiff.levels.len()
        );
        for (index, level) in tiff.levels.iter().enumerate() {
            println!(
                "  level {index}: {} x {} ({:.1} MP), {} tiles of {} x {}, compression {}",
                level.width,
                level.height,
                level.megapixels(),
                level.tile_count(),
                level.tile_width,
                level.tile_height,
                level.compression,
            );
        }

        let coarse = tiff.coarsest_level().unwrap();
        let fine = tiff.level_for(8192);
        println!("  viewer decodes level {coarse} first, then level {fine}");

        for index in [coarse, fine] {
            let tiles = tiff.levels[index].tile_count();
            let started = Instant::now();
            match tiff.decode_level(index) {
                Ok(canvas) => {
                    let ms = started.elapsed().as_secs_f64() * 1000.0;
                    println!(
                        "  level {index}: {ms:>8.1} ms  {} x {}  ({tiles} tiles, {:.2} ms/tile)",
                        canvas.width(),
                        canvas.height(),
                        ms / tiles.max(1) as f64,
                    );
                }
                Err(error) => println!("  level {index} failed: {error:#}"),
            }
        }
        println!();
    }

    /// A region decode must be pixel-identical to cropping the whole level, and
    /// far cheaper. Both halves matter: cheap but wrong is useless.
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_COG_PATH"]
    fn region_decode_matches_a_crop_of_the_full_level() {
        use std::time::Instant;

        let Ok(path) = std::env::var("GEO_REF_MAPPER_COG_PATH") else {
            return;
        };
        let mut tiff = TiledTiff::open(Path::new(&path)).unwrap();
        // A level big enough that decoding all of it is clearly wasteful.
        let index = tiff.level_for(8192);
        let level = tiff.levels[index].clone();

        let started = Instant::now();
        let whole = tiff.decode_level(index).unwrap();
        let whole_ms = started.elapsed().as_secs_f64() * 1000.0;

        // Deliberately off a tile boundary, and running past the right edge.
        let region = Region {
            x: (level.tile_width * 2 + 37).min(level.width),
            y: (level.tile_height + 11).min(level.height),
            width: 900,
            height: 700,
        };
        let started = Instant::now();
        let part = tiff.decode_region(index, region).unwrap();
        let region_ms = started.elapsed().as_secs_f64() * 1000.0;

        println!(
            "\n  level {index} ({} x {}): whole {whole_ms:.1} ms, region {}x{} {region_ms:.1} ms — {:.0}x cheaper\n",
            level.width,
            level.height,
            part.width(),
            part.height(),
            whole_ms / region_ms.max(0.001),
        );

        for y in 0..part.height() {
            for x in 0..part.width() {
                assert_eq!(
                    part.get_pixel(x, y),
                    whole.get_pixel(region.x + x, region.y + y),
                    "pixel {x},{y} of the region differs from the full decode"
                );
            }
        }
        assert!(
            region_ms < whole_ms,
            "a region must be cheaper than the level"
        );
    }

    /// Subsampled decode of a large single-level scan, with progress.
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_COG_PATH"]
    fn subsamples_a_large_single_level_scan() {
        use std::time::Instant;

        let Ok(path) = std::env::var("GEO_REF_MAPPER_COG_PATH") else {
            return;
        };
        let mut tiff = TiledTiff::open(Path::new(&path)).unwrap();
        let level = tiff.levels[0].clone();
        let factor = subsample_for(level.width, level.height, 8192);
        println!(
            "\n{} x {} ({:.0} MP), {} levels, {} tiles, factor {factor}",
            level.width,
            level.height,
            level.megapixels(),
            tiff.levels.len(),
            level.tile_count(),
        );
        println!(
            "  full RGBA would be {:.2} GB",
            level.width as f64 * level.height as f64 * 4.0 / 1e9
        );

        let started = Instant::now();
        let mut first_paint = None;
        let canvas = tiff
            .decode_level_subsampled(0, factor, |canvas, fraction| {
                if first_paint.is_none() {
                    first_paint = Some(started.elapsed());
                    println!(
                        "  first partial paint at {:.0} ms ({:.0}% of tiles, {} x {})",
                        started.elapsed().as_secs_f64() * 1000.0,
                        fraction * 100.0,
                        canvas.width(),
                        canvas.height(),
                    );
                }
            })
            .unwrap();
        println!(
            "  complete in {:.2} s -> {} x {} ({:.1} MB resident)\n",
            started.elapsed().as_secs_f64(),
            canvas.width(),
            canvas.height(),
            canvas.as_raw().len() as f64 / 1e6,
        );
    }

    /// Against the real 11-gigapixel aerial photograph, if it is present.
    #[test]
    #[ignore = "needs GEO_REF_MAPPER_COG_PATH"]
    fn reads_a_real_pyramidal_bigtiff() {
        use std::time::Instant;

        let Ok(path) = std::env::var("GEO_REF_MAPPER_COG_PATH") else {
            return;
        };
        let mut tiff = TiledTiff::open(Path::new(&path)).unwrap();

        println!(
            "\nBigTIFF: {}  levels: {}",
            tiff.big_tiff,
            tiff.levels.len()
        );
        for (index, level) in tiff.levels.iter().enumerate() {
            println!(
                "  {index}: {:>6} x {:>6}  {:>8.1} MP  {:>6} tiles  compression {}{}",
                level.width,
                level.height,
                level.megapixels(),
                level.tile_count(),
                level.compression,
                if level.is_supported() {
                    ""
                } else {
                    "  UNSUPPORTED"
                },
            );
        }

        let coarsest = tiff.coarsest_level().expect("a usable level");
        let started = Instant::now();
        let image = tiff.decode_level(coarsest).unwrap();
        println!(
            "\n  coarsest level {coarsest} decoded in {:.1} ms -> {} x {}",
            started.elapsed().as_secs_f64() * 1000.0,
            image.width(),
            image.height()
        );

        // The level that fills a 4K viewport.
        let display = tiff.level_for(3840);
        let started = Instant::now();
        let image = tiff.decode_level(display).unwrap();
        println!(
            "  display level {display} decoded in {:.2} s -> {} x {} ({:.1} MP)\n",
            started.elapsed().as_secs_f64(),
            image.width(),
            image.height(),
            image.width() as f64 * image.height() as f64 / 1e6,
        );
    }
}
