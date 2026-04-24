use crate::db::ChipVoltage;

#[derive(Debug, Clone)]
pub struct EraseType {
    pub size_bytes: u32,
    pub opcode: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParamSource {
    Database,
    Sfdp,
    DatabaseWithSfdp,
}

/// Runtime chip descriptor, built from DB and/or SFDP data.
/// All fields are intentionally public for use by callers; not all may be consumed
/// in the current command set.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ResolvedChip {
    pub name: String,
    pub mfr: u8,
    pub id1: u8,
    pub id2: u8,
    pub voltage: ChipVoltage,
    pub size_bytes: u32,
    pub page_size: u32,
    /// Smallest erase granularity in bytes.
    pub erase_size: u32,
    pub erase_types: Vec<EraseType>,
    pub addr_bytes: u8,
    pub quad: bool,
    pub source: ParamSource,
    /// Max chip erase timeout in ms. Set from SFDP typical time × 8; None means use fallback.
    pub chip_erase_max_ms: Option<u64>,
}

impl ResolvedChip {
    /// Chip erase max timeout in milliseconds.
    /// Uses SFDP-derived value (typ × 8) when available, falls back to size-based estimate.
    pub fn chip_erase_timeout_ms(&self) -> u64 {
        self.chip_erase_max_ms.unwrap_or_else(|| {
            // Fallback: ~13s per MB, floored at 60s. Conservative but not verified per-chip.
            let mb = ((self.size_bytes as u64).div_ceil(1024 * 1024)).max(1);
            (mb * 13_000).max(60_000)
        })
    }

    pub fn chip_erase_timeout_secs(&self) -> u64 {
        self.chip_erase_timeout_ms().div_ceil(1000)
    }
}
