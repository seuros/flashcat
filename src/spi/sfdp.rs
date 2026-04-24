use anyhow::{bail, Result};
use tracing::{debug, info, warn};

use crate::chip::{EraseType, ParamSource, ResolvedChip};
use crate::db::{ChipVoltage, SpiNorDef};
use crate::fpga::Voltage;
use crate::spi::read::read_setup_packet;
use crate::usb::{UsbDevice, UsbReq};

const SFDP_MAGIC: [u8; 4] = *b"SFDP";
const JEDEC_BASIC_ID: u16 = 0xFF00;

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
    if t.len() < 16 {
        bail!("JEDEC Basic table too short: {} bytes", t.len());
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
    let size_bytes = if (dw2 >> 31) & 1 == 1 {
        // density field is 2^N bits
        let n = dw2 & 0x7FFF_FFFF;
        if n < 3 { bail!("SFDP density exponent {n} too small") }
        1u32 << (n - 3) // bits → bytes
    } else {
        // density field is N bits (N+1 bits total)
        (dw2 + 1) / 8
    };

    // DW8/DW9 (indices 7,8): erase types 1-4.
    // Each 16-bit field: low byte = size exponent (2^N bytes), high byte = opcode.
    let mut erase_types = Vec::new();
    if t.len() >= 36 {
        for (dw_idx, shift) in [(7usize, 0u32), (7, 16), (8, 0), (8, 16)] {
            let word = dw(dw_idx);
            let exp    = ((word >> shift) & 0xFF) as u32;
            let opcode = ((word >> (shift + 8)) & 0xFF) as u8;
            if opcode != 0x00 && exp != 0 {
                erase_types.push(EraseType {
                    opcode,
                    size_bytes: 1u32 << exp,
                });
            }
        }
    }

    // DW11 (offset 40): page size = 2^N bytes  (JESD216A+)
    let page_size = if t.len() >= 44 {
        let exp = (dw(10) >> 4) & 0x0F;
        if exp > 0 { 1u32 << exp } else { 256 }
    } else {
        256 // default per spec
    };

    Ok(SfdpInfo {
        sfdp_rev: (sfdp_major, sfdp_minor),
        size_bytes,
        page_size,
        erase_types,
        fast_read_114,
        fast_read_144,
        dtr_supported,
    })
}

/// Try SFDP — returns None if the chip doesn't respond or magic is invalid.
pub async fn try_read_sfdp(dev: &UsbDevice) -> Option<SfdpInfo> {
    match read_sfdp(dev).await {
        Ok(info) => Some(info),
        Err(e) => {
            warn!("SFDP not available: {e}");
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

    ResolvedChip {
        name,
        mfr: rdid[0],
        id1: rdid[1],
        id2: rdid[2],
        voltage: chip_voltage,
        size_bytes: info.size_bytes,
        page_size: info.page_size,
        erase_size,
        erase_types: info.erase_types.clone(),
        addr_bytes,
        quad,
        source: ParamSource::Sfdp,
    }
}

/// Build a `ResolvedChip` using DB name/voltage and SFDP geometry.
pub fn merge_db_with_sfdp(db: &SpiNorDef, info: &SfdpInfo) -> ResolvedChip {
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
        erase_types: info.erase_types.clone(),
        addr_bytes,
        quad,
        source: ParamSource::DatabaseWithSfdp,
    }
}
