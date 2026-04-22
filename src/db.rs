use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::OnceLock;

static DB: OnceLock<Vec<SpiNorDef>> = OnceLock::new();

const DB_RON: &str = include_str!("../db/spi_nor.ron");

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub enum ChipVoltage {
    V1_8,
    V3_3,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpiNorDef {
    pub name: String,
    pub mfr: u8,
    pub id1: u8,
    pub id2: u8,
    pub size_bytes: u32,
    pub page_size: u32,
    pub erase_size: u32,
    pub addr_bytes: u8,
    pub voltage: ChipVoltage,
}

pub fn load() -> Result<&'static Vec<SpiNorDef>> {
    if let Some(db) = DB.get() {
        return Ok(db);
    }
    let parsed: Vec<SpiNorDef> = ron::from_str(DB_RON).context("failed to parse spi_nor.ron")?;
    Ok(DB.get_or_init(|| parsed))
}

pub fn lookup(mfr: u8, id1: u8, id2: u8) -> Result<Option<&'static SpiNorDef>> {
    let db = load()?;
    Ok(db.iter().find(|d| d.mfr == mfr && d.id1 == id1 && d.id2 == id2))
}
