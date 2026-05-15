//! Memory-size constants used across the codebase.
//!
//! Use these instead of inline `N * 1024 * 1024` expressions so that intent is
//! obvious at the call site and thresholds are centrally defined.

#![allow(dead_code)]

pub const KIB: u32 = 1024;
pub const MIB: u32 = 1024 * 1024;

pub const KB_4: u32 = 4 * KIB;
pub const KB_32: u32 = 32 * KIB;
pub const KB_64: u32 = 64 * KIB;

pub const MB_1: u32 = MIB;
pub const MB_2: u32 = 2 * MIB;
pub const MB_4: u32 = 4 * MIB;
pub const MB_8: u32 = 8 * MIB;
pub const MB_16: u32 = 16 * MIB;
pub const MB_32: u32 = 32 * MIB;
pub const MB_64: u32 = 64 * MIB;
pub const MB_256: u32 = 256 * MIB;
