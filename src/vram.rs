//! How much graphics memory is actually free, where the driver will say.
//!
//! wgpu reports what an adapter *is* but not what is left on it, and the
//! difference matters: a card with a game on it will still hand over a device
//! and only fail later, on the first large texture upload. Asking first is what
//! turns "started, then broke" into "chose the other one".
//!
//! Only NVIDIA answers here, through NVML — the same interface `nvidia-smi`
//! uses. It is loaded at runtime like everything else in this project, so a
//! machine without the driver simply gets `None` and falls back to ranking by
//! adapter type. AMD and Intel expose budgets through Vulkan extensions that
//! wgpu does not surface, so for those the answer is honestly unknown.

use std::ffi::c_void;

use libloading::{Library, Symbol};

type NvmlReturn = i32;
const NVML_SUCCESS: NvmlReturn = 0;

/// `nvmlMemory_t`: total, free, used — all in bytes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NvmlMemory {
    total: u64,
    free: u64,
    used: u64,
}

/// What one adapter has left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Memory {
    pub total: u64,
    pub free: u64,
    /// What the driver says processes are actually holding.
    ///
    /// Not `total - free`: that also counts memory the driver reserves for
    /// itself, which on this card is around 400 MB and would make the readout
    /// disagree with `nvidia-smi` by that much.
    pub used: u64,
}

impl Memory {
    pub fn free_mb(self) -> u64 {
        self.free / (1024 * 1024)
    }

    pub fn total_mb(self) -> u64 {
        self.total / (1024 * 1024)
    }

    pub fn used_mb(self) -> u64 {
        self.used / (1024 * 1024)
    }

    /// Fraction still available, 0 to 1.
    pub fn free_fraction(self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        self.free as f32 / self.total as f32
    }
}

/// NVML, opened once and kept for the life of the process.
///
/// The title bar re-reads memory every couple of seconds, so loading the
/// library and calling `nvmlInit` per poll would be wasteful. Initialised once
/// and never shut down: the driver cleans up at exit, and there is no moment
/// where tearing it down would help.
static NVML: std::sync::OnceLock<Option<Library>> = std::sync::OnceLock::new();

fn nvml() -> Option<&'static Library> {
    NVML.get_or_init(|| {
        let library = unsafe { Library::new("libnvidia-ml.so.1") }
            .or_else(|_| unsafe { Library::new("libnvidia-ml.so") })
            .ok()?;
        // SAFETY: `nvmlInit_v2` takes no arguments and returns a status code.
        let initialised = unsafe {
            library
                .get::<unsafe extern "C" fn() -> NvmlReturn>(b"nvmlInit_v2")
                .map(|init| init() == NVML_SUCCESS)
                .unwrap_or(false)
        };
        initialised.then_some(library)
    })
    .as_ref()
}

/// Reads free and total memory for every NVIDIA device, in enumeration order.
///
/// NVML rather than CUDA: `cudaMemGetInfo` needs a context, which is slow to
/// build and itself reserves memory — precisely what we are trying to measure
/// and avoid spending.
pub fn nvidia_memory() -> Vec<(String, Memory)> {
    unsafe { read_nvidia() }.unwrap_or_default()
}

/// Total memory in use across all NVIDIA devices, for the title bar.
///
/// `None` on a machine whose driver will not say, which is every non-NVIDIA
/// adapter — better to show nothing than a number we invented.
pub fn in_use() -> Option<Memory> {
    let devices = nvidia_memory();
    let first = devices.first()?;
    Some(first.1)
}

/// What the machine has left in ordinary RAM.
///
/// This is the honest figure for an integrated GPU: it has no memory of its
/// own, it carves what it needs out of system memory, so "free VRAM" would be a
/// number with nothing behind it. Read from `/proc/meminfo` — `MemAvailable`
/// rather than `MemFree`, because the kernel's page cache is not memory the
/// machine is short of.
pub fn system_memory() -> Option<Memory> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u64>().ok())
            .map(|kilobytes| kilobytes * 1024)
    };
    let total = field("MemTotal:")?;
    let free = field("MemAvailable:").or_else(|| field("MemFree:"))?;
    Some(Memory {
        total,
        free,
        used: total.saturating_sub(free),
    })
}

/// # Safety
///
/// Every call below is an NVML entry point used exactly as documented: the
/// out-params are stack locals we own, the device handles come from NVML itself
/// and are not used after `nvmlShutdown`, and the name buffer is sized to the
/// length NVML is told about.
unsafe fn read_nvidia() -> Option<Vec<(String, Memory)>> {
    let library = nvml()?;

    unsafe {
        let device_count: Symbol<unsafe extern "C" fn(*mut u32) -> NvmlReturn> =
            library.get(b"nvmlDeviceGetCount_v2").ok()?;
        let by_index: Symbol<unsafe extern "C" fn(u32, *mut *mut c_void) -> NvmlReturn> =
            library.get(b"nvmlDeviceGetHandleByIndex_v2").ok()?;
        let memory_info: Symbol<unsafe extern "C" fn(*mut c_void, *mut NvmlMemory) -> NvmlReturn> =
            library.get(b"nvmlDeviceGetMemoryInfo").ok()?;
        let get_name: Symbol<unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> NvmlReturn> =
            library.get(b"nvmlDeviceGetName").ok()?;

        let mut count = 0_u32;
        if device_count(&mut count) != NVML_SUCCESS {
            return None;
        }

        let mut out = Vec::new();
        for index in 0..count {
            let mut handle: *mut c_void = std::ptr::null_mut();
            if by_index(index, &mut handle) != NVML_SUCCESS || handle.is_null() {
                continue;
            }

            let mut memory = NvmlMemory::default();
            if memory_info(handle, &mut memory) != NVML_SUCCESS {
                continue;
            }

            // NVML_DEVICE_NAME_BUFFER_SIZE is 64; ask for one less so the
            // terminator always fits.
            let mut name = [0_u8; 64];
            let name = if get_name(handle, name.as_mut_ptr(), 63) == NVML_SUCCESS {
                let end = name
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(name.len());
                String::from_utf8_lossy(&name[..end]).into_owned()
            } else {
                format!("NVIDIA device {index}")
            };

            out.push((
                name,
                Memory {
                    total: memory.total,
                    free: memory.free,
                    used: memory.used,
                },
            ));
        }

        Some(out)
    }
}

/// Matches a wgpu adapter name against an NVML device name.
///
/// The two do not agree exactly — wgpu reports "NVIDIA GeForce RTX 4070 SUPER"
/// while NVML gives "NVIDIA GeForce RTX 4070 SUPER" on some drivers and drops
/// the vendor on others — so compare on the distinctive part rather than
/// demanding equality.
pub fn matches_adapter(nvml_name: &str, adapter_name: &str) -> bool {
    let normalise = |text: &str| {
        text.to_lowercase()
            .replace("nvidia", "")
            .replace("geforce", "")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (a, b) = (normalise(nvml_name), normalise(adapter_name));
    !a.is_empty() && (a == b || a.contains(&b) || b.contains(&a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_reports_megabytes_and_a_fraction() {
        let memory = Memory {
            total: 12_282 * 1024 * 1024,
            free: 10_333 * 1024 * 1024,
            used: 1_700 * 1024 * 1024,
        };
        assert_eq!(memory.total_mb(), 12_282);
        assert_eq!(memory.free_mb(), 10_333);
        assert_eq!(memory.used_mb(), 1_700);
        assert!((memory.free_fraction() - 0.841).abs() < 0.01);

        // The driver reserves memory that is neither free nor held by a
        // process, so these deliberately do not add up.
        assert!(memory.used_mb() + memory.free_mb() < memory.total_mb());
    }

    #[test]
    fn a_zero_sized_device_does_not_divide_by_zero() {
        let empty = Memory {
            total: 0,
            free: 0,
            used: 0,
        };
        assert_eq!(empty.free_fraction(), 0.0);
        assert_eq!(empty.total_mb(), 0);
    }

    #[test]
    fn adapter_names_match_across_the_two_apis() {
        assert!(matches_adapter(
            "NVIDIA GeForce RTX 4070 SUPER",
            "NVIDIA GeForce RTX 4070 SUPER"
        ));
        // NVML sometimes omits the vendor, wgpu rarely does.
        assert!(matches_adapter(
            "GeForce RTX 4070 SUPER",
            "NVIDIA GeForce RTX 4070 SUPER"
        ));
        assert!(matches_adapter("NVIDIA RTX 4070 SUPER", "RTX 4070 SUPER"));

        // Different cards must not be conflated.
        assert!(!matches_adapter(
            "NVIDIA GeForce RTX 4070 SUPER",
            "NVIDIA GeForce RTX 3060"
        ));
        assert!(!matches_adapter(
            "NVIDIA GeForce RTX 4070",
            "AMD Radeon Graphics"
        ));
        assert!(!matches_adapter("", "NVIDIA GeForce RTX 4070"));
    }

    /// Reports what this machine actually has, so the reading can be checked
    /// against `nvidia-smi` by eye.
    ///
    /// `cargo test --release bench_vram -- --ignored --nocapture`
    #[test]
    #[ignore = "reports live hardware state"]
    fn bench_vram_reports_live_memory() {
        let devices = nvidia_memory();
        if devices.is_empty() {
            println!("  no NVIDIA device reported (driver absent, or not an NVIDIA machine)");
            return;
        }
        for (name, memory) in &devices {
            println!(
                "  {name}: {} MiB free of {} MiB ({:.0}% free)",
                memory.free_mb(),
                memory.total_mb(),
                memory.free_fraction() * 100.0,
            );
            assert!(memory.total > 0);
            assert!(memory.free <= memory.total);
        }
    }
}
