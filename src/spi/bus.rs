use anyhow::Result;

use crate::usb::{UsbDevice, UsbReq};

// SPI_WR_DATA: ctrl_out arms transfer, bulk_out must follow without delay
pub(crate) async fn spibus_write(dev: &UsbDevice, data: &[u8]) -> Result<()> {
    dev.ctrl_out_nodelay(UsbReq::SpiWrData, data.len() as u32, None).await?;
    dev.bulk_out(data.to_vec()).await
}

// SPI_RD_DATA: ctrl_out arms transfer, bulk_in must follow without delay
pub(crate) async fn spibus_read(dev: &UsbDevice, len: usize) -> Result<Vec<u8>> {
    dev.ctrl_out_nodelay(UsbReq::SpiRdData, len as u32, None).await?;
    dev.bulk_in(len).await
}

pub(crate) async fn ss_enable(dev: &UsbDevice) -> Result<()> {
    dev.ctrl_out(UsbReq::SpiSsEnable, 0, None).await
}

pub(crate) async fn ss_disable(dev: &UsbDevice) -> Result<()> {
    dev.ctrl_out(UsbReq::SpiSsDisable, 0, None).await
}
