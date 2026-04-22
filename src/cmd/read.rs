use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tracing::info;

use crate::fpga::Voltage;
use crate::spi::SpiSpeed;
use crate::{spi, setup};

pub async fn cmd_read(
    voltage: Voltage,
    speed: SpiSpeed,
    file: PathBuf,
    offset: u32,
    length: Option<u32>,
) -> Result<()> {
    let dev = setup(voltage, speed).await?;
    let chip = spi::detect(&dev).await?.context("no chip detected")?;

    // validate range
    if offset >= chip.size_bytes {
        bail!("offset {offset:#x} exceeds chip size {:#x}", chip.size_bytes);
    }
    let max_len = chip.size_bytes - offset;
    let len = match length {
        Some(l) if l > max_len => {
            bail!("length {l:#x} exceeds available space {max_len:#x} at offset {offset:#x}")
        }
        Some(l) => l,
        None => max_len,
    };

    info!("reading {} bytes from {} (offset {offset:#010x})", len, chip.name);
    let data = spi::read(&dev, chip, offset, len).await?;
    std::fs::write(&file, &data).with_context(|| format!("failed to write {}", file.display()))?;
    println!("Saved {} bytes → {}", data.len(), file.display());
    Ok(())
}
