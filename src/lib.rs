//! scar — decompile and compile Apple `Assets.car` files with no Apple tooling.
//!
//! Format documentation lives in `docs/FORMAT.md`; all byte layouts there were
//! verified against a real CoreUI-974.1 catalog.

pub mod argg;
pub mod authoring;
pub mod bom;
pub mod codec;
pub mod compile;
pub mod csi;
pub mod decompile;
pub mod deepmap;
pub mod deepmap_encode;
pub mod format;
pub mod manifest;
pub mod rawimg;
pub mod rle;
pub mod widegamut;
