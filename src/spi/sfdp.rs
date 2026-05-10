use anyhow::{bail, Result};
use tracing::{debug, info, warn};

use crate::chip::{EraseType, ParamSource, ResolvedChip};
use crate::db::{ChipVoltage, SpiNorDef};
use crate::fpga::Voltage;
use crate::spi::bus::{spibus_write, ss_disable, ss_enable};
use crate::spi::read::read_setup_packet;
use crate::usb::{UsbDevice, UsbReq};

const SFDP_MAGIC: [u8; 4] = *b"SFDP";
const JEDEC_BASIC_ID: u16 = 0xFF00;

/// Issue RSTEN (0x66) + RST (0x99) to return the chip to default SPI mode.
/// Safe to call unconditionally — a no-op on chips that don't support it.
async fn soft_reset(dev: &UsbDevice) {
    let _ = async {
        ss_enable(dev).await?;
        spibus_write(dev, &[0x66]).await?;
        ss_disable(dev).await?;
        ss_enable(dev).await?;
        spibus_write(dev, &[0x99]).await?;
        ss_disable(dev).await?;
        anyhow::Ok(())
    }.await;
    // 10ms recovery — covers SST26VF tRST requirement (30µs per JESD216 is insufficient)
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
}

/// Raw SFDP read: opcode 0x5A + 3-byte addr + 1 dummy byte.
async fn sfdp_read(dev: &UsbDevice, addr: u32, len: u32) -> Result<Vec<u8>> {
    let setup = read_setup_packet(0x5A, 3, addr, len, 1);
    dev.ctrl_out(UsbReq::SpiReadFlash, 0, Some(&setup)).await?;
    let data = dev.bulk_in(len as usize).await?;
    if data.len() != len as usize {
        bail!("SFDP short read: {} of {}", data.len(), len);
    }
    Ok(data)
}

#[derive(Debug)]
pub struct SfdpInfo {
    pub sfdp_rev: (u8, u8),         // major, minor
    pub size_bytes: u32,
    pub page_size: u32,
    pub erase_types: Vec<EraseType>,
    pub fast_read_114: bool,        // 1-1-4 fast read supported
    pub fast_read_144: bool,        // 1-4-4 fast read supported
    pub dtr_supported: bool,
    /// Chip erase typical time in ms from DW11 bits[28:24]/[30:29] (JESD216A+).
    /// None if the table is too short (pre-JESD216A) or field is zero.
    pub chip_erase_typ_ms: Option<u64>,
    /// JEDEC Basic Flash Parameter table size in bytes as reported by the chip.
    /// Genuine JESD216A+ chips report ≥36 bytes; older chips (e.g. early
    /// Macronix MX25L6406E) may report only 8 bytes (DW1+DW2: capabilities +
    /// density). Anything below 36 is treated as "partial" and we fall back to
    /// the DB for erase/page parameters.
    pub table_bytes: u32,
    /// True when the JEDEC Basic table was shorter than the JESD216 minimum
    /// of 9 dwords (36 bytes). Partial tables still carry valid density info.
    pub is_partial: bool,
}

pub async fn read_sfdp(dev: &UsbDevice) -> Result<SfdpInfo> {
    // Read SFDP header (8 bytes)
    let hdr = sfdp_read(dev, 0x000000, 8).await?;
    debug!("SFDP header: {:02x?}", hdr);

    if hdr[0..4] != SFDP_MAGIC {
        bail!(
            "SFDP magic not found: {:02x} {:02x} {:02x} {:02x} (expected 53 46 44 50)",
            hdr[0], hdr[1], hdr[2], hdr[3]
        );
    }

    let sfdp_minor = hdr[4];
    let sfdp_major = hdr[5];
    let nph = hdr[6]; // number of parameter headers - 1

    debug!("SFDP v{sfdp_major}.{sfdp_minor}, {nph} extra parameter headers");

    // Read all parameter headers (8 bytes each, first at offset 8)
    let ph_count = (nph as u32) + 1;
    let ph_data = sfdp_read(dev, 8, ph_count * 8).await?;

    // Find the JEDEC Basic Flash Parameter table
    let mut jedec_ptr: Option<(u32, u32)> = None; // (offset, len_dwords)
    for i in 0..ph_count as usize {
        let ph = &ph_data[i * 8..(i + 1) * 8];
        let id_lsb = ph[0];
        let id_msb = ph[7];
        let id = ((id_msb as u16) << 8) | (id_lsb as u16);
        let len_dwords = ph[3] as u32;
        let ptr = u32::from_le_bytes([ph[4], ph[5], ph[6], 0]);
        debug!("param header {i}: id={id:#06x} len={len_dwords} dwords ptr={ptr:#08x}");

        if id == JEDEC_BASIC_ID {
            jedec_ptr = Some((ptr, len_dwords));
        }
    }

    let (ptr, len_dwords) = jedec_ptr
        .ok_or_else(|| anyhow::anyhow!("JEDEC Basic Flash Parameter table not found in SFDP"))?;

    let table = sfdp_read(dev, ptr, len_dwords * 4).await?;
    debug!("JEDEC Basic table ({} bytes): {:02x?}", table.len(), table);

    parse_jedec_basic(&table, sfdp_major, sfdp_minor)
}

fn parse_jedec_basic(t: &[u8], sfdp_major: u8, sfdp_minor: u8) -> Result<SfdpInfo> {
    // DW1 (capabilities) + DW2 (density) is the minimum we need to do anything
    // useful. Early Macronix MX25L6406E parts report exactly 8 bytes here.
    if t.len() < 8 {
        bail!(
            "JEDEC Basic table too short for density: {} bytes (need at least 8)",
            t.len()
        );
    }

    let dw = |i: usize| -> u32 {
        let b = i * 4;
        u32::from_le_bytes([t[b], t[b + 1], t[b + 2], t[b + 3]])
    };

    // DW1
    let dw1 = dw(0);
    let fast_read_114 = (dw1 >> 22) & 1 == 1;
    let fast_read_144 = (dw1 >> 21) & 1 == 1;
    let dtr_supported  = (dw1 >> 19) & 1 == 1;

    // DW2: flash memory density
    let dw2 = dw(1);
    let size_bytes: u32 = if (dw2 >> 31) & 1 == 1 {
        // density field is 2^N bits; N is the exponent for the bit count
        let n = dw2 & 0x7FFF_FFFF;
        if n < 3 { bail!("SFDP density exponent {n} too small") }
        // shift in u64 to avoid overflow when n >= 35 (1u32 << 32+ panics/wraps)
        let size_bytes_u64: u64 = 1u64 << (n as u64 - 3); // bits → bytes
        if size_bytes_u64 > u32::MAX as u64 {
            bail!("SFDP reports chip size > 4GB ({size_bytes_u64} bytes): unsupported");
        }
        size_bytes_u64 as u32
    } else {
        // density field is (N+1) bits total; guard against all-ones corrupted word
        if dw2 == 0xFFFF_FFFF {
            bail!("SFDP density word is all-ones (corrupted or unprogrammed flash)");
        }
        // dw2+1 is safe now: dw2 <= 0xFFFF_FFFE so dw2+1 <= 0xFFFF_FFFF fits u32
        ((dw2 as u64 + 1) / 8) as u32
    };

    // DW8/DW9 (indices 7,8): erase types 1-4.
    // Each 16-bit field: low byte = size exponent (2^N bytes), high byte = opcode.
    let mut erase_types = Vec::new();
    if t.len() >= 36 {
        for (dw_idx, shift) in [(7usize, 0u32), (7, 16), (8, 0), (8, 16)] {
            let word = dw(dw_idx);
            let exp    = (word >> shift) & 0xFF ;
            let opcode = ((word >> (shift + 8)) & 0xFF) as u8;
            if opcode != 0x00 && exp != 0 && exp < 32 {
                erase_types.push(EraseType {
                    opcode,
                    size_bytes: 1u32 << exp,
                });
            }
        }
    }

    // DW11 (offset 40): page size + chip erase typical time (JESD216A+)
    // Bit layout per jesd216.c (Zephyr): page_size=[7:4], erase_count=[28:24], erase_units=[30:29]
    let (page_size, chip_erase_typ_ms) = if t.len() >= 44 {
        let dw11 = dw(10);
        let ps_exp = (dw11 >> 4) & 0x0F;
        let page_size = match ps_exp {
            0 => 256,            // not specified, use default
            1..=9 => 1u32 << ps_exp,  // 2..512 bytes — plausible range
            _ => {
                tracing::warn!("SFDP reports implausible page size exponent {ps_exp}, defaulting to 256");
                256
            }
        };

        let count = ((dw11 >> 24) & 0x1F) as u64;
        let unit_ms: u64 = match (dw11 >> 29) & 0x03 {
            0 => 16,
            1 => 256,
            2 => 4_000,
            _ => 64_000,
        };
        // count == 0 means "not specified" per JESD216A; avoid a bogus 16ms timeout.
        let chip_erase_typ_ms = if count == 0 {
            None // SFDP says "not specified" — use size-based fallback
        } else {
            Some((count + 1) * unit_ms)
        };

        (page_size, chip_erase_typ_ms)
    } else {
        (256, None)
    };

    let table_bytes = t.len() as u32;
    // JESD216 minimum basic table = 9 dwords (36 bytes). Pre-JESD216 parts
    // legitimately report less; we treat those as "partial" and lean on the
    // chip DB for erase/page parameters.
    let is_partial = (t.len() as u32) < 36;

    Ok(SfdpInfo {
        sfdp_rev: (sfdp_major, sfdp_minor),
        size_bytes,
        page_size,
        erase_types,
        fast_read_114,
        fast_read_144,
        dtr_supported,
        chip_erase_typ_ms,
        table_bytes,
        is_partial,
    })
}

/// Try SFDP — returns None if the chip doesn't respond or magic is invalid.
/// Demoted to `info` since many genuine pre-JESD216 chips legitimately lack
/// SFDP, and we don't want to spam users with WARN logs about it.
pub async fn try_read_sfdp(dev: &UsbDevice) -> Option<SfdpInfo> {
    soft_reset(dev).await;
    match read_sfdp(dev).await {
        Ok(info) => Some(info),
        Err(e) => {
            info!("SFDP not available: {e}");
            None
        }
    }
}

fn voltage_to_chip_voltage(voltage: Voltage) -> ChipVoltage {
    match voltage {
        Voltage::V1_8 => ChipVoltage::V1_8,
        Voltage::V3_3 | Voltage::V5_0 => ChipVoltage::V3_3,
    }
}

fn sfdp_erase_size(info: &SfdpInfo) -> u32 {
    info.erase_types
        .iter()
        .map(|e| e.size_bytes)
        .min()
        .unwrap_or(4096)
}

fn sfdp_addr_bytes(size_bytes: u32) -> u8 {
    if size_bytes > 0x100_0000 { 4 } else { 3 }
}

/// Build a `ResolvedChip` from SFDP data alone (no DB match).
pub fn sfdp_to_resolved(info: &SfdpInfo, rdid: [u8; 3], voltage: Voltage) -> ResolvedChip {
    let erase_size = sfdp_erase_size(info);
    let addr_bytes = sfdp_addr_bytes(info.size_bytes);
    let quad = info.fast_read_114 || info.fast_read_144;
    let chip_voltage = voltage_to_chip_voltage(voltage);

    let name = format!(
        "Unknown ({:#04x}:{:#04x}:{:#04x}) via SFDP",
        rdid[0], rdid[1], rdid[2]
    );

    info!(
        "SFDP: constructed chip — {} {} bytes page={} erase={} addr={}-byte quad={}",
        name, info.size_bytes, info.page_size, erase_size, addr_bytes, quad
    );

    // Guarantee at least one erase type so erase_unit can always find an opcode.
    let erase_types = if info.erase_types.is_empty() {
        // No SFDP erase table — synthesize conservative 4KB sector erase.
        vec![crate::chip::EraseType { size_bytes: 4096, opcode: 0x20 }]
    } else {
        info.erase_types.clone()
    };

    ResolvedChip {
        name,
        mfr: rdid[0],
        id1: rdid[1],
        id2: rdid[2],
        voltage: chip_voltage,
        size_bytes: info.size_bytes,
        page_size: info.page_size,
        erase_size,
        erase_types,
        addr_bytes,
        quad,
        source: ParamSource::Sfdp,
        chip_erase_max_ms: info.chip_erase_typ_ms.map(|t| t * 8),
    }
}

/// Build a `ResolvedChip` using DB name/voltage and SFDP geometry.
///
/// When SFDP reports a partial table (pre-JESD216A density-only), we trust the
/// DB for everything except the density cross-check. When the SFDP density
/// disagrees with the DB density we warn loudly — that's the real counterfeit
/// signal (RDID matches a known part but on-chip density says otherwise).
pub fn merge_db_with_sfdp(db: &SpiNorDef, info: &SfdpInfo) -> ResolvedChip {
    let density_matches_db = info.size_bytes == db.size_bytes;
    if !density_matches_db {
        warn!(
            "{} RDID matches DB but SFDP reports {} bytes (DB says {}) — \
             possible counterfeit or remarked chip; using SFDP geometry",
            db.name, info.size_bytes, db.size_bytes
        );
    }

    if info.is_partial {
        // Partial SFDP (pre-JESD216A, e.g. Macronix MX25L6406E rev 1.0):
        // only DW1+DW2 are trustworthy. Trust the DB for erase/page geometry
        // and only adopt the SFDP density when it agrees (avoids breaking a
        // good DB entry on a suspicious density mismatch).
        info!(
            "SFDP merge: {} DB={} bytes SFDP={} bytes ({} byte basic table) — \
             using DB params; SFDP density {}",
            db.name, db.size_bytes, info.size_bytes, info.table_bytes,
            if density_matches_db { "agrees" } else { "DISAGREES" }
        );

        let quad = info.fast_read_114 || info.fast_read_144 || db.quad;
        let size_bytes = if density_matches_db { info.size_bytes } else { db.size_bytes };
        let addr_bytes = sfdp_addr_bytes(size_bytes);
        let opcode = match db.erase_size {
            4_096              => 0x20,
            32_768             => 0x52,
            65_536..=131_072   => 0xD8,
            _                  => 0x20,
        };

        return ResolvedChip {
            name: db.name.clone(),
            mfr: db.mfr,
            id1: db.id1,
            id2: db.id2,
            voltage: db.voltage,
            size_bytes,
            page_size: db.page_size,
            erase_size: db.erase_size,
            erase_types: vec![EraseType { size_bytes: db.erase_size, opcode }],
            addr_bytes,
            quad,
            source: ParamSource::DatabaseWithPartialSfdp,
            chip_erase_max_ms: None,
        };
    }

    let erase_size = sfdp_erase_size(info);
    let addr_bytes = sfdp_addr_bytes(info.size_bytes);
    let quad = info.fast_read_114 || info.fast_read_144 || db.quad;

    info!(
        "SFDP merge: {} DB={} bytes SFDP={} bytes — using SFDP geometry",
        db.name, db.size_bytes, info.size_bytes
    );

    ResolvedChip {
        name: db.name.clone(),
        mfr: db.mfr,
        id1: db.id1,
        id2: db.id2,
        voltage: db.voltage,
        size_bytes: info.size_bytes,
        page_size: info.page_size,
        erase_size,
        erase_types: if info.erase_types.is_empty() {
            // Pre-JESD216A table: DW8/DW9 were not parsed — synthesize from DB erase_size.
            // Use conventional opcodes: 0x20 = 4KB sector, 0x52 = 32KB, 0xD8 = 64KB block.
            let opcode = match db.erase_size {
                4_096              => 0x20,
                32_768             => 0x52,
                65_536..=131_072   => 0xD8,
                _                  => 0x20,
            };
            vec![EraseType { size_bytes: db.erase_size, opcode }]
        } else {
            info.erase_types.clone()
        },
        addr_bytes,
        quad,
        source: ParamSource::DatabaseWithSfdp,
        chip_erase_max_ms: info.chip_erase_typ_ms.map(|t| t * 8),
    }
}
