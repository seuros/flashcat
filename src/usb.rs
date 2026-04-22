use anyhow::{Context, Result};

mod device;
mod requests;

pub use device::UsbDevice;
pub use requests::UsbReq;

pub const VID_EC: u16 = 0x16C0;
pub const PID_PRO: u16 = 0x05E0;
pub const PID_MACH1: u16 = 0x05E1;

pub async fn connect() -> Result<UsbDevice> {
    let di = nusb::list_devices()
        .await?
        .find(|d| d.vendor_id() == VID_EC && (d.product_id() == PID_PRO || d.product_id() == PID_MACH1))
        .context("FlashcatUSB Pro not found — check USB connection and udev rules")?;

    let has_logic = matches!(di.product_id(), p if p == PID_PRO || p == PID_MACH1);
    let device = di.open().await?;
    let iface = device.claim_interface(0).await?;
    // bulk endpoints (0x81, 0x02) live in alternate setting 1
    iface.set_alt_setting(1).await?;

    Ok(UsbDevice { iface, has_logic })
}
