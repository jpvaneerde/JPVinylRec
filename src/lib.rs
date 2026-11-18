//! JPVinylRec core: everything except the GUI.
//!
//! Pipeline: load side recording -> analyse (noise floor, music bounds, gaps)
//! -> estimate speed factor (reference cross-correlation and/or database timings)
//! -> place track boundaries (gaps snapped to database timings)
//! -> resample each track by the speed factor -> tagged FLAC.

pub mod analysis;
pub mod audio;
pub mod export;
pub mod meta;
pub mod resample;
pub mod speed;
pub mod split;
