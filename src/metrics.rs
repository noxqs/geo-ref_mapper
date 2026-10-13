//! Runtime instrumentation for the DEBUG tab: what this process is actually
//! costing, sampled cheaply rather than every frame.

use std::time::{Duration, Instant};

use eframe::egui;

/// Re-reading `/proc` every frame would be wasteful; twice a second is plenty.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessMemory {
    /// Resident set size — physical memory currently held.
    pub resident: u64,
    /// Total virtual address space.
    pub virtual_size: u64,
    /// High-water mark of the resident set.
    pub peak_resident: u64,
}

/// A frame at 60 Hz has this long to do everything.
pub const FRAME_BUDGET_MS: f32 = 1000.0 / 60.0;

pub struct Metrics {
    pub memory: Option<ProcessMemory>,
    /// Smoothed cost of building the UI, in milliseconds. This is real work,
    /// unlike the gap between repaints.
    pub ui_ms: f32,
    pub worst_ui_ms: f32,
    /// Wall time since the previous repaint. egui only redraws on demand, so a
    /// long interval means the app was idle, not slow.
    pub repaint_interval_ms: f32,
    pub frames: u64,
    started: Instant,
    last_sample: Instant,
    frame_started: Option<Instant>,
}

impl Default for Metrics {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            memory: read_process_memory(),
            ui_ms: 0.0,
            worst_ui_ms: 0.0,
            repaint_interval_ms: 0.0,
            frames: 0,
            started: now,
            // Back-date so the first frame takes a fresh sample.
            last_sample: now - SAMPLE_INTERVAL,
            frame_started: None,
        }
    }
}

impl Metrics {
    /// Call at the top of the frame, before any panel is drawn.
    pub fn begin_frame(&mut self, ctx: &egui::Context) {
        self.frames += 1;
        self.frame_started = Some(Instant::now());

        let dt_ms = ctx.input(|i| i.unstable_dt) * 1000.0;
        if dt_ms.is_finite() && dt_ms > 0.0 {
            self.repaint_interval_ms = dt_ms;
        }

        if self.last_sample.elapsed() >= SAMPLE_INTERVAL {
            self.last_sample = Instant::now();
            self.memory = read_process_memory();
        }
    }

    /// Call at the very end of the frame.
    pub fn end_frame(&mut self) {
        let Some(started) = self.frame_started.take() else {
            return;
        };
        let ms = started.elapsed().as_secs_f32() * 1000.0;

        // Exponential moving average, so the readout does not flicker.
        self.ui_ms = if self.ui_ms == 0.0 {
            ms
        } else {
            self.ui_ms * 0.9 + ms * 0.1
        };
        // Ignore the first frames, which include window and font setup.
        if self.frames > 10 {
            self.worst_ui_ms = self.worst_ui_ms.max(ms);
        }
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Share of a 60 Hz frame the UI is consuming.
    pub fn budget_fraction(&self) -> f32 {
        self.ui_ms / FRAME_BUDGET_MS
    }
}

#[cfg(target_os = "linux")]
fn read_process_memory() -> Option<ProcessMemory> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_proc_status(&status)
}

#[cfg(not(target_os = "linux"))]
fn read_process_memory() -> Option<ProcessMemory> {
    None
}

/// Pulls `VmRSS`, `VmSize` and `VmHWM` out of `/proc/self/status`, which
/// reports them in kibibytes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_proc_status(status: &str) -> Option<ProcessMemory> {
    let field = |name: &str| -> Option<u64> {
        status
            .lines()
            .find(|line| line.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|kib| kib * 1024)
    };

    let resident = field("VmRSS:")?;
    Some(ProcessMemory {
        resident,
        virtual_size: field("VmSize:").unwrap_or(0),
        peak_resident: field("VmHWM:").unwrap_or(resident),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kibibyte_fields() {
        let status =
            "Name:\tgeo-ref_mapper\nVmSize:\t  2097152 kB\nVmHWM:\t   131072 kB\nVmRSS:\t    65536 kB\n";
        let memory = parse_proc_status(status).unwrap();
        assert_eq!(memory.resident, 65536 * 1024);
        assert_eq!(memory.virtual_size, 2097152 * 1024);
        assert_eq!(memory.peak_resident, 131072 * 1024);
    }

    #[test]
    fn missing_rss_is_not_reported() {
        assert!(parse_proc_status("Name:\tgeo-ref_mapper\n").is_none());
    }

    #[test]
    fn peak_falls_back_to_resident() {
        let memory = parse_proc_status("VmRSS:\t 1024 kB\n").unwrap();
        assert_eq!(memory.peak_resident, memory.resident);
    }

    #[test]
    fn ui_time_measures_the_frame_body_not_the_idle_gap() {
        let mut metrics = Metrics::default();
        metrics.frames = 100; // Past the warm-up window.

        metrics.frame_started = Some(Instant::now());
        std::thread::sleep(Duration::from_millis(5));
        metrics.end_frame();

        assert!(
            metrics.ui_ms >= 4.0,
            "expected at least the 5 ms slept, got {}",
            metrics.ui_ms
        );
        assert!(metrics.worst_ui_ms >= 4.0);
    }

    #[test]
    fn end_frame_without_begin_frame_is_ignored() {
        let mut metrics = Metrics::default();
        metrics.end_frame();
        assert_eq!(metrics.ui_ms, 0.0);
    }

    #[test]
    fn warm_up_frames_do_not_set_the_worst_case() {
        let mut metrics = Metrics::default();
        metrics.frames = 1;
        metrics.frame_started = Some(Instant::now() - Duration::from_millis(500));
        metrics.end_frame();

        assert_eq!(
            metrics.worst_ui_ms, 0.0,
            "window and font setup should not count as the worst frame"
        );
    }

    #[test]
    fn budget_fraction_is_relative_to_60hz() {
        let mut metrics = Metrics::default();
        metrics.ui_ms = FRAME_BUDGET_MS;
        assert!((metrics.budget_fraction() - 1.0).abs() < 1e-6);
    }
}

/// Names a shutdown step that made the user wait.
///
/// Closing is measured in tens of milliseconds when the pools are idle, but a
/// worker is only asked to stop *between* tasks — nothing can interrupt a
/// decode already under way, and a 2 GB map takes as long as it takes. When
/// that happens the window is gone and the pointer is still busy, with nothing
/// anywhere saying why.
///
/// Quiet below the threshold, so an ordinary exit stays silent and a slow one
/// names itself.
pub fn report_slow_join(what: &str, took: std::time::Duration) {
    const WORTH_SAYING: std::time::Duration = std::time::Duration::from_millis(200);
    if took >= WORTH_SAYING {
        eprintln!(
            "geo-ref_mapper: waited {:.0} ms at exit for {what} to finish what it was doing",
            took.as_secs_f32() * 1000.0,
        );
    }
}
