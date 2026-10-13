//! Stamps the build time into the binary.
//!
//! Only the raw Unix second is emitted here. Turning it into a date is done in
//! `src/version.rs`, where it can be tested — a build script cannot be, and the
//! civil-date arithmetic is the part worth testing.
//!
//! **No `rerun-if-changed` is printed, deliberately.** With none, Cargo re-runs
//! this script whenever anything in the package changes, which is exactly the
//! rule the stamp wants: the timestamp moves when the binary is actually
//! rebuilt, and stays put when Cargo decides there is nothing to do. Naming a
//! file here would freeze the stamp at whenever that file last changed; naming
//! a path that never exists would rebuild the crate on every single invocation.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // A clock before 1970 is the only way this fails, and a zero stamp reads
    // as "unknown" downstream rather than taking the build down.
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=GEO_REF_MAPPER_BUILD_UNIX={seconds}");
}
