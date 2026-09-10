//! Locating the large fixtures.
//!
//! The reference material is 8.8 GB of video and lives outside the repository,
//! so every test that needs it skips cleanly when it is absent rather than
//! failing. `cargo test` therefore passes on a fresh clone, and reports which
//! tests it could not run.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// The GoPro Max 2 reference clip: 8.55 GB, 25:38, MAX2, firmware H24.02.01.30.00.
pub const REFERENCE_360: &str = "sources/GS010527.360";

/// The prototype's telemetry CSV for the same clip, for parity checks.
pub const REFERENCE_CSV: &str = "sources/GS010527.telemetry.csv";

/// A MotionRecord ride export: the upstream form of an external GPS source.
pub const REFERENCE_RIDE_JSON: &str = "sources/66e6c51d_kurvenfokus.json";

/// `motion-record-analysis`'s GPX output: 3745 points carrying lean angle.
pub const EXTERNAL_GPX: &str = "../../Documents/Projects/jophj/motion-record-analysis/ride.gpx";

/// The official GPX 1.1 schema.
pub const GPX_XSD: &str =
    "../../Documents/Projects/jophj/motion-record-analysis/test/fixtures/gpx.xsd";

/// Resolve a fixture path, or `None` when it is not present on this machine.
pub fn fixture(rel: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    p.exists().then_some(p)
}

/// Resolve a fixture or print a skip note and return.
macro_rules! fixture_or_skip {
    ($rel:expr) => {
        match crate::common::fixture($rel) {
            Some(p) => p,
            None => {
                eprintln!("SKIP: fixture {} is not present on this machine", $rel);
                return;
            }
        }
    };
}
pub(crate) use fixture_or_skip;

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}
