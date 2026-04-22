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
    quad: bool,
) -> Result<()> {
    let dev = setup(voltage, speed).await?;
    let chip = spi::detect(&dev, voltage).await?.context("no chip detected")?;

    if quad && !chip.quad {
        bail!("{} does not support Quad SPI reads", chip.name);
    }
    if quad && dev.kind != crate::programmer::Programmer::Mach1 {
        bail!("--quad requires Mach1 hardware — Pro PCB5 does not route IO2/IO3 to the chip socket");
    }

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

    let data = if quad {
        info!("quad SPI mode: enabling QE bit and using SqiRdFlash");
        spi::enable_quad(&dev).await?;
        spi::sqi_setup(&dev, speed.0).await?;
        spi::read_quad(&dev, chip, offset, len).await?
    } else {
        spi::read(&dev, chip, offset, len).await?
    };

    std::fs::write(&file, &data).with_context(|| format!("failed to write {}", file.display()))?;
    println!("Saved {} bytes → {}", data.len(), file.display());
    Ok(())
}
