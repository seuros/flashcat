use anyhow::{Context, Result};
use std::time::Duration;

use crate::usb::{UsbDevice, UsbReq};

mod bus;
mod detect;
mod erase;
mod read;
mod write;

pub use detect::{detect, rdid};
pub use erase::erase_chip;
pub use read::read;
pub use write::write;

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
    // (cs=1 << 24) | (mode=0 << 16) | speed
    let w32: u32 = (1u32 << 24) | (0u32 << 16) | speed.0 as u32;
    dev.ctrl_out(UsbReq::SpiInit, w32, None)
        .await
        .context("SPI_INIT failed")?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(())
}
