//! Choosing a GPU the viewer can actually start on.
//!
//! The discrete card is the fastest place to draw, but it is also the one a
//! game will have filled. When it has no memory left, asking it for a device
//! fails and the whole application dies before a window ever appears:
//!
//! ```text
//! Error: Wgpu(RequestDeviceError(RequestDeviceError { inner: Core(Device(OutOfMemory)) }))
//! ```
//!
//! So the adapters are tried in order — discrete, then integrated, then
//! software — *before* the window is created. Probing first rather than
//! retrying `run_native` matters: a failed run may already have built a winit
//! event loop, and a second one is not portable.
//!
//! Each step down also shrinks the texture budgets, because the reason to fall
//! back is usually that memory is scarce.

use eframe::wgpu;

/// Which class of adapter the viewer ended up on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// A separate graphics card. Fastest, and the first choice.
    Discrete,
    /// The CPU's built-in graphics, which draw from system memory and so keep
    /// working when a game has filled the card.
    Integrated,
    /// A software rasteriser. Slow, but it always starts.
    Software,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Self::Discrete => "discrete GPU",
            Self::Integrated => "integrated GPU",
            Self::Software => "software rendering",
        }
    }

    /// Texture budgets in MB, as `(images, thumbnails)`.
    ///
    /// The defaults assume a card to itself. An integrated GPU is drawing from
    /// system RAM and a software rasteriser has to touch every byte on the CPU,
    /// so both get far less.
    pub fn budgets(self) -> (usize, usize) {
        match self {
            Self::Discrete => (1500, 384),
            Self::Integrated => (512, 192),
            Self::Software => (192, 64),
        }
    }

    fn matches(self, kind: wgpu::DeviceType) -> bool {
        match self {
            Self::Discrete => matches!(kind, wgpu::DeviceType::DiscreteGpu),
            Self::Integrated => {
                matches!(
                    kind,
                    wgpu::DeviceType::IntegratedGpu | wgpu::DeviceType::VirtualGpu
                )
            }
            Self::Software => matches!(kind, wgpu::DeviceType::Cpu | wgpu::DeviceType::Other),
        }
    }
}

/// Who made the adapter. Used to break ties between cards of the same class.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
    Other,
}

impl Vendor {
    fn from_id(id: u32) -> Self {
        match id {
            0x10DE => Self::Nvidia,
            0x1002 | 0x1022 => Self::Amd,
            0x8086 => Self::Intel,
            _ => Self::Other,
        }
    }

    /// Short enough for the title bar.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Nvidia => "NVIDIA",
            Self::Amd => "AMD",
            Self::Intel => "INTEL",
            Self::Other => "GPU",
        }
    }

    /// The maker's logo, from `icons/`.
    ///
    /// Compiled in rather than read at runtime, so the badge cannot end up
    /// blank because a file moved. `None` for a vendor we have no artwork for,
    /// which falls back to the drawn chip.
    pub fn logo(self) -> Option<&'static [u8]> {
        match self {
            Self::Nvidia => Some(include_bytes!("../icons/nvidia.png")),
            Self::Amd => Some(include_bytes!("../icons/amd.png")),
            Self::Intel => Some(include_bytes!("../icons/intel.png")),
            Self::Other => None,
        }
    }

    /// The maker's own brand colour, which is what makes the badge readable at
    /// a glance without reproducing a trademarked logo.
    pub fn colour(self) -> eframe::egui::Color32 {
        use eframe::egui::Color32;
        match self {
            // NVIDIA green, AMD red, Intel blue.
            Self::Nvidia => Color32::from_rgb(0x76, 0xB9, 0x00),
            Self::Amd => Color32::from_rgb(0xED, 0x1C, 0x24),
            Self::Intel => Color32::from_rgb(0x00, 0x71, 0xC5),
            Self::Other => crate::theme::TEXT_MUTED,
        }
    }

    /// Preference between adapters of the same class. Morgan asked for NVIDIA
    /// over AMD, and on this machine that is also discrete over integrated.
    fn preference(self) -> i64 {
        match self {
            Self::Nvidia => 30,
            Self::Amd => 20,
            Self::Intel => 10,
            Self::Other => 0,
        }
    }
}

/// What the probe settled on, and what it had to go through to get there.
pub struct Choice {
    pub tier: Tier,
    pub adapter_name: String,
    pub vendor: Vendor,
    /// Free and total memory, where the driver would say.
    pub memory: Option<crate::vram::Memory>,
    /// Adapters that were tried and refused, for the log and the DEBUG tab.
    pub rejected: Vec<(Tier, String)>,
}

impl Choice {
    /// True when the viewer is not on the machine's fastest option, which is
    /// worth saying out loud rather than leaving the user to wonder why it
    /// feels slow.
    pub fn is_fallback(&self) -> bool {
        self.tier != Tier::Discrete || !self.rejected.is_empty()
    }

    /// What the title bar shows: the maker, or `CPU` when nothing is drawing on
    /// hardware at all.
    pub fn badge(&self) -> &'static str {
        match self.tier {
            Tier::Software => "CPU",
            _ => self.vendor.badge(),
        }
    }

    /// Colour for the badge. The maker's own when drawing on hardware, and
    /// amber for software rendering — deliberately not red, which AMD's brand
    /// colour already occupies and which would read as an error rather than as
    /// "working, but slowly".
    pub fn badge_colour(&self) -> eframe::egui::Color32 {
        match self.tier {
            Tier::Software => crate::theme::WARNING,
            _ => self.vendor.colour(),
        }
    }

    pub fn is_software(&self) -> bool {
        self.tier == Tier::Software
    }

    /// Logo for the badge, or `None` when the drawn chip should be used —
    /// software rendering is not a vendor and has no mark of its own.
    pub fn logo(&self) -> Option<&'static [u8]> {
        (!self.is_software()).then(|| self.vendor.logo()).flatten()
    }

    pub fn summary(&self) -> String {
        match self.memory {
            Some(memory) => format!(
                "{} ({}, {} MB free of {})",
                self.adapter_name,
                self.tier.label(),
                memory.free_mb(),
                memory.total_mb()
            ),
            None => format!("{} ({})", self.adapter_name, self.tier.label()),
        }
    }

    /// Whether this adapter draws on system memory rather than its own.
    ///
    /// True for an integrated GPU, which carves its working memory out of
    /// ordinary RAM, and for the software rasteriser, which *is* ordinary RAM.
    /// For both, a "free VRAM" figure would be a number with nothing behind it,
    /// so the readout shows what the machine actually has spare instead.
    pub fn shares_system_memory(&self) -> bool {
        matches!(self.tier, Tier::Integrated | Tier::Software)
    }

    /// Texture budgets in MB, sized to what is actually free rather than to a
    /// fixed guess.
    ///
    /// A card can grant a device and still have almost nothing left — which is
    /// exactly the case a game creates. Where the driver reports free memory,
    /// take a share of that instead; where it does not, fall back to the tier's
    /// assumption.
    pub fn budgets(&self) -> (usize, usize) {
        let (tier_images, tier_thumbs) = self.tier.budgets();
        let Some(memory) = self.memory else {
            return (tier_images, tier_thumbs);
        };

        // Leave most of the card alone: whatever else is running wants it too,
        // and the driver needs headroom for its own buffers.
        let usable = (memory.free_mb() as f64 * 0.6) as usize;
        let images = (usable * 4 / 5).clamp(MINIMUM_IMAGE_MB, tier_images);
        let thumbs = (usable / 5).clamp(MINIMUM_THUMB_MB, tier_thumbs);
        (images, thumbs)
    }
}

/// Floors, so a busy card still gets enough to show something.
const MINIMUM_IMAGE_MB: usize = 128;
const MINIMUM_THUMB_MB: usize = 48;

/// Below this an adapter is treated as unusable even if it would hand over a
/// device, because the first full-resolution texture would fail.
/// How much free memory a card must report before it is worth choosing.
///
/// This is deliberately far above what the viewer needs to draw one frame. The
/// figure a driver reports is a snapshot, and a running game moves it by
/// hundreds of MB between the reading and the first paint — measured on a
/// 12 GB RTX 4070 with a game on it, which reported 774 MB free, granted a
/// device, accepted 192 MB of probe textures, and then failed out of memory on
/// the first egui texture. The margin is the point: below this, the number is
/// not trustworthy enough to bet the window on, and the integrated GPU draws
/// out of system RAM perfectly well.
///
/// Two gigabytes, set by morgan: a discrete card with less than that going
/// spare is a card with a game on it, and the iGPU is the better place to be.
const REQUIRED_FREE_MB: u64 = 2048;

/// Order to try, honouring `GEO_REF_MAPPER_RENDERER` when it names one.
///
/// The override exists because a user who already knows their card is busy
/// should not have to wait for it to fail first.
fn tiers_to_try() -> Vec<Tier> {
    match std::env::var("GEO_REF_MAPPER_RENDERER").ok().as_deref() {
        Some("software" | "cpu" | "llvmpipe") => vec![Tier::Software],
        Some("integrated" | "igpu" | "low") => vec![Tier::Integrated, Tier::Software],
        Some("discrete" | "gpu" | "high") => vec![Tier::Discrete],
        _ => vec![Tier::Discrete, Tier::Integrated, Tier::Software],
    }
}

/// Finds an adapter that will actually hand over a device.
///
/// Enumerating adapters is not enough — one can be listed and still refuse,
/// which is exactly what a full card does. So each candidate is asked for a
/// device and only kept if it says yes.
pub fn probe() -> Result<Choice, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    if adapters.is_empty() {
        return Err("no graphics adapters were found at all".to_owned());
    }

    // One NVML call for the whole machine, then matched to adapters by name.
    let nvidia = crate::vram::nvidia_memory();
    let allowed = tiers_to_try();

    // Rank every candidate first, so the best is tried first rather than
    // whichever the driver happened to list first.
    let mut candidates: Vec<(
        i64,
        Tier,
        &wgpu::Adapter,
        Vendor,
        Option<crate::vram::Memory>,
    )> = Vec::new();
    for adapter in &adapters {
        let info = adapter.get_info();
        let Some(tier) = allowed
            .iter()
            .copied()
            .find(|tier| tier.matches(info.device_type))
        else {
            continue;
        };
        let vendor = Vendor::from_id(info.vendor);
        let memory = nvidia
            .iter()
            .find(|(name, _)| crate::vram::matches_adapter(name, &info.name))
            .map(|(_, memory)| *memory);

        candidates.push((score(tier, vendor, memory), tier, adapter, vendor, memory));
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));

    let mut rejected = Vec::new();
    for (_, tier, adapter, vendor, memory) in candidates {
        let info = adapter.get_info();

        // A card that reports almost nothing free will grant a device and then
        // fail on the first large upload, which looks like a random crash.
        if let Some(memory) = memory
            && memory.free_mb() < REQUIRED_FREE_MB
        {
            rejected.push((
                tier,
                format!(
                    "{}: only {:.1} GB free, {:.1} GB is the minimum",
                    info.name,
                    memory.free_mb() as f64 / 1024.0,
                    REQUIRED_FREE_MB as f64 / 1024.0,
                ),
            ));
            continue;
        }

        match request_device(adapter) {
            Ok(()) => {
                return Ok(Choice {
                    tier,
                    adapter_name: info.name.clone(),
                    vendor,
                    memory,
                    rejected,
                });
            }
            Err(error) => rejected.push((tier, format!("{}: {error}", info.name))),
        }
    }

    Err(describe_failure(&rejected))
}

/// Higher is better. Class dominates, then how much memory is actually free,
/// then the maker.
///
/// Free memory outranks the vendor deliberately: a busy NVIDIA card is a worse
/// place to draw than an idle AMD one, whatever the general preference.
fn score(tier: Tier, vendor: Vendor, memory: Option<crate::vram::Memory>) -> i64 {
    let base = match tier {
        Tier::Discrete => 1000,
        Tier::Integrated => 500,
        Tier::Software => 100,
    };
    // Up to 200 points for free memory, so a nearly-full discrete card can
    // still lose to an emptier one but never to a software rasteriser.
    let headroom = memory.map_or(100, |memory| {
        ((memory.free_mb() as f64 / 8192.0).min(1.0) * 200.0) as i64
    });
    base + headroom + vendor.preference()
}

/// How much texture memory the probe insists on being able to allocate before
/// it calls an adapter usable.
///
/// A device can be granted on a card with nothing left to give: the grant costs
/// nothing, so it succeeds, and then the first real texture fails. This is what
/// the working set costs at startup — swapchain, egui atlas, the first picture
/// and its thumbnails — so an adapter that cannot find this much is not one we
/// can draw on.
const PROBE_MB: u64 = 384;

/// Asks an adapter for a device *and puts memory on it*, then drops both.
///
/// Asking alone proves nothing. The allocation is the test: a discrete card
/// with a game on it reports a few hundred MB free, hands over a device, and
/// then dies out of memory when the window opens — the free figure was already
/// stale by the time it was read. Failing here instead means the next tier gets
/// its turn while there is still no window on screen.
fn request_device(adapter: &wgpu::Adapter) -> Result<(), String> {
    pollster::block_on(async {
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("geo-ref_mapper-probe"),
                // Ask for the adapter's own limits rather than defaults, which
                // is what eframe will do; probing with easier limits would give
                // a false pass.
                required_limits: adapter.limits(),
                required_features: wgpu::Features::empty(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|error| error.to_string())?;

        // An out-of-memory arrives asynchronously through the error handler,
        // whose default is to panic. Catch it in a scope so it comes back as a
        // value we can act on.
        let scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

        // Split across several textures rather than one huge one: a card can
        // hold the total while having no single free block that big, which is
        // also true of the real workload.
        const EDGE: u32 = 2048; // 2048² RGBA8 = 16 MB
        let count = PROBE_MB / 16;
        let mut held = Vec::new();
        for index in 0..count {
            held.push(device.create_texture(&wgpu::TextureDescriptor {
                label: Some("geo-ref_mapper-probe-texture"),
                size: wgpu::Extent3d {
                    width: EDGE,
                    height: EDGE,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            let _ = index;
        }

        // Creation is lazy on some drivers — the memory is not committed until
        // something is written. Write one row to each so the failure, if there
        // is going to be one, happens here.
        let row = vec![0_u8; (EDGE * 4) as usize];
        for texture in &held {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &row,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(EDGE * 4),
                    rows_per_image: Some(1),
                },
                wgpu::Extent3d {
                    width: EDGE,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
        }
        queue.submit([]);
        let _ = device.poll(wgpu::PollType::wait_indefinitely());

        match scope.pop().await {
            Some(error) => Err(format!("{PROBE_MB} MB of textures refused — {error}")),
            None => Ok(()),
        }
    })
}

fn describe_failure(rejected: &[(Tier, String)]) -> String {
    if rejected.is_empty() {
        return "no usable graphics adapter was found".to_owned();
    }
    let mut lines = vec!["Every graphics adapter refused to start:".to_owned()];
    for (tier, reason) in rejected {
        lines.push(format!("  • {} — {reason}", tier.label()));
    }
    lines.join("\n")
}

/// Builds the wgpu configuration that pins eframe to the chosen tier.
pub fn configuration(tier: Tier) -> eframe::egui_wgpu::WgpuConfiguration {
    use std::sync::Arc;

    let mut configuration = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut configuration.wgpu_setup {
        setup.power_preference = match tier {
            Tier::Discrete => wgpu::PowerPreference::HighPerformance,
            Tier::Integrated | Tier::Software => wgpu::PowerPreference::LowPower,
        };
        // Choose explicitly rather than by preference: "low power" would still
        // pick the discrete card on a machine that has only one.
        setup.native_adapter_selector = Some(Arc::new(move |adapters, _surface| {
            adapters
                .iter()
                .find(|adapter| tier.matches(adapter.get_info().device_type))
                .cloned()
                .ok_or_else(|| format!("no {} is available", tier.label()))
        }));
    }
    configuration
}

/// Tells the user why nothing opened, in a window rather than on a terminal
/// they may never see.
pub fn report_failure(details: &str) {
    let message = format!(
        "geo-ref_mapper could not start because no graphics adapter would give it a device.\n\n\
         {details}\n\n\
         This usually means something else — a game, most often — is using all of \
         the graphics memory. Closing it and trying again should work.\n\n\
         You can also force a renderer:\n\
         \u{20}   GEO_REF_MAPPER_RENDERER=integrated geo-ref_mapper\n\
         \u{20}   GEO_REF_MAPPER_RENDERER=software geo-ref_mapper"
    );
    eprintln!("geo-ref_mapper: {message}");

    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("geo-ref_mapper cannot start")
        .set_description(message)
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_with_a_game_on_it_is_passed_over_for_the_integrated_one() {
        // The measured case: a 12 GB card reporting well under the floor.
        assert!(
            memory(774, 12_288).free_mb() < REQUIRED_FREE_MB,
            "774 MB free must not be enough to choose the discrete card",
        );
        // And a card with room to work is still chosen.
        assert!(memory(10_000, 12_288).free_mb() >= REQUIRED_FREE_MB);
    }

    #[test]
    fn the_floor_is_two_gigabytes() {
        assert_eq!(REQUIRED_FREE_MB, 2048);
    }

    #[test]
    fn each_tier_claims_only_its_own_device_types() {
        assert!(Tier::Discrete.matches(wgpu::DeviceType::DiscreteGpu));
        assert!(!Tier::Discrete.matches(wgpu::DeviceType::IntegratedGpu));
        assert!(!Tier::Discrete.matches(wgpu::DeviceType::Cpu));

        assert!(Tier::Integrated.matches(wgpu::DeviceType::IntegratedGpu));
        assert!(!Tier::Integrated.matches(wgpu::DeviceType::DiscreteGpu));

        assert!(Tier::Software.matches(wgpu::DeviceType::Cpu));
        assert!(!Tier::Software.matches(wgpu::DeviceType::DiscreteGpu));
    }

    #[test]
    fn falling_back_shrinks_the_texture_budgets() {
        let (discrete_images, discrete_thumbs) = Tier::Discrete.budgets();
        let (integrated_images, integrated_thumbs) = Tier::Integrated.budgets();
        let (software_images, software_thumbs) = Tier::Software.budgets();

        assert!(integrated_images < discrete_images);
        assert!(software_images < integrated_images);
        assert!(integrated_thumbs < discrete_thumbs);
        assert!(software_thumbs < integrated_thumbs);
        // Even the smallest has to hold a texture or two, or nothing displays.
        assert!(software_images >= 128);
    }

    #[test]
    fn the_environment_override_picks_a_single_tier() {
        // Not set: the whole ladder, fastest first.
        unsafe { std::env::remove_var("GEO_REF_MAPPER_RENDERER") };
        assert_eq!(
            tiers_to_try(),
            vec![Tier::Discrete, Tier::Integrated, Tier::Software]
        );

        unsafe { std::env::set_var("GEO_REF_MAPPER_RENDERER", "software") };
        assert_eq!(tiers_to_try(), vec![Tier::Software]);

        unsafe { std::env::set_var("GEO_REF_MAPPER_RENDERER", "integrated") };
        assert_eq!(tiers_to_try(), vec![Tier::Integrated, Tier::Software]);

        // Anything unrecognised falls back to the normal ladder rather than
        // refusing to start over a typo.
        unsafe { std::env::set_var("GEO_REF_MAPPER_RENDERER", "nonsense") };
        assert_eq!(
            tiers_to_try(),
            vec![Tier::Discrete, Tier::Integrated, Tier::Software]
        );
        unsafe { std::env::remove_var("GEO_REF_MAPPER_RENDERER") };
    }

    fn memory(free_mb: u64, total_mb: u64) -> crate::vram::Memory {
        crate::vram::Memory {
            free: free_mb * 1024 * 1024,
            total: total_mb * 1024 * 1024,
            used: (total_mb - free_mb) * 1024 * 1024,
        }
    }

    #[test]
    fn a_discrete_card_always_outranks_an_integrated_one() {
        // Even an almost-full discrete card beats a wide-open iGPU, because
        // "almost full" still leaves more than the integrated path ever has.
        let busy_discrete = score(Tier::Discrete, Vendor::Nvidia, Some(memory(400, 12288)));
        let idle_integrated = score(Tier::Integrated, Vendor::Amd, None);
        assert!(busy_discrete > idle_integrated);

        // And anything on hardware beats software.
        assert!(idle_integrated > score(Tier::Software, Vendor::Other, None));
    }

    #[test]
    fn nvidia_is_preferred_over_amd_when_all_else_is_equal() {
        let nvidia = score(Tier::Discrete, Vendor::Nvidia, Some(memory(8192, 12288)));
        let amd = score(Tier::Discrete, Vendor::Amd, Some(memory(8192, 12288)));
        assert!(nvidia > amd, "{nvidia} should beat {amd}");
    }

    /// The point of measuring free memory rather than trusting the label.
    #[test]
    fn a_busy_card_loses_to_an_emptier_one_of_the_same_class() {
        let busy_nvidia = score(Tier::Discrete, Vendor::Nvidia, Some(memory(300, 12288)));
        let free_amd = score(Tier::Discrete, Vendor::Amd, Some(memory(8192, 16384)));
        assert!(
            free_amd > busy_nvidia,
            "free memory should outweigh the vendor preference"
        );
    }

    #[test]
    fn budgets_shrink_to_fit_the_memory_that_is_actually_free() {
        let plenty = Choice {
            tier: Tier::Discrete,
            adapter_name: "NVIDIA".to_owned(),
            vendor: Vendor::Nvidia,
            memory: Some(memory(10_000, 12_288)),
            rejected: Vec::new(),
        };
        let (images, thumbs) = plenty.budgets();
        // Never more than the tier allows, however much is free.
        let (tier_images, tier_thumbs) = Tier::Discrete.budgets();
        assert_eq!(images, tier_images);
        assert_eq!(thumbs, tier_thumbs);

        // A card with a game on it gets far less than the tier default.
        let squeezed = Choice {
            memory: Some(memory(900, 12_288)),
            ..plenty
        };
        let (images, thumbs) = squeezed.budgets();
        assert!(
            images < tier_images,
            "{images} should be well under {tier_images}"
        );
        assert!(images >= MINIMUM_IMAGE_MB);
        assert!(thumbs >= MINIMUM_THUMB_MB);
    }

    #[test]
    fn an_adapter_that_reports_nothing_falls_back_to_the_tier_budget() {
        let unknown = Choice {
            tier: Tier::Integrated,
            adapter_name: "AMD Radeon".to_owned(),
            vendor: Vendor::Amd,
            memory: None,
            rejected: Vec::new(),
        };
        assert_eq!(unknown.budgets(), Tier::Integrated.budgets());
    }

    #[test]
    fn the_badge_names_the_maker_or_says_cpu() {
        let make = |tier, vendor| Choice {
            tier,
            adapter_name: String::new(),
            vendor,
            memory: None,
            rejected: Vec::new(),
        };
        assert_eq!(make(Tier::Discrete, Vendor::Nvidia).badge(), "NVIDIA");
        assert_eq!(make(Tier::Integrated, Vendor::Amd).badge(), "AMD");
        assert_eq!(make(Tier::Integrated, Vendor::Intel).badge(), "INTEL");
        // Software rendering is not a GPU at all, whatever the vendor id says.
        assert_eq!(make(Tier::Software, Vendor::Other).badge(), "CPU");

        // Anything but the fastest tier is flagged, so the badge can warn.
        assert!(!make(Tier::Discrete, Vendor::Nvidia).is_fallback());
        assert!(make(Tier::Integrated, Vendor::Amd).is_fallback());
        assert!(make(Tier::Software, Vendor::Other).is_fallback());
    }

    #[test]
    fn vendor_ids_map_to_the_right_makers() {
        assert_eq!(Vendor::from_id(0x10DE), Vendor::Nvidia);
        assert_eq!(Vendor::from_id(0x1002), Vendor::Amd);
        assert_eq!(Vendor::from_id(0x8086), Vendor::Intel);
        assert_eq!(Vendor::from_id(0xBEEF), Vendor::Other);
    }

    #[test]
    fn a_failure_report_names_every_adapter_that_refused() {
        let details = describe_failure(&[
            (Tier::Discrete, "NVIDIA RTX 4070: OutOfMemory".to_owned()),
            (Tier::Integrated, "AMD Radeon: OutOfMemory".to_owned()),
        ]);
        assert!(details.contains("NVIDIA RTX 4070"));
        assert!(details.contains("AMD Radeon"));
        assert!(details.contains("discrete GPU"));

        // And it still says something useful when there is nothing to list.
        assert!(!describe_failure(&[]).is_empty());
    }

    #[test]
    fn a_choice_knows_whether_it_settled_for_less() {
        let best = Choice {
            tier: Tier::Discrete,
            adapter_name: "NVIDIA".to_owned(),
            vendor: Vendor::Nvidia,
            memory: None,
            rejected: Vec::new(),
        };
        assert!(!best.is_fallback());
        assert!(best.summary().contains("discrete"));

        let fallen_back = Choice {
            tier: Tier::Integrated,
            adapter_name: "AMD Radeon".to_owned(),
            vendor: Vendor::Amd,
            memory: None,
            rejected: vec![(Tier::Discrete, "full".to_owned())],
        };
        assert!(fallen_back.is_fallback());
        assert!(fallen_back.summary().contains("AMD Radeon"));
    }
}
