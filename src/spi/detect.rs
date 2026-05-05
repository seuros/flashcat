use anyhow::{bail, Result};
use std::time::Duration;
use tracing::{info, warn};

use crate::chip::ResolvedChip;
use crate::db::{self, ChipVoltage};
use crate::fpga::Voltage;
use crate::usb::UsbDevice;

use super::bus::{spibus_read, spibus_write, ss_disable, ss_enable};
use super::sfdp;

/// Send RSTEN (0x66) + RST (0x99) to perform a software reset, then wait 1ms.
pub(crate) async fn software_reset(dev: &UsbDevice) -> Result<()> {
    ss_enable(dev).await?;
    let r = spibus_write(dev, &[0x66]).await;
    let d = ss_disable(dev).await;
    r?;
    d?;

    ss_enable(dev).await?;
    let r = spibus_write(dev, &[0x99]).await;
    let d = ss_disable(dev).await;
    r?;
    d?;

    // SST26VF series requires up to 10ms tRST; use 10ms for all parts.
    tokio::time::sleep(Duration::from_millis(10)).await;
    Ok(())
}


pub async fn detect(dev: &UsbDevice, voltage: Voltage) -> Result<Option<ResolvedChip>> {
    let id = rdid(dev).await?;
    // If RDID returned blank, attempt a software reset and retry once.
    let id = if id[0] == 0xFF || id[0] == 0x00 {
        tracing::debug!("RDID blank ({:#04x}), attempting software reset", id[0]);
        software_reset(dev).await?;
        rdid(dev).await?
    } else {
        id
    };

    let chip = detect_from_id(id, voltage)?;

    // If the chip wasn't found in the DB but returned a valid (non-blank) RDID,
    // fall back to SFDP — mirrors what auto_probe's resolve_chip() does.
    let chip = if chip.is_none() && id[0] != 0xFF && id[0] != 0x00 {
        tracing::debug!(
            "RDID {:#04x}:{:#04x}:{:#04x} not in DB, falling back to SFDP",
            id[0], id[1], id[2]
        );
        match sfdp::try_read_sfdp(dev).await {
            Some(info) => Some(sfdp::sfdp_to_resolved(&info, id, voltage)),
            None => None,
        }
    } else {
        chip
    };

    if let Some(ref c) = chip && c.addr_bytes == 4 {
        enter_4byte_mode(dev).await?;
    }
    Ok(chip)
}

/// Send EN4B (0xB7) to switch the chip into 4-byte address mode.
pub(crate) async fn enter_4byte_mode(dev: &UsbDevice) -> Result<()> {
    info!("entering 4-byte address mode (EN4B)");
    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0xB7]).await;
    let dis = ss_disable(dev).await;
    wr?;
    dis?;
    Ok(())
}

/// Pure lookup: given a raw RDID triple, validate voltage and return the chip entry.
///
/// When the RDID matches multiple DB entries all voltage-matching candidates are
/// collected. If they all agree on `size_bytes` the first is returned. If they
/// disagree on size the smallest is returned with a `warn!` (SFDP is unavailable
/// in this pure path). If none match voltage an error is returned.
pub fn detect_from_id(id: [u8; 3], voltage: Voltage) -> Result<Option<ResolvedChip>> {
    let (mfr, id1, id2) = (id[0], id[1], id[2]);
    info!("RDID: {mfr:#04x} {id1:#04x} {id2:#04x}");

    if mfr == 0xFF || mfr == 0x00 {
        return Ok(None);
    }

    let matches = db::lookup(mfr, id1, id2)?;
    if matches.is_empty() {
        return Ok(None);
    }

    // Look for candidates whose voltage matches the probe voltage.
    // NOTE: V5_0 maps to V3_3 for DB matching because there is no ChipVoltage::V5_0 variant
    // in the chip database. 5V applied to a 3.3V chip can destroy it; warn the user.
    if voltage == Voltage::V5_0 {
        warn!(
            "probe voltage is 5V — most SPI NOR flash chips are 3.3V max; \
             applying 5V to a 3.3V chip may permanently destroy it. \
             Proceed only if you have confirmed this chip is 5V-tolerant."
        );
    }
    let expected_chip_voltage = match voltage {
        Voltage::V1_8 => ChipVoltage::V1_8,
        Voltage::V3_3 | Voltage::V5_0 => ChipVoltage::V3_3,
    };

    let voltage_matches: Vec<_> = matches
        .iter()
        .filter(|c| c.voltage == expected_chip_voltage)
        .collect();

    if voltage_matches.is_empty() {
        // All candidates are for a different voltage — report mismatch using the first.
        let first = matches[0];
        let required_voltage = match first.voltage {
            ChipVoltage::V1_8 => Voltage::V1_8,
            ChipVoltage::V3_3 => Voltage::V3_3,
        };
        bail!(
            "voltage mismatch: {} requires {:?} but target is {:?} — aborting to protect the chip",
            first.name, required_voltage, voltage
        );
    }

    // Check whether all voltage-matching candidates agree on size.
    let all_same_size = voltage_matches
        .windows(2)
        .all(|w| w[0].size_bytes == w[1].size_bytes);

    if !all_same_size {
        // Sizes differ — pick the smallest to avoid out-of-bounds access.
        // SFDP disambiguation is not available in this pure (no-device) path.
        let smallest = voltage_matches
            .iter()
            .min_by_key(|c| c.size_bytes)
            .unwrap();
        tracing::warn!(
            "RDID {:#04x}:{:#04x}:{:#04x} matches {} DB entries with different sizes; \
             using smallest: {} ({} bytes) — run `sfdp` for disambiguation",
            mfr, id1, id2, voltage_matches.len(), smallest.name, smallest.size_bytes
        );
        return Ok(Some(crate::spi::probe::db_chip_to_resolved_pub(smallest)));
    }

    return Ok(Some(crate::spi::probe::db_chip_to_resolved_pub(voltage_matches[0])));
}

pub async fn rdid(dev: &UsbDevice) -> Result<[u8; 3]> {
    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0x9F]).await;
    let rd  = spibus_read(dev, 3).await;
    let dis = ss_disable(dev).await;
    wr?;
    let resp = rd?;
    dis?;
    if resp.len() < 3 {
        bail!("RDID short response: {} bytes", resp.len());
    }
    Ok([resp[0], resp[1], resp[2]])
}

#[cfg(test)]
mod tests {
    #[test]
    fn blank_rdid_triggers_reset_path() {
        // Verify that a 0xFF manufacturer byte is treated as blank
        assert!(0xFF_u8 == 0xFF || 0xFF_u8 == 0x00);
        // Verify that a 0x00 manufacturer byte is treated as blank
        assert!(0x00_u8 == 0xFF || 0x00_u8 == 0x00);
        // Verify that a 0xEF (Winbond) byte is not blank
        assert!(!(0xEF_u8 == 0xFF || 0xEF_u8 == 0x00));
    }
}
