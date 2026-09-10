//! `gmax` — a GoPro Max 2 toolkit.
//!
//! The library half exists so integration tests can drive the same code paths
//! the CLI does. See `src/main.rs` for the command surface.

pub mod cli;
pub mod ffmpeg;
pub mod fourcc;
pub mod gpx;
pub mod gpmf;
pub mod math;
pub mod merge;
pub mod moments;
pub mod mp4;
pub mod render;
pub mod telemetry;
pub mod viewer;

pub use fourcc::FourCc;
