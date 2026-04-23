use anyhow::{bail, Context, Result};
use tracing::info;

use crate::fpga::Voltage;
use crate::spi::SpiSpeed;
use crate::{spi, setup};

pub async fn cmd_erase(
    voltage: Voltage,
    speed: SpiSpeed,
    offset: Option<u32>,
    length: Option<u32>,
) -> Result<()> {
    let dev = setup(voltage, speed).await?;
    let chip = spi::detect(&dev, voltage).await?.context("no chip detected")?;

    match (offset, length) {
        (None, None) => {
            info!("chip erase: {} — this may take up to 200 seconds", chip.name);
            spi::erase_chip(&dev, chip).await?;
            println!("Erased (chip)");
        }
        (off, len) => {
            let off = off.unwrap_or(0);
            if off >= chip.size_bytes {
                bail!("offset {off:#x} exceeds chip size {:#x}", chip.size_bytes);
            }
            let max_len = chip.size_bytes - off;
            let len = match len {
                Some(l) if l > max_len => {
                    bail!("length {l:#x} exceeds available space {max_len:#x} at offset {off:#x}")
                }
                Some(l) => l,
                None => max_len,
            };
            info!("range erase: {off:#010x}..{:#010x}", off + len);
            spi::erase_range(&dev, chip, off, len).await?;
            println!("Erased {} bytes at {off:#010x}", len);
        }
    }

    Ok(())
}
