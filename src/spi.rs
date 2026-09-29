use anyhow::{Context, Result};
use std::time::Duration;

use crate::usb::{UsbDevice, UsbReq};

pub(crate) mod bus;
pub(crate) mod detect;
mod erase;
pub mod lock;
pub mod otp;
mod probe;
pub mod protect;
mod quad;
pub(crate) mod read;
pub mod sfdp;
mod write;

pub(crate) use bus::{deep_power_down, release_deep_power_down};
pub use detect::detect;
pub use erase::{erase_chip, erase_range};
pub(crate) use erase::erase_unit_opcode;
pub use lock::{global_lock, global_unlock, lock_block, read_block_lock, unlock_block};
pub use otp::{otp_geometry, read_otp_locks, read_security_register};
pub use probe::auto_probe;
pub use protect::{protect_chip, read_wp_status, unprotect_chip};
pub use quad::{enable_quad, enter_4byte_mode, read_quad, sqi_setup};
pub use read::{majority_read, read};
pub use sfdp::read_sfdp;
pub use write::{write, write_smart};
pub(crate) use write::write_block;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpiSpeed(pub u8); // MHz

impl SpiSpeed {
    pub const MHZ_1: Self = Self(1);
    pub const MHZ_2: Self = Self(2);
    pub const MHZ_4: Self = Self(4);
    pub const MHZ_8: Self = Self(8);
    pub const MHZ_12: Self = Self(12);
    pub const MHZ_16: Self = Self(16);
    pub const MHZ_24: Self = Self(24);
    pub const MHZ_32: Self = Self(32); // max for Pro PCB5

    pub const ALL: &'static [Self] = &[
        Self::MHZ_1,
        Self::MHZ_2,
        Self::MHZ_4,
        Self::MHZ_8,
        Self::MHZ_12,
        Self::MHZ_16,
        Self::MHZ_24,
        Self::MHZ_32,
    ];
}

pub async fn init(dev: &UsbDevice, speed: SpiSpeed) -> Result<()> {
    if bus::use_sqi(dev) {
        // Mach1 generic bitstream: flash access goes through the SQI engine.
        // Configure its clock divisor instead of the plain SPI engine.
        quad::sqi_setup(dev, speed.0).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        release_deep_power_down(dev).await?;
        return Ok(());
    }

    // Match the vendor host's normal SPI init path:
    // USB_SPI_INIT((mode << 16) | speed_mhz)
    //
    // The high CS/select byte is used by the separate SSPI path for FPGA
    // configuration, not by regular SPI-NOR access. Setting it here can route
    // transactions to the wrong target and produce all-0x00 / no-detect reads.
    let w32: u32 = speed.0 as u32;
    dev.ctrl_out(UsbReq::SpiInit, w32, None)
        .await
        .context("SPI_INIT failed")?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Wake the chip if a prior session left it in Deep Power-Down. Only 0xAB
    // releases DPD; without this, the first RDID of every new invocation reads
    // blank → "no chip detected" whenever residual power kept the part asleep
    // across the VCC cut. No-op on an already-awake chip.
    release_deep_power_down(dev).await?;
    Ok(())
}
