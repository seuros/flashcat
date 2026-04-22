use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tracing::info;

use crate::fpga::Voltage;
use crate::spi::SpiSpeed;
use crate::{spi, setup};

pub async fn cmd_write(
    voltage: Voltage,
    speed: SpiSpeed,
    file: PathBuf,
    offset: u32,
    verify: bool,
) -> Result<()> {
    let dev = setup(voltage, speed).await?;
    let chip = spi::detect(&dev).await?.context("no chip detected")?;
    let data =
        std::fs::read(&file).with_context(|| format!("failed to read {}", file.display()))?;

    if offset >= chip.size_bytes {
        bail!("offset {offset:#x} exceeds chip size {:#x}", chip.size_bytes);
    }
    let available = (chip.size_bytes - offset) as usize;
    if data.len() > available {
        bail!(
            "file ({} bytes) exceeds available space ({available} bytes at offset {offset:#x})",
            data.len()
        );
    }

    info!("writing {} bytes to {} at offset {offset:#010x}", data.len(), chip.name);
    spi::write(&dev, chip, offset, &data).await?;
    println!("Written {} bytes", data.len());

    if verify {
        info!("verifying...");
        let readback = spi::read(&dev, chip, offset, data.len() as u32).await?;
        if readback != data {
            let diffs = data.iter().zip(readback.iter()).filter(|(a, b)| a != b).count();
            bail!("verify failed — {diffs} bytes differ");
        }
        println!("Verify:  OK");
    }

    Ok(())
}
