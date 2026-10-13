//! Optional GPU JPEG decoding via nvJPEG, with GPU downscaling via NPP.
//!
//! Everything here is loaded at runtime with `dlopen`, so the binary still
//! builds and runs on machines with no CUDA at all — [`Gpu::available`] simply
//! returns `None` and the caller stays on the CPU path.
//!
//! The pipeline deliberately keeps full-resolution pixels on the device:
//! compressed bytes go up (a few MB), nvJPEG decodes into VRAM, NPP downscales
//! there, and only the small thumbnail comes back. Pulling a 24 MP frame across
//! PCIe would cost more than it saves.
//!
//! Decoding is CPU-bound work worth offloading: measured at ~40 ms per 24 MP
//! frame on one core, against ~1 ms to read it from NVMe.

use std::ffi::c_void;
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result, anyhow, bail};
use libloading::{Library, Symbol};

// ---------------------------------------------------------------- C ABI types

type CudaError = i32;
type NvjpegStatus = i32;
type NppStatus = i32;
type CudaStream = *mut c_void;
type NvjpegHandle = *mut c_void;
type NvjpegState = *mut c_void;

const CUDA_SUCCESS: CudaError = 0;
const NVJPEG_STATUS_SUCCESS: NvjpegStatus = 0;
const NPP_NO_ERROR: NppStatus = 0;

/// `cudaMemcpyDeviceToHost`
/// Only [`Gpu::resize_rgb8`] uploads, and that is benchmark-only — see there.
#[allow(dead_code)]
const MEMCPY_HOST_TO_DEVICE: i32 = 1;
const MEMCPY_DEVICE_TO_HOST: i32 = 2;

/// `NVJPEG_OUTPUT_RGBI` — interleaved RGB, which is what NPP and `image` want.
const NVJPEG_OUTPUT_RGBI: i32 = 5;

/// `NPPI_INTER_SUPER` — area averaging, the right filter for downscaling.
const NPPI_INTER_SUPER: i32 = 8;

/// nvJPEG writes into up to four planes; RGBI uses only the first.
#[repr(C)]
#[derive(Clone, Copy)]
struct NvjpegImage {
    channel: [*mut u8; 4],
    pitch: [usize; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NppiSize {
    width: i32,
    height: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NppiRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

// ------------------------------------------------------------ symbol loading

/// Resolved entry points. Held for the process lifetime; the `Library` fields
/// keep the shared objects mapped, which the function pointers rely on.
struct Symbols {
    _cudart: Library,
    _nvjpeg: Library,
    _nppig: Library,

    cuda_get_device_count: unsafe extern "C" fn(*mut i32) -> CudaError,
    cuda_malloc: unsafe extern "C" fn(*mut *mut c_void, usize) -> CudaError,
    cuda_free: unsafe extern "C" fn(*mut c_void) -> CudaError,
    cuda_memcpy: unsafe extern "C" fn(*mut c_void, *const c_void, usize, i32) -> CudaError,
    cuda_stream_create: unsafe extern "C" fn(*mut CudaStream) -> CudaError,
    cuda_stream_destroy: unsafe extern "C" fn(CudaStream) -> CudaError,
    cuda_stream_synchronize: unsafe extern "C" fn(CudaStream) -> CudaError,
    /// Page-locked host allocation. A plain `Vec` is pageable, which forces the
    /// driver to stage every transfer through an internal bounce buffer.
    cuda_malloc_host: unsafe extern "C" fn(*mut *mut c_void, usize) -> CudaError,
    cuda_free_host: unsafe extern "C" fn(*mut c_void) -> CudaError,
    cuda_device_synchronize: unsafe extern "C" fn() -> CudaError,

    nvjpeg_create_simple: unsafe extern "C" fn(*mut NvjpegHandle) -> NvjpegStatus,
    nvjpeg_destroy: unsafe extern "C" fn(NvjpegHandle) -> NvjpegStatus,
    nvjpeg_state_create: unsafe extern "C" fn(NvjpegHandle, *mut NvjpegState) -> NvjpegStatus,
    nvjpeg_state_destroy: unsafe extern "C" fn(NvjpegState) -> NvjpegStatus,
    nvjpeg_get_image_info: unsafe extern "C" fn(
        NvjpegHandle,
        *const u8,
        usize,
        *mut i32,
        *mut i32,
        *mut i32,
        *mut i32,
    ) -> NvjpegStatus,
    nvjpeg_decode: unsafe extern "C" fn(
        NvjpegHandle,
        NvjpegState,
        *const u8,
        usize,
        i32,
        *mut NvjpegImage,
        CudaStream,
    ) -> NvjpegStatus,

    npp_resize: unsafe extern "C" fn(
        *const u8,
        i32,
        NppiSize,
        NppiRect,
        *mut u8,
        i32,
        NppiSize,
        NppiRect,
        i32,
    ) -> NppStatus,
}

// SAFETY: these are plain C entry points with no interior state of their own.
// All mutable CUDA state (handles, streams, buffers) lives in `Gpu`, which is
// not shared between threads.
unsafe impl Send for Symbols {}
unsafe impl Sync for Symbols {}

/// Tries several names so a system install or a CUDA-toolkit install both work.
fn open_library(names: &[&str]) -> Result<Library> {
    let mut last = None;
    for name in names {
        // SAFETY: dlopen of a versioned shared object; any initialiser it runs
        // is the library's own, and we only resolve documented C symbols.
        match unsafe { Library::new(*name) } {
            Ok(library) => return Ok(library),
            Err(error) => last = Some(error),
        }
    }
    Err(anyhow!(
        "could not load any of {names:?}: {}",
        last.map(|error| error.to_string()).unwrap_or_default()
    ))
}

macro_rules! symbol {
    ($library:expr, $name:literal) => {{
        // SAFETY: the signature matches the CUDA / nvJPEG / NPP headers, and
        // the symbol outlives use because the `Library` is kept in `Symbols`.
        let found: Symbol<_> = unsafe { $library.get(concat!($name, "\0").as_bytes()) }
            .with_context(|| format!("resolving {}", $name))?;
        *found
    }};
}

impl Symbols {
    fn load() -> Result<Self> {
        let cudart = open_library(&[
            "libcudart.so",
            "libcudart.so.12",
            "/usr/local/cuda/lib64/libcudart.so",
        ])?;
        let nvjpeg = open_library(&[
            "libnvjpeg.so",
            "libnvjpeg.so.12",
            "/usr/local/cuda/lib64/libnvjpeg.so",
        ])?;
        let nppig = open_library(&[
            "libnppig.so",
            "libnppig.so.12",
            "/usr/local/cuda/lib64/libnppig.so",
        ])?;

        Ok(Self {
            cuda_get_device_count: symbol!(cudart, "cudaGetDeviceCount"),
            cuda_malloc: symbol!(cudart, "cudaMalloc"),
            cuda_free: symbol!(cudart, "cudaFree"),
            cuda_memcpy: symbol!(cudart, "cudaMemcpy"),
            cuda_stream_create: symbol!(cudart, "cudaStreamCreate"),
            cuda_stream_destroy: symbol!(cudart, "cudaStreamDestroy"),
            cuda_stream_synchronize: symbol!(cudart, "cudaStreamSynchronize"),
            cuda_malloc_host: symbol!(cudart, "cudaMallocHost"),
            cuda_free_host: symbol!(cudart, "cudaFreeHost"),
            cuda_device_synchronize: symbol!(cudart, "cudaDeviceSynchronize"),

            nvjpeg_create_simple: symbol!(nvjpeg, "nvjpegCreateSimple"),
            nvjpeg_destroy: symbol!(nvjpeg, "nvjpegDestroy"),
            nvjpeg_state_create: symbol!(nvjpeg, "nvjpegJpegStateCreate"),
            nvjpeg_state_destroy: symbol!(nvjpeg, "nvjpegJpegStateDestroy"),
            nvjpeg_get_image_info: symbol!(nvjpeg, "nvjpegGetImageInfo"),
            nvjpeg_decode: symbol!(nvjpeg, "nvjpegDecode"),

            npp_resize: symbol!(nppig, "nppiResize_8u_C3R"),

            _cudart: cudart,
            _nvjpeg: nvjpeg,
            _nppig: nppig,
        })
    }
}

/// Loaded once; `None` means this machine has no usable CUDA stack.
fn symbols() -> Option<&'static Arc<Symbols>> {
    static SYMBOLS: OnceLock<Option<Arc<Symbols>>> = OnceLock::new();
    SYMBOLS
        .get_or_init(|| match Symbols::load() {
            Ok(symbols) => Some(Arc::new(symbols)),
            Err(error) => {
                // Not an error worth shouting about: no CUDA is a normal setup.
                eprintln!("geo-ref_mapper: GPU decoding unavailable ({error:#})");
                None
            }
        })
        .as_ref()
}

/// True when a CUDA device and the nvJPEG/NPP libraries are both present.
pub fn is_available() -> bool {
    let Some(symbols) = symbols() else {
        return false;
    };
    let mut devices = 0_i32;
    // SAFETY: writes a single `int` we own.
    let status = unsafe { (symbols.cuda_get_device_count)(&mut devices) };
    status == CUDA_SUCCESS && devices > 0
}

// ------------------------------------------------------------------- device

/// A device-side scratch buffer that grows to fit the largest image seen.
struct DeviceBuffer {
    pointer: *mut c_void,
    capacity: usize,
    symbols: Arc<Symbols>,
}

impl DeviceBuffer {
    fn new(symbols: Arc<Symbols>) -> Self {
        Self {
            pointer: std::ptr::null_mut(),
            capacity: 0,
            symbols,
        }
    }

    fn reserve(&mut self, bytes: usize) -> Result<*mut u8> {
        if bytes > self.capacity {
            self.release();
            let mut pointer = std::ptr::null_mut();
            // SAFETY: `pointer` is a valid out-param; freed in `release`.
            let status = unsafe { (self.symbols.cuda_malloc)(&mut pointer, bytes) };
            if status != CUDA_SUCCESS {
                bail!("cudaMalloc of {bytes} bytes failed with status {status}");
            }
            self.pointer = pointer;
            self.capacity = bytes;
        }
        Ok(self.pointer.cast::<u8>())
    }

    fn release(&mut self) {
        if !self.pointer.is_null() {
            // SAFETY: allocated by `cudaMalloc` and not freed since.
            unsafe { (self.symbols.cuda_free)(self.pointer) };
            self.pointer = std::ptr::null_mut();
            self.capacity = 0;
        }
    }
}

impl Drop for DeviceBuffer {
    fn drop(&mut self) {
        self.release();
    }
}

/// Page-locked host memory, so `cudaMemcpy` can DMA straight out of it instead
/// of staging through the driver's own pinned bounce buffer.
struct PinnedBuffer {
    pointer: *mut c_void,
    capacity: usize,
    symbols: Arc<Symbols>,
}

impl PinnedBuffer {
    fn new(symbols: Arc<Symbols>) -> Self {
        Self {
            pointer: std::ptr::null_mut(),
            capacity: 0,
            symbols,
        }
    }

    fn reserve(&mut self, bytes: usize) -> Result<*mut u8> {
        if bytes > self.capacity {
            self.release();
            let mut pointer = std::ptr::null_mut();
            // SAFETY: `pointer` is a valid out-param; freed in `release`.
            let status = unsafe { (self.symbols.cuda_malloc_host)(&mut pointer, bytes) };
            if status != CUDA_SUCCESS {
                bail!("cudaMallocHost of {bytes} bytes failed with status {status}");
            }
            self.pointer = pointer;
            self.capacity = bytes;
        }
        Ok(self.pointer.cast::<u8>())
    }

    fn release(&mut self) {
        if !self.pointer.is_null() {
            // SAFETY: allocated by `cudaMallocHost` and not freed since.
            unsafe { (self.symbols.cuda_free_host)(self.pointer) };
            self.pointer = std::ptr::null_mut();
            self.capacity = 0;
        }
    }
}

impl Drop for PinnedBuffer {
    fn drop(&mut self) {
        self.release();
    }
}

/// Where the time went in [`Gpu::resize_rgb8`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ResizeTimings {
    /// Copying the caller's pixels into page-locked memory. Zero unless pinned
    /// staging was asked for — and avoidable entirely if the decoder wrote
    /// straight into the pinned buffer.
    pub stage_ms: f32,
    pub upload_ms: f32,
    pub resize_ms: f32,
    pub download_ms: f32,
}

impl ResizeTimings {
    /// Benchmark-only, like [`Gpu::resize_rgb8`] itself.
    #[allow(dead_code)]
    pub fn total_ms(&self) -> f32 {
        self.stage_ms + self.upload_ms + self.resize_ms + self.download_ms
    }
}

/// One GPU decoding context. **Not thread-safe** — nvJPEG state and the NPP
/// stream are per-context, so give each worker thread its own.
pub struct Gpu {
    symbols: Arc<Symbols>,
    handle: NvjpegHandle,
    state: NvjpegState,
    stream: CudaStream,
    full: DeviceBuffer,
    scaled: DeviceBuffer,
    pinned: PinnedBuffer,
}

impl Gpu {
    /// Creates a context, or `None` if this machine cannot do GPU decoding.
    pub fn new() -> Option<Self> {
        if !is_available() {
            return None;
        }
        let symbols = Arc::clone(symbols()?);

        let mut handle: NvjpegHandle = std::ptr::null_mut();
        let mut state: NvjpegState = std::ptr::null_mut();
        let mut stream: CudaStream = std::ptr::null_mut();

        // SAFETY: each call gets a valid out-param; all are torn down in `Drop`.
        unsafe {
            if (symbols.nvjpeg_create_simple)(&mut handle) != NVJPEG_STATUS_SUCCESS {
                return None;
            }
            if (symbols.nvjpeg_state_create)(handle, &mut state) != NVJPEG_STATUS_SUCCESS {
                (symbols.nvjpeg_destroy)(handle);
                return None;
            }
            if (symbols.cuda_stream_create)(&mut stream) != CUDA_SUCCESS {
                (symbols.nvjpeg_state_destroy)(state);
                (symbols.nvjpeg_destroy)(handle);
                return None;
            }
        }

        Some(Self {
            full: DeviceBuffer::new(Arc::clone(&symbols)),
            scaled: DeviceBuffer::new(Arc::clone(&symbols)),
            pinned: PinnedBuffer::new(Arc::clone(&symbols)),
            symbols,
            handle,
            state,
            stream,
        })
    }

    /// Dimensions of a JPEG without decoding it.
    pub fn image_size(&self, jpeg: &[u8]) -> Result<(u32, u32)> {
        let mut components = 0_i32;
        let mut subsampling = 0_i32;
        let mut widths = [0_i32; 4];
        let mut heights = [0_i32; 4];

        // SAFETY: `jpeg` is a valid slice; the out-params are all owned here and
        // nvJPEG writes at most four entries into the width/height arrays.
        let status = unsafe {
            (self.symbols.nvjpeg_get_image_info)(
                self.handle,
                jpeg.as_ptr(),
                jpeg.len(),
                &mut components,
                &mut subsampling,
                widths.as_mut_ptr(),
                heights.as_mut_ptr(),
            )
        };
        if status != NVJPEG_STATUS_SUCCESS {
            bail!("nvjpegGetImageInfo failed with status {status}");
        }
        if widths[0] <= 0 || heights[0] <= 0 {
            bail!("nvjpegGetImageInfo reported an empty image");
        }
        Ok((widths[0] as u32, heights[0] as u32))
    }

    /// Decodes on the GPU, downscales on the GPU, and returns only the small
    /// interleaved RGB thumbnail. Returns `(pixels, width, height)`.
    pub fn thumbnail_rgb(&mut self, jpeg: &[u8], max_edge: u32) -> Result<(Vec<u8>, u32, u32)> {
        let (width, height) = self.image_size(jpeg)?;

        let [target_width, target_height] = if width.max(height) > max_edge {
            crate::loader::fit_within(width, height, max_edge)
        } else {
            [width, height]
        };

        let source_pitch = width as usize * 3;
        let source_bytes = source_pitch * height as usize;
        let target_pitch = target_width as usize * 3;
        let target_bytes = target_pitch * target_height as usize;

        let source = self.full.reserve(source_bytes)?;
        let target = self.scaled.reserve(target_bytes)?;

        let mut output = NvjpegImage {
            channel: [
                source,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ],
            pitch: [source_pitch, 0, 0, 0],
        };

        // SAFETY: `source` has room for `height * source_pitch` bytes, which is
        // what RGBI output requires for these dimensions.
        let status = unsafe {
            (self.symbols.nvjpeg_decode)(
                self.handle,
                self.state,
                jpeg.as_ptr(),
                jpeg.len(),
                NVJPEG_OUTPUT_RGBI,
                &mut output,
                self.stream,
            )
        };
        if status != NVJPEG_STATUS_SUCCESS {
            bail!("nvjpegDecode failed with status {status}");
        }

        // The resize below runs on the default stream, so the decode has to be
        // complete before it starts.
        // SAFETY: `self.stream` was created by `cudaStreamCreate`.
        let status = unsafe { (self.symbols.cuda_stream_synchronize)(self.stream) };
        if status != CUDA_SUCCESS {
            bail!("cudaStreamSynchronize failed with status {status}");
        }

        if target_bytes != source_bytes {
            let source_size = NppiSize {
                width: width as i32,
                height: height as i32,
            };
            let source_roi = NppiRect {
                x: 0,
                y: 0,
                width: width as i32,
                height: height as i32,
            };
            let target_size = NppiSize {
                width: target_width as i32,
                height: target_height as i32,
            };
            let target_roi = NppiRect {
                x: 0,
                y: 0,
                width: target_width as i32,
                height: target_height as i32,
            };

            // SAFETY: both buffers are device memory of the sizes described by
            // the pitch and size arguments.
            let status = unsafe {
                (self.symbols.npp_resize)(
                    source,
                    source_pitch as i32,
                    source_size,
                    source_roi,
                    target,
                    target_pitch as i32,
                    target_size,
                    target_roi,
                    NPPI_INTER_SUPER,
                )
            };
            if status != NPP_NO_ERROR {
                bail!("nppiResize failed with status {status}");
            }
        }

        // Only the thumbnail crosses PCIe.
        let mut pixels = vec![0_u8; target_bytes];
        let device_source = if target_bytes != source_bytes {
            target
        } else {
            source
        };
        // SAFETY: `pixels` has exactly `target_bytes` capacity, matching the
        // device region being copied.
        let status = unsafe {
            (self.symbols.cuda_memcpy)(
                pixels.as_mut_ptr().cast::<c_void>(),
                device_source.cast::<c_void>(),
                target_bytes,
                MEMCPY_DEVICE_TO_HOST,
            )
        };
        if status != CUDA_SUCCESS {
            bail!("cudaMemcpy device-to-host failed with status {status}");
        }

        Ok((pixels, target_width, target_height))
    }

    /// Downscales already-decoded interleaved RGB8 on the GPU.
    ///
    /// Unlike [`Self::thumbnail_rgb`] this does not decode anything, so it works
    /// for any format — PNG and TIFF included, where nvJPEG is no help. The
    /// trade is that the full-resolution pixels have to cross PCIe on the way
    /// in, which for a 300 MP image is close to a gigabyte.
    ///
    /// **Measured slower than the CPU and not on the shipping path.** On the
    /// 328 MP map this takes 179 ms against 63 ms for
    /// [`crate::loader::downscale_box`] across 32 threads: the upload dominates,
    /// and area averaging parallelises perfectly on a 16-core machine. Kept so
    /// `bench_downscale` can re-check that on other hardware — a machine with
    /// fewer cores, or a resize that stayed on the device between operations,
    /// could well flip the result.
    #[allow(dead_code)]
    pub fn resize_rgb8(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
        target_width: u32,
        target_height: u32,
        pinned: bool,
    ) -> Result<(Vec<u8>, ResizeTimings)> {
        use std::time::Instant;

        let source_pitch = width as usize * 3;
        let source_bytes = source_pitch * height as usize;
        if pixels.len() < source_bytes {
            bail!(
                "expected {source_bytes} bytes for {width} x {height} RGB, got {}",
                pixels.len()
            );
        }
        let target_pitch = target_width as usize * 3;
        let target_bytes = target_pitch * target_height as usize;

        let source = self.full.reserve(source_bytes)?;
        let target = self.scaled.reserve(target_bytes)?;
        let mut timings = ResizeTimings::default();

        // Staging into page-locked memory is only a win if the copy it costs is
        // smaller than the transfer it saves — measure both halves separately.
        let host: *const c_void = if pinned {
            let staging = self.pinned.reserve(source_bytes)?;
            let started = Instant::now();
            // SAFETY: `staging` has `source_bytes` capacity and does not overlap
            // `pixels`, which is at least that long.
            unsafe {
                std::ptr::copy_nonoverlapping(pixels.as_ptr(), staging, source_bytes);
            }
            timings.stage_ms = started.elapsed().as_secs_f32() * 1000.0;
            staging.cast::<c_void>()
        } else {
            pixels.as_ptr().cast::<c_void>()
        };

        let started = Instant::now();
        // SAFETY: `source` is device memory of exactly `source_bytes`, and
        // `host` points to at least that many readable bytes.
        let status = unsafe {
            (self.symbols.cuda_memcpy)(
                source.cast::<c_void>(),
                host,
                source_bytes,
                MEMCPY_HOST_TO_DEVICE,
            )
        };
        if status != CUDA_SUCCESS {
            bail!("cudaMemcpy host-to-device failed with status {status}");
        }
        timings.upload_ms = started.elapsed().as_secs_f32() * 1000.0;
        let started = Instant::now();

        let source_size = NppiSize {
            width: width as i32,
            height: height as i32,
        };
        let source_roi = NppiRect {
            x: 0,
            y: 0,
            width: width as i32,
            height: height as i32,
        };
        let target_size = NppiSize {
            width: target_width as i32,
            height: target_height as i32,
        };
        let target_roi = NppiRect {
            x: 0,
            y: 0,
            width: target_width as i32,
            height: target_height as i32,
        };

        // SAFETY: both buffers are device memory matching the sizes and pitches
        // described by the arguments.
        let status = unsafe {
            (self.symbols.npp_resize)(
                source,
                source_pitch as i32,
                source_size,
                source_roi,
                target,
                target_pitch as i32,
                target_size,
                target_roi,
                NPPI_INTER_SUPER,
            )
        };
        if status != NPP_NO_ERROR {
            bail!("nppiResize failed with status {status}");
        }
        // NPP is asynchronous, so without this the resize time would be charged
        // to the download instead.
        // SAFETY: no arguments; blocks until the device is idle.
        let status = unsafe { (self.symbols.cuda_device_synchronize)() };
        if status != CUDA_SUCCESS {
            bail!("cudaDeviceSynchronize failed with status {status}");
        }
        timings.resize_ms = started.elapsed().as_secs_f32() * 1000.0;

        let started = Instant::now();
        let mut out = vec![0_u8; target_bytes];
        // SAFETY: `out` has exactly `target_bytes` capacity, matching the region
        // NPP wrote.
        let status = unsafe {
            (self.symbols.cuda_memcpy)(
                out.as_mut_ptr().cast::<c_void>(),
                target.cast::<c_void>(),
                target_bytes,
                MEMCPY_DEVICE_TO_HOST,
            )
        };
        if status != CUDA_SUCCESS {
            bail!("cudaMemcpy device-to-host failed with status {status}");
        }
        timings.download_ms = started.elapsed().as_secs_f32() * 1000.0;

        Ok((out, timings))
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.full.release();
        self.scaled.release();
        // SAFETY: each handle was created in `new` and is destroyed once.
        unsafe {
            (self.symbols.cuda_stream_destroy)(self.stream);
            (self.symbols.nvjpeg_state_destroy)(self.state);
            (self.symbols.nvjpeg_destroy)(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use image::ImageEncoder as _;

    #[test]
    fn detection_never_panics() {
        // Whatever the machine has, asking must be safe and repeatable.
        let first = is_available();
        assert_eq!(first, is_available());
    }

    #[test]
    fn a_context_is_created_only_when_cuda_is_present() {
        match Gpu::new() {
            Some(_) => assert!(is_available()),
            None => { /* no CUDA here, which is a supported configuration */ }
        }
    }

    /// When a GPU is present its thumbnail must match the CPU's closely. Exact
    /// equality is not expected — different resamplers — but a large mean
    /// difference would mean the pipeline is wrong (wrong pitch, swapped
    /// channels, garbage memory).
    #[test]
    fn gpu_output_matches_the_cpu_within_tolerance() {
        let Some(mut gpu) = Gpu::new() else {
            eprintln!("no CUDA device, skipping");
            return;
        };

        // A gradient with structure in both axes, so a channel swap or a pitch
        // mistake shows up clearly.
        let mut source = image::RgbImage::new(1024, 768);
        for (x, y, pixel) in source.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x / 4) as u8, (y / 3) as u8, ((x + y) / 7) as u8]);
        }
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut std::io::Cursor::new(&mut jpeg),
            95,
        )
        .encode_image(&source)
        .unwrap();

        let (pixels, width, height) = gpu.thumbnail_rgb(&jpeg, 320).unwrap();
        assert_eq!((width, height), (320, 240));
        assert_eq!(pixels.len(), (width * height * 3) as usize);

        let cpu = image::DynamicImage::ImageRgb8(source)
            .thumbnail_exact(width, height)
            .to_rgb8();

        let total: f64 = pixels
            .iter()
            .zip(cpu.as_raw().iter())
            .map(|(a, b)| (*a as f64 - *b as f64).abs())
            .sum();
        let mean = total / pixels.len() as f64;
        assert!(
            mean < 12.0,
            "GPU and CPU thumbnails differ too much (mean {mean:.1}/255)"
        );
    }
}
