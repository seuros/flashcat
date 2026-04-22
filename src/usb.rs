use anyhow::{Context, Result};

mod device;
mod requests;

pub use device::UsbDevice;
pub use requests::UsbReq;

pub const VID_EC: u16 = 0x16C0;
pub const PID_CLASSIC: u16 = 0x05DE;
pub const PID_PRO: u16 = 0x05E0;
pub const PID_MACH1: u16 = 0x05E1;

pub async fn connect() -> Result<UsbDevice> {
    use crate::programmer::Programmer;

    let di = nusb::list_devices()
        .await?
        .find(|d| {
            d.vendor_id() == VID_EC
                && matches!(d.product_id(), p if p == PID_CLASSIC || p == PID_PRO || p == PID_MACH1)
        })
        .context("FlashcatUSB not found — check USB connection and udev rules")?;

    let kind = match di.product_id() {
        PID_PRO    => Programmer::Pro5,
        PID_MACH1  => Programmer::Mach1,
        _          => Programmer::Classic,
    };

    let device = di.open().await?;
    let iface = device.claim_interface(0).await?;

    // Pro/Mach1: bulk endpoints live in alternate setting 1
    if kind.has_fpga() {
        iface.set_alt_setting(1).await?;
    }

    Ok(UsbDevice { iface, kind })
}
