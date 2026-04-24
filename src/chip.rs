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
}
