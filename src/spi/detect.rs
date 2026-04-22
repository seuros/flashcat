use anyhow::{bail, Result};
use tracing::info;

use crate::db::{self, SpiNorDef};
use crate::usb::UsbDevice;

use super::bus::{spibus_read, spibus_write, ss_disable, ss_enable};

pub async fn detect(dev: &UsbDevice) -> Result<Option<&'static SpiNorDef>> {
    let id = rdid(dev).await?;
    let (mfr, id1, id2) = (id[0], id[1], id[2]);
    info!("RDID: {mfr:#04x} {id1:#04x} {id2:#04x}");

    if mfr == 0xFF || mfr == 0x00 {
        return Ok(None);
    }

    Ok(db::lookup(mfr, id1, id2)?)
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
