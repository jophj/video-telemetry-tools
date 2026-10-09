//! End-to-end tests on short sections of the reference ride.
//!
//! Each test runs the real `gmax` binary on a few seconds cut losslessly out
//! of the 25-minute reference recording (see `tests/common/sections.rs` for
//! the catalogue and why each window was chosen), and writes everything it
//! produces to a temporary directory.
//!
//! Without the reference recording, and with no sections already cut, every
//! test here prints SKIP and passes, so `cargo test` works on a fresh clone.
//!
//! ```text
//! cargo test --test e2e -- --ignored cut_all_sections   # pre-cut (one-off)
//! cargo test --test e2e                                 # run
//! GMAX_BLESS=1 cargo test --test e2e golden             # re-bless goldens
//! ```

#[path = "../common/mod.rs"]
mod common;

mod support;

mod analyse;
mod clips;
mod convert;
mod golden;
mod gpx;
mod merge;
mod telemetry;
mod viewer;

use common::sections::{section, SECTIONS};

/// Cut every section in the catalogue now, rather than on first use.
#[test]
#[ignore = "one-off: cuts every section from the reference (about 440 MB)"]
fn cut_all_sections() {
    for s in SECTIONS {
        if section(s.name).is_none() {
            return;
        }
    }
}
