use anyhow::{bail, Result};
use tracing::info;

use crate::db::{self, ChipVoltage, SpiNorDef};
use crate::fpga::Voltage;
use crate::usb::UsbDevice;

use super::bus::{spibus_read, spibus_write, ss_disable, ss_enable};

pub async fn detect(dev: &UsbDevice, voltage: Voltage) -> Result<Option<&'static SpiNorDef>> {
    let id = rdid(dev).await?;
    let (mfr, id1, id2) = (id[0], id[1], id[2]);
    info!("RDID: {mfr:#04x} {id1:#04x} {id2:#04x}");

    if mfr == 0xFF || mfr == 0x00 {
        return Ok(None);
    }

    let chip = match db::lookup(mfr, id1, id2)? {
        Some(c) => c,
        None => return Ok(None),
    };

    let expected = match chip.voltage {
        ChipVoltage::V1_8 => Voltage::V1_8,
        ChipVoltage::V3_3 => Voltage::V3_3,
    };
    if expected != voltage {
        bail!(
            "voltage mismatch: {} requires {:?} but target is {:?} — aborting to protect the chip",
            chip.name, expected, voltage
        );
    }

    Ok(Some(chip))
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
