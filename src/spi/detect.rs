use anyhow::{bail, Result};
use tracing::info;

use crate::chip::ResolvedChip;
use crate::db::{self, ChipVoltage};
use crate::fpga::Voltage;
use crate::usb::UsbDevice;

use super::bus::{spibus_read, spibus_write, ss_disable, ss_enable};

pub async fn detect(dev: &UsbDevice, voltage: Voltage) -> Result<Option<ResolvedChip>> {
    let id = rdid(dev).await?;
    let chip = detect_from_id(id, voltage)?;
    if let Some(ref c) = chip {
        if c.addr_bytes == 4 {
            enter_4byte_mode(dev).await?;
        }
    }
    Ok(chip)
}

/// Send EN4B (0xB7) to switch the chip into 4-byte address mode.
pub(crate) async fn enter_4byte_mode(dev: &UsbDevice) -> Result<()> {
    info!("entering 4-byte address mode (EN4B)");
    ss_enable(dev).await?;
    spibus_write(dev, &[0xB7]).await?;
    ss_disable(dev).await?;
    Ok(())
}

/// Pure lookup: given a raw RDID triple, validate voltage and return the chip entry.
///
/// When the RDID matches multiple DB entries, the first one whose voltage matches
/// is returned. If none match voltage, an error is returned for the first candidate.
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

    // Look for a candidate whose voltage matches the probe voltage.
    let expected_chip_voltage = match voltage {
        Voltage::V1_8 => ChipVoltage::V1_8,
        Voltage::V3_3 | Voltage::V5_0 => ChipVoltage::V3_3,
    };

    if let Some(chip) = matches.iter().find(|c| c.voltage == expected_chip_voltage) {
        return Ok(Some(crate::spi::probe::db_chip_to_resolved_pub(chip)));
    }

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

pub async fn rdid(dev: &UsbDevice) -> Result<[u8; 3]> {
    ss_enable(dev).await?;
    spibus_write(dev, &[0x9F]).await?;
    let resp = spibus_read(dev, 3).await?;
    ss_disable(dev).await?;

    if resp.len() < 3 {
        bail!("RDID short response: {} bytes", resp.len());
    }
    Ok([resp[0], resp[1], resp[2]])
}
