//! Surviving a graphics card that refuses a texture.
//!
//! wgpu reports anything it cannot hand back through a return value — an
//! allocation that failed inside the driver, a validation error raised while a
//! command buffer is being built — through one handler per device, and the
//! default handler panics. That is why running out of graphics memory in the
//! middle of a frame killed the process outright: nothing was wrong with the
//! viewer's own state, the card simply had no room for the next texture, and
//! there was nobody to tell.
//!
//! Installing a handler turns that into a condition instead of a crash. An out
//! of memory is counted and remembered, the frame finishes drawing whatever it
//! can, and the app lets go of textures at the top of the next one — a picture
//! going soft for a moment rather than the window disappearing.
//!
//! Nothing here decides *what* to let go of. This module only catches the
//! report and holds it until somebody asks; [`crate::app`] does the shedding,
//! because it is the only thing that knows what is on screen.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use eframe::wgpu;

/// Textures the card has refused since the viewer started.
static REFUSALS: AtomicU64 = AtomicU64::new(0);

/// Errors that were not about memory. Counted separately: those are bugs, and
/// lumping them in with a full card would hide them.
static FAULTS: AtomicU64 = AtomicU64::new(0);

/// The first refusal not yet acted on, kept for the message.
///
/// First rather than latest: one failed allocation usually drags several more
/// behind it in the same frame, and the first is the one that says what
/// actually ran out.
static PENDING: Mutex<Option<String>> = Mutex::new(None);

/// How many faults are printed before the log goes quiet.
///
/// A validation error inside the paint loop repeats on every frame, so
/// printing all of them would fill a terminal in seconds and bury the first
/// one — which is the only one that says where it started.
const FAULTS_PRINTED: u64 = 5;

/// Takes over error reporting for a device.
///
/// Call once, on the device eframe built, before the first frame.
pub fn install(device: &wgpu::Device) {
    device.on_uncaptured_error(std::sync::Arc::new(handle));
}

fn handle(error: wgpu::Error) {
    match error {
        wgpu::Error::OutOfMemory { source } => {
            REFUSALS.fetch_add(1, Ordering::Relaxed);
            let mut pending = PENDING.lock().unwrap_or_else(|held| held.into_inner());
            if pending.is_none() {
                *pending = Some(source.to_string());
            }
        }
        // Not a shortage: something asked the card for a thing it does not do.
        // The window stays up — a wrong frame is more use than no window — but
        // it is said out loud rather than swallowed.
        wgpu::Error::Validation { description, .. } => report_fault("validation", &description),
        wgpu::Error::Internal { description, .. } => report_fault("internal", &description),
    }
}

fn report_fault(kind: &str, description: &str) {
    let seen = FAULTS.fetch_add(1, Ordering::Relaxed);
    if seen < FAULTS_PRINTED {
        eprintln!("geo-ref_mapper: wgpu {kind} error — {description}");
    } else if seen == FAULTS_PRINTED {
        eprintln!("geo-ref_mapper: further wgpu errors will not be printed");
    }
}

/// The refusal waiting to be acted on, if there is one, clearing it.
///
/// Called once a frame. Returning it clears the flag, so a card that refuses
/// again after the shed trips a second one rather than being answered twice
/// for the same failure.
pub fn take_refusal() -> Option<String> {
    let mut pending = PENDING.lock().unwrap_or_else(|held| held.into_inner());
    pending.take()
}

/// Textures the card has refused, for the DEBUG tab.
pub fn refusals() -> u64 {
    REFUSALS.load(Ordering::Relaxed)
}

/// wgpu errors that were not about memory, for the DEBUG tab.
pub fn faults() -> u64 {
    FAULTS.load(Ordering::Relaxed)
}

/// Fills the card until it says no, then lets go — for testing the recovery.
///
/// Runs only when `GEO_REF_MAPPER_TEST_OOM` is set. There is no other way to reach
/// this path deliberately: a card refuses a texture when something else on the
/// machine has taken the room, which is not a thing to wait around for, and
/// asking for a texture that is merely too large gives a validation error
/// rather than a refusal — a different branch entirely.
///
/// The textures are dropped before returning, so what is being tested is the
/// handling and not the shortage: the refusal is recorded, the memory comes
/// straight back, and the first frame sheds and carries on. A test that held
/// the memory would only prove the card stays full.
pub fn exhaust_for_testing(device: &wgpu::Device) {
    let Ok(setting) = std::env::var("GEO_REF_MAPPER_TEST_OOM") else {
        return;
    };
    if setting == "0" || setting.eq_ignore_ascii_case("off") {
        return;
    }

    // 4096² RGBA is 64 MB a time — large enough to walk a 12 GB card down in a
    // couple of hundred steps, small enough not to be refused for its size on
    // a card that still has room.
    const EDGE: u32 = 4096;
    const MOST: usize = 1024;

    let before = refusals();
    let mut held = Vec::new();
    while held.len() < MOST && refusals() == before {
        held.push(device.create_texture(&wgpu::TextureDescriptor {
            label: Some("geo-ref_mapper-oom-test"),
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
    }
    let asked = held.len();
    drop(held);

    if refusals() == before {
        eprintln!(
            "geo-ref_mapper: GEO_REF_MAPPER_TEST_OOM took {} MB without being refused — the card has more \
             than this test will ask for",
            asked * 64,
        );
    } else {
        eprintln!(
            "geo-ref_mapper: GEO_REF_MAPPER_TEST_OOM filled the card after {} MB and was refused; \
             the memory has been given back",
            asked * 64,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counters and the pending slot are process-wide, so this is one test
    /// rather than several that would race each other.
    #[test]
    fn a_refusal_is_held_until_it_is_taken_and_then_is_gone() {
        assert_eq!(take_refusal(), None, "nothing has failed yet");

        handle(wgpu::Error::OutOfMemory {
            source: Box::new(std::io::Error::other("no room on the card")),
        });
        // A second failure in the same frame does not overwrite the first.
        handle(wgpu::Error::OutOfMemory {
            source: Box::new(std::io::Error::other("still no room")),
        });

        assert_eq!(refusals(), 2, "both are counted");
        let taken = take_refusal().expect("a refusal was recorded");
        assert!(
            taken.contains("no room on the card"),
            "the first one is what is reported — got {taken}"
        );
        assert_eq!(
            take_refusal(),
            None,
            "taking it clears it, so one failure is answered once"
        );

        // A validation error is a bug, not a shortage: counted apart, and it
        // must not ask the app to throw its textures away.
        handle(wgpu::Error::Validation {
            source: Box::new(std::io::Error::other("bad descriptor")),
            description: "texture too large".to_owned(),
        });
        assert_eq!(faults(), 1);
        assert_eq!(refusals(), 2, "a fault is not a refusal");
        assert_eq!(take_refusal(), None);
    }
}
