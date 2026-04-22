use anyhow::Result;
use tracing::info;

use crate::db::SpiNorDef;
use crate::usb::UsbDevice;

use super::bus::{spibus_write, ss_disable, ss_enable};
use super::write::{wait_wip_long, write_enable};

pub async fn erase_chip(dev: &UsbDevice, chip: &SpiNorDef) -> Result<()> {
    info!("chip erase: {} ({} bytes)", chip.name, chip.size_bytes);
    write_enable(dev).await?;
    ss_enable(dev).await?;
    spibus_write(dev, &[0xC7]).await?; // CE
    ss_disable(dev).await?;
    wait_wip_long(dev).await
}
