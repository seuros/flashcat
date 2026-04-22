use anyhow::{Context, Result};
use tracing::info;

use crate::fpga::Voltage;
use crate::spi::SpiSpeed;
use crate::{spi, setup};

pub async fn cmd_erase(voltage: Voltage, speed: SpiSpeed) -> Result<()> {
    let dev = setup(voltage, speed).await?;
    let chip = spi::detect(&dev).await?.context("no chip detected")?;
    info!("erasing {} — this may take up to 200 seconds", chip.name);
    spi::erase_chip(&dev, chip).await?;
    println!("Erased");
    Ok(())
}
